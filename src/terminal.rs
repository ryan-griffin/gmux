use crate::{
    model::{self, Settings},
    renderer::{Renderer, Scene},
    terminal_canvas::TerminalCanvas,
    theme::Theme,
};
use gmux::Frame;
use gmux::{remote::SessionHandle, session::SessionUpdate};
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
    ],
    &[
        ("Split _Right", "win.split-right"),
        ("Split _Down", "win.split-down"),
    ],
    &[("C_lose Pane", "win.close")],
];

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
    drag_origin: ValueCell<(f64, f64)>,
    drag_position: ValueCell<Option<(f64, f64)>>,
    drag_endpoint: ValueCell<Option<(u16, u32)>>,
    drag_selection_tick: RefCell<Option<gtk::TickCallbackId>>,
    drag_scroll_timer: RefCell<Option<glib::SourceId>>,
    drag_kinetic_scrolling: ValueCell<Option<bool>>,
    search: RefCell<Vec<u8>>,
    search_query: RefCell<String>,
    selection_anchor: ValueCell<Option<usize>>,
    metrics: ValueCell<(f64, f64)>,
    grid: ValueCell<(u16, u16)>,
    grid_initialized: ValueCell<bool>,
    pending_size: ValueCell<Option<(u16, u16)>>,
    size_timer: RefCell<Option<glib::SourceId>>,
    zoom: ValueCell<f64>,
    settings: Rc<RefCell<Settings>>,
    renderer: RefCell<Renderer>,
    render_font: RefCell<Option<(pango::FontDescription, pango::Context, u32)>>,
    im: gtk::IMMulticontext,
    mouse_position: ValueCell<(f64, f64)>,
    mouse_pressed: ValueCell<i32>,
    theme: ValueCell<Theme>,
    chrome_colors: ValueCell<Option<([u8; 3], [u8; 3])>>,
    chrome_css: gtk::CssProvider,
}
impl Terminal {
    pub fn new(
        session: SessionHandle,
        settings: Rc<RefCell<Settings>>,
        on_focus: Rc<dyn Fn(u64)>,
        on_close: Rc<dyn Fn(u64)>,
        application: &gtk::Application,
    ) -> Rc<Self> {
        let id = session.borrow().id();
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
            .tooltip_text("Close pane (Ctrl+Shift+W)")
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
        status.add_css_class("dim-label");
        status.add_css_class("pane-status");
        content.append(&status);
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&area));
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
            drag_origin: ValueCell::new((0.0, 0.0)),
            drag_position: ValueCell::new(None),
            drag_endpoint: ValueCell::new(None),
            drag_selection_tick: RefCell::new(None),
            drag_scroll_timer: RefCell::new(None),
            drag_kinetic_scrolling: ValueCell::new(None),
            search: RefCell::new(Vec::new()),
            search_query: RefCell::new(String::new()),
            selection_anchor: ValueCell::new(None),
            metrics: ValueCell::new((9.6, 22.0)),
            grid: ValueCell::new(grid),
            grid_initialized: ValueCell::new(false),
            pending_size: ValueCell::new(None),
            size_timer: RefCell::new(None),
            settings,
            zoom: ValueCell::new(0.0),
            renderer: RefCell::new(Renderer::new()),
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
        terminal.refresh_theme();
        let weak = Rc::downgrade(&terminal);
        terminal.copy_action.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.copy();
            }
        });
        let weak = Rc::downgrade(&terminal);
        terminal.paste_action.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.paste();
            }
        });
        let focus_click = gtk::GestureClick::new();
        focus_click.set_button(0);
        focus_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&terminal);
        focus_click.connect_pressed(move |_, _, _, _| {
            if let Some(t) = weak.upgrade()
                && !t.context_menu.is_visible()
            {
                t.area.grab_focus();
            }
        });
        canvas.add_controller(focus_click);
        let weak = Rc::downgrade(&terminal);
        terminal.area.set_scroll_func(move |row| {
            if let Some(t) = weak.upgrade() {
                t.session.borrow().scroll_to(row);
            }
        });
        let weak = Rc::downgrade(&terminal);
        terminal.area.set_snapshot_func(move |snapshot, w, h| {
            if let Some(t) = weak.upgrade() {
                t.paint(w, h, |renderer, scene, font, context| {
                    renderer.snapshot(snapshot, scene, font, context);
                });
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(&terminal);
        focus.connect_enter(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_in();
                if t.session.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[I");
                }
                on_focus(t.id);
            }
        });
        let weak = Rc::downgrade(&terminal);
        focus.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_out();
                if t.session.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[O");
                }
                t.area.queue_draw();
            }
        });
        terminal.area.add_controller(focus);
        close.connect_clicked(move |_| on_close(id));
        let key = gtk::EventControllerKey::new();
        // Use the registered accelerators and GDK's keyboard-layout matching
        // for both press and release, instead of a separate modifier whitelist.
        let shortcuts = Rc::new(
            application
                .list_action_descriptions()
                .iter()
                .flat_map(|action| application.accels_for_action(action))
                .filter_map(gtk::accelerator_parse)
                .collect::<Vec<_>>(),
        );
        let press_shortcuts = shortcuts.clone();
        let pending_key = Rc::new(RefCell::new((String::new(), 0u32, 0u16)));
        let pending = pending_key.clone();
        let weak = Rc::downgrade(&terminal);
        terminal.im.connect_commit(move |_, text| {
            if let Some(t) = weak.upgrade() {
                if t.selection.borrow_mut().take().is_some() {
                    let _ = t.session.borrow().clear_selection();
                }
                let pending = pending.borrow();
                t.key_input(&pending.0, pending.1, pending.2, false, text);
            }
        });
        let weak = Rc::downgrade(&terminal);
        key.connect_key_pressed(move |controller, key, _, modifiers| {
            let Some(t) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if t.context_menu.is_visible() {
                return glib::Propagation::Proceed;
            }
            if crate::menu::context_menu_shortcut(key, modifiers) {
                let (cw, ch) = t.metrics.get();
                let session = t.session.borrow();
                let frame = session.snapshot().frame;
                let position = (frame.cursor_x as f64 * cw, frame.cursor_y as f64 * ch);
                drop(session);
                t.show_context_menu(position.0, position.1);
                return glib::Propagation::Stop;
            }
            if application_shortcut(controller, &press_shortcuts) {
                return glib::Propagation::Proceed;
            }
            if let Some(previous) = scroll_shortcut(key, modifiers) {
                let delta = if previous {
                    -(t.grid.get().1 as isize)
                } else {
                    t.grid.get().1 as isize
                };
                t.scroll(delta);
                return glib::Propagation::Stop;
            }
            let text = key
                .to_unicode()
                .filter(|c| !c.is_control())
                .map(|c| c.to_string())
                .unwrap_or_default();
            let cp = key.to_unicode().map(|c| c as u32).unwrap_or(0);
            *pending_key.borrow_mut() = (
                key.name().map(|n| n.to_string()).unwrap_or_default(),
                cp,
                mods(modifiers),
            );
            if !modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) && let Some(event) = controller.current_event()
                && t.im.filter_keypress(&event)
            {
                return glib::Propagation::Stop;
            }
            // Modifier presses precede copy accelerators and must retain selection.
            let modifier = controller.current_event().is_some_and(|event| {
                event
                    .downcast_ref::<gdk::KeyEvent>()
                    .is_some_and(|key| key.is_modifier())
            });
            if !modifier {
                t.stop_selection_drag();
                if t.selection.borrow_mut().take().is_some() {
                    let _ = t.session.borrow().clear_selection();
                }
            }
            t.key_input(
                key.name().as_deref().unwrap_or(""),
                cp,
                mods(modifiers),
                false,
                &text,
            );
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(&terminal);
        key.connect_key_released(move |controller, key, _, modifiers| {
            if application_shortcut(controller, &shortcuts)
                || scroll_shortcut(key, modifiers).is_some()
                || crate::menu::context_menu_shortcut(key, modifiers)
            {
                return;
            }
            if let Some(t) = weak.upgrade()
                && !t.context_menu.is_visible()
            {
                t.key_input(
                    key.name().as_deref().unwrap_or(""),
                    key.to_unicode().map(|c| c as u32).unwrap_or(0),
                    mods(modifiers),
                    true,
                    "",
                );
            }
        });
        terminal.area.add_controller(key);
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&terminal);
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade() {
                let state = gesture.current_event_state();
                if t.context_menu.is_visible() {
                    return;
                }
                if gesture.current_button() == 3
                    && (state.contains(gdk::ModifierType::SHIFT_MASK)
                        || !t.session.borrow().mode(0))
                {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    t.show_context_menu(x, y);
                    return;
                }
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.session.borrow().mode(0) {
                    let button = match gesture.current_button() {
                        1 => 1,
                        2 => 3,
                        3 => 2,
                        _ => 0,
                    };
                    t.mouse_pressed.set(button);
                    t.mouse_position.set((x, y));
                    t.area.grab_focus();
                    t.mouse(0, button, state);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        });
        let weak = Rc::downgrade(&terminal);
        click.connect_released(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade()
                && t.mouse_pressed.get() != 0
            {
                t.mouse_position.set((x, y));
                t.mouse(1, t.mouse_pressed.get(), gesture.current_event_state());
                t.mouse_pressed.set(0);
            }
        });
        terminal.area.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&terminal);
        motion.connect_motion(move |controller, x, y| {
            if let Some(t) = weak.upgrade()
                && !t.context_menu.is_visible()
            {
                t.mouse_position.set((x, y));
                let state = controller.current_event_state();
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.session.borrow().mode(0) {
                    t.mouse(2, t.mouse_pressed.get(), state);
                }
            }
        });
        terminal.area.add_controller(motion);
        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(&terminal);
        drag.connect_drag_begin(move |gesture, x, y| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
                if t.context_menu.is_visible()
                    || (!gesture
                        .current_event_state()
                        .contains(gdk::ModifierType::SHIFT_MASK)
                        && t.session.borrow().mode(0))
                {
                    return;
                }
                t.drag_kinetic_scrolling
                    .set(Some(t.scroller.is_kinetic_scrolling()));
                t.scroller.set_kinetic_scrolling(false);
                // Stop GTK's animation and reset its retained overshoot before
                // selection autoscroll takes over. Otherwise pointer motion can
                // restart the spring and pull the viewport back to the edge.
                let adjustment = t.scroller.vadjustment();
                adjustment.set_value(adjustment.value());
                adjustment.emit_by_name::<()>("value-changed", &[]);
                t.area.grab_focus();
                t.drag_origin.set((x, y));
                let offset = t.session.borrow().snapshot().frame.scrollback.offset as usize;
                let cols = t.session.borrow().snapshot().frame.cols as usize;
                t.selection_anchor
                    .set(Some(offset * cols + t.cell_at(x, y)));
                t.selection.borrow_mut().take();
                let _ = t.session.borrow().clear_selection();
                t.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&terminal);
        drag.connect_drag_update(move |_, x, y| {
            if let Some(t) = weak.upgrade()
                && t.selection_anchor.get().is_some()
            {
                let (ox, oy) = t.drag_origin.get();
                // GestureDrag begins on press. Wait for intentional movement,
                // using GTK's configured threshold rather than selecting a click.
                if t.drag_position.get().is_none()
                    && !t.area.drag_check_threshold(
                        ox.round() as i32,
                        oy.round() as i32,
                        (ox + x).round() as i32,
                        (oy + y).round() as i32,
                    )
                {
                    return;
                }
                t.drag_position.set(Some((ox + x, oy + y)));
                t.update_drag_selection();
                t.sync_selection_scroll();
            }
        });
        let weak = Rc::downgrade(&terminal);
        drag.connect_drag_end(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                if t.drag_position.get().is_some() {
                    let (ox, oy) = t.drag_origin.get();
                    t.drag_position.set(Some((ox + x, oy + y)));
                    t.update_drag_selection();
                    // Flush the final endpoint before Copy or other input can
                    // follow release, even if the next frame has not started.
                    t.send_drag_selection();
                }
                t.stop_selection_drag();
            }
        });
        let weak = Rc::downgrade(&terminal);
        drag.connect_cancel(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
            }
        });
        terminal.area.add_controller(drag);
        let weak = Rc::downgrade(&terminal);
        terminal.area.connect_unmap(move |_| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
            }
        });
        let middle = gtk::GestureClick::new();
        middle.set_button(2);
        let weak = Rc::downgrade(&terminal);
        middle.connect_pressed(move |_, _, _, _| {
            if let Some(t) = weak.upgrade() {
                t.paste_from(&t.area.primary_clipboard());
            }
        });
        terminal.area.add_controller(middle);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let mouse_scroll = Rc::new(ValueCell::new(0.0_f64));
        let pending = mouse_scroll.clone();
        scroll.connect_scroll_begin(move |_| pending.set(0.0));
        let weak = Rc::downgrade(&terminal);
        scroll.connect_scroll(move |controller, _, dy| {
            if let Some(t) = weak.upgrade() {
                let state = controller.current_event_state();
                // Shift bypasses terminal mouse tracking and scrolls history.
                // GtkScrolledWindow otherwise redirects Shift-wheel horizontally.
                if state.contains(gdk::ModifierType::SHIFT_MASK) {
                    let pixels = if controller.unit() == gdk::ScrollUnit::Surface {
                        dy
                    } else {
                        dy * 3.0 * t.metrics.get().1
                    };
                    let adjustment = t.scroller.vadjustment();
                    adjustment.set_value(adjustment.value() + pixels);
                    return glib::Propagation::Stop;
                }
                if t.session.borrow().mode(0) {
                    let delta = if controller.unit() == gdk::ScrollUnit::Surface {
                        dy / t.metrics.get().1
                    } else {
                        dy
                    };
                    let accumulated = mouse_scroll.get() + delta;
                    let steps = accumulated.trunc() as isize;
                    mouse_scroll.set(accumulated - steps as f64);
                    // Mouse buttons four and five represent the wheel.
                    for _ in 0..steps.unsigned_abs().min(8) {
                        t.mouse(0, if steps < 0 { 4 } else { 5 }, state);
                    }
                    return glib::Propagation::Stop;
                }
            }
            // Let GtkScrolledWindow apply its native wheel and touchpad behavior.
            glib::Propagation::Proceed
        });
        canvas.add_controller(scroll);
        terminal
    }
    fn drag_selection_end(&self) -> Option<usize> {
        let (x, y) = self.drag_position.get()?;
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
        let next = Some((start.min(end), start.max(end)));
        if self.selection.replace(next) != next {
            self.area.queue_draw();
        }
        // Multiple pointer events between frames share one daemon command. The
        // visible range follows the current pointer immediately, without waiting
        // for older daemon selection replies to arrive.
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
        let cols = session.snapshot().frame.cols.max(1) as usize;
        let endpoint = ((end % cols) as u16, (end / cols) as u32);
        if self.drag_endpoint.get() == Some(endpoint) {
            return;
        }
        if let Err(error) = session.select(((start % cols) as u16, (start / cols) as u32), endpoint)
        {
            self.show_status(&error.to_string());
            return;
        }
        self.drag_endpoint.set(Some(endpoint));
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
                // Wait for the viewport before requesting another scroll. GTK's
                // adjustment callback queues the row through the session writer.
                if !terminal.session.borrow().scroll_pending() {
                    let adjustment = terminal.scroller.vadjustment();
                    adjustment.set_value(adjustment.value() + delta);
                }
                glib::ControlFlow::Continue
            },
        ));
    }
    fn stop_selection_drag(&self) {
        self.drag_position.set(None);
        self.drag_endpoint.set(None);
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
    fn key_input(&self, name: &str, cp: u32, mods: u16, release: bool, text: &str) {
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
        self.session.borrow_mut().send_text(text, enter)
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
                // The drag owns the displayed selection. Replies may describe
                // a previous pointer position, including the preceding clear.
                self.update_drag_selection();
            } else {
                *self.selection.borrow_mut() = self
                    .session
                    .borrow()
                    .selection()
                    .map(|(a, b)| (a as usize, b as usize));
            }
            self.area.queue_draw();
            let frame = {
                let session = self.session.borrow();
                let screen = session.snapshot();
                let base = screen.frame.scrollback.offset as usize * screen.frame.cols as usize;
                let selection = self.selection.borrow().and_then(|(a, b)| {
                    let start = a.max(base);
                    let end = b.min(base + screen.cells.len().saturating_sub(1));
                    (start <= end).then_some((start.saturating_sub(base), end.saturating_sub(base)))
                });
                self.area.update_text(&screen, selection);
                self.area
                    .set_scrollback(screen.frame.scrollback, session.scroll_pending());
                screen.frame
            };
            self.sync_chrome(frame);
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
        if let Err(error) = self
            .session
            .borrow_mut()
            .set_cursor_style(self.settings.borrow().cursor_style)
        {
            eprintln!("{error}");
        }
        self.area.queue_draw();
    }
    pub fn refresh_theme(&self) {
        let theme = Theme::adwaita(adw::StyleManager::default().is_dark());
        let frame = {
            let session = self.session.borrow();
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
            "scale": scale, "origin": [origin.0, origin.1], "focused": self.area.has_focus(),
            "scrollback": session.snapshot().frame.scrollback,
            "scroll_pending": session.scroll_pending(),
            "selection": *self.selection.borrow(),
            "selection_drag": self.drag_position.get().map(|position| serde_json::json!({
                "anchor": self.selection_anchor.get(), "position": position,
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
        if self.selection.borrow().is_none() {
            return false;
        }
        if let Err(error) = self.session.borrow().copy() {
            self.show_status(&error.to_string());
            return false;
        }
        true
    }
    pub fn paste(self: &Rc<Self>) {
        self.paste_from(&self.area.clipboard());
    }
    fn paste_from(self: &Rc<Self>, clipboard: &gdk::Clipboard) {
        let weak = Rc::downgrade(self);
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            if let (Some(t), Ok(Some(text))) = (weak.upgrade(), result) {
                t.stop_selection_drag();
                t.selection.borrow_mut().take();
                let _ = t.session.borrow().clear_selection();
                if let Err(error) = t.session.borrow().paste(&text) {
                    t.show_status(&error.to_string());
                }
                t.area.queue_draw();
            }
        });
    }
    pub fn scroll(&self, delta: isize) {
        self.session.borrow().scroll(delta, false);
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
        let cols = ((width as f64 / cw).floor() as i32).clamp(1, 1000) as u16;
        let rows = ((height as f64 / ch).floor() as i32).clamp(1, 1000) as u16;
        let mut session = self.session.borrow_mut();
        let exited = session.exited();
        let initialized = self.grid_initialized.get();
        if self.grid.get() != (cols, rows) {
            if let Err(error) = session.resize(
                cols,
                rows,
                cw as u32,
                ch as u32,
                width.max(0) as u16,
                height.max(0) as u16,
            ) {
                eprintln!("{error}");
                return;
            }
            self.grid.set((cols, rows));
            self.selection.borrow_mut().take();
            if initialized && !exited {
                self.show_size(cols, rows);
            }
        }
        self.grid_initialized.set(true);
        let e = session.snapshot();
        let mut search = self.search.borrow_mut();
        let find = session.find_state();
        if find.highlights.is_empty() {
            search.clear();
        } else {
            search.resize(e.cells.len(), 0);
            search.fill(0);
            for &(start, end, value) in &find.highlights {
                if let Some(range) = search.get_mut(start..end) {
                    range.fill(value);
                }
            }
        }
        let base = e.frame.scrollback.offset as usize * e.frame.cols as usize;
        let selection = self.selection.borrow().and_then(|(start, end)| {
            let start = start.max(base);
            let end = end.min(base + e.cells.len().saturating_sub(1));
            (start <= end).then_some((start.saturating_sub(base), end.saturating_sub(base)))
        });
        let (scale, origin) = self.surface_position();
        let scene = Scene {
            frame: e.frame,
            cells: e.cells,
            metrics: (cw, ch),
            scale,
            origin,
            width,
            height,
            theme: self.theme.get(),
            selection,
            matches: &search,
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
fn scroll_shortcut(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<bool> {
    if !modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        return None;
    }
    match key {
        gdk::Key::Page_Up => Some(true),
        gdk::Key::Page_Down => Some(false),
        _ => None,
    }
}
fn application_shortcut(
    controller: &gtk::EventControllerKey,
    shortcuts: &[(gdk::Key, gdk::ModifierType)],
) -> bool {
    controller
        .current_event()
        .and_then(|event| event.downcast::<gdk::KeyEvent>().ok())
        .is_some_and(|event| {
            shortcuts
                .iter()
                .any(|&(key, modifiers)| event.matches(key, modifiers) != gdk::KeyMatch::None)
        })
}
fn mods(m: gdk::ModifierType) -> u16 {
    (m.contains(gdk::ModifierType::SHIFT_MASK) as u16)
        | ((m.contains(gdk::ModifierType::CONTROL_MASK) as u16) << 1)
        | ((m.contains(gdk::ModifierType::ALT_MASK) as u16) << 2)
        | ((m.contains(gdk::ModifierType::SUPER_MASK) as u16) << 3)
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
