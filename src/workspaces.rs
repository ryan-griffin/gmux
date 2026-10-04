use super::*;

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
            "{}\n{}\n{}\n{}\n{}",
            tab.name,
            tab.workspace_label.text(),
            directory
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            directory
                .as_ref()
                .map(|path| model::display_path(path))
                .unwrap_or_default(),
            if tab.branch_box.get_visible() {
                tab.branch_label.text().to_string()
            } else {
                String::new()
            },
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
    pub(super) fn default_directory(&self) -> PathBuf {
        self.selected_workspace()
            .and_then(|id| {
                self.catalog
                    .borrow()
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == id)
                    .map(|workspace| workspace.directory.clone())
            })
            .unwrap_or_else(model::home)
    }
    pub(super) fn apply_filter(&self, filter: TabFilter) {
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
            TabFilter::Unassigned => "No Workspace".into(),
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
            TabFilter::Unassigned => "unassigned".into(),
            TabFilter::Workspace(id) => id.to_string(),
        };
        if let Some(action) = self
            .window
            .lookup_action("workspace-filter")
            .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
        {
            action.set_state(&target.to_variant());
        }
        self.empty.set_title(&match filter {
            TabFilter::Workspace(_) => format!("No Tabs in {title}"),
            _ => "No Tabs".into(),
        });
        self.empty.set_description(Some(&match filter {
            TabFilter::Workspace(_) => format!("Open a tab to start a terminal in {title}."),
            _ => "Open a tab to start a terminal.".into(),
        }));
        self.refresh_sidebar();
        self.dirty.set(true);
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
    pub(super) fn refresh_workspace_ui(&self) {
        let catalog = self.catalog.borrow();
        let menu = gio::Menu::new();
        let filters = gio::Menu::new();
        for (label, target) in [("_All Tabs", "all"), ("_No Workspace", "unassigned")] {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(
                Some("win.workspace-filter"),
                Some(&target.to_variant()),
            );
            filters.append_item(&item);
        }
        menu.append_section(None, &filters);
        let workspaces = gio::Menu::new();
        let mut mnemonics = HashSet::from(['a', 'n', 'w', 'm']);
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
        let new_tabs = &self.new_tab_menu;
        new_tabs.remove_all();
        new_tabs.append(Some("_New Tab"), Some("win.new"));
        new_tabs.append(Some("New Tab in _Directory…"), Some("win.open-folder"));
        let destinations = gio::Menu::new();
        // Keep the submenu's back-button mnemonic available.
        let mut mnemonics = HashSet::from(['s']);
        for workspace in &catalog.workspaces {
            let label = crate::menu::mnemonic_label(&workspace.name, &mut mnemonics);
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(
                Some("win.new-tab-workspace"),
                Some(&workspace.id.to_variant()),
            );
            destinations.append_item(&item);
        }
        let has_workspaces = !catalog.workspaces.is_empty();
        if let Some(action) = self
            .window
            .lookup_action("new-tab-workspace")
            .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
        {
            action.set_enabled(has_workspaces);
        }
        if has_workspaces {
            new_tabs.append_submenu(Some("New Tab in Work_space"), &destinations);
        } else {
            let item = gio::MenuItem::new(Some("New Tab in Work_space"), None);
            item.set_action_and_target_value(
                Some("win.new-tab-workspace"),
                Some(&0u64.to_variant()),
            );
            new_tabs.append_item(&item);
        }
        let changing = self.changing.replace(true);
        for tab in self.tabs.borrow().iter() {
            let workspace_name = tab
                .workspace
                .and_then(|id| {
                    catalog
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)
                })
                .map(|workspace| workspace.name.as_str());
            tab.workspace_box.set_visible(workspace_name.is_some());
            if let Some(name) = workspace_name {
                tab.workspace_label.set_text(name);
                tab.workspace_label
                    .set_tooltip_text(Some(&format!("Workspace: {name}")));
            } else {
                tab.workspace_label.set_text("");
                tab.workspace_label.set_tooltip_text(None);
            }
            let menu = gio::Menu::new();
            let rename = gio::MenuItem::new(Some("_Rename Tab…"), None);
            rename.set_action_and_target_value(Some("win.rename-tab"), Some(&tab.id.to_variant()));
            menu.append_item(&rename);
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
            if tab.workspace.is_some() {
                let unassign = gio::MenuItem::new(Some("Remove _from Workspace"), None);
                unassign.set_action_and_target_value(
                    Some("win.assign-tab"),
                    Some(&(tab.id, 0u64).to_variant()),
                );
                membership.append_item(&unassign);
            }
            let create = gio::MenuItem::new(Some("Move to _New Workspace…"), None);
            create.set_action_and_target_value(
                Some("win.new-workspace-for-tab"),
                Some(&tab.id.to_variant()),
            );
            membership.append_item(&create);
            menu.append_section(None, &membership);
            let closing = gio::Menu::new();
            let close = gio::MenuItem::new(Some("_Close Tab"), None);
            close.set_action_and_target_value(Some("win.close-tab"), Some(&tab.id.to_variant()));
            closing.append_item(&close);
            menu.append_section(None, &closing);
            tab.menu.set_menu_model(Some(&menu));
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
                .map(|pane| pane.directory())
        });
        let dialog = adw::Dialog::builder()
            .title(if existing.is_some() {
                "Edit Workspace"
            } else {
                "Add Workspace"
            })
            .content_width(460)
            .content_height(340)
            .build();
        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label(if existing.is_some() { "Save" } else { "Add" });
        save.add_css_class("suggested-action");
        header.pack_start(&cancel);
        header.pack_end(&save);
        view.add_top_bar(&header);
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .description(if tab.is_some() {
                "This tab will move to the new workspace. New tabs start in its directory."
            } else {
                "New tabs in this workspace start in its directory."
            })
            .build();
        let initial_name = existing
            .as_ref()
            .map(|workspace| workspace.name.clone())
            .or_else(|| {
                tab_directory
                    .as_ref()
                    .map(|path| model::directory_name(path))
            })
            .unwrap_or_default();
        let name = adw::EntryRow::builder()
            .title("Name")
            .text(initial_name)
            .activates_default(true)
            .build();
        save.set_sensitive(!name.text().trim().is_empty());
        let button = save.downgrade();
        name.connect_changed(move |name| {
            if let Some(button) = button.upgrade() {
                button.set_sensitive(!name.text().trim().is_empty());
            }
        });
        let directory = Rc::new(RefCell::new(
            existing
                .as_ref()
                .map(|workspace| workspace.directory.clone())
                .or(tab_directory)
                .unwrap_or_else(model::home),
        ));
        let folder = adw::ActionRow::builder()
            .title("Directory")
            .subtitle(model::display_path(&directory.borrow()))
            .use_markup(false)
            .activatable(true)
            .build();
        folder.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        group.add(&name);
        group.add(&folder);
        let error = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        error.add_css_class("error");
        group.add(&error);
        page.add(&group);
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        dialog.set_default_widget(Some(&save));
        dialog.set_focus(Some(&name));
        let close = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(dialog) = close.upgrade() {
                dialog.close();
            }
        });
        let parent = self.window.downgrade();
        let selected = directory.clone();
        let name_entry = name.downgrade();
        folder.connect_activated(move |row| {
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let chooser = gtk::FileDialog::builder()
                .title("Workspace Directory")
                .initial_folder(&gio::File::for_path(&*selected.borrow()))
                .build();
            let row = row.downgrade();
            let directory = selected.clone();
            let name = name_entry.clone();
            chooser.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    if let Some(row) = row.upgrade() {
                        row.set_subtitle(&model::display_path(&path));
                    }
                    if let Some(name) = name.upgrade()
                        && name.text().trim().is_empty()
                    {
                        name.set_text(&model::directory_name(&path));
                    }
                    *directory.borrow_mut() = path;
                }
            });
        });
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        save.connect_clicked(move |button| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let command = if let Some(id) = id {
                Command::WorkspaceEdit {
                    workspace: id,
                    name: name.text().to_string(),
                    directory: directory.borrow().clone(),
                }
            } else {
                Command::WorkspaceNew {
                    name: name.text().to_string(),
                    directory: Some(directory.borrow().clone()),
                    tab,
                }
            };
            let error = error.clone();
            let close = close.clone();
            let button = button.clone();
            button.set_sensitive(false);
            ui.command(command, move |_, result| {
                button.set_sensitive(true);
                match result {
                    Ok(_) => {
                        if let Some(dialog) = close.upgrade() {
                            dialog.close();
                        }
                    }
                    Err(message) => {
                        error.set_text(&message.to_string());
                        error.set_visible(true);
                    }
                }
            });
        });
        dialog.present(Some(&self.window));
    }
    pub(super) fn manage_workspaces(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder()
            .title("Workspaces")
            .content_width(480)
            .content_height(420)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().description("Workspaces keep their names and directories after their tabs close. Removing a workspace keeps its tabs open without a workspace.").build();
        let add = adw::ButtonRow::builder()
            .title("Add Workspace…")
            .start_icon_name("list-add-symbolic")
            .build();
        group.add(&add);
        let weak = Rc::downgrade(self);
        let close = dialog.downgrade();
        add.connect_activated(move |_| {
            if let Some(ui) = weak.upgrade() {
                if let Some(dialog) = close.upgrade() {
                    dialog.close();
                }
                ui.workspace_editor(None);
            }
        });
        for workspace in &self.catalog.borrow().workspaces {
            let row = adw::ActionRow::builder()
                .title(&workspace.name)
                .subtitle(model::display_path(&workspace.directory))
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove workspace; keep its tabs open")
                .valign(gtk::Align::Center)
                .build();
            delete.add_css_class("flat");
            row.add_suffix(&delete);
            group.add(&row);
            let id = workspace.id;
            let weak = Rc::downgrade(self);
            let close = dialog.downgrade();
            row.connect_activated(move |_| {
                if let Some(ui) = weak.upgrade() {
                    if let Some(dialog) = close.upgrade() {
                        dialog.close();
                    }
                    ui.workspace_editor(Some(id));
                }
            });
            let weak = Rc::downgrade(self);
            let group = group.downgrade();
            let row = row.downgrade();
            delete.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    let Some(name) = ui
                        .catalog
                        .borrow()
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)
                        .map(|workspace| workspace.name.clone())
                    else {
                        return;
                    };
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Remove {name}?"))
                        .body("Tabs will stay open and move to No Workspace.")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("remove", "Remove Workspace");
                    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    let weak = Rc::downgrade(&ui);
                    let group = group.clone();
                    let row = row.clone();
                    dialog.connect_response(Some("remove"), move |_, _| {
                        if let Some(ui) = weak.upgrade() {
                            let group = group.clone();
                            let row = row.clone();
                            ui.command(
                                Command::WorkspaceDelete { workspace: id },
                                move |ui, result| match result {
                                    Err(error) => ui.message(&error.to_string()),
                                    Ok(_) => {
                                        if let (Some(group), Some(row)) =
                                            (group.upgrade(), row.upgrade())
                                        {
                                            group.remove(&row);
                                        }
                                    }
                                },
                            );
                        }
                    });
                    dialog.present(Some(&ui.window));
                }
            });
        }
        page.add(&group);
        view.set_content(Some(&page));
        dialog.set_child(Some(&view));
        dialog.set_focus(Some(&add));
        dialog.present(Some(&self.window));
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
            json!({"method":"_busy","panes":panes}),
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
            .heading("Close tab?")
            .body("Processes are still running in this tab. Closing it will stop them.")
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
