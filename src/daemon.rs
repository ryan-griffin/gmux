//! Headless owner of PTYs, terminal engines, and the reconnectable live layout.
use crate::{
    Cell, Frame,
    control::{Command, CommandOutput, CommandReply, CommandResult},
    ipc,
    model::{self, CursorStyle, Layout, SavedTab, Session, Settings, TabFilter, WorkspaceCatalog},
    session::TerminalSession,
    theme::Theme,
    transport::{self, Header, Metadata, Packet},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 32 * 1024 * 1024;
const MAX_NOTIFICATIONS: usize = 64;

fn poll_timeout(deadline: Option<Instant>, now: Instant) -> i32 {
    deadline.map_or(-1, |deadline| {
        // A positive fraction of a millisecond must still sleep, not spin.
        deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32
    })
}
#[derive(Default)]
struct SentScreen {
    frame: Frame,
    cells: Vec<Cell>,
}
struct Frontend {
    socket: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    attached: bool,
    visible: HashSet<u64>,
    screens: HashMap<u64, SentScreen>,
    metadata: HashMap<u64, Metadata>,
    scroll_serials: HashMap<u64, u64>,
    sent_scroll_serials: HashMap<u64, u64>,
    find: HashMap<u64, crate::search::FindState>,
    selections: HashMap<u64, Option<(u64, u64)>>,
    discarding: bool,
    paste: Option<Paste>,
}
struct Paste {
    pane: u64,
    size: usize,
    text: String,
    request: Value,
}
impl Frontend {
    fn queue(&mut self, packet: Packet) -> anyhow::Result<()> {
        // Compact after consuming at least half the buffer, rather than moving
        // pending packets for every partial socket write.
        if self.offset > 0
            && (self.offset >= self.output.len() / 2 || self.output.len() >= MAX_PENDING)
        {
            self.output.drain(..self.offset);
            self.offset = 0;
        }
        let start = self.output.len();
        packet.encode_into(&mut self.output)?;
        if self.output.len() - self.offset > MAX_PENDING {
            self.output.truncate(start);
            anyhow::bail!("Frontend is too slow");
        }
        Ok(())
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        while self.offset < self.output.len() {
            match self.socket.write(&self.output[self.offset..]) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => self.offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if self.offset == self.output.len() {
            self.output.clear();
            self.offset = 0;
        }
        Ok(())
    }
    fn requests(&mut self) -> anyhow::Result<Vec<Value>> {
        let mut buffer = [0; 16384];
        let mut requests = Vec::new();
        for _ in 0..32 {
            let n = match self.socket.read(&mut buffer) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            for part in buffer[..n].split_inclusive(|c| *c == b'\n') {
                let complete = part.ends_with(b"\n");
                if self.discarding {
                    self.discarding = !complete;
                    continue;
                }
                if self.input.len() + part.len() > 1_048_577 {
                    self.input.clear();
                    self.discarding = !complete;
                    requests.push(
                        json!({"method":"_invalid", "error":"Frontend command is too large"}),
                    );
                    continue;
                }
                self.input.extend_from_slice(part);
                if complete {
                    requests.push(serde_json::from_slice(&self.input).unwrap_or_else(
                        |_| json!({"method":"_invalid", "error":"Invalid frontend command"}),
                    ));
                    self.input.clear();
                }
            }
        }
        Ok(requests)
    }
}

struct Daemon {
    sessions: HashMap<u64, TerminalSession>,
    state: Session,
    frontend: Option<Frontend>,
    changed: HashSet<u64>,
    notifications: HashMap<u64, VecDeque<String>>,
    clipboard: HashMap<u64, String>,
    errors: HashMap<u64, String>,
    unread: HashMap<u64, u64>,
    alert_sequence: u64,
    quitting: bool,
    snapshots: u64,
    sent_rows: u64,
    started: Instant,
}
impl Daemon {
    fn catalog() -> anyhow::Result<WorkspaceCatalog> {
        let mut catalog = model::load::<WorkspaceCatalog>("workspaces.json")?;
        let mut used = HashSet::new();
        catalog.workspaces.retain(|workspace| {
            workspace.id > 0
                && workspace.id < u64::MAX
                && !workspace.name.trim().is_empty()
                && used.insert(workspace.id)
        });
        Ok(catalog)
    }
    fn new() -> anyhow::Result<Self> {
        let settings = model::load::<Settings>("settings.json")?;
        let saved = if settings.restore {
            model::load::<Session>("session.json")?
        } else {
            Session::default()
        };
        let mut daemon = Self {
            sessions: HashMap::new(),
            state: Session {
                width: saved.width,
                height: saved.height,
                maximized: saved.maximized,
                filter: saved.filter,
                active: saved.active,
                ..Session::default()
            },
            frontend: None,
            changed: HashSet::new(),
            notifications: HashMap::new(),
            clipboard: HashMap::new(),
            errors: HashMap::new(),
            unread: HashMap::new(),
            alert_sequence: 0,
            quitting: false,
            snapshots: 0,
            sent_rows: 0,
            started: Instant::now(),
        };
        let catalog = Self::catalog()?;
        let mut used: HashSet<_> = catalog.workspaces.iter().map(|w| w.id).collect();
        for tab in saved.tabs {
            if let Err(e) = tab.validate() {
                eprintln!("Skipping invalid saved tab: {e}");
                continue;
            }
            let ids = tab.layout.ids();
            if used.contains(&tab.id) || ids.iter().any(|id| used.contains(id)) {
                eprintln!("Skipping duplicate saved IDs");
                continue;
            }
            let mut spawned = Vec::new();
            let result = (|| -> anyhow::Result<()> {
                for id in &ids {
                    let directory = tab
                        .directories
                        .get(id)
                        .filter(|p| p.is_dir())
                        .cloned()
                        .unwrap_or_else(model::home);
                    let session = TerminalSession::spawn(
                        *id,
                        tab.id,
                        directory,
                        settings.cursor_style,
                        &Theme::adwaita(settings.appearance != "light"),
                    )?;
                    daemon.sessions.insert(*id, session);
                    spawned.push(*id);
                }
                Ok(())
            })();
            if let Err(e) = result {
                for id in spawned {
                    daemon.sessions.remove(&id);
                }
                eprintln!("Could not restore tab: {e}");
                continue;
            }
            used.insert(tab.id);
            used.extend(ids);
            daemon.state.tabs.push(tab);
        }
        Ok(daemon)
    }
    fn metadata(session: &TerminalSession, unread: u64) -> Metadata {
        Metadata {
            id: session.id(),
            directory: session.directory().to_path_buf(),
            title: session.title(),
            size: session.size(),
            exited: session.exited(),
            unread,
            busy: session.busy(),
            modes: [session.mode(0), session.mode(1004)],
        }
    }
    fn queue_notifications(&mut self, id: u64, messages: impl IntoIterator<Item = String>) {
        let pending = self.notifications.entry(id).or_default();
        for message in messages {
            if pending.len() == MAX_NOTIFICATIONS {
                pending.pop_front();
            }
            pending.push_back(message);
            self.alert_sequence = self.alert_sequence.wrapping_add(1).max(1);
            self.unread.insert(id, self.alert_sequence);
        }
    }
    fn pane_id(&self, request: &Value) -> anyhow::Result<u64> {
        self.resolve_pane(request["pane"].as_u64())
    }
    fn resolve_pane(&self, pane: Option<u64>) -> anyhow::Result<u64> {
        let id = pane
            .or_else(|| {
                self.state
                    .tabs
                    .iter()
                    .find(|t| t.id == self.state.active)
                    .map(|t| t.active)
            })
            .ok_or_else(|| anyhow::anyhow!("No active terminal"))?;
        anyhow::ensure!(self.sessions.contains_key(&id), "Pane {id} does not exist");
        Ok(id)
    }
    fn next(&self) -> anyhow::Result<u64> {
        self.state
            .tabs
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.layout.ids()))
            .chain(self.sessions.keys().copied())
            .chain(Self::catalog()?.workspaces.iter().map(|w| w.id))
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("No IDs remain"))
    }
    fn select_visible(&mut self) {
        if !self
            .state
            .tabs
            .iter()
            .any(|tab| tab.id == self.state.active && self.state.filter.matches(tab.workspace))
        {
            self.state.active = self
                .state
                .tabs
                .iter()
                .find(|tab| self.state.filter.matches(tab.workspace))
                .map(|tab| tab.id)
                .unwrap_or(0);
        }
    }
    fn close(&mut self, id: u64) {
        let active_index = self
            .state
            .tabs
            .iter()
            .filter(|tab| self.state.filter.matches(tab.workspace))
            .position(|tab| tab.id == self.state.active);
        self.sessions.remove(&id);
        self.unread.remove(&id);
        self.notifications.remove(&id);
        self.clipboard.remove(&id);
        self.errors.remove(&id);
        self.changed.remove(&id);
        if let Some(frontend) = &mut self.frontend {
            frontend.visible.remove(&id);
            if frontend
                .paste
                .as_ref()
                .is_some_and(|paste| paste.pane == id)
            {
                frontend.paste = None;
            }
            frontend.screens.remove(&id);
            frontend.metadata.remove(&id);
            frontend.scroll_serials.remove(&id);
            frontend.sent_scroll_serials.remove(&id);
            frontend.find.remove(&id);
            frontend.selections.remove(&id);
        }
        self.state.tabs.retain_mut(|tab| {
            if !tab.layout.ids().contains(&id) {
                return true;
            }
            let Some(layout) = tab.layout.clone().remove(id) else {
                return false;
            };
            if tab.active == id {
                let old = tab.layout.ids();
                let index = old.iter().position(|pane| *pane == id).unwrap_or(0);
                let remaining = layout.ids();
                tab.active = remaining[index.min(remaining.len() - 1)];
            }
            tab.layout = layout;
            tab.directories.remove(&id);
            true
        });
        if !self
            .state
            .tabs
            .iter()
            .any(|tab| tab.id == self.state.active)
        {
            let visible: Vec<_> = self
                .state
                .tabs
                .iter()
                .filter(|tab| self.state.filter.matches(tab.workspace))
                .map(|tab| tab.id)
                .collect();
            self.state.active = visible
                .get(
                    active_index
                        .unwrap_or(0)
                        .min(visible.len().saturating_sub(1)),
                )
                .copied()
                .unwrap_or(0);
        }
        self.select_visible();
    }
    fn refresh_state(&mut self) {
        for tab in &mut self.state.tabs {
            for id in tab.layout.ids() {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.refresh_directory();
                    tab.directories
                        .insert(id, session.directory().to_path_buf());
                }
            }
        }
    }
    fn save(&mut self) -> anyhow::Result<()> {
        self.refresh_state();
        model::save("session.json", &self.state)
    }
    fn save_result(&mut self, mut result: Value) -> Value {
        if let Err(error) = self.save() {
            result["warning"] = json!(format!("Could not save layout: {error}"));
        }
        result
    }
    fn list(&mut self) -> anyhow::Result<Value> {
        self.refresh_state();
        let catalog = Self::catalog()?;
        Ok(
            json!({"active_tab":(self.state.active!=0).then_some(self.state.active),"filter":self.state.filter,"tab_search":"","workspaces":catalog.workspaces.iter().map(|w|json!({"id":w.id,"name":w.name,"directory":w.directory,"tabs":self.state.tabs.iter().filter(|t|t.workspace==Some(w.id)).count(),"unread":self.state.tabs.iter().any(|t|t.workspace==Some(w.id)&&t.layout.ids().iter().any(|id|self.unread.contains_key(id)))})).collect::<Vec<_>>(),"tabs":self.state.tabs.iter().map(|t| json!({"id":t.id,"name":t.name,"workspace":t.workspace,"visible":self.state.filter.matches(t.workspace),"active_pane":t.active,"unread":t.layout.ids().iter().any(|id|self.unread.contains_key(id)),"layout":t.layout,"panes":t.layout.ids().iter().filter_map(|id|self.sessions.get(id)).map(|s|json!({"id":s.id(),"title":s.title(),"directory":s.directory(),"exited":s.exited(),"unread":self.unread.contains_key(&s.id())})).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
        )
    }
    fn saved_result(&mut self, output: CommandOutput) -> CommandResult {
        CommandResult {
            output,
            warning: self
                .save()
                .err()
                .map(|error| format!("Could not save layout: {error}")),
        }
    }
    fn execute(&mut self, command: Command) -> anyhow::Result<CommandResult> {
        let mut catalog = Self::catalog()?;
        match command {
            Command::New {
                directory,
                name,
                workspace,
            } => {
                let workspace = workspace.unwrap_or(match self.state.filter {
                    TabFilter::Workspace(id) => Some(id),
                    _ => None,
                });
                let definition = workspace
                    .map(|id| {
                        catalog
                            .workspaces
                            .iter()
                            .find(|w| w.id == id)
                            .ok_or_else(|| anyhow::anyhow!("Workspace {id} does not exist"))
                    })
                    .transpose()?;
                let directory = directory
                    .or_else(|| definition.map(|w| w.directory.clone()))
                    .unwrap_or_else(model::home);
                anyhow::ensure!(
                    directory.is_dir(),
                    "{} is not a directory",
                    directory.display()
                );
                let tab = self.next()?;
                let pane = tab
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("No IDs remain"))?;
                let settings = model::load::<Settings>("settings.json")?;
                let session = TerminalSession::spawn(
                    pane,
                    tab,
                    directory.clone(),
                    settings.cursor_style,
                    &Theme::adwaita(settings.appearance != "light"),
                )?;
                self.sessions.insert(pane, session);
                let name = name.filter(|n| !n.trim().is_empty());
                self.state.tabs.insert(
                    0,
                    SavedTab {
                        id: tab,
                        name: name
                            .clone()
                            .unwrap_or_else(|| model::directory_name(&directory)),
                        custom_name: name.is_some(),
                        workspace,
                        layout: Layout::Pane(pane),
                        directories: HashMap::from([(pane, directory)]),
                        active: pane,
                    },
                );
                self.state.active = tab;
                Ok(self.saved_result(CommandOutput::New {
                    tab,
                    pane,
                    workspace,
                }))
            }
            Command::Split { pane, down } => {
                let target = self.resolve_pane(pane)?;
                let tab = self
                    .state
                    .tabs
                    .iter()
                    .find(|t| t.layout.ids().contains(&target))
                    .ok_or_else(|| anyhow::anyhow!("Pane has no tab"))?;
                anyhow::ensure!(
                    tab.layout.ids().len() < 24,
                    "A tab can have at most 24 panes"
                );
                let tab_id = tab.id;
                self.sessions.get_mut(&target).unwrap().refresh_directory();
                let directory = self.sessions[&target].directory().to_path_buf();
                let pane = self.next()?;
                let settings = model::load::<Settings>("settings.json")?;
                let session = TerminalSession::spawn(
                    pane,
                    tab_id,
                    directory.clone(),
                    settings.cursor_style,
                    &Theme::adwaita(settings.appearance != "light"),
                )?;
                self.sessions.insert(pane, session);
                let tab = self.state.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
                tab.layout.split(target, pane, down);
                tab.active = pane;
                tab.directories.insert(pane, directory);
                self.state.active = tab_id;
                Ok(self.saved_result(CommandOutput::Split { pane }))
            }
            Command::Close { pane } => {
                let pane = self.resolve_pane(pane)?;
                self.close(pane);
                Ok(self.saved_result(CommandOutput::Done { ok: true }))
            }
            Command::CloseTab { tab } => {
                let panes = self
                    .state
                    .tabs
                    .iter()
                    .find(|t| t.id == tab)
                    .ok_or_else(|| anyhow::anyhow!("Tab {tab} does not exist"))?
                    .layout
                    .ids();
                for pane in panes {
                    self.close(pane);
                }
                Ok(self.saved_result(CommandOutput::Done { ok: true }))
            }
            Command::Rename { tab, name } => {
                let name = name.trim();
                anyhow::ensure!(!name.is_empty(), "Enter a tab name");
                let tab = self
                    .state
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == tab)
                    .ok_or_else(|| anyhow::anyhow!("Tab {tab} does not exist"))?;
                tab.name = name.to_owned();
                tab.custom_name = true;
                Ok(self.saved_result(CommandOutput::Done { ok: true }))
            }
            Command::WorkspaceNew {
                name,
                directory,
                tab,
            } => {
                let directory = directory.unwrap_or_else(model::home);
                anyhow::ensure!(directory.is_dir(), "Choose an existing directory");
                let name = catalog.validate_name(&name, None)?;
                if let Some(id) = tab {
                    anyhow::ensure!(
                        self.state.tabs.iter().any(|t| t.id == id),
                        "The tab has been closed"
                    );
                }
                let workspace = self.next()?;
                catalog.workspaces.push(model::WorkspaceDefinition {
                    id: workspace,
                    name,
                    directory,
                });
                model::save("workspaces.json", &catalog)?;
                let output = CommandOutput::Workspace { workspace };
                if let Some(id) = tab {
                    self.state
                        .tabs
                        .iter_mut()
                        .find(|t| t.id == id)
                        .unwrap()
                        .workspace = Some(workspace);
                    self.select_visible();
                    return Ok(self.saved_result(output));
                }
                Ok(output.into())
            }
            Command::WorkspaceEdit {
                workspace,
                name,
                directory,
            } => {
                let name = catalog.validate_name(&name, Some(workspace))?;
                anyhow::ensure!(directory.is_dir(), "Choose an existing directory");
                let definition = catalog
                    .workspaces
                    .iter_mut()
                    .find(|w| w.id == workspace)
                    .ok_or_else(|| anyhow::anyhow!("Workspace no longer exists"))?;
                definition.name = name;
                definition.directory = directory;
                model::save("workspaces.json", &catalog)?;
                Ok(CommandOutput::Workspace { workspace }.into())
            }
            Command::WorkspaceDelete { workspace } => {
                anyhow::ensure!(
                    catalog.workspaces.iter().any(|w| w.id == workspace),
                    "Workspace {workspace} does not exist"
                );
                catalog.workspaces.retain(|w| w.id != workspace);
                model::save("workspaces.json", &catalog)?;
                for tab in &mut self.state.tabs {
                    if tab.workspace == Some(workspace) {
                        tab.workspace = None;
                    }
                }
                if self.state.filter == TabFilter::Workspace(workspace) {
                    self.state.filter = TabFilter::Unassigned;
                }
                self.select_visible();
                Ok(self.saved_result(CommandOutput::Done { ok: true }))
            }
            Command::Move { tab, workspace } => {
                let tab = tab.unwrap_or(self.state.active);
                if let Some(id) = workspace {
                    anyhow::ensure!(
                        catalog.workspaces.iter().any(|w| w.id == id),
                        "Workspace {id} does not exist"
                    );
                }
                self.state
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == tab)
                    .ok_or_else(|| anyhow::anyhow!("Tab {tab} does not exist"))?
                    .workspace = workspace;
                self.select_visible();
                Ok(self.saved_result(CommandOutput::Done { ok: true }))
            }
        }
    }
    fn control(&mut self, mut request: Value) -> anyhow::Result<Value> {
        let method = request["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Request requires a method"))?;
        if crate::control::Command::supports(method) {
            let command = serde_json::from_value(request)?;
            return Ok(serde_json::to_value(self.execute(command)?)?);
        }
        match method {
            "_hello" => Ok(json!({"version":transport::VERSION,"quitting":self.quitting})),
            "daemon-info" => Ok(
                json!({"version":transport::VERSION,"pid":std::process::id(),"attached":self.frontend.as_ref().is_some_and(|f|f.attached),"panes":self.sessions.len(),"uptime_seconds":self.started.elapsed().as_secs(),"snapshots":self.snapshots,"sent_rows":self.sent_rows}),
            ),
            "_busy" => {
                let ids: Vec<u64> = serde_json::from_value(request["panes"].take())?;
                let busy = ids
                    .into_iter()
                    .any(|id| self.sessions.get(&id).is_some_and(TerminalSession::busy));
                Ok(json!({"busy":busy}))
            }
            "_command" => {
                let command: Command = serde_json::from_value(request["command"].take())?;
                let result = self.execute(command)?;
                let metadata: Vec<_> = self
                    .sessions
                    .iter()
                    .map(|(&id, session)| {
                        Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0))
                    })
                    .collect();
                Ok(serde_json::to_value(CommandReply {
                    result,
                    session: self.state.clone(),
                    catalog: Self::catalog()?,
                    metadata,
                })?)
            }
            "_focus" => {
                let tab_id = request["tab"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("focus requires a tab"))?;
                if tab_id == 0 {
                    self.state.active = 0;
                } else {
                    let pane = request["pane"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("focus requires a pane"))?;
                    let tab = self
                        .state
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == tab_id)
                        .ok_or_else(|| anyhow::anyhow!("Tab {tab_id} does not exist"))?;
                    anyhow::ensure!(
                        tab.layout.ids().contains(&pane),
                        "Pane {pane} does not belong to tab {tab_id}"
                    );
                    tab.active = pane;
                    self.state.active = tab_id;
                }
                Ok(json!({"ok":true}))
            }
            "_presentation" => {
                let state: Session = serde_json::from_value(request["session"].take())?;
                self.state.width = state.width;
                self.state.height = state.height;
                self.state.maximized = state.maximized;
                self.state.filter = state.filter;
                if state.active == 0 || self.state.tabs.iter().any(|t| t.id == state.active) {
                    self.state.active = state.active;
                }
                for saved in state.tabs {
                    if let Some(tab) = self.state.tabs.iter_mut().find(|t| t.id == saved.id) {
                        if tab.layout.ids() == saved.layout.ids() {
                            tab.layout.update_ratios(&saved.layout);
                            if tab.layout.ids().contains(&saved.active) {
                                tab.active = saved.active;
                            }
                        }
                        if !tab.custom_name {
                            tab.name = saved.name;
                        }
                    }
                }
                self.save()?;
                Ok(json!({"ok":true}))
            }
            "_state" => {
                self.refresh_state();
                Ok(
                    json!({"session":self.state,"metadata":self.sessions.iter().map(|(&id, s)|Self::metadata(s,self.unread.get(&id).copied().unwrap_or(0))).collect::<Vec<_>>() }),
                )
            }
            "_metadata" => {
                let id = self.pane_id(&request)?;
                let session = self.sessions.get_mut(&id).unwrap();
                session.refresh_directory();
                Ok(
                    json!({"meta":Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0))}),
                )
            }
            "_copy" => {
                let id = self.pane_id(&request)?;
                Ok(json!({"text":self.sessions[&id].copy()?}))
            }
            "_read" | "read" => {
                let id = self.pane_id(&request)?;
                Ok(json!({"text":self.sessions.get_mut(&id).unwrap().text()?}))
            }
            "_close" => {
                let id = request["pane"]
                    .as_u64()
                    .or_else(|| self.pane_id(&request).ok())
                    .ok_or_else(|| anyhow::anyhow!("close requires pane"))?;
                self.close(id);
                Ok(self.save_result(json!({"ok":true})))
            }
            "quit" => {
                let warning = self
                    .save()
                    .err()
                    .map(|e| format!("Could not save layout: {e}"));
                self.quitting = true;
                Ok(json!({"ok":true,"warning":warning}))
            }
            "list" => self.list(),
            "send" => {
                let id = self.pane_id(&request)?;
                let text = request["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("send requires text"))?;
                let session = self.sessions.get_mut(&id).unwrap();
                if request["paste"].as_bool().unwrap_or(false) {
                    session.send_text(text, request["enter"].as_bool().unwrap_or(false))?;
                } else {
                    session.send(text.as_bytes())?;
                }
                Ok(json!({"ok":true}))
            }
            "notify" => {
                let id = self.pane_id(&request)?;
                let message = request["message"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("notify requires message"))?;
                self.queue_notifications(id, [message.to_owned()]);
                Ok(json!({"ok":true}))
            }
            "focus" => {
                if let Some(id) = request["pane"].as_u64() {
                    self.pane_id(&request)?;
                    let tab = self
                        .state
                        .tabs
                        .iter_mut()
                        .find(|t| t.layout.ids().contains(&id))
                        .unwrap();
                    tab.active = id;
                    self.state.active = tab.id;
                    self.unread.remove(&id);
                } else if let Some(id) = request["tab"].as_u64() {
                    anyhow::ensure!(
                        self.state.tabs.iter().any(|t| t.id == id),
                        "Tab {id} does not exist"
                    );
                    self.state.active = id;
                } else if let Some(id) = request["workspace"].as_u64() {
                    anyhow::ensure!(
                        Self::catalog()?.workspaces.iter().any(|w| w.id == id),
                        "Workspace {id} does not exist"
                    );
                    self.state.filter = TabFilter::Workspace(id);
                    self.select_visible();
                } else {
                    anyhow::bail!("focus requires a tab, pane, or workspace");
                }
                Ok(self.save_result(json!({"ok":true})))
            }
            _ => anyhow::bail!("Method {method} requires an open gmux window"),
        }
    }
    fn input(&mut self, request: Value) -> anyhow::Result<()> {
        let method = request["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing command"))?;
        if method == "attach" {
            anyhow::ensure!(
                request["version"] == transport::VERSION,
                "Incompatible frontend protocol"
            );
            self.frontend.as_mut().unwrap().attached = true;
            return Ok(());
        }
        anyhow::ensure!(
            self.frontend.as_ref().is_some_and(|f| f.attached),
            "Frontend must attach first"
        );
        if method == "_invalid" {
            self.frontend.as_mut().unwrap().paste = None;
            anyhow::bail!(
                "{}",
                request["error"]
                    .as_str()
                    .unwrap_or("Invalid frontend command")
            );
        }
        if method == "subscribe" {
            let ids: HashSet<u64> = serde_json::from_value(request["panes"].clone())?;
            let frontend = self.frontend.as_mut().unwrap();
            for id in &ids {
                if !frontend.visible.contains(id) {
                    frontend.screens.remove(id);
                    self.changed.insert(*id);
                }
            }
            frontend.screens.retain(|id, _| ids.contains(id));
            frontend.visible = ids;
            return Ok(());
        }
        let id = self.pane_id(&request)?;
        match method {
            "paste-begin" => {
                self.frontend.as_mut().unwrap().paste = None;
                let size = request["size"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Paste requires a size"))?;
                anyhow::ensure!(
                    size <= transport::MAX_PASTE as u64,
                    "Paste is too large. The limit is 4 MiB."
                );
                self.frontend.as_mut().unwrap().paste = Some(Paste {
                    pane: id,
                    size: size as usize,
                    text: String::with_capacity(size as usize),
                    request,
                });
                return Ok(());
            }
            "paste-data" => {
                let mut paste = self
                    .frontend
                    .as_mut()
                    .unwrap()
                    .paste
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("No paste is in progress"))?;
                let text = request["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Paste requires text"))?;
                anyhow::ensure!(
                    paste.pane == id
                        && text.len() <= transport::PASTE_CHUNK
                        && paste.text.len() + text.len() <= paste.size,
                    "Invalid paste chunk"
                );
                paste.text.push_str(text);
                self.frontend.as_mut().unwrap().paste = Some(paste);
                return Ok(());
            }
            "paste-end" => {
                let mut paste = self
                    .frontend
                    .as_mut()
                    .unwrap()
                    .paste
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("No paste is in progress"))?;
                anyhow::ensure!(
                    paste.pane == id && paste.text.len() == paste.size,
                    "Incomplete paste"
                );
                paste.request["method"] = json!("paste");
                paste.request["text"] = json!(paste.text);
                return self.input(paste.request);
            }
            _ => {}
        }
        if method == "mark-read" {
            let through = request["through"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("mark-read requires an alert sequence"))?;
            if self
                .unread
                .get(&id)
                .is_some_and(|&sequence| sequence <= through)
            {
                self.unread.remove(&id);
                self.changed.insert(id);
            }
            return Ok(());
        }
        let session = self.sessions.get_mut(&id).unwrap();
        match method {
            "copy" => {
                self.clipboard.insert(id, session.copy()?);
            }
            "find" => session.find(request["query"].as_str().unwrap_or("")),
            "find-advance" => session.find_advance(request["previous"].as_bool().unwrap_or(false)),
            "select" => session.select(
                serde_json::from_value(request["start"].clone())?,
                serde_json::from_value(request["end"].clone())?,
            )?,
            "clear-selection" => session.clear_selection(),
            "input" => {
                let bytes: Vec<u8> = serde_json::from_value(request["bytes"].clone())?;
                session.send(&bytes)?;
            }
            "paste" => {
                let text = request["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("paste requires text"))?;
                anyhow::ensure!(
                    text.len() <= transport::MAX_PASTE,
                    "Paste is too large. The limit is 4 MiB."
                );
                let mut bytes =
                    session.paste(text, request["clipboard"].as_bool().unwrap_or(false))?;
                if request["enter"].as_bool().unwrap_or(false) {
                    bytes.push(b'\r');
                }
                if request["bottom"].as_bool().unwrap_or(false) {
                    session.scroll(0, true);
                }
                session.send(&bytes)?;
            }
            "key" => {
                let release = request["release"].as_bool().unwrap_or(false);
                let bytes = session.key(
                    request["name"].as_str().unwrap_or(""),
                    request["cp"].as_u64().unwrap_or(0) as u32,
                    request["mods"].as_u64().unwrap_or(0) as u16,
                    release,
                    request["text"].as_str().unwrap_or(""),
                );
                if !bytes.is_empty() {
                    if !release {
                        session.scroll(0, true);
                    }
                    session.send(&bytes)?;
                }
            }
            "mouse" => {
                let bytes = session.mouse(
                    serde_json::from_value(request["position"].clone())?,
                    serde_json::from_value(request["metrics"].clone())?,
                    request["action"].as_i64().unwrap_or(0) as i32,
                    request["button"].as_i64().unwrap_or(0) as i32,
                    request["mods"].as_u64().unwrap_or(0) as u16,
                    request["pressed"].as_bool().unwrap_or(false),
                );
                if !bytes.is_empty() {
                    session.send(&bytes)?;
                }
            }
            "scroll" => session.scroll(
                request["delta"].as_i64().unwrap_or(0) as isize,
                request["bottom"].as_bool().unwrap_or(false),
            ),
            "scroll-to" => {
                let row = request["row"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("scroll-to requires a row"))?;
                let serial = request["serial"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("scroll-to requires a serial"))?;
                session.scroll_to(row);
                self.frontend
                    .as_mut()
                    .unwrap()
                    .scroll_serials
                    .insert(id, serial);
            }
            "theme" => session.set_theme(&serde_json::from_value(request["theme"].clone())?)?,
            "style" => session.set_cursor_style(serde_json::from_value::<CursorStyle>(
                request["style"].clone(),
            )?)?,
            "resize" => {
                let cols = request["cols"].as_u64().unwrap_or(0);
                let rows = request["rows"].as_u64().unwrap_or(0);
                anyhow::ensure!(
                    (1..=1000).contains(&cols)
                        && (1..=1000).contains(&rows)
                        && cols * rows <= transport::MAX_CELLS as u64,
                    "Invalid terminal size"
                );
                session.resize(
                    cols as u16,
                    rows as u16,
                    request["cw"].as_u64().unwrap_or(0) as u32,
                    request["ch"].as_u64().unwrap_or(0) as u32,
                    request["width"].as_u64().unwrap_or(0) as u16,
                    request["height"].as_u64().unwrap_or(0) as u16,
                )?;
            }
            _ => anyhow::bail!("Unknown frontend command: {method}"),
        }
        self.changed.insert(id);
        Ok(())
    }
    fn updates(&mut self, metadata_due: bool) -> anyhow::Result<()> {
        let Some(frontend) = self.frontend.as_mut().filter(|f| f.attached) else {
            self.changed.clear();
            return Ok(());
        };
        for (&id, session) in &mut self.sessions {
            let notify: Vec<_> = self
                .notifications
                .remove(&id)
                .unwrap_or_default()
                .into_iter()
                .collect();
            let error = self.errors.remove(&id);
            let mut copy = self.clipboard.remove(&id);
            let changed = self.changed.contains(&id);
            let screen =
                frontend.visible.contains(&id) && (changed || !frontend.screens.contains_key(&id));
            if !metadata_due
                && !changed
                && !screen
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.contains_key(&id)
            {
                continue;
            }
            let meta = Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0));
            if !screen
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            let mut rows = Vec::new();
            let mut cells = Vec::new();
            let frame = if screen {
                let snapshot = session.snapshot()?;
                self.snapshots += 1;
                let cols = snapshot.frame.cols as usize;
                let old = frontend.screens.entry(id).or_default();
                let full =
                    old.frame.cols != snapshot.frame.cols || old.frame.rows != snapshot.frame.rows;
                if full {
                    old.cells.resize(snapshot.cells.len(), Cell::default());
                }
                for (row, data) in snapshot.cells.chunks_exact(cols).enumerate() {
                    let previous = &mut old.cells[row * cols..(row + 1) * cols];
                    if full || *previous != *data {
                        rows.push(row as u16);
                        cells.extend_from_slice(data);
                        previous.copy_from_slice(data);
                    }
                }
                self.sent_rows += rows.len() as u64;
                // A client can discard every row while detached; attachment always resends a full viewport.
                let serial = frontend.scroll_serials.get(&id).copied().unwrap_or(0);
                let send = full
                    || !rows.is_empty()
                    || old.frame != snapshot.frame
                    || frontend.sent_scroll_serials.get(&id) != Some(&serial);
                old.frame = snapshot.frame;
                send.then_some(snapshot.frame)
            } else {
                None
            };
            let find = session.find_state();
            let selection = session.selection();
            let find_changed = frontend.find.get(&id) != Some(&find);
            let selection_changed = frontend.selections.get(&id) != Some(&selection);
            if frame.is_none()
                && !find_changed
                && !selection_changed
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            frontend.find.insert(id, find.clone());
            frontend.selections.insert(id, selection);
            frontend.metadata.insert(id, meta.clone());
            frontend
                .sent_scroll_serials
                .insert(id, frontend.scroll_serials.get(&id).copied().unwrap_or(0));
            // Large selections should not share the size limit of a full viewport.
            if let Some(text) = copy.take() {
                frontend.queue(Packet {
                    header: Header {
                        meta: meta.clone(),
                        frame: None,
                        scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
                        rows: Vec::new(),
                        notifications: Vec::new(),
                        error: None,
                        find: None,
                        selection,
                        copy: Some(text),
                    },
                    cells: Vec::new(),
                })?;
            }
            frontend.queue(Packet {
                header: Header {
                    meta,
                    frame,
                    scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
                    rows,
                    notifications: notify,
                    error,
                    find: find_changed.then_some(find),
                    selection,
                    copy,
                },
                cells,
            })?;
        }
        self.changed.clear();
        frontend.flush()?;
        Ok(())
    }
}

/// Run without initializing GTK or creating a graphics context.
pub fn run() -> anyhow::Result<()> {
    let path = ipc::socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(ipc::companion_path(".lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error.into());
    }
    // Blocking these signals before spawning shells lets signalfd wake the poll loop.
    // Child shells restore their signal mask in portable-pty's exec setup.
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::sigemptyset(&mut mask);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut mask, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let signal_fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
    anyhow::ensure!(signal_fd >= 0, "Could not create signal descriptor");
    let signal_fd = unsafe { OwnedFd::from_raw_fd(signal_fd) };
    let wake = ipc::Wake::new()?;
    let server = ipc::Server::start_at(path, wake.clone())?;
    let stream_path = ipc::companion_path(".stream");
    if stream_path.exists() {
        use std::os::unix::fs::FileTypeExt;
        anyhow::ensure!(
            std::fs::symlink_metadata(&stream_path)?
                .file_type()
                .is_socket(),
            "Refusing to replace a non-socket stream path"
        );
        std::fs::remove_file(&stream_path)?;
    }
    let listener = UnixListener::bind(&stream_path)?;
    std::fs::set_permissions(&stream_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    struct SocketCleanup(PathBuf);
    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = SocketCleanup(stream_path);
    let mut daemon = Daemon::new()?;
    let mut next_metadata = Instant::now();
    let mut next_frame = Instant::now();
    let mut quit_at = None;
    let mut fds = Vec::new();
    let mut pane_indices = Vec::new();
    let mut ready = HashSet::new();
    let mut exited = Vec::new();
    loop {
        let now = Instant::now();
        if quit_at.is_some_and(|deadline| now >= deadline) {
            break;
        }
        if daemon.quitting && quit_at.is_none() {
            quit_at = Some(now + Duration::from_millis(100));
        }
        fds.clear();
        fds.extend([
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signal_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ]);
        let frontend_index = daemon.frontend.as_ref().map(|frontend| {
            let index = fds.len();
            fds.push(libc::pollfd {
                fd: frontend.socket.as_raw_fd(),
                events: libc::POLLIN
                    | if frontend.output.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            });
            index
        });
        pane_indices.clear();
        for (&id, session) in &daemon.sessions {
            if let Some((fd, events)) = session.poll_fd() {
                pane_indices.push((id, fds.len()));
                fds.push(libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                });
            }
        }
        let attached = daemon.frontend.as_ref().is_some_and(|f| f.attached);
        let pending = attached
            && (!daemon.changed.is_empty()
                || !daemon.notifications.is_empty()
                || !daemon.errors.is_empty());
        let searching = daemon.sessions.values().any(TerminalSession::find_pending);
        let held = daemon.sessions.values().any(TerminalSession::render_held);
        let mut deadline = quit_at;
        for next in [
            attached.then_some(next_metadata),
            pending.then_some(next_frame),
            held.then_some(now + Duration::from_millis(100)),
            searching.then_some(now + Duration::from_millis(5)),
        ]
        .into_iter()
        .flatten()
        {
            deadline = Some(deadline.map_or(next, |current| current.min(next)));
        }
        let timeout = poll_timeout(deadline, now);
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            for _ in 0..8 {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket.set_nonblocking(true)?;
                        daemon.frontend = Some(Frontend {
                            socket,
                            input: Vec::new(),
                            output: Vec::new(),
                            offset: 0,
                            attached: false,
                            visible: HashSet::new(),
                            screens: HashMap::new(),
                            metadata: HashMap::new(),
                            scroll_serials: HashMap::new(),
                            sent_scroll_serials: HashMap::new(),
                            find: HashMap::new(),
                            selections: HashMap::new(),
                            discarding: false,
                            paste: None,
                        });
                        next_metadata = Instant::now();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            wake.drain();
        }
        let mut child_event = false;
        if fds[2].revents & libc::POLLIN != 0 {
            loop {
                let mut info = unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() };
                let n = unsafe {
                    libc::read(
                        signal_fd.as_raw_fd(),
                        (&mut info as *mut libc::signalfd_siginfo).cast(),
                        std::mem::size_of_val(&info),
                    )
                };
                if n <= 0 {
                    break;
                }
                if info.ssi_signo == libc::SIGCHLD as u32 {
                    child_event = true;
                } else {
                    daemon.quitting = true;
                }
            }
        }
        if let Some(index) = frontend_index
            && fds[index].revents != 0
            && let Some(frontend) = daemon.frontend.as_mut()
        {
            match frontend.requests() {
                Ok(requests) => {
                    for request in requests {
                        let id = daemon.pane_id(&request).ok();
                        let attach = request["method"] == "attach";
                        if let Err(e) = daemon.input(request) {
                            if attach {
                                daemon.frontend = None;
                                break;
                            } else if let Some(id) = id {
                                daemon.errors.insert(id, e.to_string());
                            } else {
                                eprintln!("Frontend command failed: {e}");
                            }
                        }
                    }
                }
                Err(_) => {
                    daemon.frontend = None;
                }
            }
        }
        for _ in 0..32 {
            let Ok(request) = server.receiver.try_recv() else {
                break;
            };
            let method = request.body["method"].as_str().unwrap_or("");
            if !method.starts_with('_')
                && method != "quit"
                && method != "daemon-info"
                && method != "notify"
                && daemon.frontend.as_ref().is_some_and(|f| f.attached)
                && ipc::companion_path(".ui").exists()
            {
                // Forward UI actions on a worker so the owner can service backend requests
                // made by that action and keep draining terminal output.
                std::thread::spawn(move || {
                    let response = ipc::call_at(&ipc::companion_path(".ui"), request.body)
                        .unwrap_or_else(|e| json!({"error":e.to_string()}));
                    let _ = request.reply.try_send(response);
                });
            } else {
                let response = daemon
                    .control(request.body)
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                let _ = request.reply.try_send(response);
            }
        }
        let metadata_due =
            daemon.frontend.as_ref().is_some_and(|f| f.attached) && Instant::now() >= next_metadata;
        ready.clear();
        ready.extend(
            pane_indices
                .iter()
                .filter(|(_, index)| fds[*index].revents != 0)
                .map(|(id, _)| *id),
        );
        if child_event || metadata_due {
            ready.extend(daemon.sessions.keys().copied());
        }
        ready.extend(daemon.sessions.iter().filter_map(|(&id, session)| {
            (session.render_held() || session.find_pending()).then_some(id)
        }));
        // Flush queued input immediately, including terminal protocol replies.
        ready.extend(daemon.sessions.iter().filter_map(|(&id, s)| {
            s.poll_fd()
                .filter(|(_, events)| events & libc::POLLOUT != 0)
                .map(|_| id)
        }));
        exited.clear();
        for id in ready.drain() {
            let Some(session) = daemon.sessions.get_mut(&id) else {
                continue;
            };
            let update = session.poll();
            if update.changed {
                daemon.changed.insert(id);
            }
            if let Some(error) = update.error {
                daemon.errors.insert(id, error);
            }
            if metadata_due {
                session.refresh_directory();
            }
            if session.exited() {
                exited.push(id);
                daemon.changed.insert(id);
            }
            if !update.notifications.is_empty() {
                daemon.queue_notifications(id, update.notifications);
            }
        }
        if metadata_due {
            next_metadata = Instant::now() + Duration::from_secs(1);
        }
        if (Instant::now() >= next_frame
            && (!daemon.changed.is_empty()
                || !daemon.notifications.is_empty()
                || !daemon.errors.is_empty()))
            || metadata_due
            || !exited.is_empty()
        {
            if let Err(error) = daemon.updates(metadata_due) {
                eprintln!("Detaching frontend: {error}");
                daemon.frontend = None;
            }
            next_frame = Instant::now() + Duration::from_millis(16);
        } else if let Some(frontend) = daemon.frontend.as_mut()
            && frontend.flush().is_err()
        {
            daemon.frontend = None;
        }
        for id in exited.drain(..) {
            daemon.close(id);
            if let Err(e) = daemon.save() {
                eprintln!("Could not save layout after process exit: {e}");
            }
        }
        if daemon.frontend.is_none() {
            daemon.changed.clear();
            daemon.errors.clear();
        }
    }
    if let Err(error) = daemon.save() {
        eprintln!("Could not save layout during shutdown: {error}");
    }
    Ok(())
}

use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_deadlines_round_up_positive_waits_and_bound_long_waits() {
        let now = Instant::now();
        assert_eq!(poll_timeout(None, now), -1);
        assert_eq!(poll_timeout(Some(now), now), 0);
        assert_eq!(poll_timeout(Some(now - Duration::from_millis(1)), now), 0);
        for (duration, expected) in [
            (Duration::from_nanos(1), 1),
            (Duration::from_millis(1), 1),
            (Duration::from_micros(1001), 2),
            (Duration::from_millis(i32::MAX as u64 + 1), i32::MAX),
        ] {
            assert_eq!(poll_timeout(Some(now + duration), now), expected);
        }
    }

    #[test]
    fn queued_packets_survive_partial_writes_and_buffer_compaction() {
        let (socket, mut reader) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let size = 4096i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const i32).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let mut frontend = Frontend {
            socket,
            input: Vec::new(),
            output: Vec::new(),
            offset: 0,
            attached: true,
            visible: HashSet::new(),
            screens: HashMap::new(),
            metadata: HashMap::new(),
            scroll_serials: HashMap::new(),
            sent_scroll_serials: HashMap::new(),
            find: HashMap::new(),
            selections: HashMap::new(),
            discarding: false,
            paste: None,
        };
        let packet = |text| {
            let mut cell = Cell::default();
            cell.text[0] = text;
            Packet {
                header: Header {
                    meta: Metadata {
                        id: 1,
                        directory: PathBuf::from("/tmp"),
                        title: "shell".into(),
                        size: (80, 24),
                        exited: false,
                        unread: 0,
                        busy: false,
                        modes: [false; 2],
                    },
                    scroll_serial: 0,
                    frame: Some(Frame {
                        cols: 80,
                        rows: 24,
                        ..Frame::default()
                    }),
                    rows: (0..24).collect(),
                    notifications: Vec::new(),
                    error: None,
                    find: None,
                    selection: None,
                    copy: None,
                },
                cells: vec![cell; 80 * 24],
            }
        };
        let mut expected = Vec::new();
        packet(b'a').encode_into(&mut expected).unwrap();
        packet(b'b').encode_into(&mut expected).unwrap();
        frontend.queue(packet(b'a')).unwrap();
        frontend.flush().unwrap();
        assert!(frontend.offset > 0 && frontend.offset < frontend.output.len());
        let mut received = Vec::new();
        let mut buffer = [0; 16384];
        let mut queued_second = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < expected.len() {
            assert!(Instant::now() < deadline, "Timed out draining packets");
            match reader.read(&mut buffer) {
                Ok(n) if n > 0 => received.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now()
                }
                result => panic!("Unexpected socket read: {result:?}"),
            }
            if !queued_second && frontend.offset >= frontend.output.len() / 2 {
                frontend.queue(packet(b'b')).unwrap();
                assert_eq!(frontend.offset, 0, "Consumed bytes should be compacted");
                queued_second = true;
            }
            frontend.flush().unwrap();
        }
        assert!(queued_second);
        assert_eq!(received, expected);
        assert!(frontend.output.is_empty());
    }
}
