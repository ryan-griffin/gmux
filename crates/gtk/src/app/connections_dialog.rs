//! Manage local and remote terminal connections on the backend worker.
use super::management::{error_label, show_error};
use super::*;
use connections::ConnectionStatus;
use gmux_client::{model::DaemonId, network::Authentication};

#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
struct RemoteDefinition {
    name: String,
    address: String,
    authentication: Authentication,
    daemon: DaemonId,
}
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
struct LocalDefinition {
    name: String,
    kind: model::MachineKind,
    daemon: Option<DaemonId>,
}
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
struct ConnectionList {
    local: LocalDefinition,
    running: bool,
    hostname: String,
    remotes: Vec<RemoteDefinition>,
    hosting: gmux_client::network::Hosting,
    hosting_error: Option<String>,
}
struct RemoteRow {
    daemon: DaemonId,
    row: adw::ActionRow,
    icon: gtk::Image,
    kind: Cell<model::MachineKind>,
    status: gtk::Label,
    reconnect: gtk::Button,
}
pub(super) struct ConnectionControls {
    dialog: adw::Dialog,
    group: adw::PreferencesGroup,
    error: gtk::Label,
    rows: RefCell<Vec<RemoteRow>>,
    local: RefCell<Option<adw::ActionRow>>,
    definitions: RefCell<Option<ConnectionList>>,
    revision: Cell<u64>,
    loading: Cell<bool>,
    reload: Cell<bool>,
}

fn authentication_changed(initial: &Authentication, selected: u32, token: &str) -> bool {
    authentication_value(selected, token).map_or(true, |value| &value != initial)
}

fn authentication_value(selected: u32, token: &str) -> anyhow::Result<Authentication> {
    if selected == 0 {
        return Ok(Authentication::None);
    }
    let file = model::expand_home(Path::new(token.trim()));
    Ok(Authentication::Token {
        file: if file.as_os_str().is_empty() {
            file
        } else {
            std::path::absolute(file)?
        },
    })
}

fn authentication_fields(
    ui: &Ui,
    initial: &Authentication,
) -> (adw::PreferencesGroup, adw::ComboRow, adw::EntryRow) {
    let group = adw::PreferencesGroup::builder().description("None allows anyone who can reach the address to connect. Token requires matching token contents on both computers. Use a private network or an encrypted tunnel").build();
    let authentication = adw::ComboRow::builder()
        .title("Authentication")
        .model(&gtk::StringList::new(&["None", "Token"]))
        .selected(0)
        .build();
    let token = adw::EntryRow::builder()
        .title("Token File")
        .activates_default(true)
        .visible(false)
        .build();
    let browse = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Choose Token File")
        .valign(gtk::Align::Center)
        .build();
    browse.add_css_class("flat");
    browse.update_property(&[gtk::accessible::Property::Label("Choose Token File")]);
    token.add_suffix(&browse);
    if let Authentication::Token { file } = initial {
        authentication.set_selected(1);
        token.set_text(&file.to_string_lossy());
    }
    token.set_visible(authentication.selected() == 1);
    let entry = token.downgrade();
    authentication.connect_selected_notify(move |auth| {
        if let Some(token) = entry.upgrade() {
            token.set_visible(auth.selected() == 1);
        }
    });
    let parent = ui.window.downgrade();
    let entry = token.downgrade();
    browse.connect_clicked(move |_| {
        let Some(parent) = parent.upgrade() else {
            return;
        };
        let entry = entry.clone();
        gtk::FileDialog::builder()
            .title("Choose Token File")
            .build()
            .open(Some(&parent), None::<&gio::Cancellable>, move |result| {
                if let (Ok(file), Some(entry)) = (result, entry.upgrade())
                    && let Some(path) = file.path()
                {
                    entry.set_text(&path.to_string_lossy());
                }
            });
    });
    group.add(&authentication);
    group.add(&token);
    (group, authentication, token)
}

impl ConnectionControls {
    fn refresh(self: &Rc<Self>, ui: &Rc<Ui>) {
        self.revision
            .set(ui.sessions.borrow().connection_revision());
        if self.loading.replace(true) {
            self.reload.set(true);
            return;
        }
        let weak = Rc::downgrade(self);
        ui.request(json!({"method":"connection-list"}), move |ui, result| {
            let Some(controls) = weak.upgrade() else {
                return;
            };
            controls.loading.set(false);
            match result.and_then(|v| Ok(serde_json::from_value::<ConnectionList>(v)?)) {
                Ok(list) => controls.populate(ui, list),
                Err(error) => show_error(&controls.error, error),
            }
            if controls.reload.replace(false) {
                controls.refresh(ui);
            }
        });
    }
    fn populate(self: &Rc<Self>, ui: &Rc<Ui>, mut list: ConnectionList) {
        self.error.set_visible(false);
        list.remotes.sort_by(|a, b| a.name.cmp(&b.name));
        if self.definitions.borrow().as_ref() == Some(&list) {
            self.status(ui);
            return;
        }
        *self.definitions.borrow_mut() = Some(list.clone());
        if let Some(row) = self.local.borrow_mut().take() {
            self.group.remove(&row);
        }
        for remote in self.rows.borrow_mut().drain(..) {
            self.group.remove(&remote.row);
        }
        {
            let local = list.local;
            let row = adw::ActionRow::builder()
                .title(&local.name)
                .subtitle("This Computer")
                .use_markup(false)
                .activatable(true)
                .build();
            let icon = machine::icon(local.kind);
            row.add_prefix(&icon);
            let weak = Rc::downgrade(ui);
            row.connect_activated(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.request(
                        json!({"method":"connection-list"}),
                        |ui, result| match result
                            .and_then(|value| Ok(serde_json::from_value::<ConnectionList>(value)?))
                        {
                            Ok(list) => ui.local_connection_editor(
                                &list.local.name,
                                &list.hostname,
                                &list.hosting,
                                list.hosting_error.as_deref(),
                                list.running,
                            ),
                            Err(error) => ui.message(&error.to_string()),
                        },
                    );
                }
            });
            self.group.add(&row);
            *self.local.borrow_mut() = Some(row);
        }
        for remote in list.remotes {
            let row = adw::ActionRow::builder()
                .title(&remote.name)
                .subtitle(&remote.address)
                .subtitle_lines(1)
                .use_markup(false)
                .activatable(true)
                .build();
            let weak = Rc::downgrade(ui);
            let edit = remote.clone();
            row.connect_activated(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.remote_editor(Some(edit.clone()));
                }
            });
            row.set_tooltip_text(Some(&remote.address));
            let kind = ui.sessions.borrow().kind(remote.daemon);
            let icon = machine::icon(kind);
            row.add_prefix(&icon);
            let status = gtk::Label::new(None);
            status.add_css_class("dimmed");
            status.add_css_class("caption");
            row.add_suffix(&status);
            let reconnect = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text(format!("Reconnect to {}", remote.name))
                .valign(gtk::Align::Center)
                .build();
            reconnect.add_css_class("flat");
            reconnect.update_property(&[gtk::accessible::Property::Label(&format!(
                "Reconnect to {}",
                remote.name
            ))]);
            row.add_suffix(&reconnect);
            let weak = Rc::downgrade(ui);
            let controls = Rc::downgrade(self);
            let daemon = remote.daemon;
            reconnect.connect_clicked(move |_| {
                if let (Some(ui), Some(controls)) = (weak.upgrade(), controls.upgrade()) {
                    ui.sessions.borrow_mut().connect(daemon);
                    controls.status(&ui);
                }
            });
            self.group.add(&row);
            self.rows.borrow_mut().push(RemoteRow {
                daemon,
                row,
                icon,
                kind: Cell::new(kind),
                status,
                reconnect,
            });
        }
        self.status(ui);
    }
    fn status(&self, ui: &Ui) {
        let sessions = ui.sessions.borrow();
        for remote in self.rows.borrow().iter() {
            let kind = sessions.kind(remote.daemon);
            if remote.kind.replace(kind) != kind {
                machine::update_icon(&remote.icon, kind);
            }
            let (status, error, reconnect) = match sessions.connection_status(remote.daemon) {
                ConnectionStatus::Connected => ("Connected", None, false),
                ConnectionStatus::Connecting => ("Connecting…", None, false),
                ConnectionStatus::Disconnected { error } => ("Disconnected", error, true),
            };
            remote.status.set_text(status);
            remote.status.set_tooltip_text(error);
            remote.reconnect.set_sensitive(reconnect);
        }
    }
}

impl Ui {
    pub(super) fn refresh_connections_ui(self: &Rc<Self>) {
        let controls = self.connections_dialog.borrow().clone();
        let Some(controls) = controls else { return };
        if self.sessions.borrow().connection_revision() != controls.revision.get() {
            controls.refresh(self);
        }
        controls.status(self);
    }
    pub(super) fn manage_connections(self: &Rc<Self>) {
        if self.connection_editor_dialog.present(&self.window) {
            return;
        }
        if let Some(controls) = self.connections_dialog.borrow().as_ref() {
            controls.refresh(self);
            controls.dialog.present(Some(&self.window));
            return;
        }
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let add = adw::ButtonRow::builder()
            .title("Add Connection")
            .start_icon_name("list-add-symbolic")
            .build();
        group.add(&add);
        let error = error_label();
        group.add(&error);
        page.add(&group);
        view.set_content(Some(&page));
        let dialog = management::list_dialog("Connections", &view, &add);
        let controls = Rc::new(ConnectionControls {
            dialog,
            group,
            error,
            rows: RefCell::new(Vec::new()),
            local: RefCell::new(None),
            definitions: RefCell::new(None),
            revision: Cell::new(0),
            loading: Cell::new(false),
            reload: Cell::new(false),
        });
        let weak = Rc::downgrade(self);
        add.connect_activated(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.remote_editor(None);
            }
        });
        let weak = Rc::downgrade(self);
        controls.dialog.connect_closed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.connections_dialog.borrow_mut().take();
            }
        });
        *self.connections_dialog.borrow_mut() = Some(controls.clone());
        controls.refresh(self);
        controls.dialog.present(Some(&self.window));
    }
    fn remote_editor(self: &Rc<Self>, remote: Option<RemoteDefinition>) {
        let editing = remote.is_some();
        let label = if editing { "Save" } else { "Add" };
        let Some(dialog) = self.connection_editor_dialog.create(
            if editing {
                "Edit Connection"
            } else {
                "Add Connection"
            },
            &self.window,
        ) else {
            return;
        };
        let view = adw::ToolbarView::new();
        let (header, submit) = management::editor_header(&dialog, label);
        view.add_top_bar(&header);
        let page = adw::PreferencesPage::new();
        let connection = adw::PreferencesGroup::builder()
            .description(if editing { "Enter the remote address, including its port" }
                else { "Enter the remote address, including its port. Leave the name empty to use its hostname" })
            .build();
        let name = adw::EntryRow::builder()
            .title("Name")
            .activates_default(true)
            .build();
        let address = adw::EntryRow::builder()
            .title("Address")
            .activates_default(true)
            .build();
        connection.add(&name);
        connection.add(&address);
        page.add(&connection);
        let (credentials, authentication, token) = authentication_fields(
            self,
            &remote
                .as_ref()
                .map(|r| r.authentication.clone())
                .unwrap_or(Authentication::None),
        );
        if let Some(remote) = &remote {
            name.set_text(&remote.name);
            address.set_text(&remote.address);
        }
        page.add(&credentials);
        let error = error_label();
        credentials.add(&error);
        let remove = editing.then(|| management::remove_action(&page, "Remove Connection…"));
        if let (Some(remove), Some(remote)) = (remove, remote.as_ref()) {
            let weak = Rc::downgrade(self);
            let close = dialog.downgrade();
            let view = view.downgrade();
            let error = error.downgrade();
            let name = remote.name.clone();
            let daemon = remote.daemon;
            remove.connect_activated(move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let confirmation = adw::AlertDialog::builder()
                    .heading(format!("Remove {name}?"))
                    .body("Its tabs will disappear from the sidebar. Remote terminals will keep running")
                    .build();
                confirmation.add_response("cancel", "Cancel");
                confirmation.add_response("remove", "Remove Connection");
                confirmation.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                confirmation.set_default_response(Some("cancel"));
                confirmation.set_close_response("cancel");
                let weak = weak.clone();
                let close = close.clone();
                let view = view.clone();
                let error = error.clone();
                let name = name.clone();
                confirmation.connect_response(Some("remove"), move |_, _| {
                    let (Some(ui), Some(dialog), Some(view)) =
                        (weak.upgrade(), close.upgrade(), view.upgrade())
                    else { return };
                    if let Some(error) = error.upgrade() { error.set_visible(false); }
                    dialog.set_can_close(false);
                    view.set_sensitive(false);
                    let close = close.clone();
                    let view = view.downgrade();
                    let error = error.clone();
                    ui.request(json!({"method":"connection-remove", "connection":name,"daemon":daemon}), move |_, result| {
                        if let Some(dialog) = close.upgrade() { dialog.set_can_close(true); }
                        if let Some(view) = view.upgrade() { view.set_sensitive(true); }
                        match result {
                            Ok(_) => {
                                if let Some(dialog) = close.upgrade() { dialog.close(); }
                            }
                            Err(message) => {
                                if let Some(error) = error.upgrade() { show_error(&error, message); }
                            }
                        }
                    });
                });
                confirmation.present(Some(&ui.window));
            });
        }
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        // Weak widget references keep form signal handlers from retaining the dialog.
        let fields = (
            name.downgrade(),
            address.downgrade(),
            authentication.downgrade(),
            token.downgrade(),
            submit.downgrade(),
        );
        let validation_error = error.downgrade();
        let original = remote.clone();
        let validate = Rc::new(move || {
            if let Some(error) = validation_error.upgrade() {
                error.set_visible(false);
            }
            if let (Some(name), Some(address), Some(auth), Some(token), Some(submit)) = (
                fields.0.upgrade(),
                fields.1.upgrade(),
                fields.2.upgrade(),
                fields.3.upgrade(),
                fields.4.upgrade(),
            ) {
                let name = name.text();
                let address = address.text();
                let token = token.text();
                let changed = original.as_ref().is_none_or(|remote| {
                    name.trim() != remote.name
                        || address.trim() != remote.address
                        || authentication_changed(&remote.authentication, auth.selected(), &token)
                });
                submit.set_sensitive(
                    changed
                        && (!editing || !name.trim().is_empty())
                        && !address.trim().is_empty()
                        && (auth.selected() == 0 || !token.trim().is_empty()),
                );
            }
        });
        for field in [&name, &address, &token] {
            let validate = validate.clone();
            field.connect_changed(move |_| validate());
        }
        validate();
        authentication.connect_selected_notify(move |_| validate());
        let initial_focus = name.downgrade();
        let default_button = submit.downgrade();
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        let view = view.downgrade();
        submit.connect_clicked(move |button| {
            let (Some(ui), Some(dialog), Some(view)) =
                (weak.upgrade(), close.upgrade(), view.upgrade())
            else {
                return;
            };
            let authentication =
                match authentication_value(authentication.selected(), &token.text()) {
                    Ok(value) => value,
                    Err(message) => {
                        show_error(&error, message);
                        return;
                    }
                };
            error.set_visible(false);
            view.set_sensitive(false);
            dialog.set_can_close(false);
            button.set_label(if editing {
                "Saving…"
            } else {
                "Connecting…"
            });
            let button = button.downgrade();
            let error = error.downgrade();
            let close = dialog.downgrade();
            let view = view.downgrade();
            let mut request = json!({"method":"connection-add", "name":name.text().trim(),
                "address":address.text().trim(), "authentication":authentication});
            if let Some(remote) = &remote {
                request["method"] = json!("connection-edit");
                request["connection"] = json!(remote.name);
                request["daemon"] = json!(remote.daemon);
            } else if name.text().trim().is_empty() {
                request["name"] = Value::Null;
            }
            ui.request(request, move |ui, result| {
                if let Some(dialog) = close.upgrade() {
                    dialog.set_can_close(true);
                }
                if let Some(view) = view.upgrade() {
                    view.set_sensitive(true);
                }
                if let Some(button) = button.upgrade() {
                    button.set_label(label);
                }
                match result {
                    Ok(value) => {
                        if let Ok(daemon) = serde_json::from_value(value["daemon"].clone()) {
                            ui.sessions.borrow_mut().connect(daemon);
                        }
                        if let Some(dialog) = close.upgrade() {
                            dialog.close();
                        }
                    }
                    Err(message) => {
                        if let Some(error) = error.upgrade() {
                            show_error(&error, message);
                        }
                    }
                }
            });
        });
        dialog.present(Some(&self.window));
        if let Some(button) = default_button.upgrade() {
            dialog.set_default_widget(Some(&button));
        }
        if let Some(name) = initial_focus.upgrade() {
            dialog.set_focus(Some(&name));
        }
    }

    fn local_connection_editor(
        self: &Rc<Self>,
        current: &str,
        hostname: &str,
        hosting: &gmux_client::network::Hosting,
        hosting_error: Option<&str>,
        running: bool,
    ) {
        let Some(dialog) = self
            .connection_editor_dialog
            .create("Local Connection", &self.window)
        else {
            return;
        };
        let view = adw::ToolbarView::new();
        let (header, save) = management::editor_header(&dialog, "Save");
        view.add_top_bar(&header);
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let name = adw::EntryRow::builder()
            .title("Name")
            .text(current)
            .activates_default(true)
            .build();
        let reset = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Use Hostname")
            .valign(gtk::Align::Center)
            .build();
        reset.add_css_class("flat");
        reset.update_property(&[gtk::accessible::Property::Label("Use Hostname")]);
        name.add_suffix(&reset);
        let entry = name.downgrade();
        let default_name = hostname.to_owned();
        reset.connect_clicked(move |_| {
            if let Some(name) = entry.upgrade() {
                name.set_text(&default_name);
                name.grab_focus();
            }
        });
        group.add(&name);
        let error = error_label();
        group.add(&error);
        page.add(&group);
        let execution_group = adw::PreferencesGroup::builder()
            .title("Execution")
            .description("Disabling local execution disconnects this app. The daemon and its terminals keep running")
            .build();
        let local_execution = adw::SwitchRow::builder()
            .title("Local Execution")
            .subtitle("Save to start or connect now and on future launches")
            .active(self.settings.borrow().local_execution)
            .build();
        execution_group.add(&local_execution);
        let runtime = adw::ButtonRow::builder()
            .title(if running {
                "Stop Daemon…"
            } else {
                "Start Daemon"
            })
            .tooltip_text(if running {
                "Stop this daemon's terminals and disable Local Execution"
            } else {
                "Start with saved settings and enable Local Execution"
            })
            .build();
        execution_group.add(&runtime);
        page.add(&execution_group);
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        let runtime_error = error.downgrade();
        let runtime_view = view.downgrade();
        runtime.connect_activated(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            let busy_dialog = close.clone();
            let busy_view = runtime_view.clone();
            let finished_view = runtime_view.clone();
            let close = close.clone();
            let error = runtime_error.clone();
            let finish = move |ui: &Rc<Ui>, result: anyhow::Result<()>| {
                if let Some(view) = finished_view.upgrade() {
                    view.set_sensitive(true);
                }
                if let Some(dialog) = close.upgrade() {
                    dialog.set_can_close(true);
                    match result {
                        Ok(()) => {
                            dialog.close();
                        }
                        Err(message) => {
                            if let Some(error) = error.upgrade() {
                                show_error(&error, message);
                            }
                        }
                    }
                }
                if let Some(controls) = ui.connections_dialog.borrow().as_ref() {
                    controls.refresh(ui);
                }
            };
            if running {
                let confirm = adw::AlertDialog::builder()
                    .heading("Stop Local Daemon?")
                    .body("All terminals managed by this daemon will stop, including those used by other clients. Remote terminals will keep running")
                    .build();
                confirm.add_response("cancel", "Cancel");
                confirm.add_response("stop", "Stop Daemon");
                confirm.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");
                let weak = Rc::downgrade(&ui);
                let finish = RefCell::new(Some(finish));
                confirm.connect_response(Some("stop"), move |_, _| {
                    if let (Some(ui), Some(finish)) = (weak.upgrade(), finish.borrow_mut().take()) {
                        if let Some(view) = busy_view.upgrade() {
                            view.set_sensitive(false);
                        }
                        if let Some(dialog) = busy_dialog.upgrade() {
                            dialog.set_can_close(false);
                        }
                        ui.stop_local(finish);
                    }
                });
                confirm.present(Some(&ui.window));
            } else {
                if let Some(view) = busy_view.upgrade() {
                    view.set_sensitive(false);
                }
                if let Some(dialog) = busy_dialog.upgrade() {
                    dialog.set_can_close(false);
                }
                ui.set_local_execution(true, finish);
            }
        });
        let host_group = adw::PreferencesGroup::builder().title("Incoming Connections")
            .description("Enter an IP address and port to listen on. A loopback address accepts only local connections").build();
        let enabled = adw::SwitchRow::builder()
            .title("Allow Remote Connections")
            .active(hosting.enabled)
            .build();
        let address = adw::EntryRow::builder()
            .title("Listening Address")
            .text(hosting.address.to_string())
            .activates_default(true)
            .build();
        host_group.add(&enabled);
        host_group.add(&address);
        page.add(&host_group);
        let (credentials, authentication, token) =
            authentication_fields(self, &hosting.authentication);
        page.add(&credentials);
        let original_address = hosting.address;
        let fields = (
            enabled.downgrade(),
            address.downgrade(),
            credentials.downgrade(),
            authentication.downgrade(),
            token.downgrade(),
            save.downgrade(),
            error.downgrade(),
            name.downgrade(),
            reset.downgrade(),
        );
        let hostname = hostname.to_owned();
        let default_name = hostname.clone();
        let original_name = current.to_owned();
        let original_hosting = hosting.clone();
        let original_execution = self.settings.borrow().local_execution;
        let execution = local_execution.downgrade();
        let runtime_action = runtime.downgrade();
        let validate = Rc::new(move || {
            if let (
                Some(enabled),
                Some(address),
                Some(credentials),
                Some(auth),
                Some(token),
                Some(save),
                Some(name),
                Some(reset),
                Some(execution),
            ) = (
                fields.0.upgrade(),
                fields.1.upgrade(),
                fields.2.upgrade(),
                fields.3.upgrade(),
                fields.4.upgrade(),
                fields.5.upgrade(),
                fields.7.upgrade(),
                fields.8.upgrade(),
                execution.upgrade(),
            ) {
                let name = name.text();
                reset.set_sensitive(name.trim() != default_name);
                address.set_sensitive(enabled.is_active());
                credentials.set_sensitive(enabled.is_active());
                let address = address.text().trim().parse::<std::net::SocketAddr>().ok();
                let token = token.text();
                let changed = execution.is_active() != original_execution
                    || name.trim() != original_name
                    || enabled.is_active() != original_hosting.enabled
                    || address.is_none_or(|address| address != original_hosting.address)
                    || authentication_changed(
                        &original_hosting.authentication,
                        auth.selected(),
                        &token,
                    );
                if let Some(runtime) = runtime_action.upgrade() {
                    runtime.set_sensitive(!changed);
                    runtime.set_tooltip_text(if changed {
                        Some("Save or cancel changes before starting or stopping the daemon")
                    } else {
                        None
                    });
                }
                save.set_sensitive(
                    changed
                        && !name.trim().is_empty()
                        && (!enabled.is_active()
                            || (address.is_some()
                                && (auth.selected() == 0 || !token.trim().is_empty()))),
                );
            }
            if let Some(error) = fields.6.upgrade() {
                error.set_visible(false);
            }
        });
        for entry in [&name, &address, &token] {
            let validate = validate.clone();
            entry.connect_changed(move |_| validate());
        }
        let changed = validate.clone();
        authentication.connect_selected_notify(move |_| changed());
        let changed = validate.clone();
        enabled.connect_active_notify(move |_| changed());
        let changed = validate.clone();
        local_execution.connect_active_notify(move |_| changed());
        validate();
        if let Some(message) = hosting_error {
            show_error(&error, message);
        }
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        let initial_focus = name.downgrade();
        let default_button = save.downgrade();
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        let view = view.downgrade();
        save.connect_clicked(move |button| {
            let (Some(ui), Some(dialog), Some(view)) =
                (weak.upgrade(), close.upgrade(), view.upgrade())
            else {
                return;
            };
            let authentication =
                match authentication_value(authentication.selected(), &token.text()) {
                    Ok(value) => value,
                    Err(message) => {
                        show_error(&error, message);
                        return;
                    }
                };
            error.set_visible(false);
            view.set_sensitive(false);
            dialog.set_can_close(false);
            button.set_label("Saving…");
            let value = name.text();
            let value = (value.trim() != hostname).then(|| value.trim().to_owned());
            let close = dialog.downgrade();
            let view = view.downgrade();
            let button = button.downgrade();
            let error = error.downgrade();
            let hosting = gmux_client::network::Hosting {
                enabled: enabled.is_active(),
                address: address.text().trim().parse().unwrap_or(original_address),
                authentication,
            };
            let execution = local_execution.is_active();
            ui.request(
                json!({"method":"connection-edit","connection":"local","name":value,"hosting":hosting}),
                move |ui, result| {
                    let finish = move |_: &Rc<Ui>, result: anyhow::Result<()>| {
                        if let Some(dialog) = close.upgrade() {
                            dialog.set_can_close(true);
                        }
                        if let Some(view) = view.upgrade() {
                            view.set_sensitive(true);
                        }
                        if let Some(button) = button.upgrade() {
                            button.set_label("Save");
                        }
                        match result {
                            Ok(_) => {
                                if let Some(dialog) = close.upgrade() {
                                    dialog.close();
                                }
                            }
                            Err(message) => {
                                if let Some(error) = error.upgrade() {
                                    show_error(&error, message);
                                }
                            }
                        }
                    };
                    match result {
                        Ok(_) => ui.set_local_execution(execution, finish),
                        Err(error) => finish(ui, Err(error)),
                    }
                },
            );
        });
        dialog.present(Some(&self.window));
        if let Some(button) = default_button.upgrade() {
            dialog.set_default_widget(Some(&button));
        }
        if let Some(name) = initial_focus.upgrade() {
            dialog.set_focus(Some(&name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_paths_belong_to_the_frontend_and_equivalent_inputs_are_unchanged() {
        let initial = Authentication::Token {
            file: std::env::current_dir().unwrap().join("tokens/shared"),
        };
        assert!(authentication_value(1, " tokens/shared ").unwrap() == initial);
        assert!(!authentication_changed(&initial, 1, " tokens/shared "));
        assert!(
            authentication_value(1, "~/gmux-token").unwrap()
                == Authentication::Token {
                    file: model::home().join("gmux-token"),
                }
        );
        assert!(!authentication_changed(
            &Authentication::None,
            0,
            "hidden token path"
        ));
    }
}
