//! Control commands and their ordered persistence.
use super::*;
use crate::persistence::SaveMode;
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
}
pub(super) struct PendingControl {
    reply: ControlReply,
    effects: PendingEffects,
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
        let mut output_only = false;
        let outcome = (|| {
            if matches!(request.body, Request::Internal(InternalRequest::Hello)) {
                return Ok(Mutation::unchanged(ControlValue::Hello(daemon.hello())));
            }
            let (request, only) = request
                .body
                .into_execution(|pane| daemon.resolve_pane(pane))?;
            output_only = only;
            daemon.control(request)
        })();
        let (result, effects) = match outcome {
            Ok(Mutation { value, effects }) => (Ok(value), effects),
            Err(error) => (Err(error), Effects::None),
        };
        let reply = ControlReply {
            sender: request.reply,
            result,
            output_only,
        };
        match effects.start(daemon, worker, SaveMode::Explicit) {
            Progress::Saving(effects) => Some(Self { reply, effects }),
            Progress::Complete(completion) => {
                reply.complete(completion).send(daemon);
                None
            }
        }
    }
    pub(super) fn id(&self) -> u64 {
        self.effects.id
    }
    pub(super) fn complete(self, daemon: &mut Daemon, result: Result<(), String>) {
        let completion = self.effects.complete(daemon, result);
        self.reply.complete(completion).send(daemon);
    }
}
impl ControlReply {
    fn complete(mut self, completion: Completion) -> Self {
        match completion {
            Completion::Saved => (),
            Completion::Scrollback(reply) => {
                self.result = Ok(ControlValue::Execution(ExecutionReply::Scrollback(reply)));
            }
            Completion::SaveFailed(error) => {
                let warning = format!("Could not save execution or scrollback: {error}");
                let target = match &mut self.result {
                    Ok(ControlValue::Command(result)) => &mut result.warning,
                    Ok(ControlValue::Execution(ExecutionReply::Stopped(reply))) => {
                        &mut reply.warning
                    }
                    _ => unreachable!("Only execution mutations and shutdown save before replying"),
                };
                if let Some(previous) = target {
                    previous.push_str("; ");
                    previous.push_str(&warning);
                } else {
                    *target = Some(warning);
                }
            }
        }
        self
    }
    fn send(self, daemon: &Daemon) {
        let value = self
            .result
            .and_then(|value| {
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
    fn control(&mut self, request: ExecutionRequest) -> anyhow::Result<Mutation<ControlValue>> {
        let value: anyhow::Result<ControlValue> = match request {
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
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::New { tab, pane }.into(),
                )));
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
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::Split { pane }.into(),
                )));
            }
            ExecutionRequest::Close { pane } => {
                self.close(pane);
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::Done { ok: true }.into(),
                )));
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
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::Done { ok: true }.into(),
                )));
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
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::Done { ok: true }.into(),
                )));
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
                return Ok(self.execution_mutation(ControlValue::Command(
                    CommandOutput::Done { ok: true }.into(),
                )));
            }
            ExecutionRequest::Scrollback {
                max_bytes,
                max_lines,
            } => {
                if max_bytes.is_some() || max_lines.is_some() {
                    let limits = self.limits.update(max_bytes, max_lines)?;
                    return Ok(Mutation {
                        value: ControlValue::Execution(ExecutionReply::Scrollback(
                            ScrollbackReply {
                                limits,
                                saved: Some(false),
                                applied: Some(false),
                                failures: vec![],
                                error: None,
                            },
                        )),
                        effects: Effects::ScrollbackSettings(limits),
                    });
                }
                Ok(ControlValue::Execution(ExecutionReply::Scrollback(
                    ScrollbackReply {
                        limits: self.limits,
                        saved: None,
                        applied: None,
                        failures: vec![],
                        error: None,
                    },
                )))
            }
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
                    "frontend_resources": {
                        "streams": self.frontends.len(),
                        "output_capacity": self.frontends.values().map(Frontend::retained_output).sum::<usize>(),
                        "screen_cache_bytes": self.frontends.values().map(Frontend::retained_screens).sum::<usize>(),
                        "stream_limit": resources::MAX_FRONTENDS,
                        "output_limit": resources::MAX_OUTPUT,
                        "screen_cache_limit": resources::MAX_SCREEN_CACHES,
                    },
                    "scrollback_limits": self.limits,
                    "terminal_memory": self.sessions.values()
                        .map(TerminalSession::memory_usage).collect::<anyhow::Result<Vec<_>>>()?,
                    "git_subscribers": self.frontends.values().filter(|frontend| frontend.attached && frontend.has_git_subscriptions()).count(),
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
                    .filter(|(_, f)| f.view(id).is_some())
                    .max_by_key(|(client, f)| (f.focused, **client))
                    .map(|(&client, _)| client);
                let content = if let Some(client) = frontend {
                    let view = self
                        .frontends
                        .get_mut(&client)
                        .unwrap()
                        .view_mut(id)
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
                let Mutation { value, effects } = self.shutdown();
                return Ok(Mutation {
                    value: ControlValue::Execution(ExecutionReply::Stopped(value)),
                    effects,
                });
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
        };
        Ok(Mutation::unchanged(value?))
    }
    fn hello(&self) -> Value {
        json!({"version":transport::VERSION,"daemon":self.id,"pid":std::process::id(),"machine_kind":gmux_platform::unix::machine::machine_kind(),"hostname":gmux_platform::unix::machine::hostname(),"home_directory":gmux_platform::unix::paths::home(),"quitting":self.quitting})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn request(body: Value) -> (ControlRequest, mpsc::Receiver<Value>) {
        let (reply, receiver) = mpsc::sync_channel(1);
        let request = ControlRequest {
            body: Request::decode(body).unwrap(),
            reply,
        };
        (request, receiver)
    }
    fn daemon() -> Daemon {
        let mut daemon = Daemon::empty_for_test();
        daemon.state.tabs.push(ExecutionTab {
            id: 1,
            created_at: 1,
            name: "Before".into(),
            custom_name: true,
            layout: PaneTree::Pane(2),
            directories: HashMap::new(),
        });
        daemon
    }

    #[test]
    fn live_revision_precedes_save_completion_and_reply_delivery() {
        let mut daemon = daemon();
        let mut worker = Worker::with_save(gmux_platform::unix::ipc::Wake::new().unwrap(), |_| {
            anyhow::bail!("disk failure")
        });
        let (rename, reply) = request(
            json!({"method":"_command", "command":{"method":"rename", "tab":1, "name":"After"}}),
        );
        let pending = PendingControl::start(rename, &mut daemon, &worker).unwrap();
        assert_eq!(daemon.revision, 1);
        assert!(reply.try_recv().is_err());
        // A read observes the published live revision, even before the owner
        // consumes the disk result. Reply delivery itself has no effects.
        let (state, read) = request(json!({"method":"_state"}));
        assert!(PendingControl::start(state, &mut daemon, &worker).is_none());
        let state = read.recv().unwrap();
        assert_eq!(state["revision"], 1);
        assert_eq!(state["execution"]["tabs"][0]["name"], "After");
        worker.finish();
        let (id, result) = worker.results().next().unwrap();
        assert_eq!(pending.id(), id);
        pending.complete(&mut daemon, result);
        let reply = reply.recv().unwrap();
        assert_eq!(reply["revision"], 1);
        assert!(
            reply["result"]["warning"]
                .as_str()
                .unwrap()
                .contains("disk failure")
        );
        assert_eq!(daemon.state.tabs[0].name, "After");

        let (invalid, reply) = request(json!({"method":"rename", "tab":1, "name":" "}));
        assert!(PendingControl::start(invalid, &mut daemon, &worker).is_none());
        assert!(reply.recv().unwrap()["error"].is_string());
        assert_eq!(daemon.revision, 1);
    }

    #[test]
    fn submission_failure_keeps_live_execution_but_does_not_apply_settings() {
        let mut daemon = daemon();
        let limits = daemon.limits;
        let mut worker =
            Worker::with_save(gmux_platform::unix::ipc::Wake::new().unwrap(), |_| Ok(()));
        worker.finish();
        let (rename, reply) = request(json!({"method":"rename", "tab":1, "name":"Live"}));
        assert!(PendingControl::start(rename, &mut daemon, &worker).is_none());
        assert!(
            reply.recv().unwrap()["warning"]
                .as_str()
                .unwrap()
                .contains("closed")
        );
        assert_eq!(daemon.revision, 1);
        assert_eq!(daemon.state.tabs[0].name, "Live");

        let (settings, reply) = request(json!({"method":"scrollback", "max_lines":0}));
        assert!(PendingControl::start(settings, &mut daemon, &worker).is_none());
        let reply = reply.recv().unwrap();
        assert_eq!(reply["saved"], false);
        assert_eq!(reply["applied"], false);
        assert_eq!(daemon.limits, limits);
        assert_eq!(daemon.revision, 1);
    }

    #[test]
    fn shutdown_checkpoint_is_durable_only_after_success_and_failure_allows_retry() {
        let mut daemon = Daemon::empty_for_test();
        let mut worker = Worker::with_save(gmux_platform::unix::ipc::Wake::new().unwrap(), |_| {
            anyhow::bail!("checkpoint failure")
        });
        let (stop, reply) = request(json!({"method":"stop"}));
        let pending = PendingControl::start(stop, &mut daemon, &worker).unwrap();
        assert!(daemon.quitting);
        assert!(!daemon.checkpointed);
        worker.finish();
        let (_, result) = worker.results().next().unwrap();
        pending.complete(&mut daemon, result);
        assert!(
            reply.recv().unwrap()["warning"]
                .as_str()
                .unwrap()
                .contains("checkpoint failure")
        );
        assert!(!daemon.checkpointed);
        assert_eq!(daemon.revision, 0);

        let mut retry =
            Worker::with_save(gmux_platform::unix::ipc::Wake::new().unwrap(), |_| Ok(()));
        let Progress::Saving(pending) =
            daemon
                .shutdown()
                .effects
                .start(&mut daemon, &retry, SaveMode::Explicit)
        else {
            panic!("Expected shutdown save");
        };
        retry.finish();
        let (_, result) = retry.results().next().unwrap();
        assert!(matches!(
            pending.complete(&mut daemon, result),
            Completion::Saved
        ));
        assert!(daemon.checkpointed);
    }
}
