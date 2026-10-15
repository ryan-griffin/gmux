//! Layout commands and their ordered persistence.
use super::*;
use gmux_client::control::{InternalRequest, PublicRequest, Request};

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
    Execution,
}
struct ControlReply {
    kind: ReplyKind,
    sender: std::sync::mpsc::SyncSender<Value>,
    value: Value,
    broadcast: bool,
}
pub(super) struct PendingControl {
    reply: ControlReply,
    pub(super) id: u64,
}
impl PendingControl {
    pub(super) fn start(
        request: ControlRequest,
        daemon: &mut Daemon,
        worker: &Worker,
    ) -> Option<Self> {
        if daemon.quitting && request.body.mutates_state() {
            let _ = request
                .reply
                .try_send(json!({"error":"Daemon is shutting down"}));
            return None;
        }
        let kind = match &request.body {
            Request::Internal(InternalRequest::Execution { request })
                if request.mutates_state() =>
            {
                ReplyKind::Execution
            }
            Request::Internal(InternalRequest::Command { .. }) => ReplyKind::Command,
            _ => ReplyKind::Direct,
        };
        let broadcast = matches!(kind, ReplyKind::Execution | ReplyKind::Command)
            || matches!(
                &request.body,
                Request::Layout(_) | Request::Internal(InternalRequest::Command { .. })
            );
        let result = daemon.control(request.body);
        let failed = result.is_err();
        let reply = ControlReply {
            kind,
            broadcast,
            sender: request.reply,
            value: result.unwrap_or_else(|error| json!({"error":error.to_string()})),
        };
        let save = daemon.pending_write.take();
        if !failed && let Some(save) = save {
            match worker.submit(save, false) {
                Ok(id) => return Some(Self { reply, id }),
                Err(error) => reply.save_failed(error.to_string()).send(daemon),
            }
        } else {
            reply.send(daemon);
        }
        None
    }
    pub(super) fn complete(self, daemon: &mut Daemon, result: Result<(), String>) {
        let Self { reply, .. } = self;
        if let Err(error) = result {
            reply.save_failed(error).send(daemon);
        } else {
            reply.send(daemon);
        }
    }
}
impl ControlReply {
    fn save_failed(mut self, error: String) -> Self {
        let result = if matches!(self.kind, ReplyKind::Command | ReplyKind::Execution) {
            &mut self.value["result"]
        } else {
            &mut self.value
        };
        result["warning"] = json!(format!("Could not save execution or scrollback: {error}"));
        self
    }
    fn send(mut self, daemon: &mut Daemon) {
        if self.broadcast && self.value.get("error").is_none() {
            daemon.revision += 1;
            self.value["daemon"] = json!(daemon.id);
        }
        if matches!(self.kind, ReplyKind::Command | ReplyKind::Execution)
            && self.value.get("error").is_none()
        {
            self.value = (|| -> anyhow::Result<Value> {
                let result = serde_json::from_value(self.value["result"].take())?;
                let mut reply = daemon.execution_state();
                reply.result = result;
                Ok(serde_json::to_value(reply)?)
            })()
            .unwrap_or_else(|error| json!({"error":error.to_string()}));
        }
        let _ = self.sender.try_send(self.value);
    }
}
impl Daemon {
    pub(super) fn list(&mut self) -> anyhow::Result<Value> {
        self.refresh_state();
        let tabs: Vec<_> = self
            .state
            .tabs
            .iter()
            .map(|tab| {
                let ids = tab.layout.ids();
                let panes: Vec<_> = ids
                    .iter()
                    .filter_map(|id| self.sessions.get(id))
                    .map(|session| {
                        json!({
                            "id": session.id(),
                            "reference": PaneRef { daemon: self.id, id: session.id() },
                            "title": session.title(),
                            "directory": session.directory(),
                            "exited": session.exited(),
                            "unread": self.unread.contains_key(&session.id()),
                        })
                    })
                    .collect();
                json!({
                    "id": tab.id,
                    "created_at": tab.created_at,
                    "reference": TabRef { daemon: self.id, id: tab.id },
                    "name": tab.name,
                    "unread": ids.iter().any(|id| self.unread.contains_key(id)),
                    "layout": tab.layout,
                    "panes": panes,
                })
            })
            .collect();
        Ok(json!({
            "daemon": self.id,
            "tabs": tabs,
        }))
    }
    pub(super) fn result_with_state_save(&mut self, output: CommandOutput) -> CommandResult {
        self.prepare_state_save();
        output.into()
    }
    pub(super) fn execute(&mut self, command: Command) -> anyhow::Result<CommandResult> {
        match command {
            Command::New { directory, name } => {
                let directory = directory.unwrap_or_else(model::home);
                let directory = model::expand_home(&directory);
                anyhow::ensure!(
                    directory.is_dir(),
                    "{} is not a directory",
                    directory.display()
                );
                let tab = self.reserve_ids(2)?;
                let pane = tab + 1;
                let session = TerminalSession::spawn(
                    pane,
                    tab,
                    directory.clone(),
                    CursorStyle::default(),
                    &Theme::adwaita(true),
                    None,
                )?;
                self.sessions.insert(pane, session);
                let name = name.filter(|n| !n.trim().is_empty());
                let created_at = creation_timestamp(
                    self.state
                        .tabs
                        .iter()
                        .map(|tab| tab.created_at)
                        .max()
                        .unwrap_or(0),
                    SystemTime::now(),
                );
                self.state.tabs.insert(
                    0,
                    ExecutionTab {
                        id: tab,
                        created_at,
                        name: name
                            .clone()
                            .unwrap_or_else(|| model::directory_name(&directory)),
                        custom_name: name.is_some(),
                        layout: PaneTree::Pane(pane),
                        directories: HashMap::from([(pane, directory)]),
                    },
                );
                Ok(self.result_with_state_save(CommandOutput::New { tab, pane }))
            }
            Command::Split { pane, down } => {
                let target = self.resolve_pane(pane)?;
                let tab = self
                    .state
                    .tabs
                    .iter()
                    .find(|t| t.layout.contains(target))
                    .ok_or_else(|| anyhow::anyhow!("Pane has no tab"))?;
                anyhow::ensure!(
                    tab.layout.ids().len() < 24,
                    "A tab can have at most 24 panes"
                );
                let tab_id = tab.id;
                self.sessions.get_mut(&target).unwrap().refresh_directory();
                let directory = self.sessions[&target].directory().to_path_buf();
                let pane = self.reserve_ids(1)?;
                let session = TerminalSession::spawn(
                    pane,
                    tab_id,
                    directory.clone(),
                    CursorStyle::default(),
                    &Theme::adwaita(true),
                    None,
                )?;
                self.sessions.insert(pane, session);
                let tab = self.state.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
                tab.layout.split(target, pane, down);
                tab.directories.insert(pane, directory);
                Ok(self.result_with_state_save(CommandOutput::Split { pane }))
            }
            Command::Close { pane } => {
                let pane = self.resolve_pane(pane)?;
                self.close(pane);
                Ok(self.result_with_state_save(CommandOutput::Done { ok: true }))
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
                Ok(self.result_with_state_save(CommandOutput::Done { ok: true }))
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
                Ok(self.result_with_state_save(CommandOutput::Done { ok: true }))
            }
            Command::RemoveTabName { tab } => {
                let tab = self
                    .state
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == tab)
                    .ok_or_else(|| anyhow::anyhow!("Tab {tab} does not exist"))?;
                let session = self
                    .sessions
                    .get_mut(&tab.layout.ids()[0])
                    .ok_or_else(|| anyhow::anyhow!("The tab's pane has been closed"))?;
                session.refresh_directory();
                tab.name = session.title();
                tab.custom_name = false;
                Ok(self.result_with_state_save(CommandOutput::Done { ok: true }))
            }
        }
    }
    pub(super) fn control(&mut self, request: Request) -> anyhow::Result<Value> {
        match request {
            Request::Execution(request) => self.execute_remote(request),
            Request::Internal(InternalRequest::Execution { request }) => {
                self.execute_remote(request)
            }
            Request::Layout(command) => Ok(serde_json::to_value(self.execute(command)?)?),
            Request::Internal(InternalRequest::Hello) => Ok(
                json!({"version":transport::VERSION,"daemon":self.id,"pid":std::process::id(),"machine_kind":gmux_client::model::MachineKind::local(),"hostname":model::hostname(),"home_directory":model::home(),"quitting":self.quitting}),
            ),
            Request::Public(PublicRequest::DaemonInfo) => Ok(json!({
                "version": transport::VERSION,
                "daemon": self.id,
                "hostname": model::hostname(),
                "home_directory": model::home(),
                "machine_kind": model::MachineKind::local(),
                "listen": self.hosting.address(),
                "pid": std::process::id(),
                "attached": self.frontends.values().any(|f| f.attached && !f.remote),
                "frontends": self.frontends.values().filter(|f| f.attached && !f.remote).count(),
                "remote_frontends": self.frontends.values().filter(|f| f.attached && f.remote).count(),
                "panes": self.sessions.len(),
                "uptime_seconds": self.started.elapsed().as_secs(),
                "snapshots": self.snapshots,
                "sent_rows": self.sent_rows,
                "git_subscribers": self.frontends.values().filter(|frontend| frontend.attached && !frontend.git_panes.is_empty()).count(),
                "git": self.git.as_ref().map_or_else(crate::branch::Stats::default, crate::branch::Service::stats),
            })),
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
            Request::Internal(InternalRequest::State) => {
                self.refresh_state();
                Ok(serde_json::to_value(self.execution_state())?)
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
                let frontend = self
                    .frontends
                    .iter()
                    .filter(|(_, f)| f.views.contains_key(&id))
                    .max_by_key(|(client, f)| (f.focused, **client))
                    .map(|(&client, _)| client);
                let text = if let Some(client) = frontend {
                    let view = self
                        .frontends
                        .get_mut(&client)
                        .unwrap()
                        .views
                        .get_mut(&id)
                        .unwrap();
                    self.sessions
                        .get_mut(&id)
                        .unwrap()
                        .with_view(view, |engine| engine.copy())?
                } else {
                    String::new()
                };
                Ok(json!({"text":text}))
            }
            Request::Internal(InternalRequest::Read { pane })
            | Request::Public(PublicRequest::Read { pane }) => {
                let id = self.resolve_pane(pane)?;
                Ok(json!({"text":self.sessions.get_mut(&id).unwrap().text()?}))
            }
            Request::Public(PublicRequest::Quit) => {
                let checkpoint = self.prepare_checkpoint();
                let warning = match checkpoint {
                    Ok(checkpoint) => {
                        self.pending_write = Some(Save::Checkpoint(checkpoint));
                        None
                    }
                    Err(error) => {
                        self.prepare_state_save();
                        Some(format!("Could not capture scrollback: {error}"))
                    }
                };
                self.checkpointed = true;
                self.quitting = true;
                Ok(json!({"ok":true,"warning":warning}))
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
        }
    }
    fn execute_remote(
        &mut self,
        request: gmux_client::control::ExecutionRequest,
    ) -> anyhow::Result<Value> {
        use gmux_client::control::ExecutionRequest;
        let request = match request.command() {
            Ok(command) => {
                if let Command::New {
                    directory: Some(directory),
                    ..
                } = &command
                {
                    anyhow::ensure!(
                        model::is_machine_directory(directory),
                        "Remote directories must be absolute paths or start with ~/ on the remote machine"
                    );
                }
                let result = self.execute(command)?;
                return Ok(json!({"result":result}));
            }
            Err(request) => request,
        };
        match request {
            ExecutionRequest::List => self.list(),
            ExecutionRequest::DaemonInfo => {
                self.control(Request::Public(PublicRequest::DaemonInfo))
            }
            ExecutionRequest::Quit => self.control(Request::Public(PublicRequest::Quit)),
            ExecutionRequest::State => {
                self.refresh_state();
                Ok(serde_json::to_value(self.execution_state())?)
            }
            ExecutionRequest::Read { pane } => {
                self.control(Request::Internal(InternalRequest::Read {
                    pane: Some(pane),
                }))
            }
            ExecutionRequest::Send {
                pane,
                text,
                paste,
                enter,
            } => self.control(Request::Public(PublicRequest::Send {
                pane: Some(pane),
                text,
                paste,
                enter,
            })),
            ExecutionRequest::Notify { pane, message } => {
                self.control(Request::Public(PublicRequest::Notify {
                    pane: Some(pane),
                    message,
                }))
            }
            ExecutionRequest::Busy { panes } => {
                self.control(Request::Internal(InternalRequest::Busy { panes }))
            }
            ExecutionRequest::Metadata { pane } => {
                self.control(Request::Internal(InternalRequest::Metadata {
                    pane: Some(pane),
                }))
            }
            ExecutionRequest::Copy { pane } => {
                self.control(Request::Internal(InternalRequest::Copy {
                    pane: Some(pane),
                }))
            }
            _ => unreachable!("Execution commands were handled above"),
        }
    }
}
