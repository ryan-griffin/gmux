//! Manage local and remote terminal connections on the backend worker.
use super::management::{error_label, show_error};
use super::*;
use connections::ConnectionStatus;
use gmux_protocol::{model::DaemonId, network::Authentication};

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
    machine_kind: model::MachineKind,
    daemon: Option<DaemonId>,
}
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
struct ConnectionList {
    local: LocalDefinition,
    running: bool,
    hostname: String,
    remotes: Vec<RemoteDefinition>,
    remote_access: gmux_protocol::network::RemoteAccess,
    remote_access_error: Option<String>,
}
struct RemoteRow {
    daemon: DaemonId,
    row: adw::ActionRow,
    icon: gtk::Image,
    machine_kind: Cell<model::MachineKind>,
    status: gtk::Label,
    retry: gtk::Button,
}
struct LocalRow {
    row: adw::ActionRow,
    status: gtk::Label,
    retry: gtk::Button,
}

struct ConnectionFields {
    name: glib::WeakRef<adw::EntryRow>,
    address: glib::WeakRef<adw::EntryRow>,
    authentication: glib::WeakRef<adw::ComboRow>,
    token: glib::WeakRef<adw::EntryRow>,
    submit: glib::WeakRef<gtk::Button>,
    error: glib::WeakRef<gtk::Label>,
}
struct LocalConnectionFields {
    connection: ConnectionFields,
    enabled: glib::WeakRef<adw::SwitchRow>,
    credentials: glib::WeakRef<adw::PreferencesGroup>,
    reset: glib::WeakRef<gtk::Button>,
    execution: glib::WeakRef<adw::SwitchRow>,
}

fn display_address(address: &str) -> &str {
    match address.rsplit_once(':') {
        Some((host, port))
            if port.parse::<u16>() == Ok(gmux_protocol::network::DEFAULT_PORT)
                && (!host.contains(':') || host.starts_with('[')) =>
        {
            host
        }
        _ => address,
    }
}

pub(super) struct ConnectionControls {
    dialog: adw::Dialog,
    group: adw::PreferencesGroup,
    error: gtk::Label,
    rows: RefCell<Vec<RemoteRow>>,
    local: RefCell<Option<LocalRow>>,
    definitions: RefCell<Option<ConnectionList>>,
    revision: Cell<u64>,
    loading: Cell<bool>,
    reload: Cell<bool>,
}

fn retry_button(name: &str) -> gtk::Button {
    let label = format!("Retry connection to {name} now");
    let button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(&label)
        .valign(gtk::Align::Center)
        .build();
    button.add_css_class("flat");
    button.update_property(&[gtk::accessible::Property::Label(&label)]);
    button
}

fn can_retry(status: ConnectionStatus<'_>) -> bool {
    matches!(
        status,
        ConnectionStatus::Disconnected { .. }
            | ConnectionStatus::Reconnecting {
                retry_at: Some(_),
                ..
            }
    )
}

fn set_status(label: &gtk::Label, text: &str, style: &str, error: Option<&str>) {
    label.set_text(text);
    label.set_css_classes(&["caption", style]);
    label.set_tooltip_text(error);
}

fn update_status(label: &gtk::Label, status: ConnectionStatus<'_>) {
    let (text, style, error) = match status {
        ConnectionStatus::Connected => ("Connected", "success", None),
        ConnectionStatus::Connecting => ("Connecting…", "dimmed", None),
        ConnectionStatus::Reconnecting { error, .. } => ("Reconnecting…", "warning", error),
        ConnectionStatus::Disconnected { error: Some(error) } => {
            ("Disconnected", "error", Some(error))
        }
        ConnectionStatus::Disconnected { error: None } => ("Disconnected", "dimmed", None),
    };
    set_status(label, text, style, error);
}

fn authentication_changed(initial: &Authentication, selected: u32, token: &str) -> bool {
    authentication_value(selected, token).map_or(true, |value| &value != initial)
}

fn authentication_value(selected: u32, token: &str) -> anyhow::Result<Authentication> {
    if selected == 0 {
        return Ok(Authentication::None);
    }
    let file = gmux_platform::unix::paths::expand_home(Path::new(token.trim()));
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
        if let Some(local) = self.local.borrow_mut().take() {
            self.group.remove(&local.row);
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
            let icon = machine::icon(local.machine_kind);
            row.add_prefix(&icon);
            let status = gtk::Label::new(None);
            row.add_suffix(&status);
            let retry = retry_button(&local.name);
            let weak = Rc::downgrade(ui);
            retry.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade()
                    && let Some(owner) = ui.owner.upgrade()
                {
                    owner.retry_local();
                }
            });
            row.add_suffix(&retry);
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
                                &list.remote_access,
                                list.remote_access_error.as_deref(),
                            ),
                            Err(error) => ui.message(&error.to_string()),
                        },
                    );
                }
            });
            self.group.add(&row);
            *self.local.borrow_mut() = Some(LocalRow { row, status, retry });
        }
        for remote in list.remotes {
            let row = adw::ActionRow::builder()
                .title(&remote.name)
                .subtitle(display_address(&remote.address))
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
            row.add_suffix(&status);
            let retry = retry_button(&remote.name);
            row.add_suffix(&retry);
            let weak = Rc::downgrade(ui);
            let controls = Rc::downgrade(self);
            let daemon = remote.daemon;
            retry.connect_clicked(move |_| {
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
                machine_kind: Cell::new(kind),
                status,
                retry,
            });
        }
        self.status(ui);
    }
    fn status(&self, ui: &Ui) {
        let sessions = ui.sessions.borrow();
        if let Some(local) = self.local.borrow().as_ref() {
            let enabled = ui.settings.borrow().local_daemon_enabled;
            let status = sessions
                .local_daemon()
                .map_or(ConnectionStatus::Disconnected { error: None }, |id| {
                    sessions.connection_status(id)
                });
            let owner = ui.owner.upgrade();
            let local_error = owner.as_ref().map(|owner| owner.local.error());
            let error = local_error.as_ref().and_then(|error| error.as_deref());
            match status {
                _ if !enabled => set_status(&local.status, "Disabled", "dimmed", None),
                ConnectionStatus::Connecting if error.is_some() => {
                    set_status(&local.status, "Reconnecting…", "warning", error);
                }
                ConnectionStatus::Disconnected {
                    error: connection_error,
                } if owner.as_ref().is_some_and(|owner| owner.local.recovering()) => {
                    if let Some(error) = connection_error.or(error) {
                        set_status(&local.status, "Reconnecting…", "warning", Some(error));
                    } else {
                        set_status(&local.status, "Connecting…", "dimmed", None);
                    }
                }
                _ => update_status(&local.status, status),
            }
            local.retry.set_visible(enabled);
            local.retry.set_sensitive(
                enabled
                    && can_retry(status)
                    && owner.as_ref().is_some_and(|owner| !owner.local.pending()),
            );
        }
        for remote in self.rows.borrow().iter() {
            let kind = sessions.kind(remote.daemon);
            if remote.machine_kind.replace(kind) != kind {
                machine::update_icon(&remote.icon, kind);
            }
            let status = sessions.connection_status(remote.daemon);
            update_status(&remote.status, status);
            remote.retry.set_sensitive(can_retry(status));
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
            .description(if editing {
                format!("Enter a hostname or IP address. The default port is {}", gmux_protocol::network::DEFAULT_PORT)
            } else {
                format!("Enter a hostname or IP address. The default port is {}. Leave the name empty to use its hostname", gmux_protocol::network::DEFAULT_PORT)
            })
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
            address.set_text(display_address(&remote.address));
        }
        page.add(&credentials);
        let error = error_label();
        credentials.add(&error);
        let remove = editing.then(|| management::remove_action(&page, "Remove Connection…"));
        if let (Some(remove), Some(remote)) = (remove, remote.as_ref()) {
            let weak = Rc::downgrade(self);
            let editor = management::EditorRequest::new(&dialog, &view, &submit, &error);
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
                let editor = editor.clone();
                let name = name.clone();
                confirmation.connect_response(Some("remove"), move |_, _| {
                    let Some(ui) = weak.upgrade() else { return };
                    editor.begin(None);
                    let editor = editor.clone();
                    ui.request(json!({"method":"connection-remove", "connection":name,"daemon":daemon}), move |_, result| {
                        editor.finish(result.map(|_| ()));
                    });
                });
                confirmation.present(Some(&ui.window));
            });
        }
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        // Weak widget references keep form signal handlers from retaining the dialog.
        let fields = ConnectionFields {
            name: name.downgrade(),
            address: address.downgrade(),
            authentication: authentication.downgrade(),
            token: token.downgrade(),
            submit: submit.downgrade(),
            error: error.downgrade(),
        };
        let original = remote.clone();
        let validate = Rc::new(move || {
            if let Some(error) = fields.error.upgrade() {
                error.set_visible(false);
            }
            if let (Some(name), Some(address), Some(auth), Some(token), Some(submit)) = (
                fields.name.upgrade(),
                fields.address.upgrade(),
                fields.authentication.upgrade(),
                fields.token.upgrade(),
                fields.submit.upgrade(),
            ) {
                let name = name.text();
                let address = address.text();
                let token = token.text();
                let changed = original.as_ref().is_none_or(|remote| {
                    name.trim() != remote.name
                        || gmux_protocol::network::with_default_port(address.trim()).as_ref()
                            != remote.address
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
        let editor = management::EditorRequest::new(&dialog, &view, &submit, &error);
        submit.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            let authentication =
                match authentication_value(authentication.selected(), &token.text()) {
                    Ok(value) => value,
                    Err(message) => {
                        show_error(&error, message);
                        return;
                    }
                };
            editor.begin(Some(if editing {
                "Saving…"
            } else {
                "Connecting…"
            }));
            let mut request = json!({"method":"connection-add", "name":name.text().trim(),
                "address":address.text().trim(), "authentication":authentication});
            if let Some(remote) = &remote {
                request["method"] = json!("connection-edit");
                request["connection"] = json!(remote.name);
                request["daemon"] = json!(remote.daemon);
            } else if name.text().trim().is_empty() {
                request["name"] = Value::Null;
            }
            let editor = editor.clone();
            ui.request(request, move |ui, result| {
                editor.finish(result.map(|value| {
                    if let Ok(daemon) = serde_json::from_value(value["daemon"].clone()) {
                        ui.sessions.borrow_mut().connect(daemon);
                    }
                }));
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
        remote_access: &gmux_protocol::network::RemoteAccess,
        remote_access_error: Option<&str>,
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
        let local_group = adw::PreferencesGroup::new();
        let local_connection = adw::SwitchRow::builder()
            .title("Local Daemon")
            .subtitle("Run terminals on this computer")
            .active(self.settings.borrow().local_daemon_enabled)
            .build();
        local_group.add(&local_connection);
        page.add(&local_group);
        let access_group = adw::PreferencesGroup::builder()
            .title("Remote Access")
            .description(format!("Control how other computers connect when the local daemon is running. The default port is {}", gmux_protocol::network::DEFAULT_PORT))
            .build();
        let enabled = adw::SwitchRow::builder()
            .title("Allow Remote Connections")
            .active(remote_access.enabled)
            .build();
        let address = adw::EntryRow::builder()
            .title("Listening Address")
            .text(display_address(&remote_access.listen_address.to_string()))
            .activates_default(true)
            .build();
        access_group.add(&enabled);
        access_group.add(&address);
        page.add(&access_group);
        let (credentials, authentication, token) =
            authentication_fields(self, &remote_access.authentication);
        page.add(&credentials);
        let original_address = remote_access.listen_address;
        let fields = LocalConnectionFields {
            connection: ConnectionFields {
                name: name.downgrade(),
                address: address.downgrade(),
                authentication: authentication.downgrade(),
                token: token.downgrade(),
                submit: save.downgrade(),
                error: error.downgrade(),
            },
            enabled: enabled.downgrade(),
            credentials: credentials.downgrade(),
            reset: reset.downgrade(),
            execution: local_connection.downgrade(),
        };
        let hostname = hostname.to_owned();
        let default_name = hostname.clone();
        let original_name = current.to_owned();
        let original_remote_access = remote_access.clone();
        let original_execution = self.settings.borrow().local_daemon_enabled;
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
                fields.enabled.upgrade(),
                fields.connection.address.upgrade(),
                fields.credentials.upgrade(),
                fields.connection.authentication.upgrade(),
                fields.connection.token.upgrade(),
                fields.connection.submit.upgrade(),
                fields.connection.name.upgrade(),
                fields.reset.upgrade(),
                fields.execution.upgrade(),
            ) {
                let name = name.text();
                reset.set_sensitive(name.trim() != default_name);
                address.set_sensitive(enabled.is_active());
                credentials.set_sensitive(enabled.is_active());
                let address =
                    gmux_protocol::network::parse_listen_address(address.text().trim()).ok();
                let token = token.text();
                let changed = execution.is_active() != original_execution
                    || name.trim() != original_name
                    || enabled.is_active() != original_remote_access.enabled
                    || address
                        .is_none_or(|address| address != original_remote_access.listen_address)
                    || authentication_changed(
                        &original_remote_access.authentication,
                        auth.selected(),
                        &token,
                    );
                save.set_sensitive(
                    changed
                        && !name.trim().is_empty()
                        && (!enabled.is_active()
                            || (address.is_some()
                                && (auth.selected() == 0 || !token.trim().is_empty()))),
                );
            }
            if let Some(error) = fields.connection.error.upgrade() {
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
        local_connection.connect_active_notify(move |_| changed());
        validate();
        if let Some(message) = remote_access_error {
            show_error(&error, message);
        }
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        let initial_focus = name.downgrade();
        let default_button = save.downgrade();
        let weak = Rc::downgrade(self);
        let editor = management::EditorRequest::new(&dialog, &view, &save, &error);
        save.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            let authentication = match authentication_value(authentication.selected(), &token.text()) {
                Ok(value) => value,
                Err(message) => { show_error(&error, message); return; }
            };
            editor.begin(Some("Saving…"));
            let value = name.text();
            let value = (value.trim() != hostname).then(|| value.trim().to_owned());
            let abort = editor.clone();
            let editor = editor.clone();
            let finish = move |_: &Rc<Ui>, result| editor.finish(result);
            let remote_access = gmux_protocol::network::RemoteAccess {
                enabled: enabled.is_active(),
                listen_address: gmux_protocol::network::parse_listen_address(address.text().trim()).unwrap_or(original_address),
                authentication,
            };
            let execution = local_connection.is_active();
            let execution_changed = execution != original_execution;
            let confirm_stop = execution_changed && !execution && ui.settings.borrow().local_daemon_enabled;
            let apply = move |ui: &Rc<Ui>| {
                ui.request(
                    json!({"method":"connection-edit","connection":"local","name":value,"remote_access":remote_access}),
                    move |ui, result| match result {
                        Ok(_) if execution_changed && execution != ui.settings.borrow().local_daemon_enabled => {
                            ui.set_local_daemon_enabled(execution, finish);
                        }
                        Ok(_) => finish(ui, Ok(())),
                        Err(error) => finish(ui, Err(error)),
                    },
                );
            };
            if confirm_stop {
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
                let apply = RefCell::new(Some(apply));
                confirm.connect_response(None, move |_, response| {
                    if response == "stop" {
                        if let (Some(ui), Some(apply)) = (weak.upgrade(), apply.borrow_mut().take()) {
                            apply(&ui);
                        }
                    } else {
                        abort.restore();
                    }
                });
                confirm.present(Some(&ui.window));
            } else {
                apply(&ui);
            }
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
                    file: gmux_platform::unix::paths::home().join("gmux-token"),
                }
        );
        assert!(!authentication_changed(
            &Authentication::None,
            0,
            "hidden token path"
        ));
    }
}
