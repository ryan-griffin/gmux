use crate::{
    branch::{BranchCache, BranchWorker},
    commands::{COMMANDS, Query},
    find_field::FindField,
    font, ipc,
    model::{self, Layout, SavedTab, Session, Settings, TabFilter, WorkspaceCatalog},
    storage,
    terminal::Terminal,
    theme::Theme,
};
use adw::prelude::*;
use gmux::{
    control::{Command, CommandReply, CommandResult, InternalRequest, PublicRequest, Request},
    remote::SessionManager,
};
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
mod dialogs;
mod preferences;
mod split;
mod tabs;
mod window;
mod workspaces;

use actions::{TAB_ACTIONS, ZOOM_IN_SHORTCUTS};
use preferences::{AppPreferences, PreferenceControls, apply_appearance};
struct Tab {
    id: u64,
    name: String,
    custom_name: bool,
    workspace: Option<u64>,
    layout: Rc<RefCell<Layout>>,
    panes: HashMap<u64, Rc<Terminal>>,
    active: u64,
    content: gtk::Box,
    row: gtk::ListBoxRow,
    name_label: gtk::Label,
    workspace_label: gtk::Label,
    workspace_box: gtk::Box,
    path_label: gtk::Label,
    branch_label: gtk::Label,
    branch_box: gtk::Box,
    unread: HashSet<u64>,
    unread_indicator: gtk::Box,
    menu: gtk::PopoverMenu,
    branch_directory: PathBuf,
    branch_head: Option<PathBuf>,
    branch_requested: bool,
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
    settings: Rc<RefCell<Settings>>,
    preferences_dialog: RefCell<Option<Rc<PreferenceControls>>>,
    tabs: RefCell<Vec<Tab>>,
    sessions: RefCell<SessionManager>,
    catalog: RefCell<WorkspaceCatalog>,
    filter: Cell<TabFilter>,
    filter_title: adw::WindowTitle,
    workspace_filter: gtk::MenuButton,
    new_tab_menu: gio::Menu,
    empty: adw::StatusPage,
    search: gtk::SearchBar,
    search_field: FindField,
    search_entry: gtk::Text,
    search_count: gtk::Label,
    search_previous: gtk::Button,
    search_next: gtk::Button,
    search_status: Cell<Option<(bool, usize, usize, bool)>>,
    active: Cell<u64>,
    synced_focus: Cell<(u64, Option<u64>)>,
    changing: Cell<bool>,
    dirty: Cell<bool>,
    save_source: RefCell<Option<glib::SourceId>>,
    owner: std::rc::Weak<Windows>,
    id: u64,
    revision: Cell<u64>,
    detaching: Cell<bool>,
    backend: ipc::Client,
    pending: RefCell<HashMap<u64, BackendCallback>>,
    command_count: Cell<usize>,
    command_exits: RefCell<HashSet<u64>>,
    keyboard: Rc<RefCell<crate::terminal::KeyboardModifiers>>,
    update_source: RefCell<Option<glib::SourceId>>,
    branch_source: RefCell<Option<glib::SourceId>>,
    theme_handler: RefCell<Option<glib::SignalHandlerId>>,
    branches: BranchWorker,
    branch_cache: RefCell<BranchCache>,
    branch_monitors: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
    repository_monitors: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
}
type BackendCallback = Box<dyn FnOnce(&Rc<Ui>, anyhow::Result<Value>)>;

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(source) = self.update_source.get_mut().take() {
            source.remove();
        }
        if let Some(source) = self.branch_source.get_mut().take() {
            source.remove();
        }
        if let Some(handler) = self.theme_handler.get_mut().take() {
            adw::StyleManager::default().disconnect(handler);
        }
        if let Some(source) = self.save_source.get_mut().take() {
            source.remove();
        }
        for monitor in self.branch_monitors.get_mut().values() {
            monitor.cancel();
        }
        for monitor in self.repository_monitors.get_mut().values() {
            monitor.cancel();
        }
    }
}

/// App-owned window lifetime and routing. Session data arrives from the daemon.
struct Windows {
    app: adw::Application,
    endpoint: ipc::Endpoint,
    preferences: RefCell<Option<Rc<AppPreferences>>>,
    windows: RefCell<Vec<Rc<Ui>>>,
    next_id: Cell<u64>,
    server: RefCell<Option<ipc::Server>>,
    source: RefCell<Option<glib::SourceId>>,
}
impl Windows {
    fn active(&self) -> Option<Rc<Ui>> {
        let windows = self.windows.borrow();
        windows
            .iter()
            .find(|ui| ui.window.is_active())
            .or_else(|| windows.last())
            .cloned()
    }
    fn create(self: &Rc<Self>, directory: Option<PathBuf>) -> anyhow::Result<Rc<Ui>> {
        if self.preferences.borrow().is_none() {
            let preferences = AppPreferences::new(self)?;
            *self.preferences.borrow_mut() = Some(preferences);
        }
        if self.server.borrow().is_none() {
            let wake = ipc::Wake::new()?;
            *self.server.borrow_mut() = Some(ipc::Server::start_at(
                ipc::companion_path(".ui"),
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
        gmux::remote::ensure_daemon()?;
        let ui = Ui::new(
            &self.app,
            self.preferences.borrow().as_ref().unwrap().settings.clone(),
            directory,
            Rc::downgrade(self),
            id,
            self.endpoint.clone(),
        )?;
        self.windows.borrow_mut().push(ui.clone());
        ui.window.present();
        Ok(ui)
    }
    fn route(self: &Rc<Self>, request: ipc::Request) {
        let window = request.body.get("window").cloned();
        let body = match Request::decode(request.body) {
            Ok(body) => body,
            Err(error) => {
                let _ = request.reply.try_send(json!({"error":error.to_string()}));
                return;
            }
        };
        if matches!(body, Request::Public(PublicRequest::Windows)) {
            let windows = self.windows.borrow();
            let _ = request.reply.try_send(json!({"windows": windows.iter().map(|ui| json!({"id":ui.id,"active":ui.window.is_active(),"active_tab":ui.active.get()})).collect::<Vec<_>>() }));
        } else if matches!(body, Request::Public(PublicRequest::NewWindow)) {
            let response = self
                .create(None)
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
                ui.handle_control(body, request.reply);
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
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
    }
}

pub fn run(directory: Option<PathBuf>, new_window: bool) -> anyhow::Result<()> {
    gio::resources_register_include!("gmux.gresource")?;
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    application.connect_startup(|_| {
        let display = gdk::Display::default().unwrap();
        gtk::IconTheme::for_display(&display).add_resource_path("/dev/gmux/Gmux/icons");
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("../data/style.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    application.add_main_option(
        "new-window",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Open another window",
        None,
    );
    glib::set_application_name("Gmux");
    glib::set_prgname(Some("gmux"));
    let owner = Rc::new(Windows {
        app: application.clone(),
        endpoint: ipc::Endpoint::local(),
        preferences: RefCell::new(None),
        windows: RefCell::new(Vec::new()),
        next_id: Cell::new(0),
        server: RefCell::new(None),
        source: RefCell::new(None),
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
        } else if let Err(error) = activate_owner.create(None) {
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
            .arguments()
            .iter()
            .skip(1)
            .find(|arg| !arg.to_string_lossy().starts_with('-'))
            .and_then(|arg| gio::File::for_uri(&arg.to_string_lossy()).path());
        let existing = command_owner.active().filter(|_| !new);
        let result = if let Some(ui) = existing {
            if let Some(directory) = directory {
                ui.new_tab(directory, None);
            }
            ui.window.present();
            Ok(())
        } else {
            command_owner.create(directory).map(|_| ())
        };
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
                ui.new_tab(directory, None);
                ui.window.present();
            } else if let Err(error) = open_owner.create(Some(directory)) {
                *open_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
                show_startup_error(app, &error);
                return;
            }
        }
    });
    let action = gio::SimpleAction::new("focus-notification", Some(&u64::static_variant_type()));
    let notification_owner = Rc::downgrade(&owner);
    action.connect_activate(move |_, value| {
        if let (Some(ui), Some(pane)) = (
            notification_owner
                .upgrade()
                .and_then(|owner| owner.active()),
            value.and_then(|v| v.get::<u64>()),
        ) {
            ui.jump_to_pane(pane);
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
    let directory = directory.map(std::path::absolute).transpose()?;
    if let Some(directory) = &directory {
        anyhow::ensure!(
            directory.is_dir(),
            "{} is not a directory",
            directory.display()
        );
    }
    let mut arguments = vec!["gmux".to_string()];
    if new_window {
        arguments.push("--new-window".into());
    }
    if let Some(directory) = directory {
        arguments.push(gio::File::for_path(directory).uri().to_string());
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
        if self.owner.upgrade().is_some_and(|owner| {
            owner.windows.borrow().iter().any(|ui| {
                ui.window.is_active()
                    && ui
                        .active_pane()
                        .is_some_and(|terminal| terminal.id == pane && terminal.area.has_focus())
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
            Some(&pane.to_variant()),
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
                w.unread_indicator.set_visible(!w.unread.is_empty());
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
