use crate::client::{
    connections,
    control::{
        BackendReply, Command, CommandReply, CommandResult, InternalRequest, PaneRequest,
        PublicRequest, Request, TabCommand,
    },
};
use crate::{
    commands::{COMMANDS, Query},
    find_field::FindField,
    font, ipc,
    model::{self, Layout, Session, TabFilter, TabState, WorkspaceCatalog},
    settings::Settings,
    terminal::Terminal,
    theme::Theme,
};
use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

const APP_ID: &str = "dev.gmux.Gmux";
mod actions;
mod backend;
mod branches;
mod connection_menu;
mod connections_dialog;
mod dialogs;
mod feedback;
mod local;
mod machine;
mod management;
mod preferences;
mod split;
mod storage;
mod tabs;
mod window;
mod workspaces;

use actions::{TAB_ACTIONS, ZOOM_IN_SHORTCUTS};
use preferences::{AppPreferences, PreferenceControls, apply_appearance};
struct Tab {
    id: u64,
    created_at: u64,
    name: String,
    custom_name: bool,
    workspace: Option<u64>,
    layout: Rc<RefCell<Layout>>,
    panes: HashMap<u64, Rc<Terminal>>,
    active: u64,
    content: gtk::Box,
    row: gtk::ListBoxRow,
    name_label: gtk::Label,
    machine_icon: gtk::Image,
    // None means the offline icon is shown.
    machine_icon_kind: Option<model::MachineKind>,
    machine_label: gtk::Label,
    machine_box: gtk::Box,
    workspace_label: gtk::Label,
    path_label: gtk::Label,
    branch_icon: gtk::Image,
    branch_label: gtk::Label,
    unread: HashSet<u64>,
    activity: crate::activity::Indicator,
    menu: gtk::PopoverMenu,
    menu_state: Option<(bool, Option<u64>)>,
}
impl Drop for Tab {
    fn drop(&mut self) {
        self.menu.unparent();
    }
}
pub struct Ui {
    app: adw::Application,
    window: adw::ApplicationWindow,
    split: adw::OverlaySplitView,
    stack: gtk::Stack,
    sidebar: gtk::ListBox,
    sidebar_search: gtk::SearchEntry,
    sidebar_placeholder: adw::StatusPage,
    sidebar_main_menu: gtk::MenuButton,
    terminal_menu: gtk::MenuButton,
    heading: adw::WindowTitle,
    toast: adw::ToastOverlay,
    errors: Rc<feedback::Errors>,
    connection_banner: adw::Banner,
    settings: Rc<RefCell<Settings>>,
    preferences_dialog: RefCell<Option<Rc<PreferenceControls>>>,
    connections_dialog: RefCell<Option<Rc<connections_dialog::ConnectionControls>>>,
    workspaces_dialog: RefCell<Option<Rc<workspaces::WorkspaceControls>>>,
    connection_editor_dialog: management::EditorDialog,
    workspace_editor_dialog: management::EditorDialog,
    tabs: RefCell<Vec<Tab>>,
    sessions: RefCell<connections::Connections>,
    clipboard: Rc<crate::clipboard::Desktop>,
    catalog: RefCell<WorkspaceCatalog>,
    filter: Cell<TabFilter>,
    filter_title: adw::WindowTitle,
    workspace_filter: gtk::MenuButton,
    connection_menu: connection_menu::ConnectionMenu,
    workspace_menu_items: RefCell<Option<Vec<(u64, String)>>>,
    empty: adw::StatusPage,
    empty_actions: gtk::Stack,
    initial_directory: RefCell<Option<(PathBuf, Option<PathBuf>)>>,
    search: gtk::SearchBar,
    search_field: FindField,
    search_entry: gtk::Text,
    search_count: gtk::Label,
    search_previous: gtk::Button,
    search_next: gtk::Button,
    search_status: Cell<Option<(bool, usize, usize, bool)>>,
    active: Cell<u64>,
    restore_selection: Cell<Option<(u64, u64, TabFilter)>>,
    changing: Cell<bool>,
    dirty: Cell<bool>,
    save_source: RefCell<Option<glib::SourceId>>,
    owner: std::rc::Weak<Windows>,
    id: u64,
    revision: Cell<u64>,
    detaching: Cell<bool>,
    backend: Rc<crate::client::backend::Backend>,
    pending: RefCell<HashMap<u64, BackendCallback>>,
    command_count: Cell<usize>,
    creation_serial: Cell<u64>,
    pending_tab_creations: RefCell<HashMap<model::DaemonId, usize>>,
    command_exits: RefCell<HashSet<u64>>,
    keyboard: Rc<RefCell<crate::terminal::KeyboardState>>,
    update_source: RefCell<Option<glib::SourceId>>,
    deadline_source: RefCell<Option<(std::time::Instant, glib::SourceId)>>,
    theme_handler: RefCell<Option<glib::SignalHandlerId>>,
    frame_clock_handler: RefCell<Option<(gtk::gdk::FrameClock, glib::SignalHandlerId)>>,
}
type BackendCallback = Box<dyn FnOnce(&Rc<Ui>, anyhow::Result<BackendReply>)>;

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(source) = self.update_source.get_mut().take() {
            source.remove();
        }
        if let Some((_, source)) = self.deadline_source.get_mut().take() {
            source.remove();
        }
        if let Some(handler) = self.theme_handler.get_mut().take() {
            adw::StyleManager::default().disconnect(handler);
        }
        if let Some((clock, handler)) = self.frame_clock_handler.get_mut().take() {
            clock.disconnect(handler);
        }
        if let Some(source) = self.save_source.get_mut().take() {
            source.remove();
        }
    }
}

/// App-owned window lifetime and routing. Session data arrives from the daemon.
struct Windows {
    app: adw::Application,
    preferred: Option<String>,
    preferences: RefCell<Option<Rc<AppPreferences>>>,
    organization: RefCell<Option<std::sync::Arc<crate::client::Store>>>,
    windows: RefCell<Vec<Rc<Ui>>>,
    retired: RefCell<Vec<Rc<crate::client::backend::Backend>>>,
    retire_source: RefCell<Option<glib::SourceId>>,
    next_id: Cell<u64>,
    server: RefCell<Option<gmux_platform::unix::ipc::Server>>,
    source: RefCell<Option<glib::SourceId>>,
    local: local::LocalDaemon,
}
impl Windows {
    fn retire_backend(self: &Rc<Self>, backend: Rc<crate::client::backend::Backend>) {
        self.retired.borrow_mut().push(backend);
        if self.retire_source.borrow().is_some() {
            return;
        }
        // Keep queued saves alive after a window closes. Join only finished
        // workers here; remaining work is flushed when the application exits.
        let weak = Rc::downgrade(self);
        *self.retire_source.borrow_mut() = Some(glib::timeout_add_local(
            Duration::from_millis(50),
            move || {
                let Some(owner) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                owner
                    .retired
                    .borrow_mut()
                    .retain(|backend| !backend.is_finished());
                if owner.retired.borrow().is_empty() {
                    owner.retire_source.borrow_mut().take();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            },
        ));
    }
    fn active(&self) -> Option<Rc<Ui>> {
        let windows = self.windows.borrow();
        windows
            .iter()
            .find(|ui| ui.window.is_active())
            .or_else(|| windows.last())
            .cloned()
    }
    fn create(
        self: &Rc<Self>,
        directory: Option<PathBuf>,
        preferred: Option<String>,
    ) -> anyhow::Result<Rc<Ui>> {
        if self.organization.borrow().is_none() {
            *self.organization.borrow_mut() = Some(crate::client::Store::open("gtk")?);
        }
        if self.preferences.borrow().is_none() {
            let preferences = AppPreferences::new(self)?;
            *self.preferences.borrow_mut() = Some(preferences);
        }
        if self.server.borrow().is_none() {
            let wake = gmux_platform::unix::ipc::Wake::new()?;
            *self.server.borrow_mut() = Some(gmux_platform::unix::ipc::Server::start_at(
                gmux_platform::unix::ipc::companion_path(".ui"),
                wake.clone(),
            )?);
            let weak = Rc::downgrade(self);
            *self.source.borrow_mut() = Some(glib_unix::unix_fd_add_local(
                wake.as_raw_fd(),
                glib::IOCondition::IN,
                move |_, _| {
                    wake.drain();
                    let Some(owner) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    for _ in 0..16 {
                        let request = owner.server.borrow().as_ref().unwrap().receiver.try_recv();
                        let Ok(request) = request else {
                            return glib::ControlFlow::Continue;
                        };
                        owner.route(request);
                    }
                    wake.notify();
                    glib::ControlFlow::Continue
                },
            ));
        }
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let preferred = preferred
            .or_else(|| {
                self.active()
                    .and_then(|ui| ui.sessions.borrow().preferred_connection())
            })
            .or_else(|| self.preferred.clone());
        let ui = Ui::new(
            &self.app,
            self.preferences.borrow().as_ref().unwrap().settings.clone(),
            directory,
            Rc::downgrade(self),
            id,
            preferred,
        )?;
        self.windows.borrow_mut().push(ui.clone());
        self.ensure_local();
        ui.window.present();
        Ok(ui)
    }
    fn launch(
        self: &Rc<Self>,
        connection: Option<String>,
        directory: Option<PathBuf>,
        directory_base: Option<PathBuf>,
        new_window: bool,
    ) -> anyhow::Result<Rc<Ui>> {
        let ui = match self.active().filter(|_| !new_window) {
            Some(ui) => {
                if let Some(name) = connection {
                    ui.sessions.borrow_mut().select_default(name)?;
                }
                ui
            }
            None => self.create(None, connection)?,
        };
        if let Some(directory) = directory {
            *ui.initial_directory.borrow_mut() = Some((directory, directory_base));
            ui.open_initial_directory();
        }
        ui.window.present();
        Ok(ui)
    }
    fn route(self: &Rc<Self>, request: gmux_platform::unix::ipc::Request) {
        let window = request.body.get("window").cloned();
        let connection = request
            .body
            .get("connection")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let body = match Request::decode(request.body) {
            Ok(body) => body,
            Err(error) => {
                let _ = request.reply.try_send(json!({"error":error.to_string()}));
                return;
            }
        };
        if connection.is_some()
            && !matches!(
                body,
                Request::Layout(_)
                    | Request::Public(PublicRequest::Focus { .. } | PublicRequest::Launch { .. })
            )
        {
            let _ = request.reply.try_send(json!({
                "error":"Connection selection supports tab commands and focus; use gmux --connection for other execution requests"
            }));
            return;
        }
        if matches!(body, Request::Public(PublicRequest::Windows)) {
            let windows = self.windows.borrow();
            let _ = request.reply.try_send(json!({"windows": windows.iter().map(|ui| {
                let active = (ui.active.get() != 0).then(|| ui.sessions.borrow().tab_ref(ui.active.get()));
                json!({"id":ui.id,"active":ui.window.is_active(),"active_tab":active.map(|r| r.id),"active_reference":active})
            }).collect::<Vec<_>>() }));
        } else if matches!(body, Request::Public(PublicRequest::NewWindow)) {
            let response = self
                .create(None, None)
                .map(|ui| json!({"window":ui.id}))
                .unwrap_or_else(|error| json!({"error":error.to_string()}));
            let _ = request.reply.try_send(response);
        } else if let Request::Public(PublicRequest::Launch {
            connection,
            directory,
            directory_base,
            new_window,
        }) = body
        {
            let response = self
                .launch(connection, directory, directory_base, new_window)
                .map(|ui| json!({"window":ui.id}))
                .unwrap_or_else(|error| json!({"error":error.to_string()}));
            let _ = request.reply.try_send(response);
        } else {
            let ui = if let Some(id) = window {
                id.as_u64()
                    .and_then(|id| self.windows.borrow().iter().find(|ui| ui.id == id).cloned())
            } else {
                self.active()
            };
            if let Some(ui) = ui {
                ui.handle_control_on(body, request.reply, connection);
            } else {
                let _ = request
                    .reply
                    .try_send(json!({"error":"Window does not exist"}));
            }
        }
    }
}
impl Drop for Windows {
    fn drop(&mut self) {
        if let Some(source) = self.retire_source.get_mut().take() {
            source.remove();
        }
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
    }
}

pub fn run(
    directory: Option<PathBuf>,
    new_window: bool,
    connection: Option<String>,
) -> anyhow::Result<()> {
    gio::resources_register_include!("gmux.gresource")?;
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    application.add_main_option(
        "new-window",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Open another window",
        None,
    );
    application.add_main_option(
        "directory",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Open a new tab in this directory",
        Some("PATH"),
    );
    application.add_main_option(
        "connection",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Select a connection",
        Some("NAME"),
    );
    glib::set_application_name("Gmux");
    glib::set_prgname(Some("gmux-gtk"));
    let owner = Rc::new(Windows {
        app: application.clone(),
        preferred: connection.clone(),
        preferences: RefCell::new(None),
        organization: RefCell::new(None),
        windows: RefCell::new(Vec::new()),
        retired: RefCell::new(Vec::new()),
        retire_source: RefCell::new(None),
        next_id: Cell::new(0),
        server: RefCell::new(None),
        source: RefCell::new(None),
        local: local::LocalDaemon::default(),
    });
    let startup_error = Rc::new(RefCell::new(None));
    let activate_owner = Rc::downgrade(&owner);
    let activate_error = startup_error.clone();
    application.connect_activate(move |app| {
        let Some(activate_owner) = activate_owner.upgrade() else {
            return;
        };
        if let Some(ui) = activate_owner.active() {
            ui.window.present();
        } else if activate_error.borrow().is_some() {
            if let Some(window) = app.active_window() {
                window.present();
            }
        } else if let Err(error) = activate_owner.create(None, None) {
            *activate_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
            show_startup_error(app, &error);
        }
    });
    let command_owner = Rc::downgrade(&owner);
    let command_error = startup_error.clone();
    application.connect_command_line(move |app, command| {
        let Some(command_owner) = command_owner.upgrade() else {
            return glib::ExitCode::FAILURE;
        };
        if command_owner.active().is_none() && command_error.borrow().is_some() {
            if let Some(window) = app.active_window() {
                window.present();
            }
            return glib::ExitCode::SUCCESS;
        }
        let new = command
            .options_dict()
            .lookup::<bool>("new-window")
            .ok()
            .flatten()
            .unwrap_or(false);
        let directory = command
            .options_dict()
            .lookup::<String>("directory")
            .ok()
            .flatten()
            .map(PathBuf::from);
        let connection = command
            .options_dict()
            .lookup::<String>("connection")
            .ok()
            .flatten();
        let result = command_owner.launch(connection, directory, command.cwd(), new);
        if let Err(error) = result {
            *command_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
            show_startup_error(app, &error);
            glib::ExitCode::FAILURE
        } else {
            glib::ExitCode::SUCCESS
        }
    });
    let open_owner = Rc::downgrade(&owner);
    let open_error = startup_error.clone();
    application.connect_open(move |app, files, _| {
        let Some(open_owner) = open_owner.upgrade() else {
            return;
        };
        if open_owner.active().is_none() && open_error.borrow().is_some() {
            if let Some(window) = app.active_window() {
                window.present();
            }
            return;
        }
        for directory in files.iter().filter_map(gio::File::path) {
            if let Some(ui) = open_owner.active() {
                let destination = ui.sessions.borrow().default_daemon();
                if let Some(destination) = destination {
                    ui.create_tab_on(destination, Some(directory), None, ui.selected_workspace());
                } else {
                    ui.manage_connections();
                }
                ui.window.present();
            } else if let Err(error) = open_owner.create(Some(directory), None) {
                *open_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
                show_startup_error(app, &error);
                return;
            }
        }
    });
    let action = gio::SimpleAction::new(
        "focus-notification",
        Some(&<(String, u64)>::static_variant_type()),
    );
    let notification_owner = Rc::downgrade(&owner);
    action.connect_activate(move |_, value| {
        if let (Some(ui), Some(pane)) = (
            notification_owner
                .upgrade()
                .and_then(|owner| owner.active()),
            value.and_then(|v| v.get::<(String, u64)>()),
        ) && let Ok(daemon) = serde_json::from_value(json!(pane.0))
        {
            let handle = ui.sessions.borrow().lookup(daemon, pane.1);
            if let Some(handle) = handle {
                ui.jump_to_pane(handle);
            }
        }
    });
    application.add_action(&action);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let weak = Rc::downgrade(&owner);
        glib_unix::unix_signal_add_local(signal, move || {
            if let Some(owner) = weak.upgrade() {
                let windows = owner.windows.borrow().clone();
                for ui in windows {
                    ui.detach_frontend();
                }
            }
            glib::ControlFlow::Break
        });
    }
    let mut arguments = vec!["gmux-gtk".to_string()];
    if new_window {
        arguments.push("--new-window".into());
    }
    if let Some(connection) = connection {
        arguments.push("--connection".into());
        arguments.push(connection);
    }
    if let Some(directory) = directory {
        arguments.push("--directory".into());
        arguments.push(directory.to_string_lossy().into_owned());
    }
    let code = application.run_with_args(&arguments);
    let windows = owner.windows.borrow_mut().drain(..).collect::<Vec<_>>();
    for ui in windows {
        ui.tabs.borrow_mut().clear();
        ui.sessions.borrow_mut().clear();
    }
    if let Some(source) = owner.source.borrow_mut().take() {
        source.remove();
    }
    owner.server.borrow_mut().take();
    if let Some(error) = startup_error.borrow_mut().take() {
        anyhow::bail!("{error}");
    }
    anyhow::ensure!(code == glib::ExitCode::SUCCESS, "Gmux exited with {code:?}");
    Ok(())
}

fn show_startup_error(app: &adw::Application, error: &anyhow::Error) {
    // The daemon check can fail before window setup applies the saved appearance.
    if app.windows().is_empty()
        && let Ok(settings) = storage::load::<Settings>("settings.json")
    {
        apply_appearance(&settings);
    }
    let dialog = adw::AlertDialog::builder()
        .heading("Could Not Start Gmux")
        .body(format!("{error:#}"))
        .build();
    dialog.add_response("close", "_Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    // Start on the only action instead of the scrollable message.
    dialog.set_focus(dialog.default_widget().as_ref());
    // Startup can fail before a main window exists. Libadwaita creates the
    // standalone window; registering it keeps GApplication alive until dismissal.
    dialog.present(None::<&gtk::Widget>);
    let window = dialog.root().and_downcast::<gtk::Window>().unwrap();
    window.set_application(Some(app));
}

impl Ui {
    fn refresh_client_ui(self: &Rc<Self>) {
        self.refresh_connections_ui();
        let sessions = self.sessions.borrow();
        self.backend
            .retain_connections(|daemon| sessions.has_connection(daemon));
        self.connection_menu.refresh(&sessions);
        drop(sessions);
        self.refresh_workspace_ui();
    }
    pub(super) fn refresh_search(&self) {
        if let Some(pane) = self.active_pane() {
            pane.search(&self.search_entry.text());
        }
        self.update_search_count();
    }
    pub(super) fn advance_search(&self, previous: bool) {
        if let Some(pane) = self.active_pane() {
            pane.search_advance(previous);
        }
        self.update_search_count();
    }
    pub(super) fn update_search_count(&self) {
        let (current, total, pending) = self
            .active_pane()
            .map_or((0, 0, false), |pane| pane.search_status());
        let has_query = !self.search_entry.text().is_empty();
        let status = Some((has_query, current, total, pending));
        if self.search_status.replace(status) == status {
            return;
        }
        self.search_count.set_text(&if pending && total == 0 {
            "Searching…".to_owned()
        } else {
            format!("{current} of {total}")
        });
        self.search_count
            .set_visible(has_query && (total > 0 || pending));
        if has_query && total == 0 && !pending {
            self.search_field.add_css_class("error");
        } else {
            self.search_field.remove_css_class("error");
        }
        self.search_entry
            .update_state(&[gtk::accessible::State::Invalid(
                if has_query && total == 0 && !pending {
                    gtk::AccessibleInvalidState::True
                } else {
                    gtk::AccessibleInvalidState::False
                },
            )]);
        self.search_previous.set_sensitive(total > 0);
        self.search_next.set_sensitive(total > 0);
    }
    pub(super) fn message(&self, message: &str) {
        self.toast.add_toast(adw::Toast::new(message));
    }
    pub(super) fn notify(&self, tab: u64, pane: u64, message: String) {
        let reference = self.sessions.borrow().pane_ref(pane);
        if self.owner.upgrade().is_some_and(|owner| {
            owner.windows.borrow().iter().any(|ui| {
                ui.window.is_active()
                    && ui.active_pane().is_some_and(|terminal| {
                        ui.sessions.borrow().pane_ref(terminal.id) == reference
                            && terminal.area.has_focus()
                    })
            })
        }) {
            self.mark_read(pane);
            return;
        }
        let name = {
            let tabs = self.tabs.borrow();
            if let Some(w) = tabs.iter().find(|w| w.id == tab) {
                w.name.clone()
            } else {
                "Terminal".into()
            }
        };
        if !self.settings.borrow().desktop_notifications {
            return;
        }
        let notification = gio::Notification::new(&name);
        notification.set_body(Some(&message));
        notification.set_default_action_and_target_value(
            "app.focus-notification",
            Some(&(reference.daemon.to_string(), reference.id).to_variant()),
        );
        self.app.send_notification(None, &notification);
    }
    pub(super) fn mark_read(&self, pane: u64) {
        if self.set_unread(pane, false) {
            self.sessions.borrow_mut().mark_read(pane);
        }
    }
    pub(super) fn set_unread(&self, pane: u64, unread: bool) -> bool {
        for w in self.tabs.borrow_mut().iter_mut() {
            if !w.panes.contains_key(&pane) {
                continue;
            }
            let changed = if unread {
                w.unread.insert(pane)
            } else {
                w.unread.remove(&pane)
            };
            if changed {
                self.refresh_tab_activity(w);
            }
            return changed;
        }
        false
    }
    pub(super) fn jump_to_pane(self: &Rc<Self>, id: u64) {
        if let Ok(p) = self.pane(Some(id)) {
            self.focus_pane(id);
            self.window.present();
            p.area.grab_focus();
        }
    }
}
