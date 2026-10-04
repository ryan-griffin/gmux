//! Keyboard, pointer, selection, clipboard, and scrolling controllers.
use super::*;

impl Terminal {
    pub(super) fn install_clipboard_actions(self: &Rc<Self>) {
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
                t.session.borrow().scroll_to(row);
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
    pub(super) fn install_keyboard(self: &Rc<Self>, application: &gtk::Application) {
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
        let weak = Rc::downgrade(self);
        self.im.connect_commit(move |_, text| {
            if let Some(t) = weak.upgrade() {
                if t.selection.borrow_mut().take().is_some() {
                    let _ = t.session.borrow().clear_selection();
                }
                let pending = pending.borrow();
                t.key_input(&pending.0, pending.1, pending.2, false, text);
            }
        });
        let weak = Rc::downgrade(self);
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
        let weak = Rc::downgrade(self);
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
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.session.borrow().mode(0) {
                    t.mouse(2, t.mouse_pressed.get(), state);
                }
            }
        });
        self.area.add_controller(motion);
    }
    pub(super) fn install_selection(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(self);
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
        let weak = Rc::downgrade(self);
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
        let weak = Rc::downgrade(self);
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
        let weak = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
            }
        });
        self.area.add_controller(drag);
        let weak = Rc::downgrade(self);
        self.area.connect_unmap(move |_| {
            if let Some(t) = weak.upgrade() {
                t.stop_selection_drag();
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
