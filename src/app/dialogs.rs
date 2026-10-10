//! Commands and dialogs for tab and application lifecycle.
use super::*;

impl Ui {
    pub(super) fn request_close(self: &Rc<Self>, id: u64) {
        self.request(
            InternalRequest::Busy { panes: vec![id] },
            move |ui, result| match result {
                Ok(value) => ui.confirm_close(id, value["busy"].as_bool().unwrap_or(true)),
                Err(e) => ui.message(&e.to_string()),
            },
        );
    }
    pub(super) fn confirm_close(self: &Rc<Self>, id: u64, busy: bool) {
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
    pub(super) fn request_quit(self: &Rc<Self>) {
        let panes: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .flat_map(|t| t.panes.keys().copied())
            .collect();
        self.request(InternalRequest::Busy { panes }, |ui, result| match result {
            Ok(value) => ui.confirm_quit(value["busy"].as_bool().unwrap_or(true)),
            Err(e) => ui.message(&e.to_string()),
        });
    }
    pub(super) fn confirm_quit(self: &Rc<Self>, busy: bool) {
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
    pub(super) fn stop_daemon(self: &Rc<Self>) {
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
    pub(super) fn detach_frontend(self: &Rc<Self>) {
        if self.detaching.replace(true) {
            return;
        }
        self.save_session();
        self.request(InternalRequest::Hello, |ui, result| {
            if let Err(error) = result {
                eprintln!("Could not finish saving layout: {error}");
            }
            ui.close_window();
        });
    }
    pub(super) fn close_window(&self) {
        self.sessions.borrow_mut().clear();
        self.window.destroy();
        if let Some(owner) = self.owner.upgrade() {
            owner.windows.borrow_mut().retain(|ui| ui.id != self.id);
        }
    }
    pub(super) fn open_folder(self: &Rc<Self>) {
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
    pub(super) fn rename_tab(self: &Rc<Self>, id: u64) {
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
    pub(super) fn palette(self: &Rc<Self>) {
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
            let shortcut_action = if action == "close-window" {
                "close"
            } else {
                action
            };
            if let Some(shortcut) = self
                .app
                .accels_for_action(&format!("win.{shortcut_action}"))
                .first()
            {
                let key = adw::ShortcutLabel::new(shortcut);
                key.set_valign(gtk::Align::Center);
                if action == "close-window" {
                    // Close Window is always available, but it shares the
                    // pane shortcut only while the empty page is shown.
                    key.set_visible(false);
                    if let Some(empty_close) = self.window.lookup_action("close-empty-window") {
                        empty_close
                            .bind_property("enabled", &key, "visible")
                            .sync_create()
                            .build();
                    }
                }
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
    pub(super) fn shortcuts(&self) {
        let dialog = adw::ShortcutsDialog::new();
        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "Tabs",
                &[
                    ("New Tab", "new"),
                    ("New Tab in Directory…", "open-folder"),
                    ("Next Tab", "next-tab"),
                    ("Previous Tab", "previous-tab"),
                    ("Show or Hide Tabs", "sidebar"),
                ],
            ),
            (
                "Panes",
                &[
                    ("Split Right", "split-right"),
                    ("Split Down", "split-down"),
                    ("Close Pane", "close"),
                    ("Next Pane", "next-pane"),
                    ("Previous Pane", "previous-pane"),
                ],
            ),
            (
                "Terminal",
                &[
                    ("Copy Selection", "copy"),
                    ("Paste", "paste"),
                    ("Find in Terminal…", "find"),
                    ("Zoom In", "zoom-in"),
                    ("Zoom Out", "zoom-out"),
                    ("Reset Zoom", "zoom-reset"),
                ],
            ),
            (
                "Application",
                &[
                    ("New Window", "new-window"),
                    ("Main Menu", "main-menu"),
                    ("Commands…", "palette"),
                    ("Preferences", "preferences"),
                    ("Keyboard Shortcuts", "shortcuts"),
                    ("Toggle Fullscreen", "fullscreen"),
                    ("Quit Gmux", "quit"),
                ],
            ),
        ];
        for (title, actions) in sections {
            let section = adw::ShortcutsSection::new(Some(title));
            for (title, action) in *actions {
                let item = adw::ShortcutsItem::from_action(title, &format!("win.{action}"));
                if *action == "close" {
                    item.set_subtitle("Closes the window when the empty state is shown");
                }
                section.add(item);
            }
            dialog.add(section);
        }
        dialog.present(Some(&self.window));
    }
    pub(super) fn about(&self) {
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
}

fn announce_command(search: &gtk::SearchEntry, row: &gtk::ListBoxRow) {
    if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
        search.announce(&row.title(), gtk::AccessibleAnnouncementPriority::Medium);
    }
}
