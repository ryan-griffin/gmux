//! One app-wide local daemon policy, with recovery only while enabled.
use super::*;

pub(super) struct LocalDaemon {
    pending: Cell<bool>,
    retry: RefCell<Option<glib::SourceId>>,
    backoff: RefCell<gmux_client::connection::Backoff>,
    error: RefCell<Option<String>>,
}
impl Default for LocalDaemon {
    fn default() -> Self {
        Self {
            pending: Cell::new(false),
            retry: RefCell::new(None),
            backoff: RefCell::new(gmux_client::connection::Backoff::default()),
            error: RefCell::new(None),
        }
    }
}
impl LocalDaemon {
    pub(super) fn pending(&self) -> bool {
        self.pending.get()
    }
    pub(super) fn recovering(&self) -> bool {
        self.pending.get() || self.retry.borrow().is_some()
    }
    pub(super) fn error(&self) -> std::cell::Ref<'_, Option<String>> {
        self.error.borrow()
    }
    fn cancel_retry(&self) {
        if let Some(source) = self.retry.borrow_mut().take() {
            source.remove();
        }
    }
}
impl Drop for LocalDaemon {
    fn drop(&mut self) {
        self.cancel_retry();
    }
}

// A window can close while its request is queued. Let another window recover.
struct LocalOperation {
    owner: std::rc::Weak<Windows>,
    complete: bool,
}
impl LocalOperation {
    fn new(owner: &Rc<Windows>) -> Self {
        owner.local.pending.set(true);
        owner.refresh_local_controls();
        Self {
            owner: Rc::downgrade(owner),
            complete: false,
        }
    }
    fn finish(mut self) -> Option<Rc<Windows>> {
        self.complete = true;
        let owner = self.owner.upgrade()?;
        owner.local.pending.set(false);
        owner.refresh_local_controls();
        Some(owner)
    }
}
impl Drop for LocalOperation {
    fn drop(&mut self) {
        if !self.complete
            && let Some(owner) = self.owner.upgrade()
        {
            owner.local.pending.set(false);
            let weak = self.owner.clone();
            glib::idle_add_local_once(move || {
                if let Some(owner) = weak.upgrade() {
                    owner.refresh_local_controls();
                    owner.recover_local();
                }
            });
        }
    }
}

impl Windows {
    fn refresh_local_controls(&self) {
        for ui in self.windows.borrow().iter() {
            ui.refresh_connections_ui();
        }
    }
    fn local_enabled(&self) -> bool {
        self.preferences
            .borrow()
            .as_ref()
            .is_some_and(|preferences| preferences.settings.borrow().local_daemon_enabled)
    }
    fn local_disconnected(&self) -> bool {
        self.windows.borrow().iter().any(|ui| {
            let sessions = ui.sessions.borrow();
            sessions.local_daemon().is_none_or(|id| {
                matches!(
                    sessions.connection_status(id),
                    connections::ConnectionStatus::Disconnected { .. }
                )
            })
        })
    }
    pub(super) fn ensure_local(self: &Rc<Self>) {
        if !self.local_enabled()
            || self.local.pending.get()
            || self.local.retry.borrow().is_some()
            || !self.local_disconnected()
        {
            return;
        }
        let Some(ui) = self.active() else { return };
        let operation = LocalOperation::new(self);
        ui.start_local(move |ui, result| {
            let Some(owner) = operation.finish() else {
                return;
            };
            if let Err(error) = result {
                let message = error.to_string();
                if owner.local.error.borrow().as_ref() != Some(&message) {
                    ui.message(&message);
                    *owner.local.error.borrow_mut() = Some(message);
                }
                owner.recover_local();
            }
        });
    }
    pub(super) fn retry_local(self: &Rc<Self>) {
        if !self.local_enabled() || self.local.pending.get() || !self.local_disconnected() {
            return;
        }
        self.local.cancel_retry();
        self.ensure_local();
    }
    pub(super) fn recover_local(self: &Rc<Self>) {
        if !self.local_enabled() || self.local.pending.get() {
            return;
        }
        if !self.local_disconnected() {
            self.local.cancel_retry();
            if self.windows.borrow().iter().all(|ui| {
                let sessions = ui.sessions.borrow();
                sessions
                    .local_daemon()
                    .is_some_and(|id| sessions.connected(id))
            }) {
                self.local
                    .backoff
                    .borrow_mut()
                    .connected(std::time::Instant::now());
                self.local.error.borrow_mut().take();
            }
            return;
        }
        if self.local.retry.borrow().is_some() || self.windows.borrow().is_empty() {
            return;
        }
        let delay = self
            .local
            .backoff
            .borrow_mut()
            .next_delay(std::time::Instant::now());
        let weak = Rc::downgrade(self);
        *self.local.retry.borrow_mut() = Some(glib::timeout_add_local_once(delay, move || {
            if let Some(owner) = weak.upgrade() {
                owner.local.retry.borrow_mut().take();
                owner.ensure_local();
            }
        }));
        self.refresh_local_controls();
    }
}

impl Ui {
    pub(super) fn request_stop(
        self: &Rc<Self>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<gmux_protocol::control::StopReply>) + 'static,
    ) {
        self.request_reply(json!({"method":"local-daemon-stop"}), move |ui, result| {
            callback(
                ui,
                result.and_then(|reply| match reply {
                    // This local lifecycle operation acknowledges an absent daemon
                    // without receiving a daemon Stop reply.
                    BackendReply::Execution(gmux_protocol::control::ExecutionReply::Done(
                        reply,
                    )) => Ok(gmux_protocol::control::StopReply {
                        ok: reply.ok,
                        warning: None,
                    }),
                    BackendReply::Execution(reply) => reply.into_stopped(),
                    _ => anyhow::bail!("Expected a daemon stop reply"),
                }),
            );
        });
    }
    fn start_local(
        self: &Rc<Self>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        self.request_configuration(json!({"method":"local-daemon-start"}), move |ui, result| {
            let result = result.and_then(|value| {
                let machine: model::Machine = serde_json::from_value(value["local"].clone())?;
                ui.each_window(|window| {
                    window.backend.set_local(Some(machine.daemon));
                    window.sessions.borrow_mut().enable_local(machine.clone());
                    window.refresh_connections_ui();
                });
                Ok(())
            });
            callback(ui, result);
        });
    }
    pub(super) fn set_local_daemon_enabled(
        self: &Rc<Self>,
        enabled: bool,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        if owner.local.pending.get() {
            callback(
                self,
                Err(anyhow::anyhow!("The local connection is changing")),
            );
            return;
        }
        let operation = LocalOperation::new(&owner);
        owner.local.cancel_retry();
        *owner.local.backoff.borrow_mut() = gmux_client::connection::Backoff::default();
        owner.local.error.borrow_mut().take();
        let preferences = owner.preferences.borrow().as_ref().unwrap().clone();
        let finish = move |ui: &Rc<Ui>, result: anyhow::Result<()>| {
            if !enabled && result.is_err() && !ui.settings.borrow().local_daemon_enabled {
                ui.settings.borrow_mut().local_daemon_enabled = true;
                ui.persist_settings();
            }
            let Some(owner) = operation.finish() else {
                return;
            };
            owner.recover_local();
            callback(ui, result);
        };
        let weak = Rc::downgrade(self);
        let windows = Rc::downgrade(&owner);
        preferences.save_local_daemon_enabled(enabled, move |result| {
            let Some(ui) = weak
                .upgrade()
                .or_else(|| windows.upgrade().and_then(|owner| owner.active()))
            else {
                return;
            };
            if let Some(owner) = ui.owner.upgrade() {
                owner.refresh_local_controls();
            }
            if let Err(error) = result {
                finish(&ui, Err(error));
            } else if enabled {
                ui.start_local(finish);
            } else {
                ui.save_session();
                ui.request_stop(move |ui, result| {
                    let result = result.map(|_| {
                        ui.each_window(|window| {
                            window.sessions.borrow_mut().disable_local();
                            window.backend.set_local(None);
                            window.refresh_connections_ui();
                        });
                    });
                    finish(ui, result);
                });
            }
        });
    }
    fn each_window(&self, mut action: impl FnMut(&Rc<Ui>)) {
        if let Some(owner) = self.owner.upgrade() {
            for window in owner.windows.borrow().iter() {
                action(window);
            }
        }
    }
}
