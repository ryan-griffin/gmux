//! Layout commands and their ordered persistence stages.
use super::*;
use crate::control::{InternalRequest, PublicRequest, Request};

pub(super) enum CatalogEffect {
    Assign { tab: u64, workspace: u64 },
    Remove(u64),
}
impl CatalogEffect {
    fn apply(self, daemon: &mut Daemon) {
        match self {
            Self::Assign { tab, workspace } => {
                if let Some(tab) = daemon.state.tabs.iter_mut().find(|t| t.id == tab) {
                    tab.workspace = Some(workspace);
                }
            }
            Self::Remove(workspace) => {
                for tab in &mut daemon.state.tabs {
                    if tab.workspace == Some(workspace) {
                        tab.workspace = None;
                    }
                }
                if daemon.state.filter == TabFilter::Workspace(workspace) {
                    daemon.state.filter = TabFilter::Unassigned;
                }
            }
        }
        daemon.select_visible();
    }
}

pub(super) enum PreparedSave {
    Session(Session),
    Catalog {
        catalog: WorkspaceCatalog,
        effect: Option<CatalogEffect>,
    },
}
enum SaveStage {
    Session,
    Catalog { effect: Option<CatalogEffect> },
}
impl PreparedSave {
    fn into_job(self) -> (Save, SaveStage) {
        match self {
            Self::Session(session) => (Save::Session(session), SaveStage::Session),
            Self::Catalog { catalog, effect } => {
                (Save::Catalog(catalog), SaveStage::Catalog { effect })
            }
        }
    }
}
pub(super) struct ControlRequest {
    pub(super) body: Request,
    reply: std::sync::mpsc::SyncSender<Value>,
}
impl ControlRequest {
    pub(super) fn decode(request: ipc::Request) -> Option<Self> {
        match Request::decode(request.body) {
            Ok(body) => Some(Self {
                body,
                reply: request.reply,
            }),
            Err(error) => {
                let _ = request.reply.try_send(json!({"error": error.to_string()}));
                None
            }
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    Direct,
    Command,
    Presentation,
}
struct ControlReply {
    kind: ReplyKind,
    sender: std::sync::mpsc::SyncSender<Value>,
    value: Value,
}
pub(super) struct PendingControl {
    reply: ControlReply,
    pub(super) id: u64,
    stage: SaveStage,
}
impl PendingControl {
    pub(super) fn start(
        request: ControlRequest,
        daemon: &mut Daemon,
        worker: &Worker,
    ) -> Option<Self> {
        let kind = match &request.body {
            Request::Internal(InternalRequest::Command { .. }) => ReplyKind::Command,
            Request::Internal(InternalRequest::Presentation { .. }) => ReplyKind::Presentation,
            _ => ReplyKind::Direct,
        };
        let result = daemon.control(request.body);
        let failed = result.is_err();
        let reply = ControlReply {
            kind,
            sender: request.reply,
            value: result.unwrap_or_else(|error| json!({"error":error.to_string()})),
        };
        let save = daemon.pending_write.take();
        if !failed && let Some(save) = save {
            let (job, stage) = save.into_job();
            match worker.submit(job, false) {
                Ok(id) => return Some(Self { reply, id, stage }),
                Err(error) => reply.save_failed(&stage, error.to_string()).send(daemon),
            }
        } else {
            reply.send(daemon);
        }
        None
    }
    pub(super) fn complete(
        self,
        daemon: &mut Daemon,
        worker: &Worker,
        result: Result<(), String>,
    ) -> Option<Self> {
        let Self { reply, stage, .. } = self;
        if let Err(error) = result {
            reply.save_failed(&stage, error).send(daemon);
        } else if let SaveStage::Catalog {
            effect: Some(effect),
        } = stage
        {
            effect.apply(daemon);
            match worker.submit(Save::Session(daemon.session_snapshot()), false) {
                Ok(id) => {
                    return Some(Self {
                        reply,
                        id,
                        stage: SaveStage::Session,
                    });
                }
                Err(error) => reply
                    .save_failed(&SaveStage::Session, error.to_string())
                    .send(daemon),
            }
        } else {
            reply.send(daemon);
        }
        None
    }
}
impl ControlReply {
    fn save_failed(mut self, stage: &SaveStage, error: String) -> Self {
        if matches!(stage, SaveStage::Catalog { .. }) || self.kind == ReplyKind::Presentation {
            self.value = json!({"error":error});
        } else {
            let result = if self.kind == ReplyKind::Command {
                &mut self.value["result"]
            } else {
                &mut self.value
            };
            result["warning"] = json!(format!("Could not save layout: {error}"));
        }
        self
    }
    fn send(mut self, daemon: &Daemon) {
        if self.kind == ReplyKind::Command && self.value.get("error").is_none() {
            let result = (|| -> anyhow::Result<()> {
                self.value["session"] = serde_json::to_value(&daemon.state)?;
                self.value["catalog"] = serde_json::to_value(storage::load_catalog()?)?;
                self.value["metadata"] = json!(
                    daemon
                        .sessions
                        .iter()
                        .map(|(&id, session)| {
                            Daemon::metadata(session, daemon.unread.get(&id).copied().unwrap_or(0))
                        })
                        .collect::<Vec<_>>()
                );
                Ok(())
            })();
            if let Err(error) = result {
                self.value = json!({"error":error.to_string()});
            }
        }
        let _ = self.sender.try_send(self.value);
    }
}
impl Daemon {
    pub(super) fn response_with_session_save(&mut self, result: Value) -> Value {
        self.prepare_session_save();
        result
    }
    pub(super) fn list(&mut self) -> anyhow::Result<Value> {
        self.refresh_state();
        let catalog = storage::load_catalog()?;
        Ok(
            json!({"active_tab":(self.state.active!=0).then_some(self.state.active),"filter":self.state.filter,"tab_search":"","workspaces":catalog.workspaces.iter().map(|w|json!({"id":w.id,"name":w.name,"directory":w.directory,"tabs":self.state.tabs.iter().filter(|t|t.workspace==Some(w.id)).count(),"unread":self.state.tabs.iter().any(|t|t.workspace==Some(w.id)&&t.layout.ids().iter().any(|id|self.unread.contains_key(id)))})).collect::<Vec<_>>(),"tabs":self.state.tabs.iter().map(|t| json!({"id":t.id,"name":t.name,"workspace":t.workspace,"visible":self.state.filter.matches(t.workspace),"active_pane":t.active,"unread":t.layout.ids().iter().any(|id|self.unread.contains_key(id)),"layout":t.layout,"panes":t.layout.ids().iter().filter_map(|id|self.sessions.get(id)).map(|s|json!({"id":s.id(),"title":s.title(),"directory":s.directory(),"exited":s.exited(),"unread":self.unread.contains_key(&s.id())})).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
        )
    }
    pub(super) fn result_with_session_save(&mut self, output: CommandOutput) -> CommandResult {
        self.prepare_session_save();
        output.into()
    }
    pub(super) fn execute(&mut self, command: Command) -> anyhow::Result<CommandResult> {
        let mut catalog = storage::load_catalog()?;
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
                let settings = storage::load::<Settings>("settings.json")?;
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
                self.focus_tab(tab)?;
                Ok(self.result_with_session_save(CommandOutput::New {
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
                let settings = storage::load::<Settings>("settings.json")?;
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
                Ok(self.result_with_session_save(CommandOutput::Split { pane }))
            }
            Command::Close { pane } => {
                let pane = self.resolve_pane(pane)?;
                self.close(pane);
                Ok(self.result_with_session_save(CommandOutput::Done { ok: true }))
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
                Ok(self.result_with_session_save(CommandOutput::Done { ok: true }))
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
                Ok(self.result_with_session_save(CommandOutput::Done { ok: true }))
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
                self.pending_write = Some(PreparedSave::Catalog {
                    catalog,
                    effect: tab.map(|tab| CatalogEffect::Assign { tab, workspace }),
                });
                Ok(CommandOutput::Workspace { workspace }.into())
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
                self.pending_write = Some(PreparedSave::Catalog {
                    catalog,
                    effect: None,
                });
                Ok(CommandOutput::Workspace { workspace }.into())
            }
            Command::WorkspaceDelete { workspace } => {
                anyhow::ensure!(
                    catalog.workspaces.iter().any(|w| w.id == workspace),
                    "Workspace {workspace} does not exist"
                );
                catalog.workspaces.retain(|w| w.id != workspace);
                self.pending_write = Some(PreparedSave::Catalog {
                    catalog,
                    effect: Some(CatalogEffect::Remove(workspace)),
                });
                Ok(CommandOutput::Done { ok: true }.into())
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
                Ok(self.result_with_session_save(CommandOutput::Done { ok: true }))
            }
        }
    }
    pub(super) fn control(&mut self, request: Request) -> anyhow::Result<Value> {
        match request {
            Request::Layout(command) => Ok(serde_json::to_value(self.execute(command)?)?),
            Request::Internal(InternalRequest::Hello) => {
                Ok(json!({"version":transport::VERSION,"quitting":self.quitting}))
            }
            Request::Public(PublicRequest::DaemonInfo) => Ok(
                json!({"version":transport::VERSION,"pid":std::process::id(),"attached":self.frontend.as_ref().is_some_and(|f|f.attached),"panes":self.sessions.len(),"uptime_seconds":self.started.elapsed().as_secs(),"snapshots":self.snapshots,"sent_rows":self.sent_rows}),
            ),
            Request::Internal(InternalRequest::Busy { panes: ids }) => {
                let busy = ids
                    .into_iter()
                    .any(|id| self.sessions.get(&id).is_some_and(TerminalSession::busy));
                Ok(json!({"busy":busy}))
            }
            Request::Internal(InternalRequest::Command { command }) => {
                let result = self.execute(command)?;
                Ok(json!({"result":result}))
            }
            Request::Internal(InternalRequest::Focus { tab: tab_id, pane }) => {
                if tab_id == 0 {
                    self.state.active = 0;
                } else {
                    let pane = pane.ok_or_else(|| anyhow::anyhow!("focus requires a pane"))?;
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
            Request::Internal(InternalRequest::Presentation { session: state }) => {
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
                self.prepare_session_save();
                Ok(json!({"ok":true}))
            }
            Request::Internal(InternalRequest::State) => {
                self.refresh_state();
                Ok(
                    json!({"session":self.state,"metadata":self.sessions.iter().map(|(&id, s)|Self::metadata(s,self.unread.get(&id).copied().unwrap_or(0))).collect::<Vec<_>>() }),
                )
            }
            Request::Internal(InternalRequest::Metadata { pane }) => {
                let id = self.resolve_pane(pane)?;
                let session = self.sessions.get_mut(&id).unwrap();
                session.refresh_directory();
                Ok(
                    json!({"meta":Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0))}),
                )
            }
            Request::Internal(InternalRequest::Copy { pane }) => {
                let id = self.resolve_pane(pane)?;
                Ok(json!({"text":self.sessions[&id].copy()?}))
            }
            Request::Internal(InternalRequest::Read { pane })
            | Request::Public(PublicRequest::Read { pane }) => {
                let id = self.resolve_pane(pane)?;
                Ok(json!({"text":self.sessions.get_mut(&id).unwrap().text()?}))
            }
            Request::Internal(InternalRequest::Close { pane }) => {
                let id = pane
                    .or_else(|| self.resolve_pane(None).ok())
                    .ok_or_else(|| anyhow::anyhow!("close requires pane"))?;
                self.close(id);
                Ok(self.response_with_session_save(json!({"ok":true})))
            }
            Request::Public(PublicRequest::Quit) => {
                self.prepare_session_save();
                self.quitting = true;
                Ok(json!({"ok":true,"warning":null}))
            }
            Request::Public(PublicRequest::List) => self.list(),
            Request::Public(PublicRequest::Send {
                pane,
                text,
                paste,
                enter,
            }) => {
                let id = self.resolve_pane(pane)?;
                let session = self.sessions.get_mut(&id).unwrap();
                if paste {
                    session.send_text(&text, enter)?;
                } else {
                    session.send(text.as_bytes())?;
                }
                Ok(json!({"ok":true}))
            }
            Request::Public(PublicRequest::Notify { pane, message }) => {
                let id = self.resolve_pane(pane)?;
                self.queue_notifications(id, [message]);
                Ok(json!({"ok":true}))
            }
            Request::Public(PublicRequest::Focus {
                pane,
                tab,
                workspace,
            }) => {
                if let Some(id) = pane {
                    self.resolve_pane(pane)?;
                    let tab = self
                        .state
                        .tabs
                        .iter_mut()
                        .find(|t| t.layout.ids().contains(&id))
                        .unwrap();
                    tab.active = id;
                    let tab_id = tab.id;
                    self.focus_tab(tab_id)?;
                    self.unread.remove(&id);
                } else if let Some(id) = tab {
                    self.focus_tab(id)?;
                } else if let Some(id) = workspace {
                    anyhow::ensure!(
                        storage::load_catalog()?
                            .workspaces
                            .iter()
                            .any(|w| w.id == id),
                        "Workspace {id} does not exist"
                    );
                    self.state.filter = TabFilter::Workspace(id);
                    self.select_visible();
                } else {
                    anyhow::bail!("focus requires a tab, pane, or workspace");
                }
                Ok(self.response_with_session_save(json!({"ok":true})))
            }
        }
    }
}
