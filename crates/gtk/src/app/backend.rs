//! Backend requests, state reconciliation, and presentation persistence.
use super::*;
use crate::client::{
    connections::Route,
    request::{OrganizationRequest, RoutedRequest},
    window::{CommandId, Controller, Effect, Selection, TabSelection, Update},
};

impl Selection {
    fn capture(ui: &Ui) -> Self {
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

struct Presentation<'a> {
    flag: &'a Cell<bool>,
    previous: bool,
}
impl Drop for Presentation<'_> {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

impl Ui {
    pub(super) fn presenting(&self) -> impl Drop + '_ {
        Presentation {
            flag: &self.presenting,
            previous: self.presenting.replace(true),
        }
    }
    pub(super) fn navigated(&self) {
        if !self.presenting.get() && !self.changing.get() {
            self.controller.borrow_mut().navigated();
        }
    }
    pub(super) fn request_busy(
        self: &Rc<Self>,
        body: impl Into<crate::client::request::Request>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<bool>) + 'static,
    ) {
        self.request_reply(body, move |ui, result| {
            callback(
                ui,
                result.and_then(|reply| match reply {
                    BackendReply::Execution(reply) => reply.into_busy(),
                    _ => anyhow::bail!("Expected a busy reply"),
                }),
            );
        });
    }
    /// Configuration requests retain their dynamic JSON result contract.
    pub(super) fn request_configuration(
        self: &Rc<Self>,
        body: ConfigurationRequest,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<Value>) + 'static,
    ) {
        self.request_reply(body, move |ui, result| {
            callback(
                ui,
                result.and_then(|reply| match reply {
                    BackendReply::Value(value) => Ok(value),
                    _ => anyhow::bail!("Expected a configuration reply"),
                }),
            );
        });
    }
    pub(super) fn request_reply(
        self: &Rc<Self>,
        body: impl Into<crate::client::request::Request>,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<BackendReply>) + 'static,
    ) {
        let routed = self.sessions.borrow().route_request(body);
        let routed = match routed {
            Ok(value) => value,
            Err(error) => {
                callback(self, Err(error));
                return;
            }
        };
        self.submit_request(routed, callback);
    }
    fn submit_request(
        self: &Rc<Self>,
        request: RoutedRequest,
        callback: impl FnOnce(&Rc<Self>, anyhow::Result<BackendReply>) + 'static,
    ) {
        match self.backend.request(request) {
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
        let route = (|| {
            Controller::resolve(
                &mut command,
                self.active.get(),
                self.active_pane().map(|pane| pane.id),
                self.filter.get(),
            )?;
            if let Command::Move {
                tab: Some(id),
                workspace,
            } = command
            {
                let tabs = self.tabs.borrow();
                let saved = tabs
                    .iter()
                    .find(|tab| tab.id == id)
                    .ok_or_else(|| anyhow::anyhow!("Tab does not exist"))?;
                return Ok(Route {
                    request: RoutedRequest::Organization(OrganizationRequest::Assign(
                        model::DesktopTab {
                            reference: saved.id,
                            workspace,
                            layout: saved.layout.borrow().clone(),
                            active: saved.active,
                            name: saved.name.clone(),
                        },
                    )),
                    assign: None,
                });
            }
            self.sessions.borrow().route_command(command, destination)
        })();
        let submission = route.and_then(|route| self.controller.borrow_mut().begin(route));
        match submission {
            Ok((id, request)) => {
                self.save_session();
                self.command_callbacks
                    .borrow_mut()
                    .insert(id, Box::new(callback));
                self.submit_command(id, request);
            }
            Err(error) => callback(self, Err(error)),
        }
    }
    fn submit_command(self: &Rc<Self>, id: CommandId, request: RoutedRequest) {
        self.submit_request(request, move |ui, result| {
            let reply = result.and_then(|raw| {
                let mut sessions = ui.sessions.borrow_mut();
                let reply = sessions.project(raw)?;
                sessions.validate_state(&reply)?;
                Ok(reply)
            });
            let effect = ui
                .controller
                .borrow_mut()
                .reply(id, reply, Selection::capture(ui));
            match effect {
                Some(Effect::Assign { id, tab }) => {
                    ui.submit_command(
                        id,
                        RoutedRequest::Organization(OrganizationRequest::Assign(tab)),
                    );
                }
                Some(Effect::Finished {
                    id,
                    update,
                    refresh_on_failure,
                }) => {
                    let result = update.and_then(|update| ui.apply_update(update));
                    if let Ok(result) = &result
                        && let Some(warning) = &result.warning
                    {
                        ui.message(warning);
                    }
                    // Execution can succeed before its presentation save fails.
                    if refresh_on_failure && result.is_err() {
                        let state = ui.sessions.borrow_mut().snapshot();
                        if let Err(error) = ui.apply_shared(state) {
                            ui.message(&error.to_string());
                        }
                    }
                    ui.controller.borrow_mut().settled(id);
                    let callback = ui.command_callbacks.borrow_mut().remove(&id);
                    if let Some(callback) = callback {
                        callback(ui, result);
                    }
                }
                None => (),
            }
        });
    }

    pub(super) fn apply_shared(self: &Rc<Self>, reply: CommandReply) -> anyhow::Result<()> {
        self.sessions.borrow().validate_state(&reply)?;
        let update = self
            .controller
            .borrow_mut()
            .shared(reply, Selection::capture(self));
        if let Some(update) = update {
            self.apply_update(update)?;
        }
        Ok(())
    }
    fn apply_update(self: &Rc<Self>, update: Update) -> anyhow::Result<CommandResult> {
        let _presenting = self.presenting();
        let Update {
            result,
            state,
            reveal,
            previous: current,
        } = update;
        let Some((mut state, catalog)) = state else {
            if reveal {
                self.reveal_result(&result);
            }
            return Ok(result);
        };
        let previous_active = self.active.get();
        let previous_focus = gtk::prelude::GtkWindowExt::focus(&self.window);
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
        if current.active.is_some() && !state.tabs.iter().any(|tab| Some(tab.id) == current.active)
        {
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
                sessions.connected(saved.id.daemon)
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
                            sessions.connected(id.daemon) && !sessions.is_attached(*id)
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
            .find(|tab| Some(tab.id) == state.active)
            .is_some_and(|tab| self.tab_is_visible(tab));
        if state.active == previous_active && !visible && !reveal {
            // Typing in the sidebar can hide the active row without changing its terminal.
            self.update_tab_actions(state.active.is_some());
        } else if state.active.is_some() && (reveal || visible) {
            self.switch(state.active.unwrap());
        } else if state.active.is_some() {
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
                        .map(|id| self.sessions.borrow().external_pane(None, id))
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
                    "frame_interval_us": self.sessions.borrow().frame_interval().as_micros(),
                    "viewport": pane.map(|pane| pane.renderer_geometry()),
                }))
            }
            PublicRequest::List => {
                let tabs = self.tabs.borrow();
                let daemon = self.sessions.borrow().default_daemon();
                Ok(json!({
                    "daemon": daemon,
                    "window": self.id,
                    "active_tab": self.active.get().map(|tab| tab.id),
                    "active_reference": self.active.get(),
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
                        "id": tab.id.id, "created_at": tab.created_at, "name": tab.name, "workspace": tab.workspace,
                        "connection":self.sessions.borrow().connection_name(tab.id),
                        "machine_kind":self.sessions.borrow().machine_kind(tab.id),
                        "branch": tab.branch_label.text().to_string(),
                        "connected":self.sessions.borrow().connected(tab.id.daemon),
                        "reference": tab.id,
                        "visible": self.tab_is_visible(tab),
                        "active_pane": tab.active.id, "unread": !tab.unread.is_empty(),
                        "progress": self.tab_progress(tab),
                        "program_status": self.tab_program_status(tab),
                        "layout": *tab.layout.borrow(),
                        "panes": tab.layout.borrow().ids().iter().filter_map(|id| tab.panes.get(id))
                            .map(|pane| json!({"id": pane.id.id, "title": pane.title.text().to_string(),
                                "reference": pane.id,
                                "directory": pane.directory(), "exited": pane.exited(),
                                "progress": pane.progress(),
                                "program_status": pane.program_status().as_deref(),
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
            PublicRequest::Pane(gmux_protocol::control::PaneRequest::Links { pane }) => {
                let id = pane
                    .map(|id| self.sessions.borrow().external_pane(None, id))
                    .transpose()?;
                Ok(self.pane(id)?.links())
            }
            PublicRequest::Pane(gmux_protocol::control::PaneRequest::Send {
                pane,
                text,
                paste,
                enter,
            }) => {
                let id = pane
                    .map(|id| self.sessions.borrow().external_pane(None, id))
                    .transpose()?;
                let pane = self.pane(id)?;
                if paste {
                    pane.send_text(&text, enter)?;
                } else {
                    pane.send_with_enter(text.as_bytes(), enter)?;
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
                        .external_pane(connection.as_deref(), pane)?;
                    self.pane(Some(pane))?;
                    self.jump_to_pane(pane);
                } else if let Some(id) = tab {
                    let id = self
                        .sessions
                        .borrow()
                        .external_tab(connection.as_deref(), id)?;
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
    pub(crate) fn capture_window(&self, path: &Path) -> anyhow::Result<()> {
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
