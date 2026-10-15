//! Backend requests, state reconciliation, and presentation persistence.
use super::*;

mod reconcile;
pub(super) use reconcile::Selection;
use reconcile::{TabSelection, preserve_window_state, reconcile_selection, remove_exited_panes};

impl Selection {
    pub(super) fn capture(ui: &Ui) -> Self {
        Self {
            active: ui.active.get(),
            filter: ui.filter.get(),
            tabs: ui
                .tabs
                .borrow()
                .iter()
                .map(|tab| {
                    (
                        tab.id,
                        TabSelection {
                            active: tab.active,
                            workspace: tab.workspace,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl Ui {
    pub(super) fn request(
        self: &Rc<Self>,
        body: impl serde::Serialize,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<Value>) + 'static,
    ) {
        let routed = serde_json::to_value(body).and_then(|body| {
            self.sessions
                .borrow()
                .route_body(body)
                .map_err(serde::ser::Error::custom)
        });
        let routed = match routed {
            Ok(value) => value,
            Err(error) => {
                callback(self, Err(error.into()));
                return;
            }
        };
        self.submit_request(routed.0, routed.1, callback);
    }
    fn submit_request(
        self: &Rc<Self>,
        destination: Option<model::DaemonId>,
        body: Value,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<Value>) + 'static,
    ) {
        match self.backend.request(destination, body) {
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
        let destination = self.sessions.borrow().default_daemon();
        self.command_on(command, destination, callback);
    }
    pub(super) fn command_on(
        self: &Rc<Self>,
        mut command: Command,
        destination: Option<model::DaemonId>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<CommandResult>) + 'static,
    ) {
        if matches!(
            &command,
            Command::Split { pane: None, .. } | Command::Close { pane: None }
        ) && self.active_pane().is_none()
        {
            callback(self, Err(anyhow::anyhow!("No active terminal")));
            return;
        }
        // Resolve implicit targets in this window before crossing the daemon boundary.
        match &mut command {
            Command::New { workspace, .. } if workspace.is_none() => {
                *workspace = Some(self.selected_workspace());
            }
            Command::Split { pane, .. } | Command::Close { pane } if pane.is_none() => {
                *pane = self.active_pane().map(|pane| pane.id);
            }
            Command::Move { tab, .. } if tab.is_none() => {
                *tab = Some(self.active.get());
            }
            _ => (),
        }
        let reveal = matches!(command, Command::New { .. } | Command::Split { .. });
        if let Command::Move { tab, workspace } = command {
            let id = tab.unwrap_or(self.active.get());
            let session = self.view_session();
            let Some(saved) = session.tabs.iter().find(|t| t.id == id) else {
                callback(self, Err(anyhow::anyhow!("Tab does not exist")));
                return;
            };
            let tab = self.sessions.borrow().desktop_tab(saved, workspace);
            self.request(InternalRequest::DesktopTab { tab }, move |ui, result| {
                let result = result.and_then(|v| {
                    let raw = serde_json::from_value(v)?;
                    let reply = ui.sessions.borrow_mut().project(raw)?;
                    ui.apply_backend(reply, false, Selection::capture(ui))
                });
                callback(ui, result);
            });
            return;
        }
        let route = match self.sessions.borrow().route_command(command, destination) {
            Ok(route) => route,
            Err(error) => {
                callback(self, Err(error));
                return;
            }
        };
        let reveal = reveal.then(|| {
            let serial = self.creation_serial.get() + 1;
            self.creation_serial.set(serial);
            serial
        });
        let selection = Selection::capture(self);
        self.save_session();
        self.command_count.set(self.command_count.get() + 1);
        self.submit_request(route.destination, route.body, move |ui, result| {
            let projected = result.and_then(|value| {
                let raw = serde_json::from_value(value)?;
                ui.sessions.borrow_mut().project(raw)
            });
            let reply = match projected {
                Ok(reply) => reply,
                Err(error) => {
                    ui.complete_command();
                    callback(ui, Err(error));
                    return;
                }
            };
            let assignment =
                route
                    .assign
                    .and_then(|(target, workspace)| match reply.result.output {
                        crate::client::control::CommandOutput::New { tab, .. } => {
                            Some((tab, workspace))
                        }
                        crate::client::control::CommandOutput::Workspace { workspace } => {
                            target.map(|tab| (tab, Some(workspace)))
                        }
                        _ => None,
                    });
            if let Some((target, workspace)) = assignment
                && let Some(saved) = reply.session.tabs.iter().find(|t| t.id == target)
            {
                let tab = ui.sessions.borrow().desktop_tab(saved, workspace);
                let mut output = reply.result;
                if let crate::client::control::CommandOutput::New {
                    workspace: assigned,
                    ..
                } = &mut output.output
                {
                    *assigned = workspace;
                }
                ui.request(InternalRequest::DesktopTab { tab }, move |ui, result| {
                    let result = result.and_then(|value| {
                        let raw = serde_json::from_value(value)?;
                        let mut reply = ui.sessions.borrow_mut().project(raw)?;
                        reply.result = output;
                        Ok(reply)
                    });
                    ui.finish_command(result, reveal, selection, callback);
                });
            } else {
                ui.finish_command(Ok(reply), reveal, selection, callback);
            }
        });
    }
    fn finish_command(
        self: &Rc<Self>,
        reply: anyhow::Result<CommandReply>,
        reveal: Option<u64>,
        selection: Selection,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<CommandResult>),
    ) {
        // Connections can complete in different orders. An earlier slow creation
        // must not take focus from a newer tab or split the user has opened.
        let reveal = reveal.is_some_and(|serial| serial == self.creation_serial.get());
        let result = reply.and_then(|reply| self.apply_backend(reply, reveal, selection));
        if let Ok(result) = &result
            && let Some(warning) = &result.warning
        {
            self.message(warning);
        }
        self.complete_command();
        callback(self, result);
    }
    fn complete_command(&self) {
        self.command_count.set(self.command_count.get() - 1);
        if self.command_count.get() == 0 {
            self.command_exits.borrow_mut().clear();
        }
    }

    pub(super) fn apply_shared(self: &Rc<Self>, reply: CommandReply) -> anyhow::Result<()> {
        if reply.revision <= self.revision.get() {
            return Ok(());
        }
        let selection = Selection::capture(self);
        self.apply_backend(reply, false, selection)?;
        Ok(())
    }
    pub(super) fn apply_backend(
        self: &Rc<Self>,
        reply: CommandReply,
        reveal: bool,
        selection: Selection,
    ) -> anyhow::Result<CommandResult> {
        self.sessions.borrow().validate_state(&reply)?;
        let previous_active = self.active.get();
        let previous_focus = gtk::prelude::GtkWindowExt::focus(&self.window);
        if reply.revision < self.revision.get() {
            if reveal {
                self.reveal_result(&reply.result);
            }
            return Ok(reply.result);
        }
        self.revision.set(reply.revision);
        let mut reply = reply;
        // Keep observed exits until every outstanding snapshot is handled,
        // including panes that exited before they could be attached.
        remove_exited_panes(&mut reply, &self.command_exits.borrow());
        let CommandReply {
            result,
            session: mut state,
            catalog,
            ..
        } = reply;
        let mut current = Selection::capture(self);
        if let Some((desired, fallback, filter)) = self.restore_selection.get() {
            if current.active != fallback || current.filter != filter {
                self.restore_selection.set(None);
            } else if state.tabs.iter().any(|t| t.id == desired) {
                current.active = desired;
                self.restore_selection.set(None);
            }
        }
        preserve_window_state(&mut state, &catalog, &current);
        reconcile_selection(&mut state, &catalog, &selection, &current, reveal);
        // Split geometry belongs to this window. Keep ratios for surviving split nodes.
        for saved in &mut state.tabs {
            if let Some(tab) = self.tabs.borrow().iter().find(|tab| tab.id == saved.id) {
                saved.layout.update_ratios(&tab.layout.borrow());
            }
        }
        *self.catalog.borrow_mut() = catalog;
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
        // Removing a selected tab/pane chooses its adjacent survivor locally.
        // Use that choice instead of a different window's selection in the snapshot.
        if current.active != 0 && !state.tabs.iter().any(|tab| tab.id == current.active) {
            state.active = self.active.get();
        }
        for saved in &mut state.tabs {
            if current
                .tabs
                .get(&saved.id)
                .is_some_and(|tab| !saved.layout.contains(tab.active))
                && let Some(tab) = self.tabs.borrow().iter().find(|tab| tab.id == saved.id)
                && saved.layout.contains(tab.active)
            {
                saved.active = tab.active;
            }
        }
        let order: HashMap<_, _> = state
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| (tab.id, index))
            .collect();
        for saved in state.tabs {
            let existing = self.tabs.borrow().iter().any(|t| t.id == saved.id);
            let connected = {
                let sessions = self.sessions.borrow();
                sessions.connected(sessions.tab_ref(saved.id).daemon)
            };
            if !existing {
                // A connection can disappear after accepting a creation request.
                // Keep its execution in Connections until a stream can render it.
                if !connected {
                    continue;
                }
                self.restore_tab(saved, false)?;
                continue;
            }
            let new_ids: Vec<_> = {
                let tabs = self.tabs.borrow();
                let tab = tabs.iter().find(|t| t.id == saved.id).unwrap();
                saved
                    .layout
                    .ids()
                    .into_iter()
                    .filter(|id| {
                        !tab.panes.contains_key(id) || {
                            let sessions = self.sessions.borrow();
                            sessions.connected(sessions.pane_ref(*id).daemon)
                                && !sessions.has_handle(*id)
                        }
                    })
                    .collect()
            };
            if !connected && !new_ids.is_empty() {
                continue;
            }
            let mut new_panes = Vec::new();
            for id in new_ids {
                new_panes.push((id, self.terminal(id)?));
            }
            let changed = {
                let mut tabs = self.tabs.borrow_mut();
                let tab = tabs.iter_mut().find(|t| t.id == saved.id).unwrap();
                let changed = *tab.layout.borrow() != saved.layout || !new_panes.is_empty();
                for (id, _) in &new_panes {
                    if let Some(pane) = tab.panes.remove(id) {
                        super::tabs::detach(&pane.root);
                    }
                }
                *tab.layout.borrow_mut() = saved.layout;
                tab.panes.extend(new_panes);
                tab.name = if saved.custom_name {
                    saved.name
                } else {
                    tab.panes
                        .get(&saved.active)
                        .map(|pane| pane.automatic_title())
                        .filter(|title| !title.is_empty())
                        .unwrap_or(saved.name)
                };
                tab.custom_name = saved.custom_name;
                tab.created_at = saved.created_at;
                tab.workspace = saved.workspace;
                tab.active = saved.active;
                tab.name_label.set_text(&tab.name);
                changed
            };
            if changed {
                self.rebuild(saved.id);
            }
        }
        {
            let mut tabs = self.tabs.borrow_mut();
            if tabs
                .iter()
                .enumerate()
                .any(|(index, tab)| order[&tab.id] != index)
            {
                self.changing.set(true);
                tabs.sort_by_key(|tab| order[&tab.id]);
                for tab in tabs.iter() {
                    self.sidebar.remove(&tab.row);
                }
                for tab in tabs.iter() {
                    self.sidebar.append(&tab.row);
                }
                self.changing.set(false);
            }
        }
        self.filter.set(state.filter);
        self.refresh_client_ui();
        let visible = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.id == state.active)
            .is_some_and(|tab| self.tab_is_visible(tab));
        if state.active == previous_active && !visible && !reveal {
            // Typing in the sidebar can hide the active row without changing its terminal.
            self.update_tab_actions(state.active != 0);
        } else if state.active != 0 && (reveal || visible) {
            self.switch(state.active);
        } else if state.active != 0 {
            self.set_filter(state.filter);
        } else {
            self.show_empty();
        }
        self.refresh_metadata();
        if reveal {
            self.reveal_result(&result);
        } else if let Some(focus) = previous_focus.filter(|widget| widget.is_mapped()) {
            focus.grab_focus();
        }
        Ok(result)
    }
    fn reveal_result(self: &Rc<Self>, result: &CommandResult) {
        let pane = match result.output {
            crate::client::control::CommandOutput::New { pane, .. }
            | crate::client::control::CommandOutput::Split { pane } => Some(pane),
            _ => None,
        };
        if let Some(pane) = pane
            && let Ok(terminal) = self.pane(Some(pane))
        {
            self.focus_pane(pane);
            terminal.area.grab_focus();
        }
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
        let session = self.view_session();
        let desktop = self.sessions.borrow_mut().desktop(&session);
        if let Err(e) = self
            .backend
            .save_desktop(desktop, self.sessions.borrow().desktop_revision())
        {
            eprintln!("Could not save desktop: {e}");
        }
    }
    pub(super) fn view_session(&self) -> Session {
        Session {
            filter: self.filter.get(),
            active: self.active.get(),
            width: self.window.default_width(),
            height: self.window.default_height(),
            maximized: self.window.is_maximized(),
            tabs: self
                .tabs
                .borrow()
                .iter()
                .map(|w| TabState {
                    daemon: self.sessions.borrow().tab_ref(w.id).daemon,
                    id: w.id,
                    created_at: w.created_at,
                    name: w.name.clone(),
                    custom_name: w.custom_name,
                    workspace: w.workspace,
                    layout: w.layout.borrow().clone(),
                    active: w.active,
                    directories: w.panes.iter().map(|(&id, p)| (id, p.directory())).collect(),
                })
                .collect(),
        }
    }
    pub(super) fn handle_request(self: &Rc<Self>, request: PublicRequest) -> anyhow::Result<Value> {
        match request {
            PublicRequest::Detach => {
                let window = self.window.clone();
                glib::timeout_add_local_once(Duration::from_millis(50), move || {
                    window.close();
                });
                Ok(json!({"ok":true}))
            }
            PublicRequest::RendererInfo { pane } => {
                let pane = if pane.is_some() {
                    let id = pane
                        .map(|id| self.sessions.borrow().default_handle(id))
                        .transpose()?;
                    Some(self.pane(id)?)
                } else {
                    self.active_pane()
                };
                Ok(json!({
                    "terminal": pane.as_ref().map(|pane| pane.renderer_name()),
                    "gtk": self.window.renderer().map(|renderer| renderer.type_().name().to_string()),
                    "display": gtk::prelude::WidgetExt::display(&self.window).type_().name(),
                    "window_active": self.window.is_active(),
                    "window_mapped": self.window.is_mapped(),
                    "window_scale": self.window.scale_factor(),
                    "window_size": [self.window.width(), self.window.height()],
                    "frame_counter": self.window.frame_clock().map(|clock| clock.frame_counter()),
                    "viewport": pane.map(|pane| pane.renderer_geometry()),
                }))
            }
            PublicRequest::List => {
                let tabs = self.tabs.borrow();
                let daemon = self.sessions.borrow().default_daemon();
                Ok(json!({
                    "daemon": daemon,
                    "window": self.id,
                    "active_tab": (self.active.get() != 0).then(|| self.sessions.borrow().tab_ref(self.active.get()).id),
                    "active_reference": (self.active.get() != 0).then(||self.sessions.borrow().tab_ref(self.active.get())),
                    "filter": self.filter.get(),
                    "tab_search": self.sidebar_search.text().as_str(),
                    "sidebar_visible": self.split.shows_sidebar(),
                    "sidebar_collapsed": self.split.is_collapsed(),
                    "workspaces": self.catalog.borrow().workspaces.iter().map(|workspace| json!({
                        "id": workspace.id, "name": workspace.name, "directories": workspace.directories,
                        "tabs": tabs.iter().filter(|tab| tab.workspace == Some(workspace.id)).count(),
                        "unread": tabs.iter().any(|tab| tab.workspace == Some(workspace.id) && !tab.unread.is_empty()),
                    })).collect::<Vec<_>>(),
                    "tabs": tabs.iter().map(|tab| json!({
                        "id": self.sessions.borrow().tab_ref(tab.id).id, "created_at": tab.created_at, "name": tab.name, "workspace": tab.workspace,
                        "connection":self.sessions.borrow().connection_name(tab.id),
                        "machine_kind":self.sessions.borrow().machine_kind(tab.id),
                        "branch": tab.branch_label.text().to_string(),
                        "connected":self.sessions.borrow().connected(self.sessions.borrow().tab_ref(tab.id).daemon),
                        "reference": self.sessions.borrow().tab_ref(tab.id),
                        "visible": self.tab_is_visible(tab),
                        "active_pane": self.sessions.borrow().pane_ref(tab.active).id, "unread": !tab.unread.is_empty(),
                        "layout": self.sessions.borrow().raw_layout(&tab.layout.borrow()),
                        "panes": tab.layout.borrow().ids().iter().filter_map(|id| tab.panes.get(id))
                            .map(|pane| json!({"id": self.sessions.borrow().pane_ref(pane.id).id, "title": pane.title.text().to_string(),
                                "reference": self.sessions.borrow().pane_ref(pane.id),
                                "directory": pane.directory(), "exited": pane.exited(),
                                "unread": tab.unread.contains(&pane.id)})).collect::<Vec<_>>()
                    })).collect::<Vec<_>>()
                }))
            }
            PublicRequest::SearchTabs { query } => {
                let changing = self.changing.replace(true);
                self.sidebar_search.set_text(&query);
                self.changing.set(changing);
                self.refresh_sidebar();
                Ok(json!({"ok": true}))
            }
            PublicRequest::Filter { workspace, value } => {
                let filter = if let Some(id) = workspace {
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
                    match value.as_deref() {
                        Some("all") => TabFilter::All,
                        _ => {
                            anyhow::bail!("filter requires a workspace ID or value all")
                        }
                    }
                };
                self.set_filter(filter);
                Ok(json!({"ok": true}))
            }
            PublicRequest::Send {
                pane,
                text,
                paste,
                enter,
            } => {
                let id = pane
                    .map(|id| self.sessions.borrow().default_handle(id))
                    .transpose()?;
                let pane = self.pane(id)?;
                if paste {
                    pane.send_text(&text, enter)?;
                } else {
                    pane.send(text.as_bytes())?;
                }
                Ok(json!({"ok":true}))
            }
            PublicRequest::Focus {
                connection,
                pane,
                tab,
                workspace,
            } => {
                if let Some(pane) = pane {
                    let pane = self
                        .sessions
                        .borrow()
                        .external_handle(connection.as_deref(), pane)?;
                    self.pane(Some(pane))?;
                    self.jump_to_pane(pane);
                } else if let Some(id) = tab {
                    let id = self
                        .sessions
                        .borrow()
                        .external_handle(connection.as_deref(), id)?;
                    anyhow::ensure!(
                        self.tabs.borrow().iter().any(|w| w.id == id),
                        "Tab {id} does not exist"
                    );
                    self.switch(id);
                    self.window.present();
                } else if let Some(id) = workspace {
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
            PublicRequest::Screenshot { path } => {
                self.capture_window(&path)?;
                Ok(json!({"path":path}))
            }
            PublicRequest::Resize { width, height } => {
                anyhow::ensure!(
                    (360..=7680).contains(&width) && (280..=4320).contains(&height),
                    "Window size is outside the supported range"
                );
                // On a normal Wayland window, an unnecessary unmaximize can
                // trigger an old-size configure after the requested resize.
                if self.window.is_maximized() {
                    self.window.unmaximize();
                }
                self.window.set_default_size(width, height);
                Ok(json!({"ok":true}))
            }
            PublicRequest::Action { name } => {
                anyhow::ensure!(
                    self.window.lookup_action(&name).is_some(),
                    "Unknown action: {name}"
                );
                anyhow::ensure!(
                    self.window
                        .lookup_action(&name)
                        .is_some_and(|a| a.is_enabled()),
                    "Action unavailable: {name}"
                );
                anyhow::ensure!(
                    self.window
                        .lookup_action(&name)
                        .is_some_and(|a| a.parameter_type().is_none()),
                    "Action requires parameters: {name}. Use the corresponding socket method"
                );
                self.action(&name);
                Ok(json!({"ok":true}))
            }
            _ => anyhow::bail!("Request cannot be handled by this window"),
        }
    }

    // Diagnostic screenshots need a fresh snapshot even when the compositor
    // pauses the window. WidgetPaintable returns its cached render node in that
    // case. See docs/ui.md for the screenshot diagnostic's constraints.
    pub(super) fn capture_window(&self, path: &Path) -> anyhow::Result<()> {
        let bounds = self
            .window
            .compute_bounds(&self.window)
            .ok_or_else(|| anyhow::anyhow!("Window has not rendered yet"))?;
        anyhow::ensure!(
            self.window.is_mapped() && bounds.width() > 0.0 && bounds.height() > 0.0,
            "Window has not rendered yet"
        );
        // A compositor may pause an occluded window's frame clock. Complete
        // pending layout and snapshot its current contents without waiting for
        // an on-screen frame or returning a stale cached paintable.
        // Unlike width()/height(), bounds include the CSS border and padding.
        // Gmux's toplevel has no CSS margins, so this retains its allocation.
        self.window.allocate(
            bounds.width() as i32,
            bounds.height() as i32,
            self.window.baseline(),
            None,
        );
        let snapshot = gtk::Snapshot::new();
        // GTK has no public fresh root snapshot API that includes its CSS
        // decoration. Keep this exception confined to CSS snapshot construction.
        #[allow(deprecated)]
        {
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
        }
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
