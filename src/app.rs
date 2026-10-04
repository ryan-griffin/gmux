use crate::{
    branch::BranchWorker,
    commands::{COMMANDS, Query},
    find_field::FindField,
    font, ipc,
    model::{self, Layout, SavedTab, Session, Settings, TabFilter, WorkspaceCatalog},
    terminal::Terminal,
    theme::Theme,
};
use adw::prelude::*;
use gmux::{
    control::{Command, CommandReply, CommandResult},
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
#[path = "workspaces.rs"]
mod workspaces;
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
    changing: Cell<bool>,
    dirty: Cell<bool>,
    server: ipc::Server,
    wake: ipc::Wake,
    backend: ipc::Client,
    pending: RefCell<HashMap<u64, BackendCallback>>,
    update_source: RefCell<Option<glib::SourceId>>,
    branches: BranchWorker,
    branch_monitors: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
}
type BackendCallback = Box<dyn FnOnce(&Rc<Ui>, anyhow::Result<Value>)>;

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(source) = self.update_source.get_mut().take() {
            source.remove();
        }
        for monitor in self.branch_monitors.get_mut().values() {
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
    fn new(app: &adw::Application, directory: Option<PathBuf>) -> anyhow::Result<Rc<Self>> {
        gio::resources_register_include!("gmux.gresource")?;
        gtk::IconTheme::for_display(&gdk::Display::default().unwrap())
            .add_resource_path("/dev/gmux/Gmux/icons");
        let settings = Rc::new(RefCell::new(model::load::<Settings>("settings.json")?));
        let desktop_settings = font::desktop_settings();
        if let Some(desktop) = &desktop_settings {
            settings.borrow_mut().system_font = desktop.string(font::KEY).to_string();
        }
        apply_appearance(&settings.borrow());
        let wake = ipc::Wake::new()?;
        let server = ipc::Server::start_at(ipc::companion_path(".ui"), wake.clone())?;
        let mut sessions = SessionManager::connect(wake.clone())?;
        let saved_session = if let Some(directory) = directory {
            // Initial setup completes before presenting a window. Runtime commands use the worker.
            let reply: CommandReply = serde_json::from_value(ipc::call(
                json!({"method":"_command","command":Command::New { directory: Some(directory), name: None, workspace: None }}),
            )?)?;
            sessions.install_metadata(reply.metadata);
            reply.session
        } else {
            sessions.saved_session()?
        };
        let mut catalog = model::load::<WorkspaceCatalog>("workspaces.json")?;
        let mut ids = HashSet::new();
        catalog.workspaces.retain(|workspace| {
            workspace.id > 0
                && workspace.id < u64::MAX
                && !workspace.name.trim().is_empty()
                && ids.insert(workspace.id)
        });
        let session = saved_session;
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Gmux")
            .default_width(if session.width > 0 {
                session.width
            } else {
                1120
            })
            .default_height(if session.height > 0 {
                session.height
            } else {
                740
            })
            .build();
        window.set_size_request(360, 280);
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("../data/style.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let split = adw::OverlaySplitView::builder()
            .min_sidebar_width(220.0)
            .max_sidebar_width(280.0)
            .sidebar_width_fraction(0.23)
            .pin_sidebar(true)
            .show_sidebar(settings.borrow().show_sidebar)
            .build();
        // OverlaySplitView styles this pane; the GTK "sidebar" class adds a second border.
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.set_top_bar_style(adw::ToolbarStyle::Flat);
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_show_end_title_buttons(false);
        let filter_title = adw::WindowTitle::new("All Tabs", "");
        let filter_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        filter_content.append(&filter_title);
        filter_content.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let workspace_filter = gtk::MenuButton::builder()
            .child(&filter_content)
            .tooltip_text("Filter tabs by workspace")
            .build();
        workspace_filter.add_css_class("flat");
        crate::menu::setup_menu_button(&workspace_filter, "Workspace Filter");
        sidebar_header.set_title_widget(Some(&workspace_filter));
        let new_tab_menu = gio::Menu::new();
        let new = adw::SplitButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New tab (Ctrl+Shift+T)")
            .dropdown_tooltip("New tab options")
            .action_name("win.new")
            .menu_model(&new_tab_menu)
            .build();
        crate::menu::setup_split_button(&new, "New tab options");
        sidebar_header.pack_start(&new);
        sidebar_view.add_top_bar(&sidebar_header);
        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("navigation-sidebar");
        sidebar.set_selection_mode(gtk::SelectionMode::Single);
        let no_tabs = adw::StatusPage::builder().title("No Tabs").build();
        no_tabs.add_css_class("compact");
        sidebar.set_placeholder(Some(&no_tabs));
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar)
            .build();
        let sidebar_search = gtk::SearchEntry::builder()
            .placeholder_text("Search tabs")
            .tooltip_text("Search titles, workspaces, directories, and branches")
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let sidebar_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_content.append(&sidebar_search);
        sidebar_content.append(&scroller);
        sidebar_view.set_content(Some(&sidebar_content));
        split.set_sidebar(Some(&sidebar_view));

        let content_view = adw::ToolbarView::new();
        content_view.set_top_bar_style(adw::ToolbarStyle::Flat);
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        let toggle = icon_button(
            "sidebar-show-symbolic",
            "Show or hide tabs (Ctrl+Shift+B)",
            "win.sidebar",
        );
        header.pack_start(&toggle);
        let heading = adw::WindowTitle::new("Gmux", "");
        header.set_title_widget(Some(&heading));
        header.pack_end(&main_menu_button(&new_tab_menu));
        header.pack_end(&terminal_menu_button());
        content_view.add_top_bar(&header);
        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let search = gtk::SearchBar::new();
        search.add_css_class("inline");
        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let search_entry = gtk::Text::builder()
            .accessible_role(gtk::AccessibleRole::TextBox)
            .placeholder_text("Find...")
            .hexpand(true)
            .build();
        search_entry.update_property(&[gtk::accessible::Property::Label("Find...")]);
        let search_count = gtk::Label::builder()
            .visible(false)
            .tooltip_text("Current match and total matches")
            .build();
        search_count.add_css_class("dim-label");
        search_count.add_css_class("numeric");
        let search_field = FindField::new(&search_entry, &search_count);
        search_box.append(&search_field);
        let search_previous = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Previous match (Ctrl+Shift+G)")
            .sensitive(false)
            .build();
        let search_next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next match (Ctrl+G)")
            .sensitive(false)
            .build();
        search_box.append(&search_previous);
        search_box.append(&search_next);
        let search_close = icon_button("window-close-symbolic", "Close search", "win.find");
        search_box.append(&search_close);
        let search_clamp = adw::Clamp::builder()
            .maximum_size(600)
            .tightening_threshold(450)
            .hexpand(true)
            .child(&search_box)
            .build();
        search.set_child(Some(&search_clamp));
        search.connect_entry(&search_entry);
        content_box.append(&search);
        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::None)
            .build();
        stack.add_css_class("tab-content");
        let empty = adw::StatusPage::builder()
            .icon_name("utilities-terminal-symbolic")
            .title("No Tabs")
            .description("Open a tab to start a terminal.")
            .build();
        let open = adw::SplitButton::builder()
            .label("New Tab")
            .action_name("win.new")
            .menu_model(&new_tab_menu)
            .tooltip_text("New tab (Ctrl+Shift+T)")
            .dropdown_tooltip("New tab options")
            .halign(gtk::Align::Center)
            .build();
        open.add_css_class("suggested-action");
        open.add_css_class("empty-tab-button");
        crate::menu::setup_split_button(&open, "New tab options");
        empty.set_child(Some(&open));
        stack.add_named(&empty, Some("empty"));
        content_box.append(&stack);
        content_view.set_content(Some(&content_box));
        split.set_content(Some(&content_view));
        let toast = adw::ToastOverlay::new();
        toast.set_child(Some(&split));
        window.set_content(Some(&toast));
        let breakpoint =
            adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 700sp").unwrap());
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);
        let ui = Rc::new(Self {
            app: app.clone(),
            window,
            split,
            stack,
            sidebar,
            sidebar_search,
            sidebar_placeholder: no_tabs,
            heading,
            toast,
            settings,
            desktop_settings,
            tabs: RefCell::new(Vec::new()),
            sessions: RefCell::new(sessions),
            catalog: RefCell::new(catalog),
            filter: Cell::new(TabFilter::All),
            filter_title,
            workspace_filter,
            new_tab_menu,
            empty,
            search,
            search_field,
            search_entry,
            search_count,
            search_previous,
            search_next,
            search_status: Cell::new(None),
            active: Cell::new(0),
            changing: Cell::new(false),
            dirty: Cell::new(false),
            server,
            backend: ipc::Client::new(wake.clone()),
            pending: RefCell::new(HashMap::new()),
            wake: wake.clone(),
            update_source: RefCell::new(None),
            branches: BranchWorker::new(wake.clone()),
            branch_monitors: RefCell::new(HashMap::new()),
        });
        ui.install_actions();
        ui.refresh_workspace_ui();
        let weak = Rc::downgrade(&ui);
        ui.split.connect_show_sidebar_notify(move |split| {
            if let Some(ui) = weak.upgrade() {
                let visible = split.shows_sidebar();
                if ui.settings.borrow().show_sidebar != visible {
                    ui.settings.borrow_mut().show_sidebar = visible;
                    ui.persist_settings();
                }
            }
        });
        if let Some(desktop) = &ui.desktop_settings {
            let weak = Rc::downgrade(&ui);
            desktop.connect_changed(Some(font::KEY), move |desktop, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.settings.borrow_mut().system_font = desktop.string(font::KEY).to_string();
                    if ui.settings.borrow().use_system_font {
                        ui.redraw_terminals();
                    }
                }
            });
        }
        let weak = Rc::downgrade(&ui);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(ui) = weak.upgrade() {
                for tab in ui.tabs.borrow().iter() {
                    for pane in tab.panes.values() {
                        pane.refresh_theme();
                    }
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.sidebar.connect_row_selected(move |_, row| {
            if let (Some(ui), Some(row)) = (weak.upgrade(), row)
                && !ui.changing.get()
                && let Ok(id) = row.widget_name().parse()
                && id != ui.active.get()
            {
                let focused = row.has_focus();
                ui.switch(id);
                if focused && row.is_mapped() {
                    row.grab_focus();
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.sidebar.connect_row_activated(move |_, row| {
            if let Some(ui) = weak.upgrade()
                && !ui.changing.get()
                && let Ok(id) = row.widget_name().parse()
            {
                ui.switch(id);
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.sidebar_search.connect_changed(move |_| {
            if let Some(ui) = weak.upgrade()
                && !ui.changing.get()
            {
                ui.refresh_sidebar();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.sidebar_search.connect_activate(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.select_search_result();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.sidebar_search.connect_stop_search(move |entry| {
            entry.set_text("");
            if let Some(ui) = weak.upgrade()
                && let Some(pane) = ui.active_pane()
            {
                pane.area.grab_focus();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.search_entry.connect_changed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.refresh_search();
            }
        });
        for previous in [true, false] {
            let weak = Rc::downgrade(&ui);
            let button = if previous {
                &ui.search_previous
            } else {
                &ui.search_next
            };
            button.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.advance_search(previous);
                }
            });
        }
        let weak = Rc::downgrade(&ui);
        ui.search_entry.connect_activate(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.advance_search(false);
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&ui);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Escape {
                ui.search.set_search_mode(false);
                return glib::Propagation::Stop;
            }
            let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            let previous = if matches!(key, gdk::Key::g | gdk::Key::G) && control {
                Some(shift)
            } else if key == gdk::Key::Up
                || matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) && shift
            {
                Some(true)
            } else if key == gdk::Key::Down {
                Some(false)
            } else {
                None
            };
            if let Some(previous) = previous {
                ui.advance_search(previous);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        ui.search_entry.add_controller(keys);
        let weak = Rc::downgrade(&ui);
        ui.search.connect_search_mode_enabled_notify(move |search| {
            if let Some(ui) = weak.upgrade()
                && !search.is_search_mode()
            {
                ui.search_entry.set_text("");
                for tab in ui.tabs.borrow().iter() {
                    for pane in tab.panes.values() {
                        pane.search("");
                    }
                }
                if let Some(p) = ui.active_pane() {
                    p.area.grab_focus();
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.detach_frontend();
            }
            glib::Propagation::Stop
        });
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let weak = Rc::downgrade(&ui);
            glib_unix::unix_signal_add_local(signal, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.detach_frontend();
                }
                glib::ControlFlow::Break
            });
        }
        for saved in session.tabs {
            if let Err(error) = ui.restore_tab(saved, false) {
                ui.message(&format!("Could not restore a tab: {error}"));
            }
        }
        let active = {
            let tabs = ui.tabs.borrow();
            tabs.iter()
                .find(|w| w.id == session.active)
                .or_else(|| tabs.first())
                .map(|w| w.id)
        };
        ui.apply_filter(session.filter);
        if let Some(active) = active.filter(|id| {
            ui.tabs
                .borrow()
                .iter()
                .any(|tab| tab.id == *id && ui.filter.get().matches(tab.workspace))
        }) {
            ui.switch(active);
        } else {
            ui.set_filter(ui.filter.get());
        }
        if session.maximized {
            ui.window.maximize();
        }
        let weak = Rc::downgrade(&ui);
        let source =
            glib_unix::unix_fd_add_local(wake.as_raw_fd(), glib::IOCondition::IN, move |_, _| {
                wake.drain();
                let Some(ui) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let replies: Vec<_> = ui.backend.replies().collect();
                for (id, result) in replies {
                    let callback = ui.pending.borrow_mut().remove(&id);
                    if let Some(callback) = callback {
                        callback(&ui, result);
                    } else if let Err(error) = result {
                        ui.message(&error.to_string());
                    }
                }
                let updates = ui.sessions.borrow_mut().poll();
                let directory_changed = updates.values().any(|update| update.directory_changed);
                if ui.sessions.borrow().disconnected() {
                    ui.app.quit();
                    return glib::ControlFlow::Continue;
                }
                for (id, update) in updates {
                    let pane = ui
                        .tabs
                        .borrow()
                        .iter()
                        .find_map(|tab| tab.panes.get(&id).map(|pane| (tab.id, pane.clone())));
                    let Some((tab, pane)) = pane else {
                        continue;
                    };
                    if update.directory_changed {
                        ui.dirty.set(true);
                    }
                    pane.tick(&update);
                    ui.refresh_tab_title(tab);
                    if let Some(unread) = update.unread {
                        ui.set_unread(pane.id, unread);
                    }
                    for message in update.notifications {
                        ui.notify(tab, pane.id, message);
                    }
                    if pane.exited()
                        && let Err(error) = ui.close_pane(pane.id)
                    {
                        ui.message(&error.to_string());
                    }
                }
                if directory_changed {
                    ui.refresh_metadata();
                }
                ui.update_branches();
                for _ in 0..16 {
                    let Ok(request) = ui.server.receiver.try_recv() else {
                        if ui.search.is_search_mode() {
                            ui.update_search_count();
                        }
                        return glib::ControlFlow::Continue;
                    };
                    let method = request.body["method"].as_str().unwrap_or("");
                    let action_command = if method == "action" {
                        let name = request.body["name"].as_str().unwrap_or("");
                        if ui
                            .window
                            .lookup_action(name)
                            .is_some_and(|a| a.is_enabled())
                        {
                            match name {
                                "new" => Some(Command::New {
                                    directory: Some(ui.default_directory()),
                                    name: None,
                                    workspace: Some(ui.selected_workspace()),
                                }),
                                "split-right" | "split-down" => {
                                    ui.active_pane().map(|pane| Command::Split {
                                        pane: Some(pane.id),
                                        down: name == "split-down",
                                    })
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(command) = action_command {
                        let reply = request.reply;
                        ui.command(command, move |_, result| {
                            let response = result
                                .map(|_| json!({"ok":true}))
                                .unwrap_or_else(|e| json!({"error":e.to_string()}));
                            let _ = reply.try_send(response);
                        });
                        continue;
                    }
                    if gmux::control::Command::supports(method) {
                        let reply = request.reply;
                        let command = match serde_json::from_value(request.body) {
                            Ok(command) => command,
                            Err(error) => {
                                let _ = reply.try_send(json!({"error":error.to_string()}));
                                continue;
                            }
                        };
                        ui.command(command, move |_, result| {
                            let response = result
                                .and_then(|result| Ok(serde_json::to_value(result)?))
                                .unwrap_or_else(|e| json!({"error":e.to_string()}));
                            let _ = reply.try_send(response);
                        });
                        continue;
                    }
                    if method == "read" {
                        let body = json!({"method":"_read","pane":request.body["pane"]});
                        let reply = request.reply;
                        ui.request(body, move |_, result| {
                            let _ = reply.try_send(
                                result.unwrap_or_else(|e| json!({"error":e.to_string()})),
                            );
                        });
                        continue;
                    }
                    let response = ui
                        .handle_request(request.body)
                        .unwrap_or_else(|error| json!({"error":error.to_string()}));
                    if ui.dirty.replace(false) {
                        ui.save_session();
                    }
                    let _ = request.reply.try_send(response);
                }
                // Requests already queued when the fd was drained need another turn.
                ui.wake.notify();
                glib::ControlFlow::Continue
            });
        *ui.update_source.borrow_mut() = Some(source);
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local(Duration::from_secs(2), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            ui.refresh_metadata();
            if ui.dirty.replace(false) {
                ui.save_session();
            }
            glib::ControlFlow::Continue
        });
        Ok(ui)
    }
    fn terminal(self: &Rc<Self>, id: u64) -> anyhow::Result<Rc<Terminal>> {
        let weak = Rc::downgrade(self);
        let focus = Rc::new(move |id| {
            if let Some(ui) = weak.upgrade() {
                ui.focus_pane(id);
            }
        });
        let weak = Rc::downgrade(self);
        let close = Rc::new(move |id| {
            if let Some(ui) = weak.upgrade() {
                ui.request_close(id);
            }
        });
        let session = self
            .sessions
            .borrow_mut()
            .attach(id, &Theme::adwaita(adw::StyleManager::default().is_dark()))?;
        Ok(Terminal::new(
            session,
            self.settings.clone(),
            focus,
            close,
            self.app.upcast_ref(),
        ))
    }
    fn new_tab(self: &Rc<Self>, directory: PathBuf, name: Option<String>) {
        self.create_tab(directory, name, self.selected_workspace())
    }
    fn create_tab(
        self: &Rc<Self>,
        directory: PathBuf,
        name: Option<String>,
        workspace: Option<u64>,
    ) {
        self.command(
            Command::New {
                directory: Some(directory),
                name,
                workspace: Some(workspace),
            },
            |ui, result| {
                if let Err(e) = result {
                    ui.message(&e.to_string());
                }
            },
        );
    }
    fn request(
        self: &Rc<Self>,
        body: Value,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<Value>) + 'static,
    ) {
        match self.backend.request(body) {
            Ok(id) => {
                self.pending.borrow_mut().insert(id, Box::new(callback));
            }
            Err(error) => callback(self, Err(error)),
        }
    }
    fn command(
        self: &Rc<Self>,
        command: Command,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<CommandResult>) + 'static,
    ) {
        let reveal = matches!(command, Command::New { .. } | Command::Split { .. });
        self.save_session();
        self.request(
            json!({"method":"_command","command":command}),
            move |ui, result| {
                let result = result.and_then(|value| {
                    let reply: CommandReply = serde_json::from_value(value)?;
                    let result = ui.apply_backend(reply, reveal)?;
                    if let Some(warning) = result.warning.as_deref() {
                        ui.message(warning);
                    }
                    Ok(result)
                });
                callback(ui, result);
            },
        );
    }
    fn apply_backend(
        self: &Rc<Self>,
        reply: CommandReply,
        reveal: bool,
    ) -> anyhow::Result<CommandResult> {
        let previous_active = self.active.get();
        let CommandReply {
            result,
            session: state,
            catalog,
            metadata,
        } = reply;
        *self.catalog.borrow_mut() = catalog;
        self.sessions.borrow_mut().install_metadata(metadata);
        let ids: HashSet<_> = state.tabs.iter().flat_map(|t| t.layout.ids()).collect();
        let removed: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .flat_map(|t| t.panes.keys().copied())
            .filter(|id| !ids.contains(id))
            .collect();
        for id in removed {
            self.close_pane(id)?;
        }
        for saved in state.tabs {
            let existing = self.tabs.borrow().iter().any(|t| t.id == saved.id);
            if !existing {
                self.restore_tab(saved, true)?;
                continue;
            }
            let new_ids: Vec<_> = {
                let tabs = self.tabs.borrow();
                let tab = tabs.iter().find(|t| t.id == saved.id).unwrap();
                saved
                    .layout
                    .ids()
                    .into_iter()
                    .filter(|id| !tab.panes.contains_key(id))
                    .collect()
            };
            let mut new_panes = Vec::new();
            for id in new_ids {
                new_panes.push((id, self.terminal(id)?));
            }
            let changed = {
                let mut tabs = self.tabs.borrow_mut();
                let tab = tabs.iter_mut().find(|t| t.id == saved.id).unwrap();
                let changed = *tab.layout.borrow() != saved.layout;
                *tab.layout.borrow_mut() = saved.layout;
                tab.panes.extend(new_panes);
                tab.name = saved.name;
                tab.custom_name = saved.custom_name;
                tab.workspace = saved.workspace;
                tab.active = saved.active;
                tab.name_label.set_text(&tab.name);
                changed
            };
            if changed {
                self.rebuild(saved.id);
            }
        }
        self.apply_filter(state.filter);
        self.refresh_workspace_ui();
        let visible = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.id == state.active)
            .is_some_and(|tab| self.tab_is_visible(tab));
        if state.active == previous_active && !visible && !reveal {
            // Typing in the sidebar can hide the active row without changing its terminal.
        } else if state.active != 0 && (reveal || visible) {
            self.switch(state.active);
        } else if state.active != 0 {
            self.set_filter(state.filter);
        } else {
            self.show_empty();
        }
        self.refresh_metadata();
        Ok(result)
    }
    fn restore_tab(self: &Rc<Self>, saved: SavedTab, prepend: bool) -> anyhow::Result<()> {
        saved.validate()?;
        anyhow::ensure!(
            !self
                .catalog
                .borrow()
                .workspaces
                .iter()
                .any(|workspace| workspace.id == saved.id
                    || saved.layout.ids().contains(&workspace.id)),
            "Saved tab IDs conflict with a workspace"
        );
        let ids = saved.layout.ids();
        anyhow::ensure!(
            self.tabs.borrow().iter().all(|w| {
                w.id != saved.id
                    && !w.panes.contains_key(&saved.id)
                    && ids
                        .iter()
                        .all(|id| *id != w.id && !w.panes.contains_key(id))
            }),
            "Saved tab IDs conflict with another tab"
        );
        let mut panes = HashMap::new();
        for id in ids {
            match self.terminal(id) {
                Ok(terminal) => {
                    panes.insert(id, terminal);
                }
                Err(error) => {
                    for id in panes.keys() {
                        self.sessions.borrow_mut().close(*id);
                    }
                    return Err(error);
                }
            }
        }
        let active = if panes.contains_key(&saved.active) {
            saved.active
        } else {
            saved.layout.ids()[0]
        };
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.set_vexpand(true);
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&saved.id.to_string());
        let row_actions = gio::SimpleActionGroup::new();
        let unavailable = gio::SimpleAction::new("move-unavailable", None);
        unavailable.set_enabled(false);
        row_actions.add_action(&unavailable);
        row.insert_action_group("tab", Some(&row_actions));
        let row_box = gtk::Box::new(gtk::Orientation::Vertical, 3);
        row_box.add_css_class("list-row-content");
        let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let name_label = gtk::Label::builder()
            .label(&saved.name)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        name_label.add_css_class("heading");
        name_line.append(&name_label);
        let metadata = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let workspace_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        workspace_box.set_visible(false);
        let workspace_label = gtk::Label::builder()
            .xalign(0.0)
            .max_width_chars(12)
            .valign(gtk::Align::Center)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        workspace_label.add_css_class("caption-heading");
        workspace_label.add_css_class("dim-label");
        workspace_box.append(&workspace_label);
        name_line.append(&workspace_box);
        let path_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        path_label.add_css_class("caption");
        path_label.add_css_class("dim-label");
        metadata.append(&path_label);
        let branch_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        branch_box.set_visible(false);
        branch_box.add_css_class("dim-label");
        let branch_icon = gtk::Image::from_icon_name("gmux-branch-symbolic");
        branch_icon.set_pixel_size(16);
        branch_box.append(&branch_icon);
        let branch_label = gtk::Label::builder()
            .xalign(0.0)
            .max_width_chars(14)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        branch_label.add_css_class("caption");
        branch_box.append(&branch_label);
        metadata.append(&branch_box);
        let unread_indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        unread_indicator.add_css_class("tab-unread");
        unread_indicator.set_size_request(10, 10);
        unread_indicator.set_visible(false);
        unread_indicator.set_valign(gtk::Align::Center);
        unread_indicator.set_tooltip_text(Some("Unread terminal alerts"));
        name_line.append(&unread_indicator);
        row_box.append(&name_line);
        row_box.append(&metadata);
        row.set_child(Some(&row_box));
        let menu = gtk::PopoverMenu::from_model(None::<&gio::Menu>);
        crate::menu::setup_popover(&menu, "Tab Actions");
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_position(gtk::PositionType::Bottom);
        menu.set_parent(&row);
        let click = gtk::GestureClick::new();
        click.set_button(3);
        let popover = menu.downgrade();
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(menu) = popover.upgrade() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 0, 0)));
                menu.popup();
            }
        });
        row.add_controller(click);
        let middle_click = gtk::GestureClick::new();
        middle_click.set_button(2);
        middle_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        let id = saved.id;
        middle_click.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(ui) = weak.upgrade() {
                ui.request_close_tab(id);
            }
        });
        row.add_controller(middle_click);
        let keys = gtk::EventControllerKey::new();
        let popover = menu.downgrade();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if crate::menu::context_menu_shortcut(key, modifiers) {
                if let Some(menu) = popover.upgrade() {
                    menu.set_pointing_to(None);
                    menu.popup();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        row.add_controller(keys);
        self.changing.set(true);
        self.stack.add_named(&content, Some(&saved.id.to_string()));
        if prepend {
            self.sidebar.prepend(&row);
        } else {
            self.sidebar.append(&row);
        }
        let position = if prepend { 0 } else { self.tabs.borrow().len() };
        let unread: HashSet<_> = panes
            .iter()
            .filter_map(|(&id, pane)| pane.unread().then_some(id))
            .collect();
        unread_indicator.set_visible(!unread.is_empty());
        self.tabs.borrow_mut().insert(
            position,
            Tab {
                id: saved.id,
                name: saved.name,
                custom_name: saved.custom_name,
                workspace: saved.workspace.filter(|id| {
                    self.catalog
                        .borrow()
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.id == *id)
                }),
                layout: Rc::new(RefCell::new(saved.layout)),
                panes,
                active,
                content,
                row,
                name_label,
                workspace_label,
                workspace_box,
                path_label,
                branch_label,
                branch_box,
                unread,
                unread_indicator,
                menu,
                branch_directory: PathBuf::new(),
                branch_head: None,
            },
        );
        self.changing.set(false);
        self.rebuild(saved.id);
        self.refresh_workspace_ui();
        self.refresh_tab_title(saved.id);
        self.refresh_metadata();
        self.dirty.set(true);
        Ok(())
    }
    fn rebuild(self: &Rc<Self>, tab: u64) {
        let tabs = self.tabs.borrow();
        let Some(w) = tabs.iter().find(|w| w.id == tab) else {
            return;
        };
        // Detach each terminal before destroying its old split containers.
        for pane in w.panes.values() {
            detach(&pane.root);
        }
        while let Some(child) = w.content.first_child() {
            w.content.remove(&child);
        }
        let widget = layout_widget(
            &w.layout.borrow(),
            &w.panes,
            w.layout.clone(),
            vec![],
            Rc::downgrade(self),
        );
        w.content.append(&widget);
        for p in w.panes.values() {
            p.set_active(p.id == w.active);
        }
    }
    fn switch(self: &Rc<Self>, id: u64) {
        self.refresh_tab_title(id);
        let workspace = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.workspace);
        if let Some(workspace) = workspace
            && !self.filter.get().matches(workspace)
        {
            self.apply_filter(TabFilter::All);
        }
        let hidden = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.id == id)
            .is_some_and(|tab| !self.tab_is_visible(tab));
        if hidden {
            let changing = self.changing.replace(true);
            self.sidebar_search.set_text("");
            self.changing.set(changing);
            self.refresh_sidebar();
        }
        let (row, pane, name) = {
            let tabs = self.tabs.borrow();
            let Some(w) = tabs.iter().find(|w| w.id == id) else {
                return;
            };
            (
                w.row.clone(),
                w.panes.get(&w.active).cloned(),
                w.name.clone(),
            )
        };
        self.active.set(id);
        self.subscribe_visible();
        self.update_tab_actions(true);
        self.changing.set(true);
        self.sidebar.select_row(Some(&row));
        self.stack.set_visible_child_name(&id.to_string());
        self.heading.set_title(&name);
        self.window.set_title(Some(&format!("{name} — Gmux")));
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
        if let Some(p) = pane {
            for tab in self.tabs.borrow().iter().filter(|tab| tab.id == id) {
                for terminal in tab.panes.values() {
                    terminal.set_active(terminal.id == p.id);
                }
            }
            self.heading
                .set_subtitle(&model::display_path(&p.directory()));
            p.search(&self.search_entry.text());
            p.area.grab_focus();
            self.mark_read(p.id);
        }
        self.changing.set(false);
        self.dirty.set(true);
    }
    fn update_tab_actions(&self, enabled: bool) {
        for name in TAB_ACTIONS {
            if let Some(action) = self
                .window
                .lookup_action(name)
                .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
            {
                action.set_enabled(enabled);
            }
        }
    }
    fn show_empty(&self) {
        self.active.set(0);
        self.subscribe_visible();
        self.changing.set(true);
        self.sidebar.select_row(None::<&gtk::ListBoxRow>);
        self.changing.set(false);
        self.stack.set_visible_child_name("empty");
        self.heading.set_title("Gmux");
        self.heading.set_subtitle("");
        self.window.set_title(Some("Gmux"));
        self.search.set_search_mode(false);
        self.update_tab_actions(false);
        self.dirty.set(true);
    }
    fn subscribe_visible(&self) {
        let visible = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.id == self.active.get())
            .into_iter()
            .flat_map(|tab| tab.panes.keys().copied())
            .collect();
        if let Err(error) = self.sessions.borrow_mut().set_visible(visible) {
            eprintln!("{error}");
        }
    }
    fn focus_pane(self: &Rc<Self>, id: u64) {
        // GtkStack may briefly restore the previous pane's focus when showing
        // a tab. Only the explicitly selected pane should clear unread alerts.
        if self.changing.get() {
            return;
        }
        let tab = {
            let mut tabs = self.tabs.borrow_mut();
            let Some(w) = tabs.iter_mut().find(|w| w.panes.contains_key(&id)) else {
                return;
            };
            w.active = id;
            for p in w.panes.values() {
                p.set_active(p.id == id);
            }
            w.id
        };
        self.refresh_tab_title(tab);
        self.refresh_branches();
        if self.active.get() != tab {
            self.switch(tab);
        }
        self.mark_read(id);
        self.dirty.set(true);
        if let Some(p) = self.active_pane() {
            self.heading
                .set_subtitle(&model::display_path(&p.directory()));
            self.refresh_search();
        }
    }
    fn active_pane(&self) -> Option<Rc<Terminal>> {
        self.tabs
            .borrow()
            .iter()
            .find(|w| w.id == self.active.get())
            .and_then(|w| w.panes.get(&w.active).cloned())
    }
    fn refresh_search(&self) {
        if let Some(pane) = self.active_pane() {
            pane.search(&self.search_entry.text());
        }
        self.update_search_count();
    }
    fn advance_search(&self, previous: bool) {
        if let Some(pane) = self.active_pane() {
            pane.search_advance(previous);
        }
        self.update_search_count();
    }
    fn update_search_count(&self) {
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
    fn pane(&self, id: Option<u64>) -> anyhow::Result<Rc<Terminal>> {
        if let Some(id) = id {
            self.tabs
                .borrow()
                .iter()
                .find_map(|w| w.panes.get(&id).cloned())
                .ok_or_else(|| anyhow::anyhow!("Pane {id} does not exist"))
        } else {
            self.active_pane()
                .ok_or_else(|| anyhow::anyhow!("No active terminal"))
        }
    }
    fn split_pane(self: &Rc<Self>, id: u64, down: bool) {
        self.command(
            Command::Split {
                pane: Some(id),
                down,
            },
            |ui, result| {
                if let Err(e) = result {
                    ui.message(&e.to_string());
                }
            },
        );
    }
    fn close_pane(self: &Rc<Self>, id: u64) -> anyhow::Result<()> {
        let tab = self
            .tabs
            .borrow()
            .iter()
            .find(|w| w.panes.contains_key(&id))
            .map(|w| w.id)
            .ok_or_else(|| anyhow::anyhow!("Pane {id} does not exist"))?;
        self.sessions.borrow_mut().close(id);
        let was_active = self.active.get() == tab;
        let remaining = {
            let mut tabs = self.tabs.borrow_mut();
            let w = tabs.iter_mut().find(|w| w.id == tab).unwrap();
            let old_ids = w.layout.borrow().ids();
            let layout = w.layout.borrow().clone().remove(id);
            if let Some(p) = w.panes.remove(&id) {
                detach(&p.root);
            }
            w.unread.remove(&id);
            w.unread_indicator.set_visible(!w.unread.is_empty());
            if let Some(layout) = layout {
                if w.active == id {
                    let ids = layout.ids();
                    let index = old_ids.iter().position(|pane| *pane == id).unwrap_or(0);
                    w.active = ids[index.min(ids.len() - 1)];
                }
                *w.layout.borrow_mut() = layout;
                true
            } else {
                false
            }
        };
        if remaining {
            self.rebuild(tab);
            if was_active {
                self.switch(tab);
            }
        } else {
            let (w, index) = {
                let mut list = self.tabs.borrow_mut();
                let index = list.iter().position(|w| w.id == tab).unwrap();
                let visible_index = list[..index]
                    .iter()
                    .filter(|tab| self.tab_is_visible(tab))
                    .count();
                (list.remove(index), visible_index)
            };
            self.changing.set(true);
            self.stack.remove(&w.content);
            self.sidebar.remove(&w.row);
            self.changing.set(false);
            if was_active {
                let next = {
                    let list = self.tabs.borrow();
                    let visible = list
                        .iter()
                        .filter(|tab| self.tab_is_visible(tab))
                        .collect::<Vec<_>>();
                    visible
                        .get(index.min(visible.len().saturating_sub(1)))
                        .map(|tab| tab.id)
                };
                if let Some(next) = next {
                    self.switch(next);
                } else {
                    self.show_empty();
                }
            }
        }
        self.refresh_branches();
        self.refresh_sidebar();
        self.dirty.set(true);
        Ok(())
    }
    fn request_close(self: &Rc<Self>, id: u64) {
        self.request(
            json!({"method":"_busy","panes":[id]}),
            move |ui, result| match result {
                Ok(value) => ui.confirm_close(id, value["busy"].as_bool().unwrap_or(true)),
                Err(e) => ui.message(&e.to_string()),
            },
        );
    }
    fn confirm_close(self: &Rc<Self>, id: u64, busy: bool) {
        if !busy {
            self.command(Command::Close { pane: Some(id) }, |ui, result| {
                if let Err(e) = result {
                    ui.message(&e.to_string());
                }
            });
            return;
        }
        let dialog = adw::AlertDialog::builder()
            .heading("Close terminal?")
            .body("A process is still running in this pane. Closing the pane will stop it.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close terminal");
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("close"), move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.command(Command::Close { pane: Some(id) }, |ui, result| {
                    if let Err(e) = result {
                        ui.message(&e.to_string());
                    }
                });
            }
        });
        dialog.present(Some(&self.window));
    }
    fn request_quit(self: &Rc<Self>) {
        let panes: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .flat_map(|t| t.panes.keys().copied())
            .collect();
        self.request(
            json!({"method":"_busy","panes":panes}),
            |ui, result| match result {
                Ok(value) => ui.confirm_quit(value["busy"].as_bool().unwrap_or(true)),
                Err(e) => ui.message(&e.to_string()),
            },
        );
    }
    fn confirm_quit(self: &Rc<Self>, busy: bool) {
        if !busy {
            self.stop_daemon();
            return;
        }
        let dialog = adw::AlertDialog::builder().heading("Quit Gmux?").body("Processes are still running. Quitting will stop the daemon and all terminals. Your tab layout will be saved.").build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("quit", "Quit Gmux");
        dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("quit"), move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.stop_daemon();
            }
        });
        dialog.present(Some(&self.window));
    }
    fn stop_daemon(self: &Rc<Self>) {
        self.save_session();
        self.request(json!({"method":"quit"}), |ui, result| match result {
            Ok(value) => {
                if let Some(warning) = value["warning"].as_str() {
                    eprintln!("{warning}");
                }
                ui.app.quit();
            }
            Err(error) => ui.message(&error.to_string()),
        });
    }
    fn detach_frontend(self: &Rc<Self>) {
        self.save_session();
        self.request(json!({"method":"_hello"}), |ui, result| {
            if let Err(error) = result {
                eprintln!("Could not finish saving layout: {error}");
            }
            ui.app.quit();
        });
    }
    fn message(&self, message: &str) {
        self.toast.add_toast(adw::Toast::new(message));
    }
    fn notify(&self, tab: u64, pane: u64, message: String) {
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
    fn mark_read(&self, pane: u64) {
        if self.set_unread(pane, false) {
            self.sessions.borrow_mut().mark_read(pane);
        }
    }
    fn set_unread(&self, pane: u64, unread: bool) -> bool {
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
    fn jump_to_pane(self: &Rc<Self>, id: u64) {
        if let Ok(p) = self.pane(Some(id)) {
            self.focus_pane(id);
            self.window.present();
            p.area.grab_focus();
        }
    }
    fn refresh_tab_title(&self, id: u64) {
        let mut tabs = self.tabs.borrow_mut();
        let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        if tab.custom_name {
            return;
        }
        let Some(pane) = tab.panes.get(&tab.active) else {
            return;
        };
        let title = pane.title.text();
        if title.is_empty() || tab.name == title.as_str() {
            return;
        }
        tab.name = title.to_string();
        tab.name_label.set_text(&tab.name);
        if tab.id == self.active.get() {
            self.heading.set_title(&tab.name);
            self.window.set_title(Some(&format!("{} — Gmux", tab.name)));
        }
        self.dirty.set(true);
        drop(tabs);
        self.refresh_sidebar();
    }
    fn update_branches(self: &Rc<Self>) {
        let results = self.branches.results();
        if results.is_empty() {
            return;
        }
        let mut changed = false;
        for (id, directory, branch) in results {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(w) = tabs.iter_mut().find(|w| w.id == id)
                && w.branch_directory == directory
                && w.panes
                    .get(&w.active)
                    .is_some_and(|pane| pane.directory() == directory)
            {
                if w.branch_label.text() != branch.name {
                    w.branch_label.set_label(&branch.name);
                    w.branch_box.set_tooltip_text(Some(&branch.name));
                    changed = true;
                }
                let visible = !branch.name.is_empty();
                if w.branch_box.get_visible() != visible {
                    w.branch_box.set_visible(visible);
                    changed = true;
                }
                w.branch_head = branch.head;
            }
        }
        if changed {
            self.refresh_sidebar();
        }
        let heads = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|tab| tab.branch_head.clone())
            .collect::<HashSet<_>>();
        let mut monitors = self.branch_monitors.borrow_mut();
        monitors.retain(|head, monitor| {
            if heads.contains(head) {
                true
            } else {
                monitor.cancel();
                false
            }
        });
        let mut installed = HashSet::new();
        for head in heads {
            if monitors.contains_key(&head) {
                continue;
            }
            // Watch the parent so Git's atomic HEAD.lock -> HEAD rename is reported.
            let Some(parent) = head.parent() else {
                continue;
            };
            let Ok(monitor) = gio::File::for_path(parent)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };
            monitor.set_rate_limit(50);
            let weak = Rc::downgrade(self);
            let watched = head.clone();
            monitor.connect_changed(move |_, file, other, _| {
                if (file.path().as_ref() == Some(&watched)
                    || other.and_then(gio::File::path).as_ref() == Some(&watched))
                    && let Some(ui) = weak.upgrade()
                {
                    ui.refresh_branch_head(&watched);
                }
            });
            installed.insert(head.clone());
            monitors.insert(head, monitor);
        }
        drop(monitors);
        // Close the gap between the first lookup and installing its monitor.
        if !installed.is_empty() {
            let jobs = self
                .tabs
                .borrow()
                .iter()
                .filter(|tab| {
                    tab.branch_head
                        .as_ref()
                        .is_some_and(|head| installed.contains(head))
                })
                .map(|tab| (tab.id, tab.branch_directory.clone()))
                .collect();
            self.branches.refresh_subset(jobs);
        }
    }
    fn refresh_branch_head(&self, head: &Path) {
        let jobs = self
            .tabs
            .borrow()
            .iter()
            .filter(|tab| tab.branch_head.as_deref() == Some(head))
            .map(|tab| (tab.id, tab.branch_directory.clone()))
            .collect();
        self.branches.refresh_subset(jobs);
    }
    fn refresh_branches(&self) {
        let mut jobs = Vec::new();
        for tab in self.tabs.borrow_mut().iter_mut() {
            if let Some(pane) = tab.panes.get(&tab.active) {
                let directory = pane.directory();
                if tab.branch_directory != directory {
                    tab.branch_box.set_visible(false);
                    tab.branch_head = None;
                }
                tab.branch_directory = directory.clone();
                jobs.push((tab.id, directory));
            }
        }
        if jobs.is_empty() {
            for monitor in self.branch_monitors.borrow_mut().drain().map(|(_, m)| m) {
                monitor.cancel();
            }
        }
        self.branches.refresh(jobs);
    }
    fn refresh_metadata(self: &Rc<Self>) {
        let catalog = self.catalog.borrow();
        for w in self.tabs.borrow_mut().iter_mut() {
            if let Some(p) = w.panes.get(&w.active) {
                let path = p.directory();
                let display = model::display_path(&path);
                let workspace = w.workspace.and_then(|id| {
                    catalog
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)
                });
                w.path_label.set_text(&display);
                w.path_label
                    .set_tooltip_text(Some(&path.display().to_string()));
                let tooltip = workspace
                    .map(|workspace| format!("{}\n{}", workspace.name, path.display()))
                    .unwrap_or_else(|| path.display().to_string());
                w.row.set_tooltip_text(Some(&tooltip));
                if w.id == self.active.get() {
                    self.heading.set_title(&w.name);
                    self.window.set_title(Some(&format!("{} — Gmux", w.name)));
                    self.heading.set_subtitle(&display);
                }
            }
        }
        self.refresh_sidebar();
        self.refresh_branches();
    }
    fn save_session(&self) {
        let session = Session {
            filter: self.filter.get(),
            active: self.active.get(),
            width: self.window.default_width(),
            height: self.window.default_height(),
            maximized: self.window.is_maximized(),
            tabs: self
                .tabs
                .borrow()
                .iter()
                .map(|w| SavedTab {
                    id: w.id,
                    name: w.name.clone(),
                    custom_name: w.custom_name,
                    workspace: w.workspace,
                    layout: w.layout.borrow().clone(),
                    active: w.active,
                    directories: w.panes.iter().map(|(&id, p)| (id, p.directory())).collect(),
                })
                .collect(),
        };
        if let Err(e) = self
            .backend
            .save(serde_json::to_value(&session).expect("Session is serializable"))
        {
            eprintln!("Could not save session: {e}");
        }
    }
    fn install_actions(self: &Rc<Self>) {
        for (name, shortcuts) in [
            ("new", &["<Control><Shift>t"][..]),
            ("open-folder", &["<Control><Shift>o"]),
            ("split-right", &["<Control><Shift>d"]),
            ("split-down", &["<Control><Shift>e"]),
            ("close", &["<Control><Shift>w"]),
            ("close-active-tab", &[]),
            ("copy", &["<Control><Shift>c"]),
            ("paste", &["<Control><Shift>v"]),
            ("find", &["<Control><Shift>f"]),
            ("sidebar", &["<Control><Shift>b"]),
            ("palette", &["<Control><Shift>p"]),
            ("next-tab", &["<Control>Tab", "<Control>Page_Down"]),
            ("previous-tab", &["<Control><Shift>Tab", "<Control>Page_Up"]),
            ("next-pane", &["<Control><Shift>Right"]),
            ("previous-pane", &["<Control><Shift>Left"]),
            ("zoom-in", ZOOM_IN_SHORTCUTS),
            ("zoom-out", &["<Control>minus"]),
            ("zoom-reset", &["<Control>0"]),
            ("preferences", &["<Control>comma"]),
            ("shortcuts", &["<Control><Shift>question"]),
            ("fullscreen", &["F11"]),
            ("quit", &["<Control><Shift>q"]),
            ("rename", &[]),
            ("about", &[]),
            ("new-workspace", &[]),
            ("manage-workspaces", &[]),
        ] {
            let state = match name {
                "sidebar" => Some(self.split.shows_sidebar()),
                "fullscreen" => Some(self.window.is_fullscreen()),
                _ => None,
            };
            let action = match state {
                Some(state) => gio::SimpleAction::new_stateful(name, None, &state.to_variant()),
                None => gio::SimpleAction::new(name, None),
            };
            if state.is_some() {
                let weak = Rc::downgrade(self);
                action.connect_change_state(move |_, value| {
                    if let (Some(ui), Some(enabled)) =
                        (weak.upgrade(), value.and_then(|value| value.get::<bool>()))
                    {
                        match name {
                            "sidebar" => ui.split.set_show_sidebar(enabled),
                            "fullscreen" if enabled => ui.window.fullscreen(),
                            "fullscreen" => ui.window.unfullscreen(),
                            _ => (),
                        }
                    }
                });
                let weak = action.downgrade();
                if name == "sidebar" {
                    self.split.connect_show_sidebar_notify(move |split| {
                        if let Some(action) = weak.upgrade() {
                            action.set_state(&split.shows_sidebar().to_variant());
                        }
                    });
                } else {
                    self.window.connect_fullscreened_notify(move |window| {
                        if let Some(action) = weak.upgrade() {
                            action.set_state(&window.is_fullscreen().to_variant());
                        }
                    });
                }
            }
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.action(name);
                }
            });
            self.window.add_action(&action);
            self.app
                .set_accels_for_action(&format!("win.{name}"), shortcuts);
        }
        let action =
            gio::SimpleAction::new("focus-notification", Some(&u64::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(id)) = (weak.upgrade(), value.and_then(|v| v.get::<u64>())) {
                ui.jump_to_pane(id);
            }
        });
        self.app.add_action(&action);
        let action = gio::SimpleAction::new_stateful(
            "workspace-filter",
            Some(glib::VariantTy::STRING),
            &"all".to_variant(),
        );
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(filter)) =
                (weak.upgrade(), value.and_then(|v| v.get::<String>()))
            {
                let filter = match filter.as_str() {
                    "all" => TabFilter::All,
                    "unassigned" => TabFilter::Unassigned,
                    id => match id.parse() {
                        Ok(id) => TabFilter::Workspace(id),
                        Err(_) => return,
                    },
                };
                ui.set_filter(filter);
            }
        });
        self.window.add_action(&action);
        let action =
            gio::SimpleAction::new("assign-tab", Some(&<(u64, u64)>::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some((tab, workspace))) =
                (weak.upgrade(), value.and_then(|v| v.get::<(u64, u64)>()))
            {
                ui.assign_tab(tab, (workspace != 0).then_some(workspace));
            }
        });
        self.window.add_action(&action);
        let action =
            gio::SimpleAction::new("new-workspace-for-tab", Some(&u64::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(tab)) = (weak.upgrade(), value.and_then(|v| v.get::<u64>())) {
                ui.new_workspace_for_tab(tab);
            }
        });
        self.window.add_action(&action);
        let action = gio::SimpleAction::new("new-tab-workspace", Some(&u64::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(id)) = (weak.upgrade(), value.and_then(|v| v.get::<u64>())) {
                let directory = ui
                    .catalog
                    .borrow()
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == id)
                    .map(|workspace| workspace.directory.clone());
                if let Some(directory) = directory {
                    ui.create_tab(directory, None, Some(id));
                }
            }
        });
        self.window.add_action(&action);
        let action = gio::SimpleAction::new("rename-tab", Some(&u64::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(id)) = (weak.upgrade(), value.and_then(|v| v.get::<u64>())) {
                ui.rename_tab(id);
            }
        });
        self.window.add_action(&action);
        let action = gio::SimpleAction::new("close-tab", Some(&u64::static_variant_type()));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, value| {
            if let (Some(ui), Some(id)) = (weak.upgrade(), value.and_then(|v| v.get::<u64>())) {
                ui.request_close_tab(id);
            }
        });
        self.window.add_action(&action);
    }
    fn action(self: &Rc<Self>, name: &str) {
        if TAB_ACTIONS.contains(&name) && self.active.get() == 0 {
            return;
        }
        match name {
            "new" => {
                self.new_tab(self.default_directory(), None);
            }
            "open-folder" => self.open_folder(),
            "new-workspace" => self.workspace_editor(None),
            "manage-workspaces" => self.manage_workspaces(),
            "split-right" | "split-down" => {
                if let Some(p) = self.active_pane() {
                    self.split_pane(p.id, name == "split-down");
                }
            }
            "close" => {
                if let Some(p) = self.active_pane() {
                    self.request_close(p.id);
                }
            }
            "close-active-tab" => self.request_close_tab(self.active.get()),
            "copy" => {
                if let Some(p) = self.active_pane()
                    && !p.copy()
                {
                    self.message("Select terminal text to copy it");
                }
            }
            "paste" => {
                if let Some(p) = self.active_pane() {
                    p.paste();
                }
            }
            "find" => {
                let show = !self.search.is_search_mode();
                self.search.set_search_mode(show);
                if show {
                    self.search_entry.grab_focus();
                }
            }
            "sidebar" => self.split.set_show_sidebar(!self.split.shows_sidebar()),
            "palette" => self.palette(),
            "next-tab" | "previous-tab" => {
                let ids = self
                    .tabs
                    .borrow()
                    .iter()
                    .filter(|tab| self.tab_is_visible(tab))
                    .map(|w| w.id)
                    .collect::<Vec<_>>();
                if let Some(index) = ids.iter().position(|id| *id == self.active.get()) {
                    let next =
                        (index + if name == "next-tab" { 1 } else { ids.len() - 1 }) % ids.len();
                    self.switch(ids[next]);
                } else if let Some(id) = if name == "next-tab" {
                    ids.first()
                } else {
                    ids.last()
                } {
                    self.switch(*id);
                }
            }
            "next-pane" | "previous-pane" => {
                let (ids, active) = {
                    let list = self.tabs.borrow();
                    let w = list.iter().find(|w| w.id == self.active.get()).unwrap();
                    let ids = w.layout.borrow().ids();
                    (ids, w.active)
                };
                let index = ids.iter().position(|id| *id == active).unwrap();
                let next = (index
                    + if name == "next-pane" {
                        1
                    } else {
                        ids.len() - 1
                    })
                    % ids.len();
                self.jump_to_pane(ids[next]);
            }
            "zoom-in" | "zoom-out" | "zoom-reset" => {
                if let Some(p) = self.active_pane() {
                    p.zoom(match name {
                        "zoom-in" => 1.0,
                        "zoom-out" => -1.0,
                        _ => 0.0,
                    });
                }
            }
            "preferences" => self.preferences(),
            "shortcuts" => self.shortcuts(),
            "rename" => self.rename_tab(self.active.get()),
            "fullscreen" => {
                if self.window.is_fullscreen() {
                    self.window.unfullscreen();
                } else {
                    self.window.fullscreen();
                }
            }
            "quit" => self.request_quit(),
            "about" => self.about(),
            _ => (),
        }
    }
    fn open_folder(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("New Tab in Directory")
            .accept_label("New Tab")
            .build();
        let weak = Rc::downgrade(self);
        dialog.select_folder(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                if let (Some(ui), Ok(folder)) = (weak.upgrade(), result)
                    && let Some(path) = folder.path()
                {
                    ui.new_tab(path, None);
                }
            },
        );
    }
    fn rename_tab(self: &Rc<Self>, id: u64) {
        let Some(name) = self
            .tabs
            .borrow()
            .iter()
            .find(|w| w.id == id)
            .map(|tab| tab.name.clone())
        else {
            return;
        };
        let entry = adw::EntryRow::builder()
            .title("Name")
            .text(name)
            .activates_default(true)
            .build();
        let group = adw::PreferencesGroup::new();
        group.add(&entry);
        let dialog = adw::Dialog::builder()
            .title("Rename Tab")
            .content_width(400)
            .content_height(150)
            .build();
        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        let rename = gtk::Button::with_label("Rename");
        rename.add_css_class("suggested-action");
        rename.set_sensitive(!entry.text().trim().is_empty());
        header.pack_start(&cancel);
        header.pack_end(&rename);
        view.add_top_bar(&header);
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(18)
            .margin_end(18)
            .margin_top(18)
            .margin_bottom(18)
            .build();
        body.append(&group);
        view.set_content(Some(&body));
        dialog.set_child(Some(&view));
        dialog.set_default_widget(Some(&rename));
        dialog.set_focus(Some(&entry));
        let close = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(dialog) = close.upgrade() {
                dialog.close();
            }
        });
        let button = rename.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(button) = button.upgrade() {
                button.set_sensitive(!entry.text().trim().is_empty());
            }
        });
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        rename.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let name = entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                let close = close.clone();
                ui.command(
                    Command::Rename { tab: id, name },
                    move |ui, result| match result {
                        Ok(_) => {
                            if let Some(dialog) = close.upgrade() {
                                dialog.close();
                            }
                        }
                        Err(error) => ui.message(&error.to_string()),
                    },
                );
            }
        });
        dialog.present(Some(&self.window));
    }
    fn preferences(self: &Rc<Self>) {
        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        let page = adw::PreferencesPage::new();
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let themes = gtk::StringList::new(&["Follow system", "Light", "Dark"]);
        let theme = adw::ComboRow::builder()
            .title("Style")
            .model(&themes)
            .selected(match self.settings.borrow().appearance.as_str() {
                "light" => 1,
                "dark" => 2,
                _ => 0,
            })
            .build();
        let weak = Rc::downgrade(self);
        theme.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().appearance =
                    ["system", "light", "dark"][row.selected() as usize].into();
                ui.save_settings();
            }
        });
        appearance.add(&theme);
        let pane_titles = adw::SwitchRow::builder()
            .title("Pane Title Bars")
            .subtitle("Show a title and close button above each terminal")
            .active(self.settings.borrow().show_pane_titles)
            .build();
        let weak = Rc::downgrade(self);
        pane_titles.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().show_pane_titles = row.is_active();
                ui.save_settings();
            }
        });
        appearance.add(&pane_titles);
        let cursor = adw::ComboRow::builder()
            .title("Cursor Style")
            .subtitle("Applications may choose a different cursor style")
            .model(&gtk::StringList::new(&[
                "Bar",
                "Block",
                "Underline",
                "Hollow Block",
            ]))
            .selected(self.settings.borrow().cursor_style as u32)
            .build();
        let weak = Rc::downgrade(self);
        cursor.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade()
                && let Some(style) = model::CursorStyle::ALL.get(row.selected() as usize)
            {
                ui.settings.borrow_mut().cursor_style = *style;
                ui.save_settings();
            }
        });
        appearance.add(&cursor);
        page.add(&appearance);
        let font_group = adw::PreferencesGroup::new();
        let custom_font = adw::ExpanderRow::builder()
            .title("Custom Font")
            .show_enable_switch(true)
            .enable_expansion(!self.settings.borrow().use_system_font)
            .expanded(!self.settings.borrow().use_system_font)
            .build();
        let font_row = adw::ActionRow::builder()
            .title(font::custom_description(&self.settings.borrow()).to_string())
            .use_markup(false)
            .activatable(true)
            .build();
        font_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        custom_font.add_row(&font_row);
        let weak = Rc::downgrade(self);
        custom_font.connect_enable_expansion_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().use_system_font = !row.enables_expansion();
                ui.save_settings();
            }
        });
        let weak = Rc::downgrade(self);
        font_row.connect_activated(move |row| {
            if let Some(ui) = weak.upgrade() {
                let initial = font::custom_description(&ui.settings.borrow());
                let font_dialog = gtk::FontDialog::builder().title("Custom Font").build();
                let weak = Rc::downgrade(&ui);
                let row = row.downgrade();
                font_dialog.choose_font(
                    Some(&ui.window),
                    Some(&initial),
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let (Some(ui), Some(row), Ok(mut desc)) =
                            (weak.upgrade(), row.upgrade(), result)
                        {
                            let mut settings = ui.settings.borrow_mut();
                            settings.font_size =
                                (desc.size() as f64 / pango::SCALE as f64).clamp(6.0, 40.0);
                            desc.unset_fields(pango::FontMask::SIZE);
                            settings.font = desc.to_string().to_string();
                            row.set_title(&font::custom_description(&settings).to_string());
                            drop(settings);
                            ui.save_settings();
                        }
                    },
                );
            }
        });
        font_group.add(&custom_font);
        page.add(&font_group);
        let behavior = adw::PreferencesGroup::builder()
            .title("Tabs")
            .description("Reopening the window reconnects to running terminals. Restore tabs and directories with fresh shells after the background daemon stops.")
            .build();
        let restore = adw::SwitchRow::builder()
            .title("Restore tabs")
            .active(self.settings.borrow().restore)
            .build();
        let weak = Rc::downgrade(self);
        restore.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().restore = row.is_active();
                ui.save_settings();
            }
        });
        behavior.add(&restore);
        let desktop = adw::SwitchRow::builder()
            .title("Desktop notifications")
            .subtitle("Send alerts from unattended terminals to the desktop")
            .active(self.settings.borrow().desktop_notifications)
            .build();
        let weak = Rc::downgrade(self);
        desktop.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().desktop_notifications = row.is_active();
                ui.save_settings();
            }
        });
        behavior.add(&desktop);
        page.add(&behavior);
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }
    fn save_settings(&self) {
        apply_appearance(&self.settings.borrow());
        self.persist_settings();
        self.redraw_terminals();
    }
    fn persist_settings(&self) {
        if let Err(e) = self.backend.persist(self.settings.borrow().clone()) {
            self.message(&e.to_string());
        }
    }
    fn redraw_terminals(&self) {
        for w in self.tabs.borrow().iter() {
            for p in w.panes.values() {
                p.refresh_settings();
            }
        }
    }
    fn palette(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder()
            .title("Commands")
            .content_width(480)
            .content_height(460)
            .build();
        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        view.add_top_bar(&header);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search commands")
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Search commands")]);
        search.update_property(&[gtk::accessible::Property::Description(
            "Use Up and Down to select a command, and Enter to run it",
        )]);
        let search_focus = gtk::EventControllerFocus::new();
        search.add_controller(search_focus.clone());
        let weak_dialog = dialog.downgrade();
        search.connect_stop_search(move |_| {
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.close();
            }
        });
        let search_clamp = adw::Clamp::builder()
            .maximum_size(400)
            .hexpand(true)
            .child(&search)
            .build();
        header.set_title_widget(Some(&search_clamp));
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);
        search.update_relation(&[gtk::accessible::Relation::Controls(&[list.upcast_ref()])]);
        let no_commands = adw::StatusPage::builder()
            .title("No Matching Commands")
            .build();
        no_commands.add_css_class("compact");
        list.set_placeholder(Some(&no_commands));
        let mut command_rows = Vec::new();
        let scores = Rc::new(
            COMMANDS
                .iter()
                .enumerate()
                .map(|(index, command)| (command.action, (Cell::new(Some(0)), index)))
                .collect::<HashMap<_, _>>(),
        );
        let sort_scores = scores.clone();
        list.set_sort_func(move |first, second| {
            let key = |row: &gtk::ListBoxRow| {
                let (score, index) = &sort_scores[row.widget_name().as_str()];
                (score.get().unwrap_or(u32::MAX), *index)
            };
            key(first).cmp(&key(second)).into()
        });
        for command in COMMANDS {
            let action = command.action;
            let row = adw::ActionRow::builder()
                .title(command.title)
                .title_lines(1)
                .activatable(true)
                .build();
            row.set_widget_name(action);
            if let Some(action) = self.window.lookup_action(action) {
                action
                    .bind_property("enabled", &row, "sensitive")
                    .sync_create()
                    .build();
            }
            if let Some(shortcut) = self.app.accels_for_action(&format!("win.{action}")).first() {
                let key = adw::ShortcutLabel::new(shortcut);
                key.set_valign(gtk::Align::Center);
                row.add_suffix(&key);
            }
            list.append(&row);
            command_rows.push((row, command));
        }
        let viewport = gtk::Viewport::builder().child(&list).build();
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&viewport)
            .build();
        let weak_list = list.downgrade();
        let weak_viewport = viewport.downgrade();
        let weak_adjustment = scroll.vadjustment().downgrade();
        let keep_selected_visible = move || {
            if let (Some(list), Some(viewport), Some(adjustment)) = (
                weak_list.upgrade(),
                weak_viewport.upgrade(),
                weak_adjustment.upgrade(),
            ) && let Some(row) = list.selected_row()
                && let Some(bounds) = row.compute_bounds(&viewport)
            {
                // Viewport coordinates include the list's CSS padding.
                let top = adjustment.value() + f64::from(bounds.y());
                // Include Adwaita's 2px navigation-row margins around the border box.
                adjustment.clamp_page(top - 2.0, top + f64::from(bounds.height()) + 2.0);
            }
        };
        let scroll_selected = keep_selected_visible.clone();
        let selected_search = search.downgrade();
        let selected_focus = search_focus.clone();
        list.connect_row_selected(move |_, row| {
            scroll_selected();
            if let Some(search) = selected_search.upgrade() {
                if let Some(row) = row {
                    search.update_relation(&[gtk::accessible::Relation::ActiveDescendant(
                        row.upcast_ref(),
                    )]);
                    if selected_focus.contains_focus() {
                        announce_command(&search, row);
                    }
                } else {
                    search.reset_relation(gtk::AccessibleRelation::ActiveDescendant);
                }
            }
        });
        // Wait for layout to finish when filtering or resizing changes the viewport.
        scroll.vadjustment().connect_changed(move |_| {
            glib::idle_add_local_once(keep_selected_visible.clone());
        });
        view.set_content(Some(&scroll));
        dialog.set_child(Some(&view));
        let weak = Rc::downgrade(self);
        let d = dialog.downgrade();
        list.connect_row_activated(move |_, row| {
            if let Some(dialog) = d.upgrade() {
                dialog.close();
            }
            if let Some(ui) = weak.upgrade() {
                ui.action(&row.widget_name());
            }
        });
        let filter_list = list.clone();
        search.connect_search_changed(move |entry| {
            let query = Query::new(&entry.text());
            for (_, command) in &command_rows {
                scores[command.action].0.set(query.score(command));
            }
            filter_list.unselect_all();
            for (row, command) in &command_rows {
                row.set_visible(scores[command.action].0.get().is_some());
            }
            filter_list.invalidate_sort();
            let first = command_rows
                .iter()
                .filter(|(row, _)| row.is_visible() && row.is_sensitive())
                .min_by_key(|(_, command)| {
                    let (score, index) = &scores[command.action];
                    (score.get(), *index)
                })
                .map(|(row, _)| row);
            filter_list.select_row(first);
            if first.is_none() && search_focus.contains_focus() {
                let message = if command_rows.iter().any(|(row, _)| row.is_visible()) {
                    "No Available Commands"
                } else {
                    "No Matching Commands"
                };
                entry.announce(message, gtk::AccessibleAnnouncementPriority::Medium);
            }
        });
        let d = dialog.downgrade();
        let weak = Rc::downgrade(self);
        let activate_list = list.clone();
        search.connect_activate(move |_| {
            if let Some(row) = activate_list
                .selected_row()
                .filter(|row| row.is_sensitive())
            {
                if let Some(dialog) = d.upgrade() {
                    dialog.close();
                }
                if let Some(ui) = weak.upgrade() {
                    ui.action(&row.widget_name());
                }
            }
        });
        let keys = gtk::EventControllerKey::new();
        let navigation_list = list.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if !matches!(key, gdk::Key::Up | gdk::Key::Down) {
                return glib::Propagation::Proceed;
            }
            let current = navigation_list
                .selected_row()
                .map(|r| r.index())
                .unwrap_or(-1);
            let direction = if key == gdk::Key::Down { 1 } else { -1 };
            let mut index = current + direction;
            while let Some(row) = navigation_list.row_at_index(index) {
                if row.is_visible() && row.is_sensitive() {
                    navigation_list.select_row(Some(&row));
                    break;
                }
                index += direction;
            }
            glib::Propagation::Stop
        });
        search.add_controller(keys);
        list.select_row(list.row_at_index(0).as_ref());
        dialog.present(Some(&self.window));
        search.grab_focus();
        if let Some(row) = list.selected_row() {
            announce_command(&search, &row);
        }
    }
    fn shortcuts(&self) {
        let dialog = adw::ShortcutsDialog::new();
        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "Tabs",
                &[
                    ("New tab", "new"),
                    ("New tab in directory", "open-folder"),
                    ("Next tab", "next-tab"),
                    ("Previous tab", "previous-tab"),
                    ("Show or hide tabs", "sidebar"),
                ],
            ),
            (
                "Panes",
                &[
                    ("Split right", "split-right"),
                    ("Split down", "split-down"),
                    ("Close pane", "close"),
                    ("Next pane", "next-pane"),
                    ("Previous pane", "previous-pane"),
                ],
            ),
            (
                "Terminal",
                &[
                    ("Copy selection", "copy"),
                    ("Paste", "paste"),
                    ("Find in terminal", "find"),
                    ("Zoom in", "zoom-in"),
                    ("Zoom out", "zoom-out"),
                    ("Reset zoom", "zoom-reset"),
                ],
            ),
            (
                "Application",
                &[
                    ("Commands", "palette"),
                    ("Preferences", "preferences"),
                    ("Keyboard shortcuts", "shortcuts"),
                    ("Toggle fullscreen", "fullscreen"),
                    ("Quit", "quit"),
                ],
            ),
        ];
        for (title, actions) in sections {
            let section = adw::ShortcutsSection::new(Some(title));
            for (title, action) in *actions {
                section.add(adw::ShortcutsItem::from_action(
                    title,
                    &format!("win.{action}"),
                ));
            }
            dialog.add(section);
        }
        dialog.present(Some(&self.window));
    }
    fn about(&self) {
        let dialog = adw::AboutDialog::builder()
            .application_name("Gmux")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Terminal tabs for GNOME. Built with Rust, libadwaita, and libghostty-vt.")
            .license_type(gtk::License::MitX11)
            .build();
        dialog.add_acknowledgement_section(Some("Terminal engine"), &["Ghostty contributors"]);
        dialog.present(Some(&self.window));
    }
    fn handle_request(self: &Rc<Self>, request: Value) -> anyhow::Result<Value> {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Request requires a method"))?;
        let pane_id = request.get("pane").and_then(Value::as_u64);
        match method {
            "detach" => {
                let window = self.window.clone();
                glib::timeout_add_local_once(Duration::from_millis(50), move || {
                    window.close();
                });
                Ok(json!({"ok":true}))
            }
            "renderer-info" => {
                let pane = if pane_id.is_some() {
                    Some(self.pane(pane_id)?)
                } else {
                    self.active_pane()
                };
                Ok(json!({
                    "terminal": pane.as_ref().map(|pane| pane.renderer_name()),
                    "gtk": self.window.renderer().map(|renderer| renderer.type_().name().to_string()),
                    "display": gtk::prelude::WidgetExt::display(&self.window).type_().name(),
                    "window_active": self.window.is_active(),
                    "viewport": pane.map(|pane| pane.renderer_geometry()),
                }))
            }
            "list" => {
                let tabs = self.tabs.borrow();
                Ok(json!({
                    "active_tab": (self.active.get() != 0).then_some(self.active.get()),
                    "filter": self.filter.get(),
                    "tab_search": self.sidebar_search.text().as_str(),
                    "sidebar_visible": self.split.shows_sidebar(),
                    "sidebar_collapsed": self.split.is_collapsed(),
                    "workspaces": self.catalog.borrow().workspaces.iter().map(|workspace| json!({
                        "id": workspace.id, "name": workspace.name, "directory": workspace.directory,
                        "tabs": tabs.iter().filter(|tab| tab.workspace == Some(workspace.id)).count(),
                        "unread": tabs.iter().any(|tab| tab.workspace == Some(workspace.id) && !tab.unread.is_empty()),
                    })).collect::<Vec<_>>(),
                    "tabs": tabs.iter().map(|tab| json!({
                        "id": tab.id, "name": tab.name, "workspace": tab.workspace,
                        "visible": self.tab_is_visible(tab),
                        "active_pane": tab.active, "unread": !tab.unread.is_empty(),
                        "layout": *tab.layout.borrow(),
                        "panes": tab.layout.borrow().ids().iter().filter_map(|id| tab.panes.get(id))
                            .map(|pane| json!({"id": pane.id, "title": pane.title.text().to_string(),
                                "directory": pane.directory(), "exited": pane.exited(),
                                "unread": tab.unread.contains(&pane.id)})).collect::<Vec<_>>()
                    })).collect::<Vec<_>>()
                }))
            }
            "search-tabs" => {
                let query = request
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("search-tabs requires a query"))?;
                let changing = self.changing.replace(true);
                self.sidebar_search.set_text(query);
                self.changing.set(changing);
                self.refresh_sidebar();
                Ok(json!({"ok": true}))
            }
            "filter" => {
                let filter = if let Some(id) = request.get("workspace").and_then(Value::as_u64) {
                    anyhow::ensure!(
                        self.catalog
                            .borrow()
                            .workspaces
                            .iter()
                            .any(|workspace| workspace.id == id),
                        "Workspace {id} does not exist"
                    );
                    TabFilter::Workspace(id)
                } else {
                    match request.get("value").and_then(Value::as_str) {
                        Some("all") => TabFilter::All,
                        Some("unassigned") => TabFilter::Unassigned,
                        _ => {
                            anyhow::bail!("filter requires a workspace ID or value all/unassigned")
                        }
                    }
                };
                self.set_filter(filter);
                Ok(json!({"ok": true}))
            }
            "send" => {
                let pane = self.pane(pane_id)?;
                let text = request
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("send requires text"))?;
                if request
                    .get("paste")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    pane.send_text(
                        text,
                        request
                            .get("enter")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )?;
                } else {
                    pane.send(text.as_bytes())?;
                }
                Ok(json!({"ok":true}))
            }
            "focus" => {
                if let Some(pane) = pane_id {
                    self.pane(Some(pane))?;
                    self.jump_to_pane(pane);
                } else if let Some(id) = request.get("tab").and_then(Value::as_u64) {
                    anyhow::ensure!(
                        self.tabs.borrow().iter().any(|w| w.id == id),
                        "Tab {id} does not exist"
                    );
                    self.switch(id);
                    self.window.present();
                } else if let Some(id) = request.get("workspace").and_then(Value::as_u64) {
                    anyhow::ensure!(
                        self.catalog
                            .borrow()
                            .workspaces
                            .iter()
                            .any(|workspace| workspace.id == id),
                        "Workspace {id} does not exist"
                    );
                    self.set_filter(TabFilter::Workspace(id));
                    self.window.present();
                } else {
                    anyhow::bail!("focus requires a tab, workspace, or pane");
                }
                Ok(json!({"ok":true}))
            }
            "screenshot" => {
                let path = request
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("screenshot requires a path"))?;
                self.capture_window(path)?;
                Ok(json!({"path":path}))
            }
            "resize" => {
                let width = request
                    .get("width")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow::anyhow!("resize requires width"))?;
                let height = request
                    .get("height")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow::anyhow!("resize requires height"))?;
                anyhow::ensure!(
                    (360..=7680).contains(&width) && (280..=4320).contains(&height),
                    "Window size is outside the supported range"
                );
                self.window.unmaximize();
                self.window.set_default_size(width as i32, height as i32);
                Ok(json!({"ok":true}))
            }
            "action" => {
                let name = request
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("action requires a name"))?;
                anyhow::ensure!(
                    self.window.lookup_action(name).is_some(),
                    "Unknown action: {name}"
                );
                anyhow::ensure!(
                    self.window
                        .lookup_action(name)
                        .is_some_and(|a| a.is_enabled()),
                    "Action unavailable: {name}"
                );
                anyhow::ensure!(
                    self.window
                        .lookup_action(name)
                        .is_some_and(|a| a.parameter_type().is_none()),
                    "Action requires parameters: {name}. Use the corresponding socket method"
                );
                self.action(name);
                Ok(json!({"ok":true}))
            }
            _ => anyhow::bail!("Unknown method: {method}"),
        }
    }

    // Diagnostic screenshots need a fresh snapshot even when the compositor
    // pauses the window. WidgetPaintable returns its cached render node in that
    // case. Keep the deprecated CSS background/frame calls confined here until
    // GTK offers a public replacement that also captures the window decoration.
    #[allow(deprecated)]
    fn capture_window(&self, path: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.window.is_mapped() && self.window.allocated_width() > 0,
            "Window has not rendered yet"
        );
        // A compositor may pause an occluded window's frame clock. Complete
        // pending layout and snapshot its current contents without waiting for
        // an on-screen frame or returning a stale cached paintable.
        self.window.allocate(
            self.window.allocated_width(),
            self.window.allocated_height(),
            self.window.allocated_baseline(),
            None,
        );
        let snapshot = gtk::Snapshot::new();
        let style = self.window.style_context();
        let width = self.window.width() as f64;
        let height = self.window.height() as f64;
        snapshot.render_background(&style, 0.0, 0.0, width, height);
        let mut child = self.window.first_child();
        while let Some(widget) = child {
            self.window.snapshot_child(&widget, &snapshot);
            child = widget.next_sibling();
        }
        snapshot.render_frame(&style, 0.0, 0.0, width, height);
        let node = snapshot
            .to_node()
            .ok_or_else(|| anyhow::anyhow!("Window has not rendered yet"))?;
        let renderer = self
            .window
            .renderer()
            .ok_or_else(|| anyhow::anyhow!("Window has no renderer"))?;
        renderer.render_texture(&node, None).save_to_png(path)?;
        Ok(())
    }
}

const TAB_ACTIONS: &[&str] = &[
    "split-right",
    "split-down",
    "close",
    "close-active-tab",
    "copy",
    "paste",
    "find",
    "rename",
    "next-tab",
    "previous-tab",
    "next-pane",
    "previous-pane",
    "zoom-in",
    "zoom-out",
    "zoom-reset",
];
const ZOOM_IN_SHORTCUTS: &[&str] = &["<Control>plus", "<Control>equal"];
fn announce_command(search: &gtk::SearchEntry, row: &gtk::ListBoxRow) {
    if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
        search.announce(&row.title(), gtk::AccessibleAnnouncementPriority::Medium);
    }
}
fn icon_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .build()
}
fn main_menu_button(tabs: &gio::Menu) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append_section(None, tabs);
    let visibility = gio::Menu::new();
    visibility.append(Some("Show _Tabs"), Some("win.sidebar"));
    visibility.append(Some("_Fullscreen"), Some("win.fullscreen"));
    menu.append_section(None, &visibility);
    let tools = gio::Menu::new();
    tools.append(Some("_Manage Workspaces…"), Some("win.manage-workspaces"));
    tools.append(Some("_Commands…"), Some("win.palette"));
    // Keep Quit available because closing the window leaves the daemon running.
    // This deliberately differs from the HIG's usual primary-menu guidance.
    tools.append(Some("_Quit Gmux"), Some("win.quit"));
    menu.append_section(None, &tools);
    let app = gio::Menu::new();
    app.append(Some("_Preferences"), Some("win.preferences"));
    app.append(Some("_Keyboard Shortcuts"), Some("win.shortcuts"));
    app.append(Some("_About Gmux"), Some("win.about"));
    menu.append_section(None, &app);
    let button = menu_button("open-menu-symbolic", "Main Menu", &menu);
    button.set_primary(true);
    button
}
fn terminal_menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let terminal = gio::Menu::new();
    terminal.append(Some("Split _Right"), Some("win.split-right"));
    terminal.append(Some("Split _Down"), Some("win.split-down"));
    terminal.append(Some("_Find in Terminal…"), Some("win.find"));
    menu.append_section(None, &terminal);
    let zoom = gio::Menu::new();
    let zoom_in = gio::MenuItem::new(Some("_Zoom In"), Some("win.zoom-in"));
    // GTK does not infer a displayed accelerator for alternative shortcuts.
    zoom_in.set_attribute_value("accel", Some(&ZOOM_IN_SHORTCUTS[0].to_variant()));
    zoom.append_item(&zoom_in);
    zoom.append(Some("Zoom _Out"), Some("win.zoom-out"));
    zoom.append(Some("Re_set Zoom"), Some("win.zoom-reset"));
    menu.append_section(None, &zoom);
    let close = gio::Menu::new();
    close.append(Some("Close _Pane"), Some("win.close"));
    close.append(Some("Close _Tab"), Some("win.close-active-tab"));
    menu.append_section(None, &close);
    menu_button("view-more-symbolic", "Terminal Actions", &menu)
}
fn menu_button(icon: &str, label: &'static str, menu: &gio::Menu) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .icon_name(icon)
        .tooltip_text(label)
        .menu_model(menu)
        .build();
    crate::menu::setup_menu_button(&button, label);
    button
}
fn apply_appearance(settings: &Settings) {
    adw::StyleManager::default().set_color_scheme(match settings.appearance.as_str() {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
}
fn detach(widget: &gtk::Box) {
    if let Some(parent) = widget.parent() {
        if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
            if paned.start_child().as_ref() == Some(widget.upcast_ref()) {
                paned.set_start_child(None::<&gtk::Widget>);
            } else {
                paned.set_end_child(None::<&gtk::Widget>);
            }
        } else if let Some(box_) = parent.downcast_ref::<gtk::Box>() {
            box_.remove(widget);
        }
    }
}
fn layout_widget(
    layout: &Layout,
    panes: &HashMap<u64, Rc<Terminal>>,
    saved: Rc<RefCell<Layout>>,
    path: Vec<bool>,
    ui: std::rc::Weak<Ui>,
) -> gtk::Widget {
    match layout {
        Layout::Pane(id) => panes[id].root.clone().upcast(),
        Layout::Split {
            vertical,
            first,
            second,
            ratio,
        } => {
            let paned = gtk::Paned::new(if *vertical {
                gtk::Orientation::Vertical
            } else {
                gtk::Orientation::Horizontal
            });
            paned.add_css_class("tab-split");
            // GtkPaned clips overflow by default, including the card shadows
            // on its outer edges. Each terminal already clips its own contents.
            paned.set_overflow(gtk::Overflow::Visible);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            // Narrow handles extend their hit area into adjacent scrollbars.
            // Keep input within the existing 6px separator styled in CSS.
            paned.set_wide_handle(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            let mut first_path = path.clone();
            first_path.push(false);
            let mut second_path = path.clone();
            second_path.push(true);
            paned.set_start_child(Some(&layout_widget(
                first,
                panes,
                saved.clone(),
                first_path,
                ui.clone(),
            )));
            paned.set_end_child(Some(&layout_widget(
                second,
                panes,
                saved.clone(),
                second_path,
                ui.clone(),
            )));
            let ratio = ratio.clamp(0.1, 0.9);
            let vertical = *vertical;
            let initialized = Rc::new(Cell::new(false));
            let applied = initialized.clone();
            paned.add_tick_callback(move |paned, _| {
                let dimension = if vertical {
                    paned.height()
                } else {
                    paned.width()
                };
                if dimension > 0 {
                    paned.set_position((dimension as f64 * ratio) as i32);
                    applied.set(true);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            paned.connect_position_notify(move |paned| {
                // Initial allocation can notify before the saved proportion is
                // applied. Do not persist that temporary divider position.
                if !initialized.get() {
                    return;
                }
                let dimension = if vertical {
                    paned.height()
                } else {
                    paned.width()
                };
                if dimension <= 0 {
                    return;
                }
                let mut layout = saved.borrow_mut();
                let mut node = &mut *layout;
                for end in &path {
                    if let Layout::Split { first, second, .. } = node {
                        node = if *end { second } else { first };
                    }
                }
                if let Layout::Split { ratio, .. } = node {
                    *ratio = (paned.position() as f64 / dimension as f64).clamp(0.1, 0.9);
                }
                if let Some(ui) = ui.upgrade() {
                    ui.dirty.set(true);
                }
            });
            paned.upcast()
        }
    }
}
