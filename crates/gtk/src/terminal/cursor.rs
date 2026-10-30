use gmux_protocol::{Frame, cells::mouse_shape};
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CursorState {
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    style: u8,
    visible: u8,
    blink: u8,
}

impl From<Frame> for CursorState {
    fn from(frame: Frame) -> Self {
        Self {
            x: frame.cursor_x,
            y: frame.cursor_y,
            cols: frame.cols,
            rows: frame.rows,
            style: frame.cursor_style,
            visible: frame.cursor_visible,
            blink: frame.cursor_blink,
        }
    }
}

/// One sleeping source per eligible pane, scheduled only for the next blink.
/// All signal closures and the source hold weak references to this owner.
pub(super) struct Cursor {
    widget: glib::WeakRef<gtk::Widget>,
    settings: gtk::Settings,
    settings_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    window_handler: RefCell<Option<(gtk::Window, glib::SignalHandlerId)>>,
    source: RefCell<Option<glib::SourceId>>,
    default_enabled: Cell<bool>,
    state: Cell<CursorState>,
    pointer: Cell<Option<u8>>,
    requested_pointer: Cell<u8>,
    link_hover: Cell<bool>,
    link_cursor: Option<gdk::Cursor>,
    terminal_cursor: RefCell<Option<gdk::Cursor>>,
    exited: Cell<bool>,
    visible: Cell<bool>,
    started: Cell<Instant>,
    deadline: Cell<Instant>,
}

impl Cursor {
    pub(super) fn new(
        widget: &impl IsA<gtk::Widget>,
        defaults: impl Fn(bool) + 'static,
    ) -> Rc<Self> {
        let widget = widget.as_ref();
        let settings = widget.settings();
        let default_blink =
            settings.is_gtk_cursor_blink() && settings.gtk_cursor_blink_timeout() > 0;
        let cursor = Rc::new(Self {
            widget: widget.downgrade(),
            settings,
            settings_handlers: RefCell::new(Vec::new()),
            window_handler: RefCell::new(None),
            source: RefCell::new(None),
            default_enabled: Cell::new(default_blink),
            state: Cell::new(CursorState::default()),
            pointer: Cell::new(None),
            requested_pointer: Cell::new(mouse_shape::TEXT),
            link_hover: Cell::new(false),
            link_cursor: gdk::Cursor::from_name("pointer", None),
            terminal_cursor: RefCell::new(gdk::Cursor::from_name("text", None)),
            exited: Cell::new(false),
            visible: Cell::new(true),
            started: Cell::new(Instant::now()),
            deadline: Cell::new(Instant::now()),
        });
        cursor.update_pointer();
        let defaults = Rc::new(defaults);
        for property in [
            "gtk-cursor-blink",
            "gtk-cursor-blink-time",
            "gtk-cursor-blink-timeout",
        ] {
            let weak = Rc::downgrade(&cursor);
            let defaults = defaults.clone();
            let handler = cursor
                .settings
                .connect_notify_local(Some(property), move |_, _| {
                    if let Some(cursor) = weak.upgrade() {
                        notify_default(
                            &cursor.default_enabled,
                            cursor.settings.is_gtk_cursor_blink(),
                            cursor.settings.gtk_cursor_blink_timeout(),
                            |blink| defaults(blink),
                        );
                        // Preferences are infrequent. Replace the pending
                        // timeout so shorter and longer timings apply at once.
                        cursor.stop();
                        cursor.restart();
                    }
                });
            cursor.settings_handlers.borrow_mut().push(handler);
        }
        let weak = Rc::downgrade(&cursor);
        widget.connect_map(move |_| {
            if let Some(cursor) = weak.upgrade() {
                cursor.restart();
            }
        });
        let weak = Rc::downgrade(&cursor);
        widget.connect_unmap(move |_| {
            if let Some(cursor) = weak.upgrade() {
                cursor.restart();
            }
        });
        let weak = Rc::downgrade(&cursor);
        widget.connect_has_focus_notify(move |_| {
            if let Some(cursor) = weak.upgrade() {
                cursor.restart();
            }
        });
        let weak = Rc::downgrade(&cursor);
        widget.connect_root_notify(move |_| {
            if let Some(cursor) = weak.upgrade() {
                cursor.bind_window();
            }
        });
        cursor.bind_window();
        cursor
    }

    fn bind_window(self: &Rc<Self>) {
        if let Some((window, handler)) = self.window_handler.borrow_mut().take() {
            window.disconnect(handler);
        }
        if let Some(window) = self
            .widget
            .upgrade()
            .and_then(|w| w.root())
            .and_downcast::<gtk::Window>()
        {
            let weak = Rc::downgrade(self);
            let handler = window.connect_is_active_notify(move |_| {
                if let Some(cursor) = weak.upgrade() {
                    cursor.restart();
                }
            });
            *self.window_handler.borrow_mut() = Some((window, handler));
        }
        self.restart();
    }

    pub(super) fn default_blink(&self) -> bool {
        self.settings.is_gtk_cursor_blink() && self.settings.gtk_cursor_blink_timeout() > 0
    }

    pub(super) fn visible(&self) -> bool {
        self.visible.get()
    }
    pub(super) fn running(&self) -> bool {
        self.source.borrow().is_some()
    }

    pub(super) fn update(self: &Rc<Self>, frame: Frame, exited: bool) {
        let state = CursorState::from(frame);
        let previous = self.state.replace(state);
        if self.requested_pointer.replace(frame.mouse_shape) != frame.mouse_shape {
            *self.terminal_cursor.borrow_mut() =
                gdk::Cursor::from_name(mouse_shape::name(frame.mouse_shape), None);
        }
        self.update_pointer();
        let was_exited = self.exited.replace(exited);
        if previous != state || was_exited != exited {
            self.restart();
        }
    }

    pub(super) fn set_link_hover(&self, hover: bool) {
        self.link_hover.set(hover);
        self.update_pointer();
    }
    fn update_pointer(&self) {
        let shape = if self.link_hover.get() {
            mouse_shape::POINTER
        } else {
            self.requested_pointer.get()
        };
        if self.pointer.replace(Some(shape)) != Some(shape)
            && let Some(widget) = self.widget.upgrade()
        {
            if self.link_hover.get() {
                widget.set_cursor(self.link_cursor.as_ref());
            } else {
                widget.set_cursor(self.terminal_cursor.borrow().as_ref());
            }
        }
    }
    pub(super) fn restart(self: &Rc<Self>) {
        self.set_visible(true);
        let now = Instant::now();
        self.started.set(now);
        self.deadline.set(now + self.half_cycle());
        self.schedule();
    }

    fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) != visible
            && let Some(widget) = self.widget.upgrade()
        {
            // The renderer retains every other row, including for block cursors.
            widget.queue_draw();
        }
    }

    fn eligible(&self) -> bool {
        let state = self.state.get();
        self.default_blink()
            && state.blink != 0
            && state.visible != 0
            && state.x < state.cols
            && state.y < state.rows
            && !self.exited.get()
            && self.widget.upgrade().is_some_and(|widget| {
                widget.is_mapped()
                    && widget.has_focus()
                    && widget
                        .root()
                        .and_downcast::<gtk::Window>()
                        .is_some_and(|w| w.is_active())
            })
    }

    fn half_cycle(&self) -> Duration {
        Duration::from_millis((self.settings.gtk_cursor_blink_time().max(2) / 2) as u64)
    }

    fn stop(&self) {
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
    }

    fn schedule(self: &Rc<Self>) {
        let now = Instant::now();
        let expires = self.started.get()
            + Duration::from_secs(self.settings.gtk_cursor_blink_timeout().max(0) as u64);
        if !self.eligible() || now >= expires {
            self.stop();
            self.set_visible(true);
            return;
        }
        // Input extends the desired deadline without replacing a pending source.
        // Its earlier wake will rearm instead of hiding the cursor too soon.
        if self.running() {
            return;
        }
        let delay = self
            .deadline
            .get()
            .min(expires)
            .saturating_duration_since(now);
        // GLib takes whole milliseconds. Round up to avoid repeated early wakes.
        let delay = Duration::from_millis(delay.as_nanos().div_ceil(1_000_000) as u64);
        let weak = Rc::downgrade(self);
        *self.source.borrow_mut() = Some(glib::timeout_add_local_once(delay, move || {
            if let Some(cursor) = weak.upgrade() {
                cursor.source.borrow_mut().take();
                let now = Instant::now();
                if now >= cursor.deadline.get()
                    && now
                        < cursor.started.get()
                            + Duration::from_secs(
                                cursor.settings.gtk_cursor_blink_timeout().max(0) as u64
                            )
                    && cursor.eligible()
                {
                    cursor.set_visible(!cursor.visible.get());
                    cursor.deadline.set(now + cursor.half_cycle());
                }
                cursor.schedule();
            }
        }));
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
        for handler in self.settings_handlers.get_mut().drain(..) {
            self.settings.disconnect(handler);
        }
        if let Some((window, handler)) = self.window_handler.get_mut().take() {
            window.disconnect(handler);
        }
    }
}

fn notify_default(current: &Cell<bool>, enabled: bool, timeout: i32, notify: impl FnOnce(bool)) {
    let effective = enabled && timeout > 0;
    if current.replace(effective) != effective {
        notify(effective);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_notify_only_effective_changes() {
        let current = Cell::new(true);
        let mut notifications = Vec::new();
        // Positive timing changes, repeated notifications, timeout zero while
        // disabled, and enabling while timeout remains zero must not transmit.
        for (enabled, timeout) in [
            (true, 20),
            (true, 1),
            (true, 0),
            (true, 0),
            (false, 0),
            (false, 10),
            (true, 10),
            (false, 10),
            (true, 0),
            (true, 10),
        ] {
            notify_default(&current, enabled, timeout, |value| {
                notifications.push(value)
            });
        }
        assert_eq!(notifications, [false, true, false, true]);
    }
}
