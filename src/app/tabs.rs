//! Tabs, pane layout, and focus.
use super::*;

impl Ui {
    pub(super) fn terminal(self: &Rc<Self>, id: u64) -> anyhow::Result<Rc<Terminal>> {
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
            self.keyboard.clone(),
        ))
    }
    pub(super) fn new_tab(self: &Rc<Self>, directory: PathBuf, name: Option<String>) {
        self.create_tab(directory, name, self.selected_workspace())
    }
    pub(super) fn create_tab(
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
    pub(super) fn restore_tab(
        self: &Rc<Self>,
        saved: SavedTab,
        prepend: bool,
    ) -> anyhow::Result<()> {
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
                branch_requested: false,
            },
        );
        self.changing.set(false);
        self.rebuild(saved.id);
        self.refresh_workspace_ui();
        self.refresh_tab_title(saved.id);
        self.refresh_metadata();
        self.mark_dirty();
        Ok(())
    }
    pub(super) fn rebuild(self: &Rc<Self>, tab: u64) {
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
    pub(super) fn switch(self: &Rc<Self>, id: u64) {
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
        self.sync_focus();
        self.mark_dirty();
    }
    pub(super) fn sync_focus(&self) {
        let focus = (self.active.get(), self.active_pane().map(|pane| pane.id));
        if focus != self.synced_focus.get() {
            match self.backend.focus(focus.0, focus.1) {
                Ok(()) => self.synced_focus.set(focus),
                Err(error) => eprintln!("Could not sync terminal focus: {error}"),
            }
        }
    }
    pub(super) fn update_tab_actions(&self, enabled: bool) {
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
    pub(super) fn show_empty(self: &Rc<Self>) {
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
        self.sync_focus();
        self.mark_dirty();
    }
    pub(super) fn subscribe_visible(&self) {
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
    pub(super) fn focus_pane(self: &Rc<Self>, id: u64) {
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
        self.refresh_metadata();
        if self.active.get() != tab {
            self.switch(tab);
        }
        self.mark_read(id);
        self.sync_focus();
        self.mark_dirty();
        self.refresh_search();
    }
    pub(super) fn active_pane(&self) -> Option<Rc<Terminal>> {
        self.tabs
            .borrow()
            .iter()
            .find(|w| w.id == self.active.get())
            .and_then(|w| w.panes.get(&w.active).cloned())
    }
    pub(super) fn pane(&self, id: Option<u64>) -> anyhow::Result<Rc<Terminal>> {
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
    pub(super) fn split_pane(self: &Rc<Self>, id: u64, down: bool) {
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
    pub(super) fn close_pane(self: &Rc<Self>, id: u64) -> anyhow::Result<()> {
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
        self.mark_dirty();
        Ok(())
    }
    pub(super) fn refresh_tab_title(self: &Rc<Self>, id: u64) {
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
        self.mark_dirty();
        drop(tabs);
        self.refresh_sidebar();
    }
    pub(super) fn refresh_metadata(self: &Rc<Self>) {
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
                    ui.mark_dirty();
                }
            });
            paned.upcast()
        }
    }
}
