//! Control commands and their ordered persistence.
use super::*;
use gmux_protocol::control::{ExecutionRequest, InternalRequest, Request};

pub(super) struct ControlRequest {
    pub(super) body: Request,
    reply: std::sync::mpsc::SyncSender<Value>,
}
impl ControlRequest {
    pub(super) fn decode(request: gmux_platform::unix::ipc::Request) -> Option<Self> {
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
    Snapshot,
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
    limits: Option<gmux_protocol::retention::ScrollbackLimits>,
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
        let mut limits = None;
        let mut kind = ReplyKind::Direct;
        let mut broadcast = false;
        let result: anyhow::Result<Value> = (|| {
            if matches!(request.body, Request::Internal(InternalRequest::Hello)) {
                return Ok(daemon.hello());
            }
            let (request, output_only) = request
                .body
                .into_execution(|pane| daemon.resolve_pane(pane))?;
            if let ExecutionRequest::Scrollback {
                max_bytes,
                max_lines,
            } = request
                && (max_bytes.is_some() || max_lines.is_some())
            {
                let candidate = daemon.limits.update(max_bytes, max_lines)?;
                daemon.pending_write = Some(Save::Settings(candidate));
                limits = Some(candidate);
                return Ok(json!({"limits": candidate, "saved": false, "applied": false}));
            }
            broadcast = request.changes_layout();
            if broadcast && !output_only {
                kind = ReplyKind::Snapshot;
            }
            let mut value = daemon.control(request)?;
            Ok(if output_only {
                value["result"].take()
            } else {
                value
            })
        })();
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
                Ok(id) => return Some(Self { reply, id, limits }),
                Err(error) => {
                    if limits.is_some() {
                        reply
                            .settings_failed(error.to_string(), daemon.limits)
                            .send(daemon);
                    } else {
                        reply.save_failed(error.to_string()).send(daemon);
                    }
                }
            }
        } else {
            reply.send(daemon);
        }
        None
    }
    pub(super) fn complete(self, daemon: &mut Daemon, result: Result<(), String>) {
        let Self {
            mut reply, limits, ..
        } = self;
        match result {
            Ok(()) => {
                if let Some(limits) = limits {
                    reply.value = daemon.apply_scrollback_limits(limits);
                }
            }
            Err(error) => {
                reply = if limits.is_some() {
                    reply.settings_failed(error, daemon.limits)
                } else {
                    reply.save_failed(error)
                };
            }
        }
        reply.send(daemon);
    }
}
impl ControlReply {
    fn settings_failed(
        mut self,
        error: String,
        current: gmux_protocol::retention::ScrollbackLimits,
    ) -> Self {
        self.value["limits"] = json!(current);
        self.value["error"] = json!(format!(
            "Could not durably save scrollback settings; running limits and history are unchanged: {error}"
        ));
        self.value["saved"] = json!(false);
        self.value["applied"] = json!(false);
        self
    }
    fn save_failed(mut self, error: String) -> Self {
        let result = if self.kind == ReplyKind::Snapshot {
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
        if self.kind == ReplyKind::Snapshot && self.value.get("error").is_none() {
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
    fn apply_scrollback_limits(
        &mut self,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> Value {
        // Publication precedes irreversible pruning. Never roll back a policy
        // after applying it to only part of the history.
        self.limits = limits;
        let mut failures = Vec::new();
        for (&id, session) in &mut self.sessions {
            if let Err(error) = session.set_scrollback_limits(limits) {
                failures.push(json!({"pane": id, "error": error.to_string()}));
            }
            self.changed.insert(id);
        }
        let mut result = json!({"limits": limits, "saved": true, "applied": failures.is_empty()});
        if !failures.is_empty() {
            result["error"] = json!(
                "Settings were saved, but some terminals could not apply them. Removed history cannot be restored"
            );
            result["failures"] = json!(failures);
        }
        result
    }
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
                            "busy": session.busy(),
                            "command": session.command_state(),
                            "progress": session.progress(),
                            "program_status": session.program_status(),
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
                    "progress": gmux_protocol::progress::representative(ids.iter().filter_map(|id|
                        self.sessions.get(id).map(|s| (*id, s.progress())))),
                    "program_status": gmux_protocol::program_status::representative(ids.iter().filter_map(|id|
                        self.sessions.get(id).map(|s| (*id, s.program_status())))),
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
    fn result_with_state_save(&mut self, output: CommandOutput) -> Value {
        self.prepare_state_save();
        json!({"result":CommandResult::from(output)})
    }
    fn control(&mut self, request: ExecutionRequest) -> anyhow::Result<Value> {
        match request {
            ExecutionRequest::New { directory, name } => {
                let directory = directory.unwrap_or_else(gmux_platform::unix::paths::home);
                let directory = gmux_platform::unix::paths::expand_home(&directory);
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
                    self.limits,
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
                        name: name.clone().unwrap_or_else(|| {
                            gmux_platform::unix::paths::directory_name(&directory)
                        }),
                        custom_name: name.is_some(),
                        layout: PaneTree::Pane(pane),
                        directories: HashMap::from([(pane, directory)]),
                    },
                );
                Ok(self.result_with_state_save(CommandOutput::New { tab, pane }))
            }
            ExecutionRequest::Split { pane: target, down } => {
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
                    self.limits,
                )?;
                self.sessions.insert(pane, session);
                let tab = self.state.tabs.iter_mut().find(|t| t.id == tab_id).unwrap();
                tab.layout.split(target, pane, down);
                tab.directories.insert(pane, directory);
                Ok(self.result_with_state_save(CommandOutput::Split { pane }))
            }
            ExecutionRequest::Close { pane } => {
                self.close(pane);
                Ok(self.result_with_state_save(CommandOutput::Done { ok: true }))
            }
            ExecutionRequest::CloseTab { tab } => {
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
            ExecutionRequest::Rename { tab, name } => {
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
            ExecutionRequest::ResetName { tab } => {
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
            ExecutionRequest::Scrollback { .. } => Ok(json!({"limits": self.limits})),
            ExecutionRequest::Status => Ok(json!({
                "version": transport::VERSION,
                "daemon": self.id,
                "hostname": gmux_platform::unix::machine::hostname(),
                "home_directory": gmux_platform::unix::paths::home(),
                "machine_kind": gmux_platform::unix::machine::machine_kind(),
                "bound_address": self.remote_access.bound_address(),
                "pid": std::process::id(),
                "local_clients": self.frontends.values().filter(|f| f.attached && !f.remote).count(),
                "remote_clients": self.frontends.values().filter(|f| f.attached && f.remote).count(),
                "panes": self.sessions.len(),
                "uptime_seconds": self.started.elapsed().as_secs(),
                "snapshots": self.snapshots,
                "sent_rows": self.sent_rows,
                "scrollback_limits": self.limits,
                "terminal_memory": self.sessions.values()
                    .map(TerminalSession::memory_usage).collect::<anyhow::Result<Vec<_>>>()?,
                "git_subscribers": self.frontends.values().filter(|frontend| frontend.attached && !frontend.git_panes.is_empty()).count(),
                "git": self.git.as_ref().map_or_else(crate::branch::Stats::default, crate::branch::Service::stats),
            })),
            ExecutionRequest::Busy { panes: ids } => {
                let busy = ids
                    .into_iter()
                    .any(|id| self.sessions.get(&id).is_some_and(TerminalSession::busy));
                Ok(json!({"busy":busy}))
            }
            ExecutionRequest::State => {
                self.refresh_state();
                Ok(serde_json::to_value(self.execution_state())?)
            }
            ExecutionRequest::Metadata { pane: id } => {
                let session = self.sessions.get_mut(&id).unwrap();
                session.refresh_directory();
                Ok(
                    json!({"meta":Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0)),
                        "program_status_records": session.program_status_records()}),
                )
            }
            ExecutionRequest::CommandOutput {
                pane: id,
                point,
                formats,
            } => {
                let content = self
                    .sessions
                    .get_mut(&id)
                    .unwrap()
                    .command_output(point, formats)?;
                Ok(serde_json::to_value(content)?)
            }
            ExecutionRequest::Copy { pane: id, formats } => {
                let frontend = self
                    .frontends
                    .iter()
                    .filter(|(_, f)| f.views.contains_key(&id))
                    .max_by_key(|(client, f)| (f.focused, **client))
                    .map(|(&client, _)| client);
                let content = if let Some(client) = frontend {
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
                        .with_view(view, |engine| engine.copy_content(formats))?
                } else {
                    String::new().into()
                };
                Ok(serde_json::to_value(content)?)
            }
            ExecutionRequest::Links { pane: id } => self.sessions.get_mut(&id).unwrap().links(),
            ExecutionRequest::Read { pane: id } => {
                Ok(json!({"text":self.sessions.get_mut(&id).unwrap().text()?}))
            }
            ExecutionRequest::Stop => {
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
            ExecutionRequest::List => self.list(),
            ExecutionRequest::Send {
                pane: id,
                text,
                paste,
                enter,
            } => {
                let session = self.sessions.get_mut(&id).unwrap();
                if paste {
                    session.send_text(&text, enter)?;
                } else {
                    session.send_with_enter(text.as_bytes(), enter)?;
                }
                Ok(json!({"ok":true}))
            }
            ExecutionRequest::Notify { pane: id, message } => {
                self.queue_notifications(id, [message]);
                Ok(json!({"ok":true}))
            }
        }
    }
    fn hello(&self) -> Value {
        json!({"version":transport::VERSION,"daemon":self.id,"pid":std::process::id(),"machine_kind":gmux_platform::unix::machine::machine_kind(),"hostname":gmux_platform::unix::machine::hostname(),"home_directory":gmux_platform::unix::paths::home(),"quitting":self.quitting})
    }
}
