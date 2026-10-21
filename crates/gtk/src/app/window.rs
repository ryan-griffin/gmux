//! Window construction and widget setup.
use super::*;
use crate::client::control::BackendReply;

impl Ui {
    pub(super) fn new(
        app: &adw::Application,
        settings: Rc<RefCell<Settings>>,
        directory: Option<PathBuf>,
        owner: std::rc::Weak<Windows>,
        id: u64,
        preferred: Option<String>,
    ) -> anyhow::Result<Rc<Self>> {
        let wake = ipc::Wake::new()?;
        let store = owner
            .upgrade()
            .unwrap()
            .organization
            .borrow()
            .as_ref()
            .unwrap()
            .clone();
        let (sessions, initial) = connections::Connections::new(
            wake.clone(),
            store.clone(),
            settings.borrow().local_daemon_enabled,
            preferred,
        )?;
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
            .tooltip_text("Filter Tabs by Workspace")
            .build();
        workspace_filter.add_css_class("flat");
        crate::menu::setup_menu_button(&workspace_filter, "Tab Filter");
        sidebar_header.set_title_widget(Some(&workspace_filter));
        let connection_menu = connection_menu::ConnectionMenu::new();
        let sidebar_main_menu = main_menu_button(connection_menu.model());
        let new_tab_popover = connection_menu.dropdown();
        let empty_tab_popover = connection_menu.dropdown();
        let main_menu_popover = sidebar_main_menu
            .popover()
            .unwrap()
            .downcast::<gtk::PopoverMenu>()
            .unwrap();
        connection_menu.attach(&main_menu_popover);
        sidebar_header.pack_end(&sidebar_main_menu);
        let new = adw::SplitButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Tab (Ctrl+Shift+T)")
            .dropdown_tooltip("New Tab Options")
            .action_name("win.new")
            .popover(&new_tab_popover)
            .build();
        crate::menu::setup_split_button(&new, "New Tab Options");
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
            .tooltip_text("Search Tabs")
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
            .placeholder_text("Find in terminal")
            .hexpand(true)
            .build();
        search_entry.update_property(&[gtk::accessible::Property::Label("Find in terminal")]);
        let search_count = gtk::Label::builder()
            .visible(false)
            .tooltip_text("Current Match and Total Matches")
            .build();
        search_count.add_css_class("dimmed");
        search_count.add_css_class("numeric");
        let search_field = FindField::new(&search_entry, &search_count);
        search_box.append(&search_field);
        let search_previous = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Previous Match (Ctrl+Shift+G)")
            .sensitive(false)
            .build();
        let search_next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text("Next Match (Ctrl+G)")
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
            .description("Open a tab to start a terminal")
            .build();
        let open = adw::SplitButton::builder()
            .label("New Tab")
            .action_name("win.new")
            .popover(&empty_tab_popover)
            .tooltip_text("New Tab (Ctrl+Shift+T)")
            .dropdown_tooltip("New Tab Options")
            .halign(gtk::Align::Center)
            .build();
        open.add_css_class("suggested-action");
        open.add_css_class("empty-tab-button");
        crate::menu::setup_split_button(&open, "New Tab Options");
        let connections = gtk::Button::builder()
            .label("Manage Connections…")
            .action_name("win.manage-connections")
            .halign(gtk::Align::Center)
            .build();
        connections.add_css_class("pill");
        connections.add_css_class("suggested-action");
        let empty_actions = gtk::Stack::new();
        empty_actions.add_named(&open, Some("new"));
        empty_actions.add_named(&connections, Some("connections"));
        empty.set_child(Some(&empty_actions));
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
        let keyboard = Rc::new(RefCell::new(crate::terminal::KeyboardModifiers::default()));
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
            connections_dialog: RefCell::new(None),
            workspaces_dialog: RefCell::new(None),
            connection_editor_dialog: management::EditorDialog::default(),
            workspace_editor_dialog: management::EditorDialog::default(),
            tabs: RefCell::new(Vec::new()),
            sessions: RefCell::new(sessions),
            catalog: RefCell::new(catalog),
            filter: Cell::new(TabFilter::All),
            filter_title,
            workspace_filter,
            connection_menu,
            workspace_menu_items: RefCell::new(None),
            empty,
            empty_actions,
            initial_directory: RefCell::new(directory.map(|path| (path, None))),
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
            save_source: RefCell::new(None),
            owner,
            id,
            revision: Cell::new(initial.revision),
            restore_selection: Cell::new(None),
            detaching: Cell::new(false),
            backend: Rc::new(crate::client::backend::Backend::new(
                ipc::Endpoint::local(),
                wake.clone(),
                store,
                None,
            )),
            pending: RefCell::new(HashMap::new()),
            command_count: Cell::new(0),
            creation_serial: Cell::new(0),
            command_exits: RefCell::new(HashSet::new()),
            keyboard,
            update_source: RefCell::new(None),
            deadline_source: RefCell::new(None),
            theme_handler: RefCell::new(None),
            frame_clock_handler: RefCell::new(None),
        });
        let weak = Rc::downgrade(&ui);
        crate::terminal::KeyboardModifiers::install(
            &ui.window,
            ui.keyboard.clone(),
            move |active| {
                if let Some(ui) = weak.upgrade() {
                    for tab in ui.tabs.borrow().iter() {
                        for pane in tab.panes.values() {
                            pane.link_modifiers_changed(active);
                        }
                    }
                }
            },
        );
        let weak = Rc::downgrade(&ui);
        ui.window.connect_realize(move |window| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if let Some((clock, handler)) = ui.frame_clock_handler.borrow_mut().take() {
                clock.disconnect(handler);
            }
            if let Some(clock) = window.frame_clock() {
                let weak = Rc::downgrade(&ui);
                let handler = clock.connect_after_paint(move |clock| {
                    if let Some(ui) = weak.upgrade() {
                        let (micros, _) = clock.refresh_info(clock.frame_time());
                        ui.sessions
                            .borrow_mut()
                            .set_frame_interval(Duration::from_micros(
                                micros.clamp(1_000, 1_000_000) as u64,
                            ));
                    }
                });
                *ui.frame_clock_handler.borrow_mut() = Some((clock, handler));
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.connect_is_active_notify(move |window| {
            if let Some(ui) = weak.upgrade() {
                if let Err(error) = ui.sessions.borrow().set_focused(window.is_active()) {
                    ui.message(&error.to_string());
                }
                if window.is_active()
                    && let Some(pane) = ui.active_pane()
                {
                    ui.mark_read(pane.id);
                }
            }
        });
        ui.install_actions();
        ui.refresh_client_ui();
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
        if session.active != 0 && !ui.tabs.borrow().iter().any(|t| t.id == session.active) {
            ui.restore_selection
                .set(Some((session.active, ui.active.get(), ui.filter.get())));
        }
        let weak = Rc::downgrade(&ui);
        let source =
            glib_unix::unix_fd_add_local(wake.as_raw_fd(), glib::IOCondition::IN, move |_, _| {
                wake.drain();
                let Some(ui) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let replies = ui.backend.replies();
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
                let updates_git_changed = updates.values().any(|update| update.git_changed);
                for (id, mut update) in updates {
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
                    if let Some(error) = &mut update.error {
                        let sessions = ui.sessions.borrow();
                        if !sessions.is_local(id)
                            && !sessions.connected(sessions.pane_ref(id).daemon)
                        {
                            error.push_str(
                                if matches!(
                                    sessions.connection_status(sessions.pane_ref(id).daemon),
                                    connections::ConnectionStatus::Reconnecting { .. }
                                ) {
                                    ". Reconnecting…"
                                } else {
                                    ". Check Manage Connections"
                                },
                            );
                        }
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
                let status_changed = ui.sessions.borrow_mut().take_status_changed();
                if directory_changed || status_changed {
                    ui.refresh_metadata();
                }
                if status_changed {
                    ui.refresh_empty();
                    ui.refresh_connections_ui();
                    ui.connection_menu
                        .refresh_availability(&ui.sessions.borrow());
                    if let Some(owner) = ui.owner.upgrade() {
                        owner.recover_local();
                    }
                }
                if updates_git_changed {
                    ui.update_branches();
                }
                let state = ui.sessions.borrow_mut().take_state();
                if let Some(state) = state
                    && let Err(error) = ui.apply_shared(state)
                {
                    ui.message(&error.to_string());
                }
                ui.open_initial_directory();
                if ui.search.is_search_mode() {
                    ui.update_search_count();
                }
                if ui.command_count.get() == 0 && ui.pending.borrow().is_empty() {
                    let tabs = ui.tabs.borrow();
                    let retained = tabs
                        .iter()
                        .flat_map(|tab| std::iter::once(tab.id).chain(tab.layout.borrow().ids()))
                        .chain(ui.restore_selection.get().map(|(desired, _, _)| desired));
                    ui.sessions.borrow_mut().prune_handles(retained);
                }
                ui.schedule_deadlines(&wake);
                glib::ControlFlow::Continue
            });
        *ui.update_source.borrow_mut() = Some(source);
        ui.sessions.borrow_mut().connect_remotes();
        Ok(ui)
    }
    /// Wake for connection retries and coalesced scroll releases. Waiting
    /// creates no permanent worker; deadlines are rescheduled on every poll.
    fn schedule_deadlines(self: &Rc<Self>, wake: &ipc::Wake) {
        let next = {
            let sessions = self.sessions.borrow();
            sessions
                .next_retry()
                .into_iter()
                .chain(sessions.next_scroll())
                .min()
        };
        let mut pending = self.deadline_source.borrow_mut();
        if pending.as_ref().map(|(at, _)| *at) == next {
            return;
        }
        if let Some((_, source)) = pending.take() {
            source.remove();
        }
        if let Some(at) = next {
            let weak = Rc::downgrade(self);
            let wake = wake.clone();
            let source = glib::timeout_add_local_once(
                at.saturating_duration_since(std::time::Instant::now())
                    .max(Duration::from_millis(1)),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.deadline_source.borrow_mut().take();
                        wake.notify();
                    }
                },
            );
            *pending = Some((at, source));
        }
    }
    pub(super) fn open_initial_directory(self: &Rc<Self>) {
        let ready = {
            let sessions = self.sessions.borrow();
            sessions
                .default_daemon()
                .is_some_and(|id| sessions.connected(id))
        };
        if ready {
            let directory = self.initial_directory.borrow_mut().take();
            if let Some((directory, directory_base)) = directory {
                self.command(
                    Command::New {
                        directory: Some(directory),
                        directory_base,
                        name: None,
                        workspace: Some(self.selected_workspace()),
                    },
                    |ui, result| {
                        if let Err(error) = result {
                            ui.message(&error.to_string());
                        }
                    },
                );
            }
        }
    }

    pub(super) fn handle_control_on(
        self: &Rc<Self>,
        request: Request,
        reply: std::sync::mpsc::SyncSender<Value>,
        connection: Option<String>,
    ) {
        let ui = self;
        if matches!(request, Request::Public(PublicRequest::Quit)) {
            ui.save_session();
            let local_connected = {
                let sessions = ui.sessions.borrow();
                sessions
                    .local_daemon()
                    .is_some_and(|id| sessions.connected(id))
            };
            if local_connected {
                ui.request(json!({"method":"local-daemon-stop"}), move |ui, result| {
                    let success = result.is_ok();
                    let _ = reply.try_send(
                        result.unwrap_or_else(|error| json!({"error":error.to_string()})),
                    );
                    if success {
                        ui.app.quit();
                    }
                });
            } else {
                let _ = reply.try_send(json!({"ok":true}));
                ui.app.quit();
            }
            return;
        }
        if matches!(request, Request::Internal(InternalRequest::State)) {
            let snapshot = ui.sessions.borrow_mut().snapshot();
            let _ = reply.try_send(serde_json::to_value(snapshot).unwrap());
            return;
        }
        let action_command = if let Request::Public(PublicRequest::Action { name }) = &request {
            if ui
                .window
                .lookup_action(name)
                .is_some_and(|a| a.is_enabled())
            {
                match name.as_str() {
                    "new" => Some(Command::New {
                        directory_base: None,
                        directory: None,
                        name: None,
                        workspace: Some(ui.selected_workspace()),
                    }),
                    "split-right" | "split-down" => ui.active_pane().map(|pane| {
                        Command::Tab(TabCommand::Split {
                            pane: Some(pane.id),
                            down: name == "split-down",
                        })
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
            ui.command(command, move |_, result| {
                let response = result
                    .map(|_| json!({"ok":true}))
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                let _ = reply.try_send(response);
            });
            return;
        }
        if let Request::Internal(body) = request {
            ui.save_session();
            ui.request_reply(body, move |ui, result| {
                let result = result.and_then(|raw| {
                    if let BackendReply::Value(value) = raw {
                        return Ok(value);
                    }
                    let response = match &raw {
                        BackendReply::Execution { reply } => serde_json::to_value(reply)?,
                        BackendReply::Organization { result } => json!({"result":result}),
                        BackendReply::Value(_) => unreachable!(),
                    };
                    let state = ui.sessions.borrow_mut().project(raw)?;
                    ui.apply_backend(state, false, backend::Selection::capture(ui))?;
                    Ok(response)
                });
                let _ = reply.try_send(result.unwrap_or_else(|e| json!({"error":e.to_string()})));
            });
            return;
        }
        if let Request::Layout(mut command) = request {
            let active_pane = ui.active_pane().map(|pane| pane.id);
            let active_tab = (ui.active.get() != 0).then_some(ui.active.get());
            let destination = match ui.sessions.borrow().incoming_command(
                &mut command,
                connection.as_deref(),
                active_pane,
                active_tab,
            ) {
                Ok(destination) => destination,
                Err(error) => {
                    let _ = reply.try_send(json!({"error":error.to_string()}));
                    return;
                }
            };
            ui.command_on(command, destination, move |ui, result| {
                let response = result
                    .and_then(|result| ui.sessions.borrow().external_result(result, destination))
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                let _ = reply.try_send(response);
            });
            return;
        }
        if let Request::Public(PublicRequest::Pane(PaneRequest::Notify { pane, message })) = request
        {
            let target = pane
                .map(|id| ui.sessions.borrow().default_handle(id))
                .transpose()
                .and_then(|id| ui.pane(id));
            let pane = match target {
                Ok(pane) => pane,
                Err(error) => {
                    let _ = reply.try_send(json!({"error":error.to_string()}));
                    return;
                }
            };
            ui.request(
                json!({"method":"notify","pane":pane.id,"message":message}),
                move |_, result| {
                    let _ = reply.try_send(
                        result.unwrap_or_else(|error| json!({"error":error.to_string()})),
                    );
                },
            );
            return;
        }
        if let Request::Public(PublicRequest::Pane(PaneRequest::Read { pane })) = request {
            let pane = match pane
                .map(|id| ui.sessions.borrow().default_handle(id))
                .transpose()
                .and_then(|id| ui.pane(id))
            {
                Ok(pane) => pane,
                Err(error) => {
                    let _ = reply.try_send(json!({"error":error.to_string()}));
                    return;
                }
            };
            if ui.sessions.borrow().is_visible(pane.id) {
                let _ = reply.try_send(json!({"text":pane.text()}));
                return;
            }
            let body = InternalRequest::Read {
                pane: Some(pane.id),
            };
            ui.request(body, move |_, result| {
                let _ = reply.try_send(result.unwrap_or_else(|e| json!({"error":e.to_string()})));
            });
            return;
        }
        let response = match request {
            Request::Public(request) => ui.handle_request(request),
            _ => Err(anyhow::anyhow!("Request requires the daemon socket")),
        };
        let response = response.unwrap_or_else(|error| json!({"error":error.to_string()}));
        if ui.dirty.replace(false) {
            ui.save_session();
        }
        let _ = reply.try_send(response);
    }
}

fn icon_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .build()
}
fn main_menu_button(connections: &gio::Menu) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let fullscreen = gio::Menu::new();
    fullscreen.append(Some("_Fullscreen"), Some("win.fullscreen"));
    menu.append_section(None, &fullscreen);
    let creation = gio::Menu::new();
    creation.append(Some("_New Tab"), Some("win.new"));
    creation.append_submenu(Some("New Tab _via Connection"), connections);
    creation.append(Some("New _Window"), Some("win.new-window"));
    menu.append_section(None, &creation);
    let tools = gio::Menu::new();
    tools.append(Some("_Manage Workspaces…"), Some("win.manage-workspaces"));
    tools.append(Some("Manage _Connections…"), Some("win.manage-connections"));
    tools.append(Some("C_ommands…"), Some("win.palette"));
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
    let remove_name = gio::MenuItem::new(
        Some("Use _Automatic Tab Name"),
        Some("win.reset-active-tab-name"),
    );
    remove_name.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    tab.append_item(&remove_name);
    menu.append_section(None, &tab);
    let terminal = gio::Menu::new();
    terminal.append(Some("Split _Right"), Some("win.split-right"));
    terminal.append(Some("Split _Down"), Some("win.split-down"));
    terminal.append(Some("_Find in Terminal…"), Some("win.find"));
    terminal.append(Some("_Clear Screen"), Some("win.clear-screen"));
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
