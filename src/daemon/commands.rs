//! Control commands and their ordered persistence.
use super::*;
use gmux_protocol::control::*;

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
enum ControlValue {
    Execution(ExecutionReply),
    Command(CommandResult),
    Hello(Value),
}
struct ControlReply {
    sender: std::sync::mpsc::SyncSender<Value>,
    result: anyhow::Result<ControlValue>,
    output_only: bool,
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
        let mut broadcast = false;
        let mut output_only = false;
        let result = (|| {
            if matches!(request.body, Request::Internal(InternalRequest::Hello)) {
                return Ok(ControlValue::Hello(daemon.hello()));
            }
            let (request, only) = request
                .body
                .into_execution(|pane| daemon.resolve_pane(pane))?;
            output_only = only;
            if let ExecutionRequest::Scrollback {
                max_bytes,
                max_lines,
            } = request
                && (max_bytes.is_some() || max_lines.is_some())
            {
                let candidate = daemon.limits.update(max_bytes, max_lines)?;
                daemon.pending_write = Some(Save::Settings(candidate));
                limits = Some(candidate);
                return Ok(ControlValue::Execution(ExecutionReply::Scrollback(
                    ScrollbackReply {
                        limits: candidate,
                        saved: Some(false),
                        applied: Some(false),
                        failures: vec![],
                        error: None,
                    },
                )));
            }
            broadcast = request.changes_layout();
            daemon.control(request)
        })();
        let failed = result.is_err();
        let reply = ControlReply {
            sender: request.reply,
            result,
            output_only,
            broadcast,
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
                    reply.result = Ok(ControlValue::Execution(ExecutionReply::Scrollback(
                        daemon.apply_scrollback_limits(limits),
                    )));
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
        self.result = Ok(ControlValue::Execution(ExecutionReply::Scrollback(
            ScrollbackReply {
                limits: current,
                saved: Some(false),
                applied: Some(false),
                failures: vec![],
                error: Some(format!(
                    "Could not durably save scrollback settings; running limits and history are unchanged: {error}"
                )),
            },
        )));
        self
    }
    fn save_failed(mut self, error: String) -> Self {
        let warning = Some(format!("Could not save execution or scrollback: {error}"));
        match &mut self.result {
            Ok(ControlValue::Command(result)) => result.warning = warning,
            Ok(ControlValue::Execution(ExecutionReply::Stopped(reply))) => reply.warning = warning,
            _ => (),
        }
        self
    }
    fn send(self, daemon: &mut Daemon) {
        let value = self
            .result
            .and_then(|value| {
                if self.broadcast {
                    daemon.revision += 1;
                }
                Ok(match value {
                    ControlValue::Command(result) if self.output_only => {
                        serde_json::to_value(CommandOutcome {
                            daemon: daemon.id,
                            result,
                        })?
                    }
                    ControlValue::Command(result) => {
                        let mut reply = daemon.execution_reply();
                        reply.result = result;
                        serde_json::to_value(reply)?
                    }
                    ControlValue::Execution(reply) => serde_json::to_value(reply)?,
                    ControlValue::Hello(value) => value,
                })
            })
            .unwrap_or_else(|error| json!({"error":error.to_string()}));
        let _ = self.sender.try_send(value);
    }
}
impl Daemon {
    fn apply_scrollback_limits(
        &mut self,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> ScrollbackReply {
        // Publication precedes irreversible pruning. Never roll back a policy
        // after applying it to only part of the history.
        self.limits = limits;
        let mut failures = Vec::new();
        for (&id, session) in &mut self.sessions {
            if let Err(error) = session.set_scrollback_limits(limits) {
                failures.push(PaneFailure {
                    pane: id,
                    error: error.to_string(),
                });
            }
            self.changed.insert(id);
        }
        let applied = failures.is_empty();
        ScrollbackReply {
            limits, saved: Some(true), applied: Some(applied), failures,
            error: (!applied).then(|| "Settings were saved, but some terminals could not apply them. Removed history cannot be restored".into()),
        }
    }
    pub(super) fn list(&mut self) -> anyhow::Result<ExecutionList> {
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
                    .map(|session| ListPane {
                        id: session.id(),
                        reference: PaneRef {
                            daemon: self.id,
                            id: session.id(),
                        },
                        title: session.title(),
                        directory: session.directory().to_owned(),
                        exited: session.exited(),
                        busy: session.busy(),
                        command: session.command_state(),
                        progress: session.progress(),
                        program_status: session.program_status().cloned(),
                        unread: self.unread.contains_key(&session.id()),
                    })
                    .collect();
                ListTab {
                    id: tab.id,
                    created_at: tab.created_at,
                    reference: TabRef {
                        daemon: self.id,
                        id: tab.id,
                    },
                    name: tab.name.clone(),
                    unread: ids.iter().any(|id| self.unread.contains_key(id)),
                    progress: gmux_protocol::progress::representative(
                        ids.iter()
                            .filter_map(|id| self.sessions.get(id).map(|s| (*id, s.progress()))),
                    ),
                    program_status: gmux_protocol::program_status::representative(
                        ids.iter().filter_map(|id| {
                            self.sessions.get(id).map(|s| (*id, s.program_status()))
                        }),
                    ),
                    layout: tab.layout.clone(),
                    panes,
                }
            })
            .collect();
        Ok(ExecutionList {
            daemon: self.id,
            tabs,
        })
    }
    fn result_with_state_save(&mut self, output: CommandOutput) -> ControlValue {
        self.prepare_state_save();
        ControlValue::Command(output.into())
    }
    fn control(&mut self, request: ExecutionRequest) -> anyhow::Result<ControlValue> {
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
            ExecutionRequest::Scrollback { .. } => Ok(ControlValue::Execution(
                ExecutionReply::Scrollback(ScrollbackReply {
                    limits: self.limits,
                    saved: None,
                    applied: None,
                    failures: vec![],
                    error: None,
                }),
            )),
            ExecutionRequest::Status => {
                Ok(ControlValue::Execution(ExecutionReply::Status(json!({
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
                }))))
            }
            ExecutionRequest::Busy { panes: ids } => {
                let busy = ids
                    .into_iter()
                    .any(|id| self.sessions.get(&id).is_some_and(TerminalSession::busy));
                Ok(ControlValue::Execution(ExecutionReply::Busy(BusyReply {
                    busy,
                })))
            }
            ExecutionRequest::State => {
                self.refresh_state();
                Ok(ControlValue::Execution(ExecutionReply::Snapshot(Box::new(
                    self.execution_reply(),
                ))))
            }
            ExecutionRequest::Metadata { pane: id } => {
                let session = self.sessions.get_mut(&id).unwrap();
                session.refresh_directory();
                Ok(ControlValue::Execution(ExecutionReply::Metadata(Box::new(
                    PaneMetadata {
                        meta: Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0)),
                        program_status_records: session
                            .program_status_records()
                            .into_iter()
                            .cloned()
                            .collect(),
                    },
                ))))
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
                Ok(ControlValue::Execution(ExecutionReply::Content(content)))
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
                Ok(ControlValue::Execution(ExecutionReply::Content(content)))
            }
            ExecutionRequest::Links { pane: id } => Ok(ControlValue::Execution(
                ExecutionReply::Links(self.sessions.get_mut(&id).unwrap().links()?),
            )),
            ExecutionRequest::Read { pane: id } => Ok(ControlValue::Execution(
                ExecutionReply::Content(self.sessions.get_mut(&id).unwrap().text()?.into()),
            )),
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
                Ok(ControlValue::Execution(ExecutionReply::Stopped(
                    StopReply { ok: true, warning },
                )))
            }
            ExecutionRequest::List => {
                Ok(ControlValue::Execution(ExecutionReply::List(self.list()?)))
            }
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
                Ok(ControlValue::Execution(ExecutionReply::Done(
                    Acknowledgement { ok: true },
                )))
            }
            ExecutionRequest::Notify { pane: id, message } => {
                self.queue_notifications(id, [message]);
                Ok(ControlValue::Execution(ExecutionReply::Done(
                    Acknowledgement { ok: true },
                )))
            }
        }
    }
    fn hello(&self) -> Value {
        json!({"version":transport::VERSION,"daemon":self.id,"pid":std::process::id(),"machine_kind":gmux_platform::unix::machine::machine_kind(),"hostname":gmux_platform::unix::machine::hostname(),"home_directory":gmux_platform::unix::paths::home(),"quitting":self.quitting})
    }
}
