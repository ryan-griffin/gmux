//! One app-wide local daemon policy, with recovery only while enabled.
use super::*;

pub(super) struct LocalDaemon {
    pending: Cell<bool>,
    retry: RefCell<Option<glib::SourceId>>,
    delay: Cell<u64>,
    error: RefCell<Option<String>>,
}
impl Default for LocalDaemon {
    fn default() -> Self {
        Self {
            pending: Cell::new(false),
            retry: RefCell::new(None),
            delay: Cell::new(1),
            error: RefCell::new(None),
        }
    }
}
impl LocalDaemon {
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
        Self {
            owner: Rc::downgrade(owner),
            complete: false,
        }
    }
    fn finish(mut self) -> Option<Rc<Windows>> {
        self.complete = true;
        let owner = self.owner.upgrade()?;
        owner.local.pending.set(false);
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
                    owner.recover_local();
                }
            });
        }
    }
}

impl Windows {
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
    pub(super) fn recover_local(self: &Rc<Self>) {
        if !self.local_enabled() || self.local.pending.get() {
            return;
        }
        if !self.local_disconnected() {
            self.local.cancel_retry();
            self.local.delay.set(1);
            self.local.error.borrow_mut().take();
            return;
        }
        if self.local.retry.borrow().is_some() || self.windows.borrow().is_empty() {
            return;
        }
        let delay = self.local.delay.get();
        self.local.delay.set((delay * 2).min(30));
        let weak = Rc::downgrade(self);
        *self.local.retry.borrow_mut() = Some(glib::timeout_add_local_once(
            Duration::from_secs(delay),
            move || {
                if let Some(owner) = weak.upgrade() {
                    owner.local.retry.borrow_mut().take();
                    owner.ensure_local();
                }
            },
        ));
    }
}

impl Ui {
    fn start_local(
        self: &Rc<Self>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        self.request(json!({"method":"local-daemon-start"}), move |ui, result| {
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
        owner.local.delay.set(1);
        owner.local.error.borrow_mut().take();
        let preferences = owner.preferences.borrow().as_ref().unwrap().clone();
        let finish = move |ui: &Rc<Ui>, result: anyhow::Result<()>| {
            let Some(owner) = operation.finish() else {
                return;
            };
            if !enabled && result.is_err() && !ui.settings.borrow().local_daemon_enabled {
                ui.settings.borrow_mut().local_daemon_enabled = true;
                ui.persist_settings();
            }
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
            if let Err(error) = result {
                finish(&ui, Err(error));
            } else if enabled {
                ui.start_local(finish);
            } else {
                ui.save_session();
                ui.request(json!({"method":"local-daemon-stop"}), move |ui, result| {
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
