//! Keyboard, pointer, selection, clipboard, and scrolling controllers.
use super::*;

const SHIFT: u16 = 1 << 0;
const CONTROL: u16 = 1 << 1;
const ALT: u16 = 1 << 2;
const SUPER: u16 = 1 << 3;
const CAPS_LOCK: u16 = 1 << 4;
// Action-only modifiers, removed before encoding terminal input.
const META: u16 = 1 << 5;
const HYPER: u16 = 1 << 6;

// GDK has no public repeat flag. Linux keycodes (including the XKB offset)
// fit here. Unknown/zero codes are always presses rather than guessed repeats.
// Fixed storage bounds both memory and work, without allocating on key events.
#[derive(Default)]
struct HeldKeys([u64; 16]);

impl HeldKeys {
    fn contains(&self, code: u32) -> bool {
        code != 0
            && self
                .0
                .get(code as usize / 64)
                .is_some_and(|word| word & (1 << (code % 64)) != 0)
    }

    fn press(&mut self, code: u32) -> KeyAction {
        let Some(word) = self.0.get_mut(code as usize / 64).filter(|_| code != 0) else {
            return KeyAction::Press;
        };
        let bit = 1 << (code % 64);
        let action = if *word & bit == 0 {
            KeyAction::Press
        } else {
            KeyAction::Repeat
        };
        *word |= bit;
        action
    }

    fn release(&mut self, code: u32) -> bool {
        let Some(word) = self.0.get_mut(code as usize / 64) else {
            return false;
        };
        let bit = 1 << (code % 64);
        let held = *word & bit != 0;
        *word &= !bit;
        held
    }
}

#[derive(Default)]
pub(crate) struct KeyboardState {
    // Two bits per modifier, in protocol order: Shift L/R, Control L/R,
    // Alt L/R, Super L/R, Meta L/R, Hyper L/R. Each pair preserves the other
    // side on release; Meta/Hyper do not change VT input encoding.
    held: u16,
    encoded: u16,
    caps_lock: Option<bool>,
    extra_modifiers: bool,
    keys: HeldKeys,
    action: KeyAction,
    ignored_keys: HeldKeys,
    ignored_event: bool,
    shortcut_event: bool,
}

impl KeyboardState {
    pub(crate) fn install(
        window: &impl IsA<gtk::Window>,
        keyboard: Rc<RefCell<Self>>,
        changed: impl Fn(bool) + 'static,
    ) {
        let window = window.as_ref();
        let changed = Rc::new(changed);
        // GTK can activate application accelerators before the terminal's key
        // controller runs. Remember those physical keys here as well.
        let shortcuts = Rc::new(window.application().map_or_else(Vec::new, |application| {
            application
                .list_action_descriptions()
                .iter()
                .flat_map(|action| application.accels_for_action(action))
                .filter_map(gtk::accelerator_parse)
                .collect::<Vec<_>>()
        }));
        // Track the whole window so shortcuts and focus changes between panes
        // cannot hide a modifier press or release from the next terminal.
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pressed = keyboard.clone();
        let press_changed = changed.clone();
        let press_shortcuts = shortcuts.clone();
        controller.connect_key_pressed(move |controller, key, code, modifiers| {
            let shortcut = application_shortcut(controller, &press_shortcuts);
            let (before, active) = {
                let mut keyboard = pressed.borrow_mut();
                let before = keyboard.links_active();
                let action = keyboard.keys.press(code);
                if shortcut {
                    keyboard.ignored_keys.press(code);
                }
                keyboard.ignored_event = keyboard.ignored_keys.contains(code);
                keyboard.shortcut_event = shortcut;
                keyboard.update(key, modifiers, action);
                (before, keyboard.links_active())
            };
            // A pointer can enter with Ctrl already held, without this window
            // observing its press. Always publish modifier events, including
            // releases whose before/after encoded states are both empty.
            if before != active || modifier_event(controller) {
                press_changed(active);
            }
            glib::Propagation::Proceed
        });
        let released = keyboard.clone();
        let release_changed = changed.clone();
        controller.connect_key_released(move |controller, key, code, modifiers| {
            let shortcut = application_shortcut(controller, &shortcuts);
            let (before, active) = {
                let mut keyboard = released.borrow_mut();
                let before = keyboard.links_active();
                keyboard.keys.release(code);
                keyboard.ignored_event = keyboard.ignored_keys.release(code);
                keyboard.shortcut_event = shortcut;
                keyboard.update(key, modifiers, KeyAction::Release);
                (before, keyboard.links_active())
            };
            if before != active || modifier_event(controller) {
                release_changed(active);
            }
        });
        window.add_controller(controller);
        // A pane, menu, or other widget can consume a release. Start a fresh
        // input sequence whenever its destination changes, even within a window.
        let focus_widget = keyboard.clone();
        window.connect_focus_widget_notify(move |_| {
            let mut keyboard = focus_widget.borrow_mut();
            keyboard.keys = HeldKeys::default();
            keyboard.ignored_event = false;
            keyboard.shortcut_event = false;
            keyboard.action = KeyAction::Press;
        });
        let teardown = keyboard.clone();
        window.connect_unrealize(move |_| *teardown.borrow_mut() = Self::default());
        let focus = keyboard.clone();
        window.connect_is_active_notify(move |window| {
            if !window.is_active() {
                *focus.borrow_mut() = Self::default();
                changed(false);
            }
        });
    }

    fn links_active(&self) -> bool {
        !self.extra_modifiers && self.encoded & (SHIFT | CONTROL | ALT | SUPER) == CONTROL
    }

    fn update(&mut self, key: gdk::Key, modifiers: gdk::ModifierType, action: KeyAction) -> u16 {
        self.action = action;
        let release = action == KeyAction::Release;
        let mut encoded = mods(modifiers);
        for (mask, flag) in [
            (gdk::ModifierType::META_MASK, META),
            (gdk::ModifierType::HYPER_MASK, HYPER),
        ] {
            if modifiers.contains(mask) {
                encoded |= flag;
            }
        }
        for (group, mask) in [SHIFT, CONTROL, ALT, SUPER, META, HYPER]
            .into_iter()
            .enumerate()
        {
            if encoded & mask == 0 {
                self.held &= !(3 << (2 * group));
            }
        }
        let modifier = match key {
            gdk::Key::Shift_L => Some((0, SHIFT, false)),
            gdk::Key::Shift_R => Some((0, SHIFT, true)),
            gdk::Key::Control_L => Some((1, CONTROL, false)),
            gdk::Key::Control_R => Some((1, CONTROL, true)),
            gdk::Key::Alt_L => Some((2, ALT, false)),
            gdk::Key::Alt_R => Some((2, ALT, true)),
            gdk::Key::Super_L => Some((3, SUPER, false)),
            gdk::Key::Super_R => Some((3, SUPER, true)),
            gdk::Key::Meta_L => Some((4, META, false)),
            gdk::Key::Meta_R => Some((4, META, true)),
            gdk::Key::Hyper_L => Some((5, HYPER, false)),
            gdk::Key::Hyper_R => Some((5, HYPER, true)),
            _ => None,
        };
        if let Some((group, mask, right)) = modifier {
            let sides = 3 << (2 * group);
            let side = 1 << (2 * group + u32::from(right));
            // GTK reports state before this event. If another side was held
            // before the window gained focus, infer it from that state.
            if !release && encoded & mask != 0 && self.held & sides == 0 {
                self.held |= sides ^ side;
            }
            if release {
                self.held &= !side;
            } else {
                self.held |= side;
            }
            encoded &= !mask;
            if self.held & sides != 0 {
                encoded |= mask;
            }
        } else if key == gdk::Key::Caps_Lock {
            if release {
                // GDK can retain the previous lock state on the release
                // that finishes unlocking Caps Lock.
                if let Some(locked) = self.caps_lock.take() {
                    encoded = (encoded & !CAPS_LOCK) | if locked { CAPS_LOCK } else { 0 };
                }
            } else if action == KeyAction::Press {
                encoded ^= CAPS_LOCK;
                self.caps_lock = Some(encoded & CAPS_LOCK != 0);
            }
        }
        self.extra_modifiers = encoded & (META | HYPER) != 0;
        encoded &= !(META | HYPER);
        self.encoded = encoded;
        encoded
    }
}

impl Terminal {
    pub(super) fn install_clipboard_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        for (name, copy) in [("select-output", false), ("copy-output", true)] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(t) = weak.upgrade() {
                    t.select_context_output(copy);
                }
            });
            group.add_action(&action);
        }
        // Keep clipboard and output actions in the same terminal action group.
        group.add_action(&self.copy_action);
        group.add_action(&self.paste_action);
        group.add_action(&self.copy_link_action);
        self.area.insert_action_group("terminal", Some(&group));
        let weak = Rc::downgrade(self);
        self.copy_action.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.copy();
            }
        });
        let weak = Rc::downgrade(self);
        self.paste_action.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.paste();
            }
        });
        let weak = Rc::downgrade(self);
        self.copy_link_action.connect_activate(move |_, parameter| {
            if let Some(t) = weak.upgrade()
                && let Some(uri) = parameter.and_then(|value| value.str())
            {
                t.area.clipboard().set_text(uri);
            }
        });
        let weak = Rc::downgrade(self);
        self.context_menu.connect_closed(move |_| {
            if let Some(t) = weak.upgrade() {
                t.refresh_link_hover();
            }
        });
        let weak = Rc::downgrade(self);
        self.area
            .connect_query_tooltip(move |_, x, y, keyboard, tooltip| {
                let Some(t) = weak.upgrade() else {
                    return false;
                };
                let displayed = t.displayed_links.borrow();
                let (x, y) = if keyboard {
                    let frame = displayed.frame;
                    (
                        frame.cursor_x as f64 * displayed.metrics.0,
                        frame.cursor_y as f64 * displayed.metrics.1,
                    )
                } else {
                    (x as f64, y as f64)
                };
                if let Some(uri) = displayed.target_at(x, y) {
                    tooltip.set_text(Some(uri));
                    true
                } else {
                    false
                }
            });
        let middle = gtk::GestureClick::new();
        middle.set_button(2);
        let weak = Rc::downgrade(self);
        middle.connect_pressed(move |_, _, _, _| {
            if let Some(t) = weak.upgrade() {
                t.paste_from(&t.area.primary_clipboard());
            }
        });
        self.area.add_controller(middle);
    }
    pub(super) fn install_focus(self: &Rc<Self>, canvas: &gtk::Overlay, on_focus: Rc<dyn Fn(u64)>) {
        let focus_click = gtk::GestureClick::new();
        focus_click.set_button(0);
        focus_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        focus_click.connect_pressed(move |_, _, _, _| {
            if let Some(t) = weak.upgrade()
                && !t.context_menu.is_visible()
            {
                t.area.grab_focus();
            }
        });
        canvas.add_controller(focus_click);
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_in();
                if t.session.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[I");
                }
                on_focus(t.id);
            }
        });
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_out();
                if t.session.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[O");
                }
                t.area.queue_draw();
            }
        });
        self.area.add_controller(focus);
    }
    pub(super) fn install_canvas_callbacks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_scroll_func(move |row| {
            if let Some(t) = weak.upgrade() {
                t.session.borrow_mut().scroll_to(row);
                t.update_drag_selection();
                t.sync_accessible_selection();
                t.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        self.area.set_snapshot_func(move |snapshot, w, h| {
            if let Some(t) = weak.upgrade() {
                t.paint(w, h, |renderer, scene, font, context| {
                    renderer.snapshot(snapshot, scene, font, context);
                });
            }
        });
    }
    pub(super) fn install_keyboard(self: &Rc<Self>, keyboard: Rc<RefCell<KeyboardState>>) {
        let key = gtk::EventControllerKey::new();
        let pending_key = Rc::new(ValueCell::new(None::<(gdk::Key, u16, KeyAction)>));
        let pending = pending_key.clone();
        let weak = Rc::downgrade(self);
        self.im.connect_commit(move |_, text| {
            if let Some(t) = weak.upgrade() {
                if t.selection.borrow_mut().take().is_some() {
                    let _ = t.session.borrow().clear_selection();
                }
                // Only a synchronous, unchanged text commit belongs to this
                // physical key. Composition and asynchronous commits are text
                // input, with no stale key identity, modifiers, or repeat action.
                if let Some((key, mods, action)) = pending.take()
                    && let Some(cp) = key.to_unicode()
                    && text.len() == cp.len_utf8()
                    && text.starts_with(cp)
                {
                    t.key_input(
                        key.name().as_deref().unwrap_or(""),
                        cp as u32,
                        mods,
                        action,
                        text,
                    );
                } else {
                    t.key_input("", 0, 0, KeyAction::Press, text);
                }
            }
        });
        let weak = Rc::downgrade(self);
        let press_keyboard = keyboard.clone();
        key.connect_key_pressed(move |controller, key, code, modifiers| {
            let Some(t) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if t.context_menu.is_visible() {
                return glib::Propagation::Proceed;
            }
            let (encoded_modifiers, action, shortcut, ignored) = {
                let keyboard = press_keyboard.borrow();
                (
                    keyboard.encoded,
                    keyboard.action,
                    keyboard.shortcut_event,
                    keyboard.ignored_event,
                )
            };
            if crate::menu::context_menu_shortcut(key, modifiers) {
                press_keyboard.borrow_mut().ignored_keys.press(code);
                let displayed = t.displayed_links.borrow();
                let (cw, ch) = displayed.metrics;
                let frame = displayed.frame;
                let position = (frame.cursor_x as f64 * cw, frame.cursor_y as f64 * ch);
                drop(displayed);
                t.show_context_menu(position.0, position.1);
                return glib::Propagation::Stop;
            }
            if shortcut {
                return glib::Propagation::Proceed;
            }
            if let Some(previous) = scroll_shortcut(key, modifiers) {
                press_keyboard.borrow_mut().ignored_keys.press(code);
                let delta = if previous {
                    -(t.grid.get().1 as isize)
                } else {
                    t.grid.get().1 as isize
                };
                t.scroll(delta);
                return glib::Propagation::Stop;
            }
            // Keep a consumed shortcut's repeats out of terminal input if its
            // modifiers change before the physical key is released.
            if ignored {
                return glib::Propagation::Stop;
            }
            let cp = key.to_unicode().map(|c| c as u32).unwrap_or(0);
            if !modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) && let Some(event) = controller.current_event()
            {
                pending_key.set(Some((key, encoded_modifiers, action)));
                let filtered = t.im.filter_keypress(&event);
                let committed = pending_key.take().is_none();
                if filtered || committed {
                    return glib::Propagation::Stop;
                }
            }
            // Modifier presses precede copy accelerators and must retain selection.
            let modifier = modifier_event(controller);
            if !modifier {
                t.stop_selection_drag();
                if t.selection.borrow_mut().take().is_some() {
                    let _ = t.session.borrow().clear_selection();
                }
            }
            let text = key
                .to_unicode()
                .filter(|c| !c.is_control())
                .map(|c| c.to_string())
                .unwrap_or_default();
            t.key_input(
                key.name().as_deref().unwrap_or(""),
                cp,
                encoded_modifiers,
                action,
                &text,
            );
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(self);
        key.connect_key_released(move |_, key, _, modifiers| {
            let (encoded_modifiers, ignored) = {
                let keyboard = keyboard.borrow();
                (
                    keyboard.encoded,
                    keyboard.ignored_event || keyboard.shortcut_event,
                )
            };
            if ignored
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
                    encoded_modifiers,
                    KeyAction::Release,
                    "",
                );
            }
        });
        self.area.add_controller(key);
    }
    pub(super) fn install_pointer(self: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade() {
                let state = gesture.current_event_state();
                if t.context_menu.is_visible() {
                    return;
                }
                // Ctrl alone explicitly opens an OSC 8 target. Other clicks
                // retain selection and terminal mouse-reporting behavior.
                if gesture.current_button() == 1 && link_modifiers(state) && t.open_link(x, y) {
                    t.stop_selection_drag();
                    gesture.set_state(gtk::EventSequenceState::Claimed);
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
        let weak = Rc::downgrade(self);
        click.connect_released(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade()
                && t.mouse_pressed.get() != 0
            {
                t.mouse_position.set((x, y));
                t.mouse(1, t.mouse_pressed.get(), gesture.current_event_state());
                t.mouse_pressed.set(0);
            }
        });
        self.area.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |controller, x, y| {
            if let Some(t) = weak.upgrade()
                && !t.context_menu.is_visible()
            {
                t.mouse_position.set((x, y));
                let state = controller.current_event_state();
                t.link_modifiers_changed(link_modifiers(state));
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.session.borrow().mode(0) {
                    t.mouse(2, t.mouse_pressed.get(), state);
                }
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_enter(move |controller, x, y| {
            if let Some(t) = weak.upgrade() {
                t.pointer_inside.set(true);
                t.mouse_position.set((x, y));
                t.link_modifiers_changed(link_modifiers(controller.current_event_state()));
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.pointer_inside.set(false);
                t.refresh_link_hover();
            }
        });
        self.area.add_controller(motion);
    }
    pub(super) fn install_selection(self: &Rc<Self>) {
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |gesture, presses, x, y| {
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
                t.selection_mode.set(
                    if gesture
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK)
                    {
                        SelectionMode::Rectangle
                    } else {
                        match (presses - 1) % 3 {
                            1 => SelectionMode::Word,
                            2 => SelectionMode::Line,
                            _ => SelectionMode::Cell,
                        }
                    },
                );
                let offset = t.session.borrow().snapshot().frame.scrollback.offset as usize;
                let cols = t.session.borrow().snapshot().frame.cols as usize;
                t.selection_anchor
                    .set(Some(offset * cols + t.cell_at(x, y)));
                t.selection.borrow_mut().take();
                let _ = t.session.borrow().clear_selection();
                if matches!(
                    t.selection_mode.get(),
                    SelectionMode::Word | SelectionMode::Line
                ) {
                    t.update_drag_selection();
                    t.send_drag_selection();
                }
                t.area.queue_draw();
            }
        });
        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, x, y| {
            if let Some(t) = weak.upgrade()
                && t.selection_anchor.get().is_some()
            {
                let (ox, oy) = t.drag_origin.get();
                // Use GTK's configured threshold to wait for intentional movement.
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
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                if t.drag_position.get().is_some() {
                    let (ox, oy) = t.drag_origin.get();
                    t.drag_position.set(Some((ox + x, oy + y)));
                    t.update_drag_selection();
                }
                // Flush pending bounds before Copy can follow release, including
                // a stationary word/line selection refreshed by terminal output.
                t.send_drag_selection();
                t.stop_selection_drag();
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
            }
        });
        self.area.add_controller(click.clone());
        self.area.add_controller(drag.clone());
        click.group_with(&drag);
        let weak = Rc::downgrade(self);
        self.area.connect_unmap(move |_| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
                t.pointer_inside.set(false);
                t.refresh_link_hover();
            }
        });
    }
    pub(super) fn install_scrolling(self: &Rc<Self>, canvas: &gtk::Overlay) {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let mouse_scroll = Rc::new(ValueCell::new(0.0_f64));
        let pending = mouse_scroll.clone();
        scroll.connect_scroll_begin(move |_| pending.set(0.0));
        let weak = Rc::downgrade(self);
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
    }
}

fn link_modifiers(modifiers: gdk::ModifierType) -> bool {
    crate::menu::shortcut_modifiers(modifiers) == gdk::ModifierType::CONTROL_MASK
}
fn scroll_shortcut(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<bool> {
    if crate::menu::shortcut_modifiers(modifiers) != gdk::ModifierType::SHIFT_MASK {
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

fn modifier_event(controller: &gtk::EventControllerKey) -> bool {
    controller.current_event().is_some_and(|event| {
        event
            .downcast_ref::<gdk::KeyEvent>()
            .is_some_and(|key| key.is_modifier())
    })
}
pub(super) fn mods(m: gdk::ModifierType) -> u16 {
    [
        (gdk::ModifierType::SHIFT_MASK, SHIFT),
        (gdk::ModifierType::CONTROL_MASK, CONTROL),
        (gdk::ModifierType::ALT_MASK, ALT),
        (gdk::ModifierType::SUPER_MASK, SUPER),
        (gdk::ModifierType::LOCK_MASK, CAPS_LOCK),
    ]
    .into_iter()
    .fold(0, |encoded, (mask, flag)| {
        encoded | if m.contains(mask) { flag } else { 0 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_keys_are_independent_and_tracking_is_bounded() {
        let mut keys = HeldKeys::default();
        let mut other_window = HeldKeys::default();
        assert_eq!(keys.press(38), KeyAction::Press);
        assert_eq!(keys.press(38), KeyAction::Repeat);
        assert_eq!(keys.press(38), KeyAction::Repeat);
        assert_eq!(keys.press(50), KeyAction::Press);
        assert_eq!(other_window.press(38), KeyAction::Press);
        assert!(keys.release(38));
        assert!(!keys.release(38));
        assert_eq!(keys.press(38), KeyAction::Press);
        assert_eq!(keys.press(50), KeyAction::Repeat);
        assert_eq!(keys.press(1023), KeyAction::Press);
        assert_eq!(keys.press(1023), KeyAction::Repeat);
        assert!(keys.release(1023));
        assert!(!keys.contains(1023));
        for unknown in [0, 1024, u32::MAX] {
            assert_eq!(keys.press(unknown), KeyAction::Press);
            assert_eq!(keys.press(unknown), KeyAction::Press);
            assert!(!keys.release(unknown));
        }
    }

    #[test]
    fn history_scroll_shortcuts_require_only_shift() {
        let shift = gdk::ModifierType::SHIFT_MASK;
        assert_eq!(scroll_shortcut(gdk::Key::Page_Up, shift), Some(true));
        assert_eq!(scroll_shortcut(gdk::Key::Page_Down, shift), Some(false));
        assert_eq!(
            scroll_shortcut(gdk::Key::Page_Up, shift | gdk::ModifierType::LOCK_MASK),
            Some(true)
        );
        for extra in [
            gdk::ModifierType::CONTROL_MASK,
            gdk::ModifierType::ALT_MASK,
            gdk::ModifierType::SUPER_MASK,
            gdk::ModifierType::HYPER_MASK,
            gdk::ModifierType::META_MASK,
        ] {
            assert_eq!(scroll_shortcut(gdk::Key::Page_Up, shift | extra), None);
            assert_eq!(scroll_shortcut(gdk::Key::Page_Down, shift | extra), None);
        }
        assert_eq!(
            scroll_shortcut(gdk::Key::Page_Up, gdk::ModifierType::empty()),
            None
        );
    }

    #[test]
    fn keyboard_modifiers_include_caps_lock_state() {
        assert_eq!(mods(gdk::ModifierType::LOCK_MASK), 16);
        assert_eq!(
            mods(gdk::ModifierType::LOCK_MASK | gdk::ModifierType::CONTROL_MASK),
            18
        );
    }

    #[test]
    fn modifier_events_include_the_current_press_and_release_and_both_sides() {
        for (left, right, mask, encoded) in [
            (
                gdk::Key::Shift_L,
                gdk::Key::Shift_R,
                gdk::ModifierType::SHIFT_MASK,
                1,
            ),
            (
                gdk::Key::Control_L,
                gdk::Key::Control_R,
                gdk::ModifierType::CONTROL_MASK,
                2,
            ),
            (
                gdk::Key::Alt_L,
                gdk::Key::Alt_R,
                gdk::ModifierType::ALT_MASK,
                4,
            ),
            (
                gdk::Key::Super_L,
                gdk::Key::Super_R,
                gdk::ModifierType::SUPER_MASK,
                8,
            ),
        ] {
            let mut keyboard = KeyboardState::default();
            assert_eq!(
                keyboard.update(left, gdk::ModifierType::empty(), KeyAction::Press),
                encoded
            );
            assert_eq!(keyboard.update(left, mask, KeyAction::Release), 0);
            assert_eq!(
                keyboard.update(left, gdk::ModifierType::empty(), KeyAction::Press),
                encoded
            );
            assert_eq!(keyboard.update(left, mask, KeyAction::Press), encoded);
            assert_eq!(keyboard.update(right, mask, KeyAction::Press), encoded);
            assert_eq!(keyboard.update(left, mask, KeyAction::Release), encoded);
            assert_eq!(keyboard.update(right, mask, KeyAction::Release), 0);

            // A modifier can already be held when the window gains focus.
            assert_eq!(keyboard.update(right, mask, KeyAction::Press), encoded);
            assert_eq!(keyboard.update(right, mask, KeyAction::Release), encoded);
            assert_eq!(keyboard.update(left, mask, KeyAction::Release), 0);
        }
    }

    #[test]
    fn caps_lock_events_report_the_lock_state_after_each_toggle() {
        let mut keyboard = KeyboardState::default();
        let control = gdk::ModifierType::CONTROL_MASK;
        let locked = control | gdk::ModifierType::LOCK_MASK;
        assert_eq!(
            keyboard.update(gdk::Key::Caps_Lock, control, KeyAction::Press),
            18
        );
        assert_eq!(
            keyboard.update(gdk::Key::Caps_Lock, locked, KeyAction::Release),
            18
        );
        assert_eq!(
            keyboard.update(gdk::Key::Caps_Lock, locked, KeyAction::Press),
            2
        );
        assert_eq!(
            keyboard.update(gdk::Key::Caps_Lock, locked, KeyAction::Release),
            2
        );
        // State from later non-modifier events also clears missed releases.
        assert_eq!(
            keyboard.update(gdk::Key::Shift_L, control, KeyAction::Press),
            3
        );
        assert_eq!(keyboard.update(gdk::Key::a, control, KeyAction::Press), 2);
        assert_eq!(keyboard.held, 0);
    }

    #[test]
    fn link_modifiers_follow_after_event_state_and_ignore_lock_bits() {
        let mut keyboard = KeyboardState::default();
        let control = gdk::ModifierType::CONTROL_MASK;
        keyboard.update(
            gdk::Key::Control_L,
            gdk::ModifierType::empty(),
            KeyAction::Press,
        );
        assert!(keyboard.links_active());
        keyboard.update(gdk::Key::Control_R, control, KeyAction::Press);
        keyboard.update(gdk::Key::Control_L, control, KeyAction::Release);
        assert!(keyboard.links_active());
        for extra in [
            gdk::ModifierType::SHIFT_MASK,
            gdk::ModifierType::ALT_MASK,
            gdk::ModifierType::SUPER_MASK,
            gdk::ModifierType::META_MASK,
            gdk::ModifierType::HYPER_MASK,
        ] {
            keyboard.update(gdk::Key::a, control | extra, KeyAction::Press);
            assert!(!keyboard.links_active());
        }
        keyboard.update(
            gdk::Key::a,
            control | gdk::ModifierType::LOCK_MASK,
            KeyAction::Press,
        );
        assert!(keyboard.links_active());
        keyboard.update(gdk::Key::Control_R, control, KeyAction::Release);
        assert!(!keyboard.links_active());
    }

    #[test]
    fn meta_and_hyper_sides_update_link_actions_without_changing_vt_modifiers() {
        let control = gdk::ModifierType::CONTROL_MASK;
        for (left, right, mask) in [
            (
                gdk::Key::Meta_L,
                gdk::Key::Meta_R,
                gdk::ModifierType::META_MASK,
            ),
            (
                gdk::Key::Hyper_L,
                gdk::Key::Hyper_R,
                gdk::ModifierType::HYPER_MASK,
            ),
        ] {
            let mut keyboard = KeyboardState::default();
            keyboard.update(
                gdk::Key::Control_L,
                gdk::ModifierType::empty(),
                KeyAction::Press,
            );
            assert_eq!(keyboard.update(left, control, KeyAction::Press), CONTROL);
            assert!(!keyboard.links_active());
            assert_eq!(
                keyboard.update(left, control | mask, KeyAction::Release),
                CONTROL
            );
            assert!(keyboard.links_active());
            keyboard.update(left, control, KeyAction::Press);
            keyboard.update(right, control | mask, KeyAction::Press);
            keyboard.update(left, control | mask, KeyAction::Release);
            assert!(!keyboard.links_active());
            assert_eq!(
                keyboard.update(right, control | mask, KeyAction::Release),
                CONTROL
            );
            assert!(keyboard.links_active());
            // Infer the opposite side when focus arrived after its press.
            keyboard.update(right, control | mask, KeyAction::Press);
            keyboard.update(right, control | mask, KeyAction::Release);
            assert!(!keyboard.links_active());
            keyboard.update(left, control | mask, KeyAction::Release);
            assert!(keyboard.links_active());
        }
    }
}
