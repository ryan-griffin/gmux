use crate::{
    model,
    renderer::{Renderer, Scene},
    settings::Settings,
    terminal_canvas::TerminalCanvas,
    theme::Theme,
};
use gmux_client::{Frame, transport::SelectionMode};
use gmux_client::{remote::SessionHandle, session::SessionUpdate};
use gtk::{gdk, gio, glib, graphene, pango, prelude::*};
use std::{
    cell::{Cell as ValueCell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

const CONTEXT_MENU_SECTIONS: &[&[(&str, &str)]] = &[
    &[
        ("_Copy", "terminal.copy"),
        ("_Paste", "terminal.paste"),
        ("_Find…", "win.find"),
        ("Clear _Screen", "win.clear-screen"),
    ],
    &[
        ("Split _Right", "win.split-right"),
        ("Split _Down", "win.split-down"),
    ],
    &[("C_lose Pane", "win.close")],
];

#[derive(Default)]
struct SearchHighlights {
    ranges: Vec<(usize, usize, u8)>,
    cells: Vec<u8>,
}

impl SearchHighlights {
    fn update(&mut self, len: usize, ranges: &[(usize, usize, u8)]) -> &[u8] {
        if ranges.is_empty() {
            self.ranges.clear();
            self.cells.clear();
        } else if self.cells.len() != len || self.ranges != ranges {
            self.ranges.clear();
            self.ranges.extend_from_slice(ranges);
            self.cells.resize(len, 0);
            self.cells.fill(0);
            for &(start, end, value) in ranges {
                if let Some(range) = self.cells.get_mut(start..end) {
                    range.fill(value);
                }
            }
        }
        &self.cells
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DragSelectionRequest {
    endpoint: (u16, u32),
    text_revision: u64,
    grid: (u16, u16),
}

pub struct Terminal {
    pub id: u64,
    pub root: gtk::Box,
    pub area: TerminalCanvas,
    pub title: gtk::Label,
    title_bar: gtk::CenterBox,
    status: gtk::Label,
    size_label: gtk::Label,
    size_revealer: gtk::Revealer,
    scroller: gtk::ScrolledWindow,
    context_menu: gtk::PopoverMenu,
    copy_action: gio::SimpleAction,
    paste_action: gio::SimpleAction,
    session: SessionHandle,
    selection: RefCell<Option<(usize, usize)>>,
    selection_mode: ValueCell<SelectionMode>,
    selection_preview: RefCell<gmux_client::selection::Preview>,
    drag_origin: ValueCell<(f64, f64)>,
    drag_position: ValueCell<Option<(f64, f64)>>,
    drag_request: ValueCell<Option<DragSelectionRequest>>,
    drag_selection_tick: RefCell<Option<gtk::TickCallbackId>>,
    drag_scroll_timer: RefCell<Option<glib::SourceId>>,
    drag_kinetic_scrolling: ValueCell<Option<bool>>,
    search: RefCell<SearchHighlights>,
    search_query: RefCell<String>,
    selection_anchor: ValueCell<Option<usize>>,
    metrics: ValueCell<(f64, f64)>,
    grid: ValueCell<(u16, u16)>,
    pixel_geometry: ValueCell<Option<(u32, u32, u16, u16)>>,
    pending_size: ValueCell<Option<(u16, u16)>>,
    size_timer: RefCell<Option<glib::SourceId>>,
    zoom: ValueCell<f64>,
    settings: Rc<RefCell<Settings>>,
    renderer: RefCell<Renderer>,
    cursor: Rc<cursor::Cursor>,
    render_font: RefCell<Option<(pango::FontDescription, pango::Context, u32)>>,
    im: gtk::IMMulticontext,
    mouse_position: ValueCell<(f64, f64)>,
    mouse_pressed: ValueCell<i32>,
    theme: ValueCell<Theme>,
    chrome_colors: ValueCell<Option<([u8; 3], [u8; 3])>>,
    chrome_css: gtk::CssProvider,
}
mod cursor;
mod input;
pub(crate) use input::KeyboardModifiers;
use input::mods;

impl Terminal {
    pub(crate) fn new(
        id: u64,
        session: SessionHandle,
        settings: Rc<RefCell<Settings>>,
        on_focus: Rc<dyn Fn(u64)>,
        on_close: Rc<dyn Fn(u64)>,
        application: &gtk::Application,
        keyboard: Rc<RefCell<KeyboardModifiers>>,
    ) -> Rc<Self> {
        let directory = session.borrow().directory().to_path_buf();
        let grid = session.borrow().size();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("card");
        root.add_css_class("terminal-pane");
        root.set_widget_name(&format!("terminal-pane-{id}"));
        root.set_hexpand(true);
        root.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("pane-content");
        content.add_css_class("pane-clip");
        content.set_overflow(gtk::Overflow::Hidden);
        content.set_hexpand(true);
        content.set_vexpand(true);
        root.append(&content);
        let toolbar = gtk::CenterBox::new();
        toolbar.add_css_class("pane-toolbar");
        toolbar.set_visible(settings.borrow().show_pane_titles);
        let title = gtk::Label::builder()
            .label(model::directory_name(&directory))
            .xalign(0.5)
            .ellipsize(pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        title.add_css_class("caption");
        title.set_tooltip_text(Some(&directory.display().to_string()));
        toolbar.set_center_widget(Some(&title));
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close Pane (Ctrl+Shift+W)")
            .build();
        close.add_css_class("circular");
        close.add_css_class("flat");
        close.add_css_class("pane-close");
        close.set_valign(gtk::Align::Center);
        toolbar.set_end_widget(Some(&close));
        content.append(&toolbar);
        let area = TerminalCanvas::new();
        area.add_css_class("terminal-canvas");
        area.set_cursor_from_name(Some("text"));
        area.set_tooltip_text(Some("Terminal"));
        area.update_property(&[gtk::accessible::Property::Label("Terminal")]);
        let menu = gio::Menu::new();
        for entries in CONTEXT_MENU_SECTIONS {
            let section = gio::Menu::new();
            for &(label, action) in *entries {
                let item = gio::MenuItem::new(Some(label), Some(action));
                if let Some(name) = action.strip_prefix("terminal.")
                    && let Some(accel) = application
                        .accels_for_action(&format!("win.{name}"))
                        .first()
                {
                    item.set_attribute_value("accel", Some(&accel.to_variant()));
                }
                section.append_item(&item);
            }
            menu.append_section(None, &section);
        }
        let context_menu = gtk::PopoverMenu::from_model(Some(&menu));
        crate::menu::setup_popover(&context_menu, "Terminal Actions");
        context_menu.set_has_arrow(false);
        context_menu.set_halign(gtk::Align::Start);
        context_menu.set_position(gtk::PositionType::Bottom);
        context_menu.set_cursor_from_name(Some("default"));
        context_menu.set_parent(&area);
        let copy_action = gio::SimpleAction::new("copy", None);
        let paste_action = gio::SimpleAction::new("paste", None);
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&copy_action);
        actions.add_action(&paste_action);
        area.insert_action_group("terminal", Some(&actions));
        let scroller = gtk::ScrolledWindow::builder()
            .child(&area)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .hexpand(true)
            .vexpand(true)
            .build();
        let canvas = gtk::Overlay::builder()
            .child(&scroller)
            .hexpand(true)
            .vexpand(true)
            .build();
        let size_label = gtk::Label::new(None);
        size_label.add_css_class("pane-size");
        let size_revealer = gtk::Revealer::builder()
            .child(&size_label)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .build();
        canvas.add_overlay(&size_revealer);
        canvas.set_measure_overlay(&size_revealer, false);
        canvas.set_clip_overlay(&size_revealer, true);
        content.append(&canvas);
        let status = gtk::Label::builder().xalign(1.0).visible(false).build();
        status.add_css_class("caption");
        status.add_css_class("dimmed");
        status.add_css_class("pane-status");
        content.append(&status);
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&area));
        let cursor_session = session.clone();
        let cursor_settings = settings.clone();
        let cursor = cursor::Cursor::new(&area, move |blink| {
            if let Err(error) = cursor_session
                .borrow()
                .set_cursor_style(cursor_settings.borrow().cursor_style, blink)
            {
                eprintln!("{error}");
            }
        });
        let terminal = Rc::new(Self {
            id,
            root,
            area,
            title,
            title_bar: toolbar,
            status,
            size_label,
            size_revealer,
            scroller,
            context_menu,
            copy_action,
            paste_action,
            session,
            selection: RefCell::new(None),
            selection_mode: ValueCell::new(SelectionMode::Cell),
            selection_preview: RefCell::new(gmux_client::selection::Preview::default()),
            drag_origin: ValueCell::new((0.0, 0.0)),
            drag_position: ValueCell::new(None),
            drag_request: ValueCell::new(None),
            drag_selection_tick: RefCell::new(None),
            drag_scroll_timer: RefCell::new(None),
            drag_kinetic_scrolling: ValueCell::new(None),
            search: RefCell::new(SearchHighlights::default()),
            search_query: RefCell::new(String::new()),
            selection_anchor: ValueCell::new(None),
            metrics: ValueCell::new((9.6, 22.0)),
            grid: ValueCell::new(grid),
            pixel_geometry: ValueCell::new(None),
            pending_size: ValueCell::new(None),
            size_timer: RefCell::new(None),
            settings,
            zoom: ValueCell::new(0.0),
            renderer: RefCell::new(Renderer::new()),
            cursor,
            render_font: RefCell::new(None),
            im,
            mouse_position: ValueCell::new((0.0, 0.0)),
            mouse_pressed: ValueCell::new(0),
            theme: ValueCell::new(Theme::adwaita(adw::StyleManager::default().is_dark())),
            chrome_colors: ValueCell::new(None),
            chrome_css: gtk::CssProvider::new(),
        });
        gtk::style_context_add_provider_for_display(
            &terminal.root.display(),
            &terminal.chrome_css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // The daemon may still have the previous on-disk cursor default while
        // a preference save is pending. New views use the application snapshot.
        terminal.refresh_settings();
        terminal.refresh_theme();
        terminal.install_clipboard_actions();
        terminal.install_focus(&canvas, on_focus);
        terminal.install_canvas_callbacks();
        close.connect_clicked(move |_| on_close(id));
        terminal.install_keyboard(application, keyboard);
        terminal.install_pointer();
        terminal.install_selection();
        terminal.install_scrolling(&canvas);
        terminal
    }
    fn drag_selection_end(&self) -> Option<usize> {
        let (x, y) = self.drag_position.get().or_else(|| {
            (self.selection_mode.get() != SelectionMode::Cell).then(|| self.drag_origin.get())
        })?;
        let frame = self.session.borrow().snapshot().frame;
        Some(frame.scrollback.offset as usize * frame.cols as usize + self.cell_at(x, y))
    }
    fn update_drag_selection(self: &Rc<Self>) {
        let Some(start) = self.selection_anchor.get() else {
            return;
        };
        let Some(end) = self.drag_selection_end() else {
            return;
        };
        let next = self.selection_preview.borrow_mut().range(
            &self.session.borrow().snapshot(),
            start,
            end,
            self.selection_mode.get(),
        );
        if self.selection.replace(next) != next {
            self.area.queue_draw();
            self.sync_accessible_selection();
        }
        // The highlight follows the pointer immediately. Coalesce daemon commands
        // to one per frame; the engine resolves any bounds outside the viewport.
        if self.drag_selection_tick.borrow().is_none() {
            let weak = Rc::downgrade(self);
            *self.drag_selection_tick.borrow_mut() =
                Some(self.area.add_tick_callback(move |_, _| {
                    if let Some(terminal) = weak.upgrade() {
                        terminal.drag_selection_tick.borrow_mut().take();
                        terminal.send_drag_selection();
                    }
                    glib::ControlFlow::Break
                }));
        }
    }
    fn send_drag_selection(&self) {
        let Some(start) = self.selection_anchor.get() else {
            return;
        };
        let Some(end) = self.drag_selection_end() else {
            return;
        };
        let session = self.session.borrow();
        let screen = session.snapshot();
        let cols = screen.frame.cols.max(1) as usize;
        let endpoint = ((end % cols) as u16, (end / cols) as u32);
        let request = DragSelectionRequest {
            endpoint,
            // Cell selection keeps fixed bounds when only the text changes.
            text_revision: if self.selection_mode.get() == SelectionMode::Cell {
                0
            } else {
                screen.text_revision
            },
            grid: (screen.frame.cols, screen.frame.rows),
        };
        if self.drag_request.get() == Some(request) {
            return;
        }
        if let Err(error) = session.select(
            ((start % cols) as u16, (start / cols) as u32),
            endpoint,
            self.selection_mode.get(),
        ) {
            self.show_status(&error.to_string());
            return;
        }
        self.drag_request.set(Some(request));
    }
    fn sync_accessible_selection(&self) {
        let session = self.session.borrow();
        let screen = session.snapshot();
        let base = screen.frame.scrollback.offset as usize * screen.frame.cols as usize;
        let selection = self.selection.borrow().and_then(|(a, b)| {
            let start = a.max(base);
            let end = b.min(base + screen.cells.len().saturating_sub(1));
            (start <= end).then_some((start.saturating_sub(base), end.saturating_sub(base)))
        });
        self.area.update_text(&screen, selection);
    }
    fn selection_scroll_delta(&self) -> f64 {
        let Some((_, y)) = self.drag_position.get() else {
            return 0.0;
        };
        let height = self.area.height() as f64;
        let row_height = self.metrics.get().1;
        let edge = row_height.min(height / 2.0);
        let distance = if y < edge {
            y - edge
        } else if y > height - edge {
            y - (height - edge)
        } else {
            return 0.0;
        };
        // One row per tick near the edge; faster, capped scrolling outside it.
        distance.signum() * (distance.abs() / row_height).ceil().clamp(1.0, 8.0) * row_height
    }
    fn sync_selection_scroll(self: &Rc<Self>) {
        if self.selection_scroll_delta() == 0.0 {
            if let Some(timer) = self.drag_scroll_timer.borrow_mut().take() {
                timer.remove();
            }
            return;
        }
        if self.drag_scroll_timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        *self.drag_scroll_timer.borrow_mut() = Some(glib::timeout_add_local(
            Duration::from_millis(50),
            move || {
                let Some(terminal) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let delta = terminal.selection_scroll_delta();
                if delta == 0.0 || !terminal.area.is_mapped() {
                    terminal.drag_scroll_timer.borrow_mut().take();
                    return glib::ControlFlow::Break;
                }
                let adjustment = terminal.scroller.vadjustment();
                adjustment.set_value(adjustment.value() + delta);
                glib::ControlFlow::Continue
            },
        ));
    }
    fn stop_selection_drag(&self) {
        self.drag_position.set(None);
        self.drag_request.set(None);
        self.selection_anchor.set(None);
        if let Some(tick) = self.drag_selection_tick.borrow_mut().take() {
            tick.remove();
        }
        if let Some(timer) = self.drag_scroll_timer.borrow_mut().take() {
            timer.remove();
        }
        if let Some(kinetic_scrolling) = self.drag_kinetic_scrolling.take() {
            self.scroller.set_kinetic_scrolling(kinetic_scrolling);
        }
    }
    fn show_context_menu(&self, x: f64, y: f64) {
        self.area.grab_focus();
        self.copy_action
            .set_enabled(self.selection.borrow().is_some());
        self.paste_action.set_enabled(
            self.area
                .clipboard()
                .formats()
                .contains_type(String::static_type()),
        );
        self.context_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            x.round().clamp(0.0, self.area.width().max(1) as f64 - 1.0) as i32,
            y.round().clamp(0.0, self.area.height().max(1) as f64 - 1.0) as i32,
            1,
            1,
        )));
        self.context_menu.popup();
    }
    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.session.borrow().send(bytes)
    }
    pub fn send_with_enter(&self, bytes: &[u8], enter: bool) -> anyhow::Result<()> {
        self.cursor.restart();
        self.session.borrow().send_with_enter(bytes, enter)
    }
    fn key_input(&self, name: &str, cp: u32, mods: u16, release: bool, text: &str) {
        if !release {
            self.cursor.restart();
        }
        if let Err(error) = self.session.borrow().key(name, cp, mods, release, text) {
            self.show_status(&error.to_string());
        }
    }
    fn mouse(&self, action: i32, button: i32, modifiers: gdk::ModifierType) {
        let (x, y) = self.mouse_position.get();
        let (cw, ch) = self.metrics.get();
        let result = self.session.borrow().mouse(
            (x.max(0.0) as f32, y.max(0.0) as f32),
            (cw as u32, ch as u32),
            action,
            button,
            mods(modifiers),
            self.mouse_pressed.get() != 0,
        );
        if let Err(error) = result {
            self.show_status(&error.to_string());
        }
    }
    pub fn send_text(&self, text: &str, enter: bool) -> anyhow::Result<()> {
        self.cursor.restart();
        self.session.borrow_mut().send_text(text, enter)
    }
    pub fn text(&self) -> String {
        self.session.borrow().text()
    }
    pub fn tick(self: &Rc<Self>, update: &SessionUpdate) {
        if let Some(error) = &update.error {
            self.show_status(error);
        }
        if let Some(text) = &update.copy {
            self.area.clipboard().set_text(text);
        }
        if update.changed {
            if self.selection_anchor.get().is_some() {
                if let Some(origin) = self.session.borrow().selection_origin() {
                    self.selection_anchor.set(Some(origin as usize));
                }
                // The drag owns the displayed selection. Replies may describe
                // a previous pointer position, including the preceding clear.
                self.update_drag_selection();
            } else if !self.session.borrow().selection_pending() {
                *self.selection.borrow_mut() = self
                    .session
                    .borrow()
                    .selection()
                    .map(|(a, b)| (a as usize, b as usize));
            }
            self.area.queue_draw();
            self.sync_accessible_selection();
            let frame = {
                let session = self.session.borrow();
                let screen = session.snapshot();
                self.area
                    .set_scrollback(screen.frame.scrollback, session.scroll_pending());
                screen.frame
            };
            self.sync_chrome(frame);
            self.cursor.update(frame, self.exited());
        }
        if update.directory_changed {
            self.title
                .set_tooltip_text(Some(&self.directory().display().to_string()));
        }
        self.refresh_title();
    }
    fn show_size(self: &Rc<Self>, cols: u16, rows: u16) {
        // Coalesce allocations, and update the overlay outside GTK's snapshot.
        if self.pending_size.replace(Some((cols, rows))).is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(terminal) = weak.upgrade() else {
                return;
            };
            let Some((cols, rows)) = terminal.pending_size.take() else {
                return;
            };
            terminal.size_label.set_text(&format!("{cols} × {rows}"));
            terminal.size_revealer.set_reveal_child(true);
            if let Some(timer) = terminal.size_timer.borrow_mut().take() {
                timer.remove();
            }
            let weak = Rc::downgrade(&terminal);
            *terminal.size_timer.borrow_mut() = Some(glib::timeout_add_local_once(
                Duration::from_millis(800),
                move || {
                    if let Some(terminal) = weak.upgrade() {
                        terminal.size_timer.borrow_mut().take();
                        terminal.size_revealer.set_reveal_child(false);
                    }
                },
            ));
        });
    }
    fn show_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
    }
    fn refresh_title(&self) {
        let session = self.session.borrow();
        let text = session.title();
        if self.title.text() != text {
            self.title.set_text(text);
        }
    }
    pub fn refresh_settings(&self) {
        self.render_font.borrow_mut().take();
        self.title_bar
            .set_visible(self.settings.borrow().show_pane_titles);
        if let Err(error) = self.session.borrow_mut().set_cursor_style(
            self.settings.borrow().cursor_style,
            self.cursor.default_blink(),
        ) {
            eprintln!("{error}");
        }
        self.area.queue_draw();
    }
    pub fn refresh_theme(&self) {
        let theme = Theme::adwaita(adw::StyleManager::default().is_dark());
        let frame = {
            let mut session = self.session.borrow_mut();
            if let Err(error) = session.set_theme(&theme) {
                eprintln!("{error}");
                return;
            }
            session.snapshot().frame
        };
        self.theme.set(theme);
        self.sync_chrome(frame);
        self.area.queue_draw();
    }
    fn sync_chrome(&self, frame: Frame) {
        let colors = (frame.background, frame.foreground);
        if self.chrome_colors.get() == Some(colors) {
            return;
        }
        self.chrome_colors.set(Some(colors));
        let [br, bg, bb] = frame.background;
        let [fr, fg, fb] = frame.foreground;
        self.chrome_css.load_from_string(&format!(
            "#terminal-pane-{} {{ --terminal-bg: rgb({br}, {bg}, {bb}); --terminal-fg: rgb({fr}, {fg}, {fb}); }}",
            self.id
        ));
    }
    pub fn directory(&self) -> PathBuf {
        self.session.borrow().directory().to_path_buf()
    }
    pub fn automatic_title(&self) -> String {
        self.session.borrow().title().to_owned()
    }
    pub fn branch(&self) -> String {
        self.session.borrow().branch().to_owned()
    }
    pub fn exited(&self) -> bool {
        self.session.borrow().exited()
    }
    pub fn set_active(&self, active: bool) {
        if active {
            self.root.add_css_class("active-pane");
        } else {
            self.root.remove_css_class("active-pane");
        }
        self.area.queue_draw();
    }
    pub fn renderer_name(&self) -> &'static str {
        "gsk"
    }
    pub fn unread(&self) -> bool {
        self.session.borrow().unread()
    }
    pub fn renderer_geometry(&self) -> serde_json::Value {
        let (scale, origin) = self.surface_position();
        let (cw, ch) = self.metrics.get();
        let frame = self.session.borrow().snapshot().frame;
        let (cols, rows) = (frame.cols.max(1), frame.rows.max(1));
        let scrollbar = self.scroller.vscrollbar();
        let scrollbar_origin = scrollbar
            .compute_point(&self.area, &graphene::Point::new(0.0, 0.0))
            .map(|point| [origin.0 + point.x() as f64, origin.1 + point.y() as f64]);
        let adjustment = self.scroller.vadjustment();
        let session = self.session.borrow();
        serde_json::json!({
            "width": self.area.width(), "height": self.area.height(),
            "cols": cols, "rows": rows, "cell_width": cw, "cell_height": ch,
            "title_bar_visible": self.title_bar.is_visible(), "cursor_style": frame.cursor_style,
            "cursor_blink": frame.cursor_blink, "cursor_blink_running": self.cursor.running(),
            "cursor_phase_visible": self.cursor.visible(), "mouse_shape": frame.mouse_shape,
            "pointer_name": self.area.cursor().and_then(|cursor| cursor.name()).map(|name| name.to_string()),
            "scale": scale, "origin": [origin.0, origin.1], "focused": self.area.has_focus(),
            "scrollback": session.snapshot().frame.scrollback,
            "scroll_pending": session.scroll_pending(),
            "scroll_cache_bytes": session.scroll_cache_bytes(),
            "selection": *self.selection.borrow(),
            "selection_drag": self.drag_position.get().map(|position| serde_json::json!({
                "anchor": self.selection_anchor.get(), "position": position,
                "mode": self.selection_mode.get(),
            })),
            "scrollbar": {
                "visible": scrollbar.is_mapped(),
                "origin": scrollbar_origin,
                "width": scrollbar.width(), "height": scrollbar.height(),
                "value": adjustment.value(), "upper": adjustment.upper(),
                "page_size": adjustment.page_size(),
            },
        })
    }
    pub fn copy(&self) -> bool {
        self.send_drag_selection();
        // Paint can invalidate the displayed range before a resize reply arrives.
        // The daemon validates its tracked selection in command order.
        if let Err(error) = self.session.borrow().copy() {
            self.show_status(&error.to_string());
            return false;
        }
        true
    }
    pub fn paste(self: &Rc<Self>) {
        self.paste_from(&self.area.clipboard());
    }
    pub fn clear_screen(&self) {
        if let Err(error) = self.session.borrow_mut().clear_screen() {
            self.show_status(&error.to_string());
            return;
        }
        self.stop_selection_drag();
    }
    fn paste_from(self: &Rc<Self>, clipboard: &gdk::Clipboard) {
        let weak = Rc::downgrade(self);
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            if let (Some(t), Ok(Some(text))) = (weak.upgrade(), result) {
                t.stop_selection_drag();
                t.selection.borrow_mut().take();
                let _ = t.session.borrow().clear_selection();
                t.cursor.restart();
                if let Err(error) = t.session.borrow().paste(&text) {
                    t.show_status(&error.to_string());
                }
                t.area.queue_draw();
            }
        });
    }
    pub fn scroll(&self, delta: isize) {
        let adjustment = self.scroller.vadjustment();
        adjustment.set_value(adjustment.value() + delta as f64 * self.metrics.get().1);
    }
    pub fn search(&self, query: &str) {
        if *self.search_query.borrow() == query {
            return;
        }
        if let Err(error) = self.session.borrow().find(query) {
            self.show_status(&error.to_string());
            return;
        }
        *self.search_query.borrow_mut() = query.to_owned();
    }
    pub fn search_advance(&self, previous: bool) {
        if let Err(error) = self.session.borrow().find_advance(previous) {
            self.show_status(&error.to_string());
        }
    }
    pub fn search_status(&self) -> (usize, usize, bool) {
        let session = self.session.borrow();
        let state = session.find_state();
        (state.current, state.total, state.pending)
    }
    pub fn zoom(&self, delta: f64) {
        self.render_font.borrow_mut().take();
        self.zoom.set(if delta == 0.0 {
            0.0
        } else {
            (self.zoom.get() + delta).clamp(-6.0, 20.0)
        });
        self.area.queue_draw();
    }
    fn cell_at(&self, x: f64, y: f64) -> usize {
        let (cw, ch) = self.metrics.get();
        let frame = self.session.borrow().snapshot().frame;
        let (cols, rows) = (frame.cols.max(1), frame.rows.max(1));
        let x = (x / cw).floor().max(0.0) as usize;
        let y = (y / ch).floor().max(0.0) as usize;
        y.min(rows as usize - 1) * cols as usize + x.min(cols as usize - 1)
    }
    fn paint(
        self: &Rc<Self>,
        width: i32,
        height: i32,
        draw: impl FnOnce(&mut Renderer, &Scene<'_>, &pango::FontDescription, &pango::Context),
    ) {
        let font = self
            .render_font
            .borrow()
            .as_ref()
            .map(|(font, _, _)| font.clone())
            .unwrap_or_else(|| crate::font::description(&self.settings.borrow(), self.zoom.get()));
        let context = self.area.pango_context();
        if self
            .render_font
            .borrow()
            .as_ref()
            .is_none_or(|(_, previous, serial)| previous != &context || *serial != context.serial())
        {
            let layout = pango::Layout::new(&context);
            layout.set_font_description(Some(&font));
            layout.set_text("M");
            let (width, height) = layout.pixel_size();
            let row_height = (height + 2).max(1) as f64;
            self.metrics.set((width.max(1) as f64, row_height));
            // Updating adjustments can resize the scrollbar. Leave the snapshot first.
            let canvas = self.area.clone();
            glib::idle_add_local_once(move || canvas.set_row_height(row_height));
            *self.render_font.borrow_mut() =
                Some((font.clone(), context.clone(), context.serial()));
        }
        let (cw, ch) = self.metrics.get();
        self.area.set_text_metrics(cw as f32, ch as f32);
        // GTK supplies the small canvas margins. Use its full allocation so
        // the grid, selections, and pointer coordinates share the same origin.
        let (cols, rows) = gmux_client::viewport::constrain(
            (width as f64 / cw).floor().max(0.0) as usize,
            (height as f64 / ch).floor().max(0.0) as usize,
        );
        let mut session = self.session.borrow_mut();
        let exited = session.exited();
        let pixel_geometry = (
            cw as u32,
            ch as u32,
            width.clamp(0, u16::MAX as i32) as u16,
            height.clamp(0, u16::MAX as i32) as u16,
        );
        let grid_changed = self.grid.get() != (cols, rows);
        if grid_changed || self.pixel_geometry.get() != Some(pixel_geometry) {
            if let Err(error) = session.resize(
                cols,
                rows,
                pixel_geometry.0,
                pixel_geometry.1,
                pixel_geometry.2,
                pixel_geometry.3,
            ) {
                eprintln!("{error}");
                return;
            }
            let initialized = self.pixel_geometry.replace(Some(pixel_geometry)).is_some();
            self.grid.set((cols, rows));
            if grid_changed {
                self.selection.borrow_mut().take();
                if initialized && !exited {
                    self.show_size(cols, rows);
                }
            }
        }
        let e = session.snapshot();
        let mut search = self.search.borrow_mut();
        let find = session.find_state();
        let matches = search.update(e.cells.len(), &find.highlights);
        let base = e.frame.scrollback.offset as usize * e.frame.cols as usize;
        let selection = self.selection.borrow().and_then(|(start, end)| {
            let start = start.max(base);
            let end = end.min(base + e.cells.len().saturating_sub(1));
            (start <= end).then_some((start.saturating_sub(base), end.saturating_sub(base)))
        });
        let (scale, origin) = self.surface_position();
        let scene = Scene {
            frame: Frame {
                cursor_visible: e.frame.cursor_visible * u8::from(self.cursor.visible()),
                ..e.frame
            },
            cells: e.cells,
            metrics: (cw, ch),
            scale,
            origin,
            width,
            height,
            theme: self.theme.get(),
            selection,
            matches,
            focused: self.area.has_focus(),
            exited,
        };
        draw(&mut self.renderer.borrow_mut(), &scene, &font, &context);
        if scene.cursor_visible() {
            self.im.set_cursor_location(&gdk::Rectangle::new(
                (e.frame.cursor_x as f64 * cw) as i32,
                (e.frame.cursor_y as f64 * ch) as i32,
                cw as i32,
                ch as i32,
            ));
        }
    }
    fn surface_position(&self) -> (f64, (f64, f64)) {
        self.area
            .native()
            .map(|native| {
                let scale = native
                    .surface()
                    .map(|surface| surface.scale())
                    .unwrap_or(1.0);
                let origin = self
                    .area
                    .compute_point(&native, &graphene::Point::new(0.0, 0.0));
                let (surface_x, surface_y) = native.surface_transform();
                let origin = origin
                    // GTK translates widget drawing into the surface by this
                    // offset, including the space reserved for window shadows.
                    .map(|point| (point.x() as f64 + surface_x, point.y() as f64 + surface_y))
                    .unwrap_or((0.0, 0.0));
                (scale, origin)
            })
            .unwrap_or((1.0, (0.0, 0.0)))
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.context_menu.unparent();
        if let Some(timer) = self.size_timer.get_mut().take() {
            timer.remove();
        }
        if let Some(timer) = self.drag_scroll_timer.get_mut().take() {
            timer.remove();
        }
        if let Some(tick) = self.drag_selection_tick.get_mut().take() {
            tick.remove();
        }
        gtk::style_context_remove_provider_for_display(&self.root.display(), &self.chrome_css);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_highlights_follow_ranges_navigation_and_viewport_size() {
        let mut cache = SearchHighlights::default();
        assert!(cache.update(6, &[]).is_empty());
        assert_eq!(cache.cells.capacity(), 0);
        let ranges = [(0, 2, 1), (3, 5, 2)];
        assert_eq!(cache.update(6, &ranges), [1, 1, 0, 2, 2, 0]);
        // An unchanged snapshot retains the populated cells without rebuilding.
        cache.cells[2] = 9;
        assert_eq!(cache.update(6, &ranges), [1, 1, 9, 2, 2, 0]);
        assert_eq!(cache.update(7, &ranges), [1, 1, 0, 2, 2, 0, 0]);
        assert_eq!(
            cache.update(7, &[(0, 2, 2), (3, 5, 1)]),
            [2, 2, 0, 1, 1, 0, 0]
        );
        assert_eq!(cache.update(3, &[(1, 3, 2)]), [0, 2, 2]);
        assert!(cache.update(3, &[]).is_empty());
        assert_eq!(cache.update(3, &[(1, 3, 2)]), [0, 2, 2]);
    }
}
