//! Client-local streams, qualified identities, and command routing across daemons.
use super::request::{self, Request as WorkerRequest, RoutedRequest, Target};
use super::{
    Snapshot, Store, Subscription,
    control::{BackendReply, Command, CommandOutput, CommandReply, CommandResult, TabCommand},
    model::{
        DaemonId, DesktopState, DesktopTab, ExecutionSnapshot, Machine, MachineKind, PaneRef,
        Session, TabRef, TabState,
    },
};
use gmux_client::{ipc, remote::SessionHandle};
use gmux_protocol::{control as execution, session::SessionUpdate, theme::Theme};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gmux_client::connection::Connection;
pub use gmux_client::connection::Status as ConnectionStatus;

/// Stream events qualify pane targets as they enter the GTK adapter.
pub enum ClipboardEvent {
    Write(gmux_protocol::clipboard::Write),
    Read {
        pane: PaneRef,
        event: gmux_protocol::clipboard::ReadEvent,
    },
}
struct Origin {
    machine: Machine,
    home_directory: Option<PathBuf>,
    link: Connection,
    execution: ExecutionSnapshot,
    // Retained across snapshots/reconnects so closing the largest live ID is visible.
    observed_id: u64,
    revision: u64,
}
impl Origin {
    fn new(machine: Machine, wake: gmux_platform::unix::ipc::Wake) -> Self {
        let mut link = Connection::new(
            machine.daemon,
            ipc::Endpoint::remote(machine.name.clone(), Some(machine.daemon)),
            wake,
        );
        link.set_auto_reconnect(true);
        Self {
            machine,
            home_directory: None,
            link,
            execution: ExecutionSnapshot::default(),
            observed_id: 0,
            revision: 0,
        }
    }
}
pub struct Connections {
    primary: Option<DaemonId>,
    local: Option<DaemonId>,
    local_enabled: bool,
    preferred: Option<String>,
    definitions: Vec<gmux_client::connections::Remote>,
    watcher: gmux_client::unix::Watch,
    store: Arc<Store>,
    client_revision: u64,
    _subscription: Subscription,
    origins: HashMap<DaemonId, Origin>,
    poll_cursor: Option<DaemonId>,
    revision: u64,
    desktop: DesktopState,
    organization: Arc<Snapshot>,
    wake: gmux_platform::unix::ipc::Wake,
    snapshot: Option<CommandReply>,
    attached: HashSet<PaneRef>,
    focused: Cell<bool>,
    frame_interval: Duration,
    status_changed: bool,
}
pub struct Route {
    pub request: RoutedRequest,
    pub assign: Option<Assignment>,
}
pub enum Assignment {
    CreatedTab { workspace: Option<u64> },
    CreatedWorkspace { tab: TabRef },
}
impl Assignment {
    pub fn resolve(self, output: &CommandOutput) -> Option<(TabRef, Option<u64>)> {
        match (self, output) {
            (Self::CreatedTab { workspace }, CommandOutput::New { tab, .. }) => {
                Some((*tab, workspace))
            }
            (Self::CreatedWorkspace { tab }, CommandOutput::Workspace { workspace }) => {
                Some((tab, Some(*workspace)))
            }
            _ => None,
        }
    }
}
impl Connections {
    pub fn new(
        wake: gmux_platform::unix::ipc::Wake,
        store: Arc<Store>,
        local_enabled: bool,
        preferred: Option<String>,
    ) -> anyhow::Result<(Self, CommandReply)> {
        // Observe edits made while loading the initial catalog, too.
        let watcher = gmux_client::unix::Watch::new(wake.clone())?;
        let catalog = gmux_client::connections::Catalog::load()?;
        let subscription = store.subscribe(wake.clone());
        let (organization, client_revision) = store.snapshot();
        let primary = match preferred.as_deref() {
            Some("local") => None,
            Some(name) => Some(catalog.remote(name, None)?.daemon),
            None if local_enabled => None,
            None => catalog.remotes.first().map(|remote| remote.daemon),
        };
        let origins = catalog
            .connections()
            .into_iter()
            .map(|machine| (machine.daemon, Origin::new(machine, wake.clone())))
            .collect();
        let mut hub = Self {
            primary,
            local: None,
            local_enabled,
            preferred,
            definitions: catalog.remotes,
            watcher,
            store: store.clone(),
            client_revision,
            _subscription: subscription,
            origins,
            poll_cursor: None,
            revision: 0,
            desktop: organization.desktop.clone(),
            organization,
            wake,
            snapshot: None,
            attached: HashSet::new(),
            focused: Cell::new(false),
            frame_interval: gmux_protocol::transport::DEFAULT_FRAME_INTERVAL,
            status_changed: false,
        };
        let initial = hub.combined(CommandOutput::Done { ok: true }.into());
        Ok((hub, initial))
    }
    pub fn local_daemon(&self) -> Option<DaemonId> {
        self.local
    }
    pub fn enable_local(&mut self, machine: Machine) {
        let daemon = machine.daemon;
        if self.local.is_some_and(|id| id != daemon) {
            self.disable_local();
        }
        self.local_enabled = true;
        self.local = Some(daemon);
        self.origins
            .entry(daemon)
            .or_insert_with(|| Origin::new(machine, self.wake.clone()))
            .link
            .set_auto_reconnect(false);
        if self.preferred.as_deref().is_none_or(|name| name == "local") || self.primary.is_none() {
            self.primary = Some(daemon);
        }
        self.status_changed = true;
        self.revision += 1;
        self.connect(daemon);
    }
    pub fn disable_local(&mut self) {
        self.local_enabled = false;
        if let Some(id) = self.local.take() {
            self.origins.remove(&id);
            self.attached.retain(|pane| pane.daemon != id);
            if self.primary == Some(id) {
                self.primary = None;
            }
        }
        if self.primary.is_none() {
            self.primary = self.fallback_daemon();
        }
        self.status_changed = true;
        self.revision += 1;
        self.wake.notify();
    }
    fn fallback_daemon(&self) -> Option<DaemonId> {
        // An enabled local daemon may still be starting and have no identity yet.
        if self.local_enabled || self.preferred.as_deref() == Some("local") {
            self.local
        } else {
            self.local
                .or_else(|| self.definitions.first().map(|remote| remote.daemon))
        }
    }
    pub fn snapshot(&mut self) -> CommandReply {
        self.refresh_organization();
        let mut reply = self.combined(CommandOutput::Done { ok: true }.into());
        reply.desktop = Some(self.desktop.clone());
        reply
    }
    pub fn default_daemon(&self) -> Option<DaemonId> {
        self.primary
    }
    pub fn preferred_connection(&self) -> Option<String> {
        match self.primary {
            Some(id) if Some(id) == self.local => Some("local".into()),
            Some(id) => self
                .origins
                .get(&id)
                .map(|origin| origin.machine.name.clone()),
            None => self.preferred.clone(),
        }
    }
    pub fn select_default(&mut self, name: String) -> anyhow::Result<()> {
        self.refresh_definitions()?;
        self.primary = if name == "local" {
            self.local
        } else {
            Some(self.connection_id(&name)?)
        };
        self.preferred = Some(name);
        self.status_changed = true;
        self.wake.notify();
        Ok(())
    }
    pub fn connection_revision(&self) -> u64 {
        self.revision
    }
    pub fn desktop_revision(&self) -> u64 {
        self.client_revision
    }
    pub fn is_local(&self, id: TabRef) -> bool {
        Some(id.daemon) == self.local
    }
    pub fn connection_name(&self, id: TabRef) -> String {
        self.origins
            .get(&id.daemon)
            .map(|o| o.machine.name.clone())
            .unwrap_or_default()
    }
    pub fn has_connection(&self, daemon: DaemonId) -> bool {
        self.origins.contains_key(&daemon)
    }
    pub fn connections(&self) -> Vec<Machine> {
        let mut machines: Vec<_> = self.origins.values().map(|o| o.machine.clone()).collect();
        machines.sort_by(|a, b| {
            (Some(a.daemon) != self.local, &a.name).cmp(&(Some(b.daemon) != self.local, &b.name))
        });
        machines
    }
    pub fn machine_kind(&self, id: TabRef) -> MachineKind {
        self.kind(id.daemon)
    }
    pub fn display_directory(&self, daemon: DaemonId, path: &Path) -> String {
        self.origins
            .get(&daemon)
            .and_then(|origin| origin.home_directory.as_ref())
            .map_or_else(
                || path.display().to_string(),
                |home| gmux_platform::paths::display_path(path, home),
            )
    }
    pub fn kind(&self, daemon: DaemonId) -> MachineKind {
        self.origins
            .get(&daemon)
            .map_or(MachineKind::Unknown, |origin| origin.machine.machine_kind)
    }
    pub fn connection_id(&self, name: &str) -> anyhow::Result<DaemonId> {
        if name == "local" {
            return self
                .local
                .ok_or_else(|| anyhow::anyhow!("Local execution is not enabled"));
        }
        self.origins
            .values()
            .find(|o| o.machine.name == name)
            .map(|o| o.machine.daemon)
            .ok_or_else(|| anyhow::anyhow!("Connection {name} does not exist"))
    }
    fn selected_daemon(&self, connection: Option<&str>) -> anyhow::Result<DaemonId> {
        connection
            .map(|name| self.connection_id(name))
            .transpose()?
            .or(self.primary)
            .ok_or_else(|| anyhow::anyhow!("No connection is selected"))
    }
    pub fn external_pane(&self, connection: Option<&str>, id: u64) -> anyhow::Result<PaneRef> {
        let daemon = self.selected_daemon(connection)?;
        anyhow::ensure!(
            self.origins.get(&daemon).is_some_and(|o| o
                .execution
                .tabs
                .iter()
                .any(|t| t.layout.contains(id))),
            "Pane {id} does not exist on the selected connection"
        );
        Ok(PaneRef { daemon, id })
    }
    pub fn external_tab(&self, connection: Option<&str>, id: u64) -> anyhow::Result<TabRef> {
        let daemon = self.selected_daemon(connection)?;
        anyhow::ensure!(
            self.origins
                .get(&daemon)
                .is_some_and(|o| o.execution.tabs.iter().any(|t| t.id == id)),
            "Tab {id} does not exist on the selected connection"
        );
        Ok(TabRef { daemon, id })
    }
    pub fn connected(&self, daemon: DaemonId) -> bool {
        self.origins
            .get(&daemon)
            .is_some_and(|o| o.link.status() == ConnectionStatus::Connected)
    }
    pub fn connection_status(&self, daemon: DaemonId) -> ConnectionStatus<'_> {
        self.origins
            .get(&daemon)
            .map_or(ConnectionStatus::Disconnected { error: None }, |origin| {
                origin.link.status()
            })
    }
    pub fn connect_remotes(&mut self) {
        let ids: Vec<_> = self.origins.keys().copied().collect();
        for id in ids {
            self.connect(id);
        }
    }
    pub fn next_retry(&self) -> Option<Instant> {
        self.origins
            .values()
            .filter_map(|origin| origin.link.next_retry())
            .min()
    }
    /// Earliest deadline among attached connections' coalesced scroll targets.
    pub fn next_scroll(&self) -> Option<Instant> {
        self.origins
            .values()
            .filter_map(|origin| origin.link.next_scroll())
            .min()
    }
    pub fn connect(&mut self, id: DaemonId) {
        let Some(origin) = self.origins.get(&id) else {
            return;
        };
        let endpoint = if Some(id) == self.local {
            ipc::Endpoint::local()
        } else {
            ipc::Endpoint::remote(origin.machine.name.clone(), Some(id))
        };
        self.connect_endpoint(id, endpoint);
    }
    fn connect_endpoint(&mut self, id: DaemonId, endpoint: ipc::Endpoint) {
        let Some(origin) = self.origins.get_mut(&id) else {
            return;
        };
        origin.link.set_endpoint(endpoint);
        if origin.link.connect() {
            // Every attempt creates a new session manager. Existing terminal
            // widgets must attach to it, including after a catalog edit cancels
            // the old link before poll can observe its disconnection.
            self.attached.retain(|pane| pane.daemon != id);
            self.status_changed = true;
        }
    }
    /// Combine an execution snapshot with this client's organization and other connections.
    #[cfg(test)]
    pub fn project_execution(
        &mut self,
        reply: execution::CommandReply,
    ) -> anyhow::Result<CommandReply> {
        self.project(BackendReply::Execution(
            execution::ExecutionReply::Snapshot(Box::new(reply)),
        ))
    }
    pub fn project(&mut self, reply: BackendReply) -> anyhow::Result<CommandReply> {
        self.refresh_organization();
        let result = match reply {
            BackendReply::Execution(execution::ExecutionReply::Snapshot(reply)) => {
                // Control replies can create panes before their stream snapshot arrives.
                // Stream metadata is installed by Connection::poll instead.
                if let Some(origin) = self.origins.get_mut(&reply.daemon)
                    && reply.revision >= origin.revision
                    && let Some(manager) = origin.link.manager_mut()
                {
                    manager.install_metadata(&reply.metadata, reply.revision);
                }
                self.ingest_execution(*reply)?
            }
            BackendReply::Organization { result } => result,
            BackendReply::Value(_) | BackendReply::Execution(_) => {
                anyhow::bail!("Expected an execution or organization reply")
            }
        };
        Ok(self.combined(result))
    }
    fn refresh_definitions(&mut self) -> anyhow::Result<()> {
        let catalog = gmux_client::connections::Catalog::load()?;
        let keep: HashSet<_> = catalog
            .remotes
            .iter()
            .map(|r| r.daemon)
            .chain(self.local)
            .collect();
        self.origins.retain(|id, _| keep.contains(id));
        self.attached.retain(|pane| keep.contains(&pane.daemon));
        let mut reconnect = Vec::new();
        for remote in &catalog.remotes {
            let previous = self.definitions.iter().find(|r| r.daemon == remote.daemon);
            let connection_changed = previous.is_none_or(|old| {
                old.address != remote.address || old.authentication != remote.authentication
            });
            let renamed = previous.is_some_and(|old| old.name != remote.name);
            let origin = self.origins.entry(remote.daemon).or_insert_with(|| {
                Origin::new(
                    Machine {
                        name: remote.name.clone(),
                        daemon: remote.daemon,
                        machine_kind: remote.machine_kind,
                    },
                    self.wake.clone(),
                )
            });
            origin.machine.name = remote.name.clone();
            origin.machine.machine_kind = remote.machine_kind;
            if connection_changed
                || (renamed
                    && matches!(
                        origin.link.status(),
                        ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. }
                    ))
            {
                origin.link.cancel();
                reconnect.push(remote.daemon);
            } else if renamed {
                origin.link.set_endpoint(ipc::Endpoint::remote(
                    remote.name.clone(),
                    Some(remote.daemon),
                ));
            }
        }
        if let Some(local) = self.local {
            self.origins.get_mut(&local).unwrap().machine.name = catalog.local_name().into();
        }
        self.definitions = catalog.remotes;
        if self
            .primary
            .is_none_or(|id| !self.origins.contains_key(&id))
        {
            self.primary = self.fallback_daemon();
        }
        self.status_changed = true;
        self.revision += 1;
        for id in reconnect {
            self.connect(id);
        }
        Ok(())
    }
    fn refresh_organization(&mut self) {
        let (organization, revision) = self.store.snapshot();
        if !Arc::ptr_eq(&self.organization, &organization) {
            self.desktop = organization.desktop.clone();
            self.organization = organization;
        }
        self.client_revision = revision;
    }
    fn ingest_execution(
        &mut self,
        reply: execution::CommandReply,
    ) -> anyhow::Result<CommandResult> {
        let result = self
            .organization
            .execution_result(reply.daemon, reply.result);
        let daemon = reply.daemon;
        let origin = self
            .origins
            .get_mut(&daemon)
            .ok_or_else(|| anyhow::anyhow!("Unknown daemon"))?;
        if reply.revision >= origin.revision {
            origin.machine.machine_kind = reply.machine_kind;
            origin.home_directory = Some(reply.home_directory);
            origin.revision = reply.revision;
            origin.observed_id = reply
                .execution
                .tabs
                .iter()
                .map(|tab| tab.id.max(tab.layout.max_id()))
                .fold(origin.observed_id, u64::max);
            origin.execution = reply.execution;
        }
        Ok(result)
    }
    fn combined(&mut self, result: CommandResult) -> CommandReply {
        let desktop_indices: HashMap<_, _> = self
            .desktop
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| (tab.reference, index))
            .collect();
        let tab_count = self.origins.values().map(|o| o.execution.tabs.len()).sum();
        let mut tabs = Vec::with_capacity(tab_count);
        for (&daemon, origin) in &self.origins {
            for tab in &origin.execution.tabs {
                let reference = TabRef { daemon, id: tab.id };
                let saved = desktop_indices
                    .get(&reference)
                    .map(|&index| &self.desktop.tabs[index]);
                let tab = TabState::from_execution(daemon, tab, saved);
                tabs.push(tab);
            }
        }
        tabs.sort_unstable_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.daemon.cmp(&b.id.daemon))
                .then_with(|| b.id.id.cmp(&a.id.id))
        });
        let active = self.desktop.active;
        self.revision += 1;
        CommandReply {
            daemon: self.primary,
            revision: self.revision,
            result,
            session: Session {
                tabs,
                active,
                filter: self.desktop.filter,
                width: self.desktop.width,
                height: self.desktop.height,
                maximized: self.desktop.maximized,
            },
            catalog: self.organization.catalog.clone(),
            desktop: None,
        }
    }
    /// Apply local definitions before remote snapshots and produce one view for the batch.
    fn project_execution_batch(
        &mut self,
        mut replies: Vec<execution::CommandReply>,
    ) -> anyhow::Result<CommandReply> {
        self.refresh_organization();
        replies.sort_by_key(|reply| (Some(reply.daemon) != self.primary, reply.revision));
        for reply in replies {
            if self.origins.contains_key(&reply.daemon) {
                self.ingest_execution(reply)?;
            }
        }
        Ok(self.combined(CommandOutput::Done { ok: true }.into()))
    }
    pub fn validate_state(&self, reply: &CommandReply) -> anyhow::Result<()> {
        anyhow::ensure!(
            reply.daemon == self.primary,
            "State belongs to a different desktop"
        );
        Ok(())
    }
    pub fn set_focused(&self, active: bool) -> anyhow::Result<()> {
        self.focused.set(active);
        for o in self.origins.values() {
            if let Some(m) = o.link.manager()
                && let Err(error) = m.set_focused(active)
                && Some(o.machine.daemon) == self.local
            {
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn set_frame_interval(&mut self, interval: Duration) {
        if self.frame_interval == interval {
            return;
        }
        self.frame_interval = interval;
        for origin in self.origins.values_mut() {
            if let Some(manager) = origin.link.manager_mut() {
                manager.set_frame_interval(interval);
            }
        }
    }
    pub fn frame_interval(&self) -> Duration {
        self.frame_interval
    }
    pub fn attach(&mut self, reference: PaneRef, theme: &Theme) -> anyhow::Result<SessionHandle> {
        let manager = self
            .origins
            .get_mut(&reference.daemon)
            .and_then(|o| o.link.manager_mut())
            .ok_or_else(|| anyhow::anyhow!("Connection is disconnected"))?;
        manager.set_clipboard_writes(true)?;
        manager.set_clipboard_reads(true)?;
        let handle = manager.attach(reference.id, theme)?;
        self.attached.insert(reference);
        Ok(handle)
    }
    pub fn is_attached(&self, id: PaneRef) -> bool {
        self.attached.contains(&id)
    }
    pub fn set_visible(&mut self, references: HashSet<PaneRef>) -> anyhow::Result<()> {
        for (&daemon, o) in &mut self.origins {
            if let Some(m) = o.link.manager_mut() {
                let result = m.set_visible(
                    references
                        .iter()
                        .filter(|r| r.daemon == daemon)
                        .map(|r| r.id)
                        .collect(),
                );
                if Some(daemon) == self.local {
                    result?;
                }
            }
        }
        Ok(())
    }
    pub fn is_visible(&self, pane: PaneRef) -> bool {
        self.origins
            .get(&pane.daemon)
            .and_then(|o| o.link.manager())
            .is_some_and(|m| m.is_visible(pane.id))
    }
    pub fn set_git_panes(&mut self, references: HashSet<PaneRef>) -> anyhow::Result<()> {
        for (&daemon, origin) in &mut self.origins {
            if let Some(manager) = origin.link.manager_mut() {
                let result = manager.set_git_panes(
                    references
                        .iter()
                        .filter(|pane| pane.daemon == daemon)
                        .map(|pane| pane.id)
                        .collect(),
                );
                if Some(daemon) == self.local {
                    result?;
                }
            }
        }
        Ok(())
    }
    pub fn mark_read(&mut self, pane: PaneRef) {
        if let Some(m) = self
            .origins
            .get_mut(&pane.daemon)
            .and_then(|o| o.link.manager_mut())
        {
            m.mark_read(pane.id);
        }
    }
    pub fn close(&mut self, pane: PaneRef) {
        self.attached.remove(&pane);
        if let Some(m) = self
            .origins
            .get_mut(&pane.daemon)
            .and_then(|o| o.link.manager_mut())
        {
            m.close(pane.id);
        }
    }
    pub fn clear(&mut self) {
        for o in self.origins.values_mut() {
            self.status_changed |= o.link.cancel();
        }
        self.attached.clear();
    }
    pub fn take_state(&mut self) -> Option<CommandReply> {
        self.snapshot.take()
    }
    pub fn take_status_changed(&mut self) -> bool {
        std::mem::take(&mut self.status_changed)
    }
    #[cfg(test)]
    pub fn poll(&mut self) -> HashMap<PaneRef, SessionUpdate> {
        self.poll_with_clipboard(|_| {})
    }
    #[cfg(test)]
    pub fn poll_with_clipboard(
        &mut self,
        mut clipboard: impl FnMut(gmux_protocol::clipboard::Write),
    ) -> HashMap<PaneRef, SessionUpdate> {
        self.poll_with_clipboard_events(|event, _| {
            if let ClipboardEvent::Write(write) = event {
                clipboard(write);
            }
        })
    }
    #[cfg(test)]
    pub fn poll_with_clipboard_events(
        &mut self,
        clipboard: impl FnMut(ClipboardEvent, Option<String>),
    ) -> HashMap<PaneRef, SessionUpdate> {
        let mut budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 64);
        self.poll_with_budget(&mut budget, clipboard)
    }
    pub fn poll_with_budget(
        &mut self,
        budget: &mut gmux_platform::work::Budget,
        mut clipboard: impl FnMut(ClipboardEvent, Option<String>),
    ) -> HashMap<PaneRef, SessionUpdate> {
        if self.watcher.changed()
            && let Err(error) = self.refresh_definitions()
        {
            eprintln!("Could not refresh connections: {error}");
        }
        let mut states = Vec::new();
        let mut updates = HashMap::new();
        let mut lost = Vec::new();
        let now = Instant::now();
        let mut ids: Vec<_> = self.origins.keys().copied().collect();
        ids.sort_unstable();
        let start = self
            .poll_cursor
            .map_or(0, |last| ids.partition_point(|id| *id <= last));
        let count = ids.len();
        ids.rotate_left(if start == count { 0 } else { start });
        for daemon in ids {
            if !budget.take() {
                self.wake.notify();
                break;
            }
            self.poll_cursor = Some(daemon);
            let origin = self.origins.get_mut(&daemon).unwrap();
            let was_connected = origin.link.status() == ConnectionStatus::Connected;
            let label = (Some(daemon) != self.local).then(|| origin.machine.name.clone());
            let update = origin.link.poll_with_budget(now, budget, |event| {
                let event = match event {
                    gmux_protocol::clipboard::Event::Write(write) => ClipboardEvent::Write(write),
                    gmux_protocol::clipboard::Event::Read { pane, event } => ClipboardEvent::Read {
                        pane: PaneRef { daemon, id: pane },
                        event,
                    },
                };
                clipboard(event, label.clone());
            });
            if update.status_changed {
                self.status_changed = true;
                if let Some(manager) = origin.link.manager() {
                    if let Err(error) = manager.set_focused(self.focused.get()) {
                        eprintln!("Could not set connection focus: {error}");
                    }
                    origin.revision = 0;
                }
                if let Some(manager) = origin.link.manager_mut() {
                    manager.set_frame_interval(self.frame_interval);
                }
            }
            for (id, update) in update.panes {
                let pane = PaneRef { daemon, id };
                updates.insert(pane, update);
            }
            if let Some(state) = update.state {
                states.push(state);
            }
            if was_connected && origin.link.status() != ConnectionStatus::Connected {
                lost.push(daemon);
            }
        }
        if !states.is_empty()
            || self.status_changed
            || self.client_revision != self.store.revision()
        {
            match self.project_execution_batch(states) {
                Ok(reply) => self.snapshot = Some(reply),
                Err(error) => eprintln!("{error}"),
            }
        }
        for daemon in lost {
            let Some(origin) = self.origins.get(&daemon) else {
                continue;
            };
            let name = origin.machine.name.clone();
            let panes: Vec<_> = origin
                .execution
                .tabs
                .iter()
                .flat_map(|tab| tab.layout.ids())
                .collect();
            for id in panes {
                let pane = PaneRef { daemon, id };
                self.attached.remove(&pane);
                updates
                    .entry(pane)
                    .or_insert_with(SessionUpdate::default)
                    .error
                    .get_or_insert_with(|| format!("Connection to {name} lost"));
            }
        }
        updates
    }
    pub fn desktop(&mut self, session: &Session) -> DesktopState {
        let mut desktop = self.desktop.clone();
        let current_refs: HashSet<_> = session.tabs.iter().map(|t| t.id).collect();
        let mut live_tabs: HashMap<DaemonId, HashSet<u64>> = HashMap::new();
        desktop.tabs.retain(|t| {
            !current_refs.contains(&t.reference)
                && self.origins.get(&t.reference.daemon).is_none_or(|origin| {
                    // Organization can arrive before execution. Confirm absence only
                    // through IDs observed in accepted topology, never stale replies.
                    origin.link.manager().is_none()
                        || t.reference.id > origin.observed_id
                        || live_tabs
                            .entry(t.reference.daemon)
                            .or_insert_with(|| {
                                origin.execution.tabs.iter().map(|tab| tab.id).collect()
                            })
                            .contains(&t.reference.id)
                })
        });
        let mut current: Vec<_> = session
            .tabs
            .iter()
            .map(|tab| self.desktop_tab(tab, tab.workspace))
            .collect();
        current.extend(desktop.tabs);
        desktop.tabs = current;
        desktop.active = session.active;
        desktop.filter = session.filter;
        desktop.width = session.width;
        desktop.height = session.height;
        desktop.maximized = session.maximized;
        self.desktop = desktop.clone();
        desktop
    }
    pub fn desktop_tab(&self, tab: &TabState, workspace: Option<u64>) -> DesktopTab {
        DesktopTab {
            reference: tab.id,
            workspace,
            layout: tab.layout.clone(),
            active: tab.active,
            name: tab.name.clone(),
        }
    }
    pub fn route_request(
        &self,
        request: impl Into<WorkerRequest>,
    ) -> anyhow::Result<RoutedRequest> {
        Ok(match request.into() {
            WorkerRequest::Organization(request) => RoutedRequest::Organization(request),
            WorkerRequest::Configuration(mut request) => {
                use gmux_client::connections::ConfigurationRequest as C;
                match &mut request {
                    C::ConnectionEdit {
                        connection, daemon, ..
                    }
                    | C::ConnectionRemove { connection, daemon } => {
                        if let Some(origin) = self.origins.values().find(|origin| {
                            Some(origin.machine.daemon) != self.local
                                && origin.machine.name == *connection
                        }) {
                            anyhow::ensure!(
                                daemon.is_none_or(|id| id == origin.machine.daemon),
                                "Remote connection identity changed"
                            );
                            *daemon = Some(origin.machine.daemon);
                        }
                    }
                    C::ConnectionList
                    | C::RemoteAccessGet
                    | C::RemoteAccessSet { .. }
                    | C::ConnectionAdd { .. }
                    | C::LocalDaemonStart
                    | C::LocalDaemonStop => (),
                }
                RoutedRequest::Configuration(request)
            }
            WorkerRequest::Execution(request) => {
                let selected = Cell::new(None);
                let check = |id| -> anyhow::Result<u64> {
                    let (origin, raw) = id;
                    anyhow::ensure!(
                        selected.get().is_none_or(|id| id == origin),
                        "Panes belong to different connections"
                    );
                    selected.set(Some(origin));
                    Ok(raw)
                };
                let request = request
                    .map_ids(&mut |pane| check((pane.daemon, pane.id)), &mut |tab| {
                        check((tab.daemon, tab.id))
                    })?;
                let daemon = selected
                    .get()
                    .or(self.primary)
                    .ok_or_else(|| anyhow::anyhow!("No connection is selected"))?;
                RoutedRequest::Execution {
                    target: self.target(daemon)?,
                    request,
                }
            }
        })
    }
    fn target(&self, daemon: DaemonId) -> anyhow::Result<Target> {
        let origin = self
            .origins
            .get(&daemon)
            .ok_or_else(|| anyhow::anyhow!("Connection no longer exists"))?;
        Ok(if Some(daemon) == self.local {
            Target::Local(daemon)
        } else {
            Target::Remote {
                name: origin.machine.name.clone(),
                daemon,
            }
        })
    }
    /// Resolve socket IDs and implicit window targets before dispatch. A selector
    /// constrains the target even when it is also the window's default connection.
    pub fn incoming_command(
        &self,
        command: Command<u64, u64>,
        connection: Option<&str>,
        active_pane: Option<PaneRef>,
        active_tab: Option<TabRef>,
    ) -> anyhow::Result<(Command, Option<DaemonId>)> {
        let selected = connection
            .map(|name| self.connection_id(name))
            .transpose()?;
        let destination = Cell::new(selected.or(self.primary));
        if matches!(
            command,
            Command::WorkspaceAdd { .. }
                | Command::WorkspaceEdit { .. }
                | Command::WorkspaceRemove { .. }
        ) {
            anyhow::ensure!(
                connection.is_none(),
                "Workspace commands control the GTK app; omit connection"
            );
        }
        let mut command = command.map_ids(
            &mut |id| {
                let r = self.external_pane(connection, id)?;
                destination.set(Some(r.daemon));
                Ok(r)
            },
            &mut |id| {
                let r = self.external_tab(connection, id)?;
                destination.set(Some(r.daemon));
                Ok(r)
            },
        )?;
        match &mut command {
            Command::Tab(TabCommand::Split { pane, .. } | TabCommand::Close { pane })
                if pane.is_none() =>
            {
                let active = active_pane.ok_or_else(|| anyhow::anyhow!("No active pane"))?;
                anyhow::ensure!(
                    selected.is_none_or(|d| d == active.daemon),
                    "Specify a pane on the selected connection"
                );
                *pane = Some(active);
                destination.set(Some(active.daemon));
            }
            Command::Move { tab, .. } if tab.is_none() => {
                let active = active_tab.ok_or_else(|| anyhow::anyhow!("No active tab"))?;
                anyhow::ensure!(
                    selected.is_none_or(|d| d == active.daemon),
                    "Specify a tab on the selected connection"
                );
                *tab = Some(active);
                destination.set(Some(active.daemon));
            }
            _ => (),
        }
        Ok((command, destination.get()))
    }
    pub fn route_command(
        &self,
        mut command: Command,
        destination: Option<DaemonId>,
    ) -> anyhow::Result<Route> {
        let mut daemon = destination;
        command.resolve_directory(daemon.is_some() && daemon == self.local)?;
        let mut assign = None;
        let creates_pane = matches!(
            command,
            Command::New { .. } | Command::Tab(TabCommand::Split { .. })
        );
        match &mut command {
            Command::New {
                workspace,
                directory,
                ..
            } => {
                let workspace = workspace.flatten();
                *directory = self.store.starting_directory(
                    workspace,
                    daemon.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Enable the Local Daemon or add a connection in Manage Connections"
                        )
                    })?,
                    directory.take(),
                )?;
                assign = Some(Assignment::CreatedTab { workspace });
            }
            Command::Tab(TabCommand::Split { pane: Some(id), .. })
            | Command::Tab(TabCommand::Close { pane: Some(id) }) => {
                daemon = Some(id.daemon);
            }
            Command::Tab(TabCommand::CloseTab { tab })
            | Command::Tab(TabCommand::Rename { tab, .. })
            | Command::Tab(TabCommand::ResetName { tab }) => {
                daemon = Some(tab.daemon);
            }
            Command::WorkspaceAdd { tab, .. } => {
                assign = tab.take().map(|tab| Assignment::CreatedWorkspace { tab });
            }
            _ => (),
        }
        let request = match command.into_execution() {
            Ok(command) => RoutedRequest::Execution {
                target: self
                    .target(daemon.ok_or_else(|| anyhow::anyhow!("No connection is selected"))?)?,
                request: execution::Request::Internal(execution::InternalRequest::Command {
                    command: command.map_ids(&mut |pane| Ok(pane.id), &mut |tab| Ok(tab.id))?,
                }),
            },
            Err(command) => {
                RoutedRequest::Organization(request::OrganizationRequest::Command(command))
            }
        };
        if creates_pane {
            anyhow::ensure!(
                daemon.is_some_and(|id| self.connected(id)),
                "Connection is disconnected or connecting. Wait for reconnection or check Manage Connections before creating a tab or pane"
            );
        }
        Ok(Route { request, assign })
    }
}

#[cfg(test)]
mod tests;
