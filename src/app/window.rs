//! Window construction and widget setup.
use super::*;

impl Ui {
    pub(super) fn new(
        app: &adw::Application,
        settings: Rc<RefCell<Settings>>,
        directory: Option<PathBuf>,
        owner: std::rc::Weak<Windows>,
        id: u64,
        endpoint: ipc::Endpoint,
    ) -> anyhow::Result<Rc<Self>> {
        let wake = ipc::Wake::new()?;
        let mut sessions = SessionManager::connect_at(endpoint.clone(), wake.clone())?;
        let initial: CommandReply = if let Some(directory) = directory {
            // Initial setup completes before presenting a window. Runtime commands use the worker.
            let reply: CommandReply =
                serde_json::from_value(endpoint.call(InternalRequest::Command {
                    command: Command::New {
                        directory: Some(directory),
                        name: None,
                        workspace: None,
                    },
                })?)?;
            reply
        } else {
            serde_json::from_value(endpoint.call(InternalRequest::State)?)?
        };
        sessions.install_metadata(initial.metadata);
        let catalog = initial.catalog;
        let session = initial.session;
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
        css.load_from_string(include_str!("../../data/style.css"));
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
        let sidebar_main_menu = main_menu_button(&new_tab_menu);
        sidebar_header.pack_end(&sidebar_main_menu);
        let new = adw::SplitButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Tab (Ctrl+Shift+T)")
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
            "Show or Hide Tabs (Ctrl+Shift+B)",
            "win.sidebar",
        );
        header.pack_start(&toggle);
        let heading = adw::WindowTitle::new("Gmux", "");
        header.set_title_widget(Some(&heading));
        let terminal_menu = terminal_menu_button();
        terminal_menu.set_visible(false);
        header.pack_end(&terminal_menu);
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
            .tooltip_text("New Tab (Ctrl+Shift+T)")
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
        let keyboard = crate::terminal::KeyboardModifiers::install(&window);
        let ui = Rc::new(Self {
            app: app.clone(),
            window,
            split,
            stack,
            sidebar,
            sidebar_search,
            sidebar_placeholder: no_tabs,
            sidebar_main_menu,
            terminal_menu,
            heading,
            toast,
            settings,
            preferences_dialog: RefCell::new(None),
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
            synced_focus: Cell::new((u64::MAX, None)),
            changing: Cell::new(false),
            dirty: Cell::new(false),
            save_source: RefCell::new(None),
            owner,
            id,
            revision: Cell::new(initial.revision),
            detaching: Cell::new(false),
            backend: endpoint.client(wake.clone()),
            pending: RefCell::new(HashMap::new()),
            command_count: Cell::new(0),
            command_exits: RefCell::new(HashSet::new()),
            keyboard,
            update_source: RefCell::new(None),
            branch_source: RefCell::new(None),
            theme_handler: RefCell::new(None),
            branches: BranchWorker::new(wake.clone()),
            branch_cache: RefCell::new(BranchCache::default()),
            branch_monitors: RefCell::new(HashMap::new()),
            repository_monitors: RefCell::new(HashMap::new()),
        });
        let weak = Rc::downgrade(&ui);
        ui.window.connect_is_active_notify(move |window| {
            if let Some(ui) = weak.upgrade() {
                if let Err(error) = ui.sessions.borrow().set_focused(window.is_active()) {
                    ui.message(&error.to_string());
                }
                if window.is_active() {
                    ui.synced_focus.set((u64::MAX, None));
                    ui.sync_focus();
                    if let Some(pane) = ui.active_pane() {
                        ui.mark_read(pane.id);
                    }
                }
            }
        });
        ui.install_actions();
        ui.refresh_workspace_ui();
        let weak = Rc::downgrade(&ui);
        ui.split.connect_show_sidebar_notify(move |split| {
            if let Some(ui) = weak.upgrade() {
                let visible = split.shows_sidebar();
                // Existing windows keep their own visibility. Save only the
                // default for future windows in the shared preferences.
                if ui.settings.borrow().show_sidebar != visible {
                    ui.settings.borrow_mut().show_sidebar = visible;
                    ui.persist_settings();
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        let handler = adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(ui) = weak.upgrade() {
                for tab in ui.tabs.borrow().iter() {
                    for pane in tab.panes.values() {
                        pane.refresh_theme();
                    }
                }
            }
        });
        *ui.theme_handler.borrow_mut() = Some(handler);
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
                    ui.close_window();
                    return glib::ControlFlow::Break;
                }
                for (id, update) in updates {
                    if update.exited && ui.command_count.get() > 0 {
                        ui.command_exits.borrow_mut().insert(id);
                    }
                    let pane = ui
                        .tabs
                        .borrow()
                        .iter()
                        .find_map(|tab| tab.panes.get(&id).map(|pane| (tab.id, pane.clone())));
                    let Some((tab, pane)) = pane else {
                        for message in update.notifications {
                            ui.notify(0, id, message);
                        }
                        continue;
                    };
                    if update.directory_changed {
                        ui.mark_dirty();
                    }
                    pane.tick(&update);
                    ui.refresh_tab_title(tab);
                    if let Some(unread) = update.unread {
                        ui.set_unread(pane.id, unread);
                    }
                    for message in update.notifications {
                        ui.notify(tab, pane.id, message);
                    }
                    if (update.exited || pane.exited())
                        && let Err(error) = ui.close_pane(pane.id)
                    {
                        ui.message(&error.to_string());
                    }
                }
                if directory_changed {
                    ui.refresh_metadata();
                }
                ui.update_branches();
                let state = ui.sessions.borrow_mut().take_state();
                if let Some(state) = state
                    && let Err(error) = ui.apply_shared(state)
                {
                    ui.message(&error.to_string());
                }
                if ui.search.is_search_mode() {
                    ui.update_search_count();
                }
                glib::ControlFlow::Continue
            });
        *ui.update_source.borrow_mut() = Some(source);
        let weak = Rc::downgrade(&ui);
        let source = glib::timeout_add_local(Duration::from_secs(30), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Retry failed watches and lookups. Working watches need no polling.
            ui.refresh_unwatched_branches();
            glib::ControlFlow::Continue
        });
        *ui.branch_source.borrow_mut() = Some(source);
        Ok(ui)
    }
    pub(super) fn handle_control(self: &Rc<Self>, request: ipc::Request) {
        let ui = self;
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
                    "split-right" | "split-down" => ui.active_pane().map(|pane| Command::Split {
                        pane: Some(pane.id),
                        down: name == "split-down",
                    }),
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
            return;
        }
        if gmux::control::Command::supports(method) {
            let reply = request.reply;
            let command = match serde_json::from_value(request.body) {
                Ok(command) => command,
                Err(error) => {
                    let _ = reply.try_send(json!({"error":error.to_string()}));
                    return;
                }
            };
            ui.command(command, move |_, result| {
                let response = result
                    .and_then(|result| Ok(serde_json::to_value(result)?))
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                let _ = reply.try_send(response);
            });
            return;
        }
        if method == "read" {
            let pane = match backend::RequestTargets::decode(&request.body)
                .and_then(|targets| ui.pane(targets.pane))
            {
                Ok(pane) => pane,
                Err(error) => {
                    let _ = request.reply.try_send(json!({"error":error.to_string()}));
                    return;
                }
            };
            if ui.sessions.borrow().is_visible(pane.id) {
                let _ = request.reply.try_send(json!({"text":pane.text()}));
                return;
            }
            let body = InternalRequest::Read {
                pane: Some(pane.id),
            };
            let reply = request.reply;
            ui.request(body, move |_, result| {
                let _ = reply.try_send(result.unwrap_or_else(|e| json!({"error":e.to_string()})));
            });
            return;
        }
        let response = ui
            .handle_request(request.body)
            .unwrap_or_else(|error| json!({"error":error.to_string()}));
        if ui.dirty.replace(false) {
            ui.save_session();
        }
        let _ = request.reply.try_send(response);
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
    let fullscreen = gio::Menu::new();
    fullscreen.append(Some("_Fullscreen"), Some("win.fullscreen"));
    menu.append_section(None, &fullscreen);
    // Mirror tab choices into the creation group, keeping New Window out of
    // the specialized New Tab dropdowns and retaining live workspace updates.
    let creation = gio::Menu::new();
    for index in 0..tabs.n_items() {
        creation.append_item(&gio::MenuItem::from_model(tabs, index));
    }
    creation.append(Some("New _Window"), Some("win.new-window"));
    let updated = creation.clone();
    tabs.connect_items_changed(move |tabs, position, removed, added| {
        for _ in 0..removed {
            updated.remove(position);
        }
        for index in position..position + added {
            updated.insert_item(index, &gio::MenuItem::from_model(tabs, index));
        }
    });
    menu.append_section(None, &creation);
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
    menu_button("open-menu-symbolic", "Main Menu", &menu)
}
fn terminal_menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let tab = gio::Menu::new();
    tab.append(Some("Re_name Tab…"), Some("win.rename"));
    let remove_name =
        gio::MenuItem::new(Some("R_emove Tab Name"), Some("win.remove-active-tab-name"));
    remove_name.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    tab.append_item(&remove_name);
    menu.append_section(None, &tab);
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
