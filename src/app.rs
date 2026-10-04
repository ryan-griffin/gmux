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
    control::{Command, CommandReply, CommandResult, InternalRequest},
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
mod tabs;
mod window;
mod workspaces;

use actions::{TAB_ACTIONS, ZOOM_IN_SHORTCUTS};
use preferences::apply_appearance;
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
    heading: adw::WindowTitle,
    toast: adw::ToastOverlay,
    settings: Rc<RefCell<Settings>>,
    desktop_settings: Option<gio::Settings>,
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
    server: ipc::Server,
    wake: ipc::Wake,
    backend: ipc::Client,
    pending: RefCell<HashMap<u64, BackendCallback>>,
    update_source: RefCell<Option<glib::SourceId>>,
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

pub fn run(directory: Option<PathBuf>) -> anyhow::Result<()> {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    glib::set_application_name("Gmux");
    glib::set_prgname(Some("gmux"));
    let holder: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));
    let startup_error = Rc::new(RefCell::new(None));
    let activate_error = startup_error.clone();
    let activate_holder = holder.clone();
    let directory = directory.map(std::path::absolute).transpose()?;
    if let Some(directory) = &directory {
        anyhow::ensure!(
            directory.is_dir(),
            "{} is not a directory",
            directory.display()
        );
    }
    application.connect_activate(move |app| {
        if let Some(ui) = activate_holder.borrow().as_ref() {
            ui.window.present();
            return;
        }
        match Ui::new(app, None) {
            Ok(ui) => {
                ui.window.present();
                *activate_holder.borrow_mut() = Some(ui);
            }
            Err(error) => {
                *activate_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
                show_startup_error(app, &error);
            }
        }
    });
    let open_holder = holder.clone();
    let open_error = startup_error.clone();
    application.connect_open(move |app, files, _| {
        let directories = files.iter().filter_map(gio::File::path).collect::<Vec<_>>();
        let Some(first) = directories.first() else {
            return;
        };
        let existing = open_holder.borrow().as_ref().cloned();
        let (ui, skip) = if let Some(ui) = existing {
            (ui, 0)
        } else {
            match Ui::new(app, Some(first.clone())) {
                Ok(ui) => {
                    *open_holder.borrow_mut() = Some(ui.clone());
                    (ui, 1)
                }
                Err(error) => {
                    *open_error.borrow_mut() = Some(format!("Could not start Gmux: {error:#}"));
                    show_startup_error(app, &error);
                    return;
                }
            }
        };
        for directory in directories.into_iter().skip(skip) {
            ui.new_tab(directory, None);
        }
        ui.window.present();
    });
    // Forward explicit directories through GApplication's open signal so an
    // already-running instance receives them too.
    let mut arguments = vec!["gmux".to_string()];
    if let Some(directory) = directory {
        arguments.push(gio::File::for_path(directory).uri().to_string());
    }
    let code = application.run_with_args(&arguments);
    if let Some(error) = startup_error.borrow_mut().take() {
        anyhow::bail!("{error}");
    }
    if let Some(ui) = holder.borrow_mut().take() {
        ui.tabs.borrow_mut().clear();
        ui.sessions.borrow_mut().clear();
    }
    anyhow::ensure!(code == glib::ExitCode::SUCCESS, "Gmux exited with {code:?}");
    Ok(())
}

fn show_startup_error(app: &adw::Application, error: &anyhow::Error) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Gmux")
        .default_width(560)
        .default_height(340)
        .build();
    let page = adw::StatusPage::builder()
        .title("Could Not Start Gmux")
        .description(format!("{error:#}"))
        .icon_name("dialog-warning-symbolic")
        .build();
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::Center);
    let weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    page.set_child(Some(&close));
    window.set_content(Some(&page));
    window.present();
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
        if self.window.is_active()
            && self
                .active_pane()
                .is_some_and(|terminal| terminal.id == pane && terminal.area.has_focus())
        {
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
