//! Backend requests, state reconciliation, and presentation persistence.
use super::*;

impl Ui {
    pub(super) fn request(
        self: &Rc<Self>,
        body: impl serde::Serialize,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<Value>) + 'static,
    ) {
        match self.backend.request(body) {
            Ok(id) => {
                self.pending.borrow_mut().insert(id, Box::new(callback));
            }
            Err(error) => callback(self, Err(error)),
        }
    }
    pub(super) fn command(
        self: &Rc<Self>,
        command: Command,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<CommandResult>) + 'static,
    ) {
        let reveal = matches!(command, Command::New { .. } | Command::Split { .. });
        self.save_session();
        self.request(InternalRequest::Command { command }, move |ui, result| {
            let result = result.and_then(|value| {
                let reply: CommandReply = serde_json::from_value(value)?;
                let result = ui.apply_backend(reply, reveal)?;
                if let Some(warning) = result.warning.as_deref() {
                    ui.message(warning);
                }
                Ok(result)
            });
            callback(ui, result);
        });
    }
    pub(super) fn apply_backend(
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
    pub(super) fn mark_dirty(self: &Rc<Self>) {
        self.dirty.set(true);
        if self.save_source.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        *self.save_source.borrow_mut() = Some(glib::timeout_add_local_once(
            Duration::from_secs(2),
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.save_source.borrow_mut().take();
                    if ui.dirty.replace(false) {
                        ui.save_session();
                    }
                }
            },
        ));
    }
    pub(super) fn save_session(&self) {
        if let Some(source) = self.save_source.borrow_mut().take() {
            source.remove();
        }
        self.dirty.set(false);
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
        if let Err(e) = self.backend.save(session) {
            eprintln!("Could not save session: {e}");
        }
    }
    pub(super) fn handle_request(self: &Rc<Self>, request: Value) -> anyhow::Result<Value> {
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
    pub(super) fn capture_window(&self, path: &str) -> anyhow::Result<()> {
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
