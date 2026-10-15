//! GTK's opt-in policy for execution on this computer.
use super::*;

impl Ui {
    pub(super) fn start_local(
        self: &Rc<Self>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        self.request(json!({"method":"local-start"}), move |ui, result| {
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

    pub(super) fn set_local_execution(
        self: &Rc<Self>,
        enabled: bool,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        if enabled {
            self.start_local(move |ui, result| {
                if result.is_ok() {
                    ui.settings.borrow_mut().local_execution = true;
                    ui.persist_settings();
                }
                callback(ui, result);
            });
        } else {
            self.disconnect_local();
            callback(self, Ok(()));
        }
    }

    fn each_window(&self, mut action: impl FnMut(&Rc<Ui>)) {
        if let Some(owner) = self.owner.upgrade() {
            for window in owner.windows.borrow().iter() {
                action(window);
            }
        }
    }

    fn disconnect_local(&self) {
        self.settings.borrow_mut().local_execution = false;
        self.persist_settings();
        self.each_window(|window| {
            window.sessions.borrow_mut().disable_local();
            window.backend.set_local(None);
            window.refresh_connections_ui();
        });
    }

    pub(super) fn stop_local(
        self: &Rc<Self>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        self.save_session();
        self.request(json!({"method":"local-stop"}), move |ui, result| {
            let result = result.map(|_| ui.disconnect_local());
            callback(ui, result);
        });
    }
}
