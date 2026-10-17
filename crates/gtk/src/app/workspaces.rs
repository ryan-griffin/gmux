use super::management::{error_label, show_error};
use super::*;

pub(super) struct WorkspaceControls {
    dialog: adw::Dialog,
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,
    definitions: RefCell<Vec<model::WorkspaceDefinition>>,
}
impl WorkspaceControls {
    fn refresh(&self, ui: &Rc<Ui>) {
        let catalog = ui.catalog.borrow();
        if *self.definitions.borrow() == catalog.workspaces {
            return;
        }
        let definitions = catalog.workspaces.clone();
        drop(catalog);
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        for workspace in &definitions {
            let row = adw::ActionRow::builder()
                .title(&workspace.name)
                .subtitle(match workspace.directories.len() {
                    0 => "Home Directories".into(),
                    1 => "1 Starting Directory".into(),
                    count => format!("{count} Starting Directories"),
                })
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
            let id = workspace.id;
            let weak = Rc::downgrade(ui);
            row.connect_activated(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.workspace_editor(Some(id));
                }
            });
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
        }
        *self.definitions.borrow_mut() = definitions;
    }
}

impl Ui {
    pub(super) fn tab_is_visible(&self, tab: &Tab) -> bool {
        if !self.filter.get().matches(tab.workspace) {
            return false;
        }
        let query = self.sidebar_search.text().to_lowercase();
        if query.trim().is_empty() {
            return true;
        }
        let directory = tab.panes.get(&tab.active).map(|pane| pane.directory());
        let searchable = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            tab.name,
            tab.workspace_label.text(),
            directory
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            directory
                .as_ref()
                .map(|path| self.display_directory(tab.active, path))
                .unwrap_or_default(),
            tab.branch_label.text(),
            self.sessions.borrow().connection_name(tab.id),
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| searchable.contains(word))
    }
    pub(super) fn refresh_sidebar(&self) {
        let changing = self.changing.replace(true);
        for tab in self.tabs.borrow().iter() {
            let visible = self.tab_is_visible(tab);
            tab.row.set_visible(visible);
        }
        self.changing.set(changing);
        let searching = !self.sidebar_search.text().trim().is_empty();
        self.sidebar_placeholder.set_title(if searching {
            "No Matching Tabs"
        } else {
            "No Tabs"
        });
    }
    pub(super) fn select_search_result(self: &Rc<Self>) {
        let id = {
            let tabs = self.tabs.borrow();
            tabs.iter()
                .filter(|tab| self.tab_is_visible(tab))
                .find(|tab| tab.id == self.active.get())
                .or_else(|| tabs.iter().find(|tab| self.tab_is_visible(tab)))
                .map(|tab| tab.id)
        };
        if let Some(id) = id {
            self.switch(id);
        }
    }
    pub(super) fn selected_workspace(&self) -> Option<u64> {
        match self.filter.get() {
            TabFilter::Workspace(id) => Some(id),
            _ => None,
        }
    }
    pub(super) fn apply_filter(self: &Rc<Self>, filter: TabFilter) {
        let filter = match filter {
            TabFilter::Workspace(id)
                if !self.catalog.borrow().workspaces.iter().any(|w| w.id == id) =>
            {
                TabFilter::All
            }
            filter => filter,
        };
        self.filter.set(filter);
        let title = match filter {
            TabFilter::All => "All Tabs".into(),
            TabFilter::Workspace(id) => self
                .catalog
                .borrow()
                .workspaces
                .iter()
                .find(|w| w.id == id)
                .unwrap()
                .name
                .clone(),
        };
        self.filter_title.set_title(&title);
        let target = match filter {
            TabFilter::All => "all".into(),
            TabFilter::Workspace(id) => id.to_string(),
        };
        if let Some(action) = self
            .window
            .lookup_action("workspace-filter")
            .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
        {
            action.set_state(&target.to_variant());
        }
        self.refresh_empty();
        self.refresh_sidebar();
        self.mark_dirty();
    }
    pub(super) fn refresh_empty(&self) {
        let (connected, connecting, configured) = {
            let sessions = self.sessions.borrow();
            let connections = sessions.connections();
            (
                connections
                    .iter()
                    .any(|connection| sessions.connected(connection.daemon)),
                connections.iter().any(|connection| {
                    matches!(
                        sessions.connection_status(connection.daemon),
                        connections::ConnectionStatus::Connecting
                    )
                }),
                !connections.is_empty(),
            )
        };
        self.empty_actions
            .set_visible_child_name(if connected { "new" } else { "connections" });
        if !connected {
            let (title, description) = if connecting {
                (
                    "Connecting…",
                    "Open Manage Connections to check connection status",
                )
            } else if configured {
                (
                    "Not Connected",
                    "Open Manage Connections to start the local daemon or reconnect to a remote daemon",
                )
            } else {
                (
                    "No Connections",
                    "Enable the Local Daemon or add a remote connection in Manage Connections",
                )
            };
            self.empty.set_title(title);
            self.empty.set_description(Some(description));
        } else {
            let title = self.filter_title.title();
            let workspace = matches!(self.filter.get(), TabFilter::Workspace(_));
            self.empty.set_title(&if workspace {
                format!("No Tabs in {title}")
            } else {
                "No Tabs".into()
            });
            self.empty.set_description(Some(&if workspace {
                format!("Open a tab to start a terminal in {title}")
            } else {
                "Open a tab to start a terminal".into()
            }));
        }
    }
    pub(super) fn set_filter(self: &Rc<Self>, filter: TabFilter) {
        self.apply_filter(filter);
        let selected = {
            let tabs = self.tabs.borrow();
            tabs.iter()
                .filter(|tab| self.tab_is_visible(tab))
                .find(|tab| tab.id == self.active.get())
                .or_else(|| tabs.iter().find(|tab| self.tab_is_visible(tab)))
                .map(|tab| tab.id)
        };
        if let Some(id) = selected {
            self.switch(id);
        } else {
            self.show_empty();
        }
        self.refresh_metadata();
    }
    pub(super) fn refresh_workspace_ui(self: &Rc<Self>) {
        if let Some(controls) = self.workspaces_dialog.borrow().as_ref() {
            controls.refresh(self);
        }
        let catalog = self.catalog.borrow();
        let items: Vec<_> = catalog
            .workspaces
            .iter()
            .map(|w| (w.id, w.name.clone()))
            .collect();
        let catalog_changed = self.workspace_menu_items.borrow().as_ref() != Some(&items);
        if catalog_changed {
            let menu = gio::Menu::new();
            let filters = gio::Menu::new();
            let all = gio::MenuItem::new(Some("_All Tabs"), None);
            all.set_action_and_target_value(
                Some("win.workspace-filter"),
                Some(&"all".to_variant()),
            );
            filters.append_item(&all);
            menu.append_section(None, &filters);
            let mut mnemonics = HashSet::from(['a', 'w', 'm']);
            let workspaces = gio::Menu::new();
            for workspace in &catalog.workspaces {
                let label = crate::menu::mnemonic_label(&workspace.name, &mut mnemonics);
                let item = gio::MenuItem::new(Some(&label), None);
                item.set_action_and_target_value(
                    Some("win.workspace-filter"),
                    Some(&workspace.id.to_string().to_variant()),
                );
                workspaces.append_item(&item);
            }
            if !catalog.workspaces.is_empty() {
                menu.append_section(Some("Workspaces"), &workspaces);
            }
            let manage = gio::Menu::new();
            manage.append(Some("Add _Workspace…"), Some("win.new-workspace"));
            manage.append(Some("_Manage Workspaces…"), Some("win.manage-workspaces"));
            menu.append_section(None, &manage);
            self.workspace_filter.set_menu_model(Some(&menu));
            *self.workspace_menu_items.borrow_mut() = Some(items);
        }
        let changing = self.changing.replace(true);
        for tab in self.tabs.borrow_mut().iter_mut() {
            let state = (tab.custom_name, tab.workspace);
            if !catalog_changed && tab.menu_state == Some(state) {
                continue;
            }
            let workspace_name = tab
                .workspace
                .and_then(|id| {
                    catalog
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)
                })
                .map(|workspace| workspace.name.as_str());
            if let Some(name) = workspace_name {
                tab.workspace_label.set_text(name);
            } else {
                tab.workspace_label.set_text("");
            }
            let menu = gio::Menu::new();
            let rename = gio::MenuItem::new(Some("_Rename Tab…"), None);
            rename.set_action_and_target_value(Some("win.rename-tab"), Some(&tab.id.to_variant()));
            menu.append_item(&rename);
            if tab.custom_name {
                let remove_name = gio::MenuItem::new(Some("Use _Automatic Tab Name"), None);
                remove_name.set_action_and_target_value(
                    Some("win.reset-name"),
                    Some(&tab.id.to_variant()),
                );
                menu.append_item(&remove_name);
            }
            let membership = gio::Menu::new();
            let destinations = gio::Menu::new();
            let mut mnemonics = HashSet::from(['m']);
            for workspace in catalog
                .workspaces
                .iter()
                .filter(|workspace| Some(workspace.id) != tab.workspace)
            {
                let label = crate::menu::mnemonic_label(&workspace.name, &mut mnemonics);
                let item = gio::MenuItem::new(Some(&label), None);
                item.set_action_and_target_value(
                    Some("win.assign-tab"),
                    Some(&(tab.id, workspace.id).to_variant()),
                );
                destinations.append_item(&item);
            }
            if destinations.n_items() > 0 {
                membership.append_submenu(Some("_Move to Workspace"), &destinations);
            } else {
                membership.append(Some("_Move to Workspace"), Some("tab.move-unavailable"));
            }
            let create = gio::MenuItem::new(Some("Move to _New Workspace…"), None);
            create.set_action_and_target_value(
                Some("win.new-workspace-for-tab"),
                Some(&tab.id.to_variant()),
            );
            membership.append_item(&create);
            if tab.workspace.is_some() {
                let unassign = gio::MenuItem::new(Some("Remove _from Workspace"), None);
                unassign.set_action_and_target_value(
                    Some("win.assign-tab"),
                    Some(&(tab.id, 0u64).to_variant()),
                );
                membership.append_item(&unassign);
            }
            menu.append_section(None, &membership);
            let closing = gio::Menu::new();
            let close = gio::MenuItem::new(Some("_Close Tab"), None);
            close.set_action_and_target_value(Some("win.close-tab"), Some(&tab.id.to_variant()));
            closing.append_item(&close);
            menu.append_section(None, &closing);
            tab.menu.set_menu_model(Some(&menu));
            tab.menu_state = Some(state);
        }
        self.changing.set(changing);
        self.apply_filter(self.filter.get());
    }
    pub(super) fn assign_tab(self: &Rc<Self>, id: u64, workspace: Option<u64>) {
        self.command(
            Command::Move {
                tab: Some(id),
                workspace,
            },
            |ui, result| {
                if let Err(e) = result {
                    ui.message(&e.to_string());
                }
            },
        );
    }
    pub(super) fn workspace_editor(self: &Rc<Self>, id: Option<u64>) {
        self.workspace_editor_with_tab(id, None);
    }
    pub(super) fn new_workspace_for_tab(self: &Rc<Self>, tab: u64) {
        if self.tabs.borrow().iter().any(|current| current.id == tab) {
            self.workspace_editor_with_tab(None, Some(tab));
        }
    }
    fn workspace_editor_with_tab(self: &Rc<Self>, id: Option<u64>, tab: Option<u64>) {
        let existing = id.and_then(|id| {
            self.catalog
                .borrow()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .cloned()
        });
        let tab_directory = tab.and_then(|id| {
            self.tabs
                .borrow()
                .iter()
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.panes.get(&tab.active))
                .map(|pane| (self.sessions.borrow().tab_ref(id).daemon, pane.directory()))
        });
        let Some(dialog) = self.workspace_editor_dialog.create(
            if existing.is_some() {
                "Edit Workspace"
            } else {
                "Add Workspace"
            },
            &self.window,
        ) else {
            return;
        };
        let view = adw::ToolbarView::new();
        let (header, save) =
            management::editor_header(&dialog, if existing.is_some() { "Save" } else { "Add" });
        view.add_top_bar(&header);
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        if tab.is_some() {
            group.set_description(Some("This tab will move to the new workspace"));
        }
        let initial_name = existing
            .as_ref()
            .map(|workspace| workspace.name.clone())
            .or_else(|| {
                tab_directory
                    .as_ref()
                    .map(|(_, path)| model::directory_name(path))
            })
            .unwrap_or_default();
        let name = adw::EntryRow::builder()
            .title("Name")
            .text(initial_name)
            .activates_default(true)
            .build();
        group.add(&name);
        page.add(&group);
        let local = self.sessions.borrow().local_daemon();
        let mut directories = existing
            .as_ref()
            .map(|workspace| workspace.directories.clone())
            .unwrap_or_default();
        if let Some((machine, path)) = tab_directory {
            directories.insert(machine, path);
        }
        let initial_directories = directories;
        let folders = adw::PreferencesGroup::builder()
            .title("Starting Directories")
            .build();
        let mut fields = Vec::new();
        for machine in self.sessions.borrow().connections() {
            let directory = Rc::new(RefCell::new(
                initial_directories
                    .get(&machine.daemon)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from("~")),
            ));
            let text = self
                .sessions
                .borrow()
                .display_directory(machine.daemon, &directory.borrow());
            let folder = adw::EntryRow::builder()
                .title(format!("Directory on {}", machine.name))
                .text(text)
                .activates_default(true)
                .build();
            let selected = directory.clone();
            folder.connect_changed(move |row| {
                *selected.borrow_mut() = PathBuf::from(row.text().trim());
            });
            let reset = gtk::Button::builder()
                .icon_name("edit-undo-symbolic")
                .tooltip_text("Use Home Directory")
                .valign(gtk::Align::Center)
                .build();
            reset.add_css_class("flat");
            reset.set_sensitive(folder.text().trim() != "~");
            reset.update_property(&[gtk::accessible::Property::Label("Use Home Directory")]);
            folder.add_suffix(&reset);
            let weak_reset = reset.downgrade();
            folder.connect_changed(move |row| {
                if let Some(reset) = weak_reset.upgrade() {
                    reset.set_sensitive(row.text().trim() != "~");
                }
            });
            let entry = folder.downgrade();
            reset.connect_clicked(move |_| {
                if let Some(entry) = entry.upgrade() {
                    entry.set_text("~");
                }
            });
            if Some(machine.daemon) == local {
                let browse = gtk::Button::builder()
                    .icon_name("folder-open-symbolic")
                    .tooltip_text("Choose Directory")
                    .valign(gtk::Align::Center)
                    .build();
                browse.add_css_class("flat");
                browse.update_property(&[gtk::accessible::Property::Label("Choose Directory")]);
                folder.add_suffix(&browse);
                let parent = self.window.downgrade();
                let selected = directory.clone();
                let name_entry = name.downgrade();
                let directory_entry = folder.downgrade();
                browse.connect_clicked(move |_| {
                    let Some(parent) = parent.upgrade() else {
                        return;
                    };
                    let path = selected.borrow();
                    let initial_folder = if path.as_os_str().is_empty() {
                        model::home()
                    } else {
                        model::expand_home(&path)
                    };
                    drop(path);
                    let chooser = gtk::FileDialog::builder()
                        .title("Choose Directory")
                        .initial_folder(&gio::File::for_path(initial_folder))
                        .build();
                    let row = directory_entry.clone();
                    let directory = selected.clone();
                    let name = name_entry.clone();
                    chooser.select_folder(
                        Some(&parent),
                        None::<&gio::Cancellable>,
                        move |result| {
                            if let Ok(file) = result
                                && let Some(path) = file.path()
                            {
                                if let Some(row) = row.upgrade() {
                                    row.set_text(&model::display_path(&path));
                                }
                                if let Some(name) = name.upgrade()
                                    && name.text().trim().is_empty()
                                {
                                    name.set_text(&model::directory_name(&path));
                                }
                                *directory.borrow_mut() = path;
                            }
                        },
                    );
                });
            }
            folders.add(&folder);
            fields.push((machine.daemon, directory, folder));
        }
        let error = error_label();
        if fields.is_empty() {
            group.add(&error);
        } else {
            folders.add(&error);
            page.add(&folders);
        }
        let remove = existing
            .as_ref()
            .map(|_| management::remove_action(&page, "Remove Workspace…"));
        let validation_fields: Vec<_> = fields.iter().map(|(_, _, row)| row.downgrade()).collect();
        let weak_name = name.downgrade();
        let weak_save = save.downgrade();
        let validation_error = error.downgrade();
        let validate = Rc::new(move || {
            if let (Some(name), Some(button)) = (weak_name.upgrade(), weak_save.upgrade()) {
                button.set_sensitive(
                    !name.text().trim().is_empty()
                        && validation_fields.iter().all(|row| {
                            row.upgrade()
                                .is_some_and(|row| !row.text().trim().is_empty())
                        }),
                );
            }
            if let Some(error) = validation_error.upgrade() {
                error.set_visible(false);
            }
        });
        for row in std::iter::once(&name).chain(fields.iter().map(|(_, _, row)| row)) {
            let validate = validate.clone();
            row.connect_changed(move |_| validate());
        }
        validate();
        if let (Some(remove), Some(existing)) = (remove, existing.as_ref()) {
            let weak = Rc::downgrade(self);
            let close = dialog.downgrade();
            let view = view.downgrade();
            let error = error.downgrade();
            let id = existing.id;
            let name = existing.name.clone();
            remove.connect_activated(move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let confirmation = adw::AlertDialog::builder()
                    .heading(format!("Remove {name}?"))
                    .body("Tabs will stay open without a workspace")
                    .build();
                confirmation.add_response("cancel", "Cancel");
                confirmation.add_response("remove", "Remove Workspace");
                confirmation
                    .set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                confirmation.set_default_response(Some("cancel"));
                confirmation.set_close_response("cancel");
                let weak = weak.clone();
                let close = close.clone();
                let view = view.clone();
                let error = error.clone();
                confirmation.connect_response(Some("remove"), move |_, _| {
                    let (Some(ui), Some(dialog), Some(view)) =
                        (weak.upgrade(), close.upgrade(), view.upgrade())
                    else {
                        return;
                    };
                    if let Some(error) = error.upgrade() {
                        error.set_visible(false);
                    }
                    dialog.set_can_close(false);
                    view.set_sensitive(false);
                    let close = close.clone();
                    let view = view.downgrade();
                    let error = error.clone();
                    ui.command(
                        Command::WorkspaceRemove { workspace: id },
                        move |_, result| {
                            if let Some(dialog) = close.upgrade() {
                                dialog.set_can_close(true);
                            }
                            if let Some(view) = view.upgrade() {
                                view.set_sensitive(true);
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
                        },
                    );
                });
                confirmation.present(Some(&ui.window));
            });
        }
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        let initial_focus = name.downgrade();
        let default_button = save.downgrade();
        let weak = Rc::downgrade(self);
        let editor = management::EditorRequest::new(&dialog, &view, &save, &error);
        let fields: Vec<_> = fields
            .into_iter()
            .map(|(machine, path, _)| (machine, path))
            .collect();
        save.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut directories = initial_directories.clone();
            for (machine, path) in &fields {
                let path = path.borrow();
                if *path == std::path::Path::new("~") {
                    directories.remove(machine);
                } else {
                    let path = if Some(*machine) == local {
                        match std::path::absolute(model::expand_home(&path)) {
                            Ok(path) => path,
                            Err(message) => {
                                show_error(&error, message);
                                return;
                            }
                        }
                    } else {
                        path.clone()
                    };
                    directories.insert(*machine, path);
                }
            }
            let command = if let Some(id) = id {
                Command::WorkspaceEdit {
                    workspace: id,
                    name: name.text().to_string(),
                    directory: None,
                    directories: Some(directories),
                }
            } else {
                Command::WorkspaceAdd {
                    name: name.text().to_string(),
                    directory: None,
                    directories,
                    tab,
                }
            };
            editor.begin(None);
            let editor = editor.clone();
            ui.command(command, move |_, result| editor.finish(result.map(|_| ())));
        });
        dialog.present(Some(&self.window));
        if let Some(button) = default_button.upgrade() {
            dialog.set_default_widget(Some(&button));
        }
        if let Some(name) = initial_focus.upgrade() {
            dialog.set_focus(Some(&name));
        }
    }
    pub(super) fn manage_workspaces(self: &Rc<Self>) {
        if self.workspace_editor_dialog.present(&self.window) {
            return;
        }
        if let Some(controls) = self.workspaces_dialog.borrow().as_ref() {
            controls.dialog.present(Some(&self.window));
            return;
        }
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let add = adw::ButtonRow::builder()
            .title("Add Workspace")
            .start_icon_name("list-add-symbolic")
            .build();
        group.add(&add);
        page.add(&group);
        view.set_content(Some(&page));
        let dialog = management::list_dialog("Workspaces", &view, &add);
        let controls = Rc::new(WorkspaceControls {
            dialog,
            group,
            rows: RefCell::new(Vec::new()),
            definitions: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(self);
        add.connect_activated(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.workspace_editor(None);
            }
        });
        let weak = Rc::downgrade(self);
        controls.dialog.connect_closed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.workspaces_dialog.borrow_mut().take();
            }
        });
        controls.refresh(self);
        controls.dialog.present(Some(&self.window));
        *self.workspaces_dialog.borrow_mut() = Some(controls);
    }
    pub(super) fn request_close_tab(self: &Rc<Self>, id: u64) {
        let panes: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.panes.keys().copied().collect())
            .unwrap_or_default();
        if panes.is_empty() {
            return;
        }
        self.request(
            InternalRequest::Busy { panes },
            move |ui, result| match result {
                Ok(value) => ui.confirm_close_tab(id, value["busy"].as_bool().unwrap_or(true)),
                Err(e) => ui.message(&e.to_string()),
            },
        );
    }
    fn confirm_close_tab(self: &Rc<Self>, id: u64, busy: bool) {
        let close = move |ui: &Rc<Self>| {
            ui.command(Command::CloseTab { tab: id }, |ui, result| {
                if let Err(e) = result {
                    ui.message(&e.to_string());
                }
            });
        };
        if !busy {
            close(self);
            return;
        }
        let dialog = adw::AlertDialog::builder()
            .heading("Close Tab?")
            .body("Closing this tab will stop the processes in all its panes")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close Tab");
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(Some("close"), move |_, _| {
            if let Some(ui) = weak.upgrade() {
                close(&ui);
            }
        });
        dialog.present(Some(&self.window));
    }
}
