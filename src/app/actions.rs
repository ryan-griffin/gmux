//! Window actions and keyboard accelerators.
use super::*;

impl Ui {
    pub(super) fn install_actions(self: &Rc<Self>) {
        for (name, shortcuts) in [
            ("new", &["<Control><Shift>t"][..]),
            ("open-folder", &["<Control><Shift>o"]),
            ("split-right", &["<Control><Shift>d"]),
            ("split-down", &["<Control><Shift>e"]),
            ("close", &[CLOSE_SHORTCUT]),
            ("close-empty-window", &[]),
            ("close-window", &[]),
            ("close-active-tab", &[]),
            ("copy", &["<Control><Shift>c"]),
            ("paste", &["<Control><Shift>v"]),
            ("find", &["<Control><Shift>f"]),
            ("sidebar", &["<Control><Shift>b"]),
            ("search-tabs", &[]),
            ("palette", &["<Control><Shift>p"]),
            ("main-menu", &["F10"]),
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
        let empty_shortcuts = gtk::ShortcutController::new();
        empty_shortcuts.set_scope(gtk::ShortcutScope::Global);
        // The pane action is disabled on the empty page. Attach its fallback
        // here so it only participates while this page is mapped.
        empty_shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(CLOSE_SHORTCUT),
            Some(gtk::NamedAction::new("win.close-empty-window")),
        ));
        self.empty.add_controller(empty_shortcuts);
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
    pub(super) fn action(self: &Rc<Self>, name: &str) {
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
            "close-empty-window" if self.active.get() == 0 => self.window.close(),
            "close-window" => self.window.close(),
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
            "search-tabs" => {
                self.split.set_show_sidebar(true);
                self.sidebar_search.grab_focus();
            }
            "palette" => self.palette(),
            "main-menu" => {
                self.split.set_show_sidebar(true);
                self.sidebar_main_menu.popup();
            }
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
                    let Some(w) = list.iter().find(|w| w.id == self.active.get()) else {
                        eprintln!("Active tab {} missing for pane switch", self.active.get());
                        return;
                    };
                    let ids = w.layout.borrow().ids();
                    (ids, w.active)
                };
                if ids.is_empty() {
                    return;
                }
                let Some(index) = ids.iter().position(|id| *id == active) else {
                    eprintln!("Active pane {active} missing for pane switch");
                    return;
                };
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
}

const CLOSE_SHORTCUT: &str = "<Control><Shift>w";

pub(super) const TAB_ACTIONS: &[&str] = &[
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
pub(super) const ZOOM_IN_SHORTCUTS: &[&str] = &["<Control>plus", "<Control>equal"];
