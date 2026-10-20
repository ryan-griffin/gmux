use gmux_client::Frame;
use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

/// One sleeping source per eligible pane, scheduled only for the next blink.
/// All signal closures and the source hold weak references to this owner.
pub(super) struct Cursor {
    widget: glib::WeakRef<gtk::Widget>,
    settings: gtk::Settings,
    settings_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    window_handler: RefCell<Option<(gtk::Window, glib::SignalHandlerId)>>,
    source: RefCell<Option<glib::SourceId>>,
    frame: Cell<Frame>,
    pointer: Cell<Option<u8>>,
    exited: Cell<bool>,
    visible: Cell<bool>,
    started: Cell<Instant>,
}

impl Cursor {
    pub(super) fn new(
        widget: &impl IsA<gtk::Widget>,
        defaults: impl Fn(bool) + 'static,
    ) -> Rc<Self> {
        let widget = widget.as_ref();
        let cursor = Rc::new(Self {
            widget: widget.downgrade(),
            settings: widget.settings(),
            settings_handlers: RefCell::new(Vec::new()),
            window_handler: RefCell::new(None),
            source: RefCell::new(None),
            frame: Cell::new(Frame::default()),
            pointer: Cell::new(None),
            exited: Cell::new(false),
            visible: Cell::new(true),
            started: Cell::new(Instant::now()),
        });
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
                        defaults(cursor.default_blink());
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
        let previous = self.frame.replace(frame);
        if self.pointer.replace(Some(frame.mouse_shape)) != Some(frame.mouse_shape)
            && let Some(widget) = self.widget.upgrade()
        {
            widget.set_cursor_from_name(Some(pointer_name(frame.mouse_shape)));
        }
        let cursor = |f: Frame| {
            (
                f.cursor_x,
                f.cursor_y,
                f.cursor_style,
                f.cursor_visible,
                f.cursor_blink,
                f.cols,
                f.rows,
            )
        };
        let was_exited = self.exited.replace(exited);
        if cursor(previous) != cursor(frame) || was_exited != exited {
            self.restart();
        }
    }

    pub(super) fn restart(self: &Rc<Self>) {
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
        self.set_visible(true);
        self.started.set(Instant::now());
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
        let frame = self.frame.get();
        self.default_blink()
            && frame.cursor_blink != 0
            && frame.cursor_visible != 0
            && frame.cursor_x < frame.cols
            && frame.cursor_y < frame.rows
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

    fn schedule(self: &Rc<Self>) {
        let remaining = Duration::from_secs(self.settings.gtk_cursor_blink_timeout().max(0) as u64)
            .saturating_sub(self.started.get().elapsed());
        if !self.eligible() || remaining.is_zero() {
            self.set_visible(true);
            return;
        }
        let half_cycle =
            Duration::from_millis((self.settings.gtk_cursor_blink_time().max(2) / 2) as u64);
        let weak = Rc::downgrade(self);
        *self.source.borrow_mut() = Some(glib::timeout_add_local_once(
            half_cycle.min(remaining),
            move || {
                if let Some(cursor) = weak.upgrade() {
                    cursor.source.borrow_mut().take();
                    cursor.set_visible(!cursor.visible.get());
                    cursor.schedule();
                }
            },
        ));
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

// Same order as the pinned GhosttyMouseShape C enum. GTK uses W3C names and
// provides native fallbacks when a cursor theme lacks a particular shape.
pub(super) fn pointer_name(shape: u8) -> &'static str {
    const NAMES: &[&str] = &[
        "default",
        "context-menu",
        "help",
        "pointer",
        "progress",
        "wait",
        "cell",
        "crosshair",
        "text",
        "vertical-text",
        "alias",
        "copy",
        "move",
        "no-drop",
        "not-allowed",
        "grab",
        "grabbing",
        "all-scroll",
        "col-resize",
        "row-resize",
        "n-resize",
        "e-resize",
        "s-resize",
        "w-resize",
        "ne-resize",
        "nw-resize",
        "se-resize",
        "sw-resize",
        "ew-resize",
        "ns-resize",
        "nesw-resize",
        "nwse-resize",
        "zoom-in",
        "zoom-out",
    ];
    NAMES.get(shape as usize).copied().unwrap_or("text")
}
