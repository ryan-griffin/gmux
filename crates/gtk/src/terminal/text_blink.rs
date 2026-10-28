use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

/// Frontend-only SGR text blinking. All visible panes in the active window
/// share the desktop timing, independently of keyboard focus and cursor state.
pub(super) struct TextBlink {
    widget: glib::WeakRef<gtk::Widget>,
    settings: gtk::Settings,
    settings_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    window_handler: RefCell<Option<(gtk::Window, glib::SignalHandlerId)>>,
    source: RefCell<Option<glib::SourceId>>,
    content: Cell<bool>,
    visible: Cell<bool>,
}

impl TextBlink {
    pub(super) fn new(widget: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let widget = widget.as_ref();
        let blink = Rc::new(Self {
            widget: widget.downgrade(),
            settings: widget.settings(),
            settings_handlers: RefCell::default(),
            window_handler: RefCell::default(),
            source: RefCell::default(),
            content: Cell::new(false),
            visible: Cell::new(true),
        });
        for property in [
            "gtk-enable-animations",
            "gtk-cursor-blink",
            "gtk-cursor-blink-time",
        ] {
            let weak = Rc::downgrade(&blink);
            let handler = blink
                .settings
                .connect_notify_local(Some(property), move |_, _| {
                    if let Some(blink) = weak.upgrade() {
                        blink.stop();
                        blink.set_visible(true);
                        blink.schedule();
                    }
                });
            blink.settings_handlers.borrow_mut().push(handler);
        }
        let weak = Rc::downgrade(&blink);
        widget.connect_map(move |_| {
            if let Some(blink) = weak.upgrade() {
                blink.schedule();
            }
        });
        let weak = Rc::downgrade(&blink);
        widget.connect_unmap(move |_| {
            if let Some(blink) = weak.upgrade() {
                blink.schedule();
            }
        });
        let weak = Rc::downgrade(&blink);
        widget.connect_root_notify(move |_| {
            if let Some(blink) = weak.upgrade() {
                blink.bind_window();
            }
        });
        blink.bind_window();
        blink
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
                if let Some(blink) = weak.upgrade() {
                    blink.schedule();
                }
            });
            *self.window_handler.borrow_mut() = Some((window, handler));
        }
        self.schedule();
    }

    pub(super) fn visible(&self) -> bool {
        self.visible.get()
    }

    pub(super) fn running(&self) -> bool {
        self.source.borrow().is_some()
    }

    pub(super) fn update(self: &Rc<Self>, content: bool) {
        if self.content.replace(content) != content {
            self.schedule();
        }
    }

    fn eligible(&self) -> bool {
        self.content.get()
            && self.settings.is_gtk_enable_animations()
            && self.settings.is_gtk_cursor_blink()
            && self.widget.upgrade().is_some_and(|widget| {
                widget.is_mapped()
                    && widget
                        .root()
                        .and_downcast::<gtk::Window>()
                        .is_some_and(|w| w.is_active())
            })
    }

    fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) != visible
            && let Some(widget) = self.widget.upgrade()
        {
            widget.queue_draw();
        }
    }

    fn stop(&self) {
        if let Some(source) = self.source.borrow_mut().take() {
            source.remove();
        }
    }

    fn schedule(self: &Rc<Self>) {
        if !self.eligible() {
            self.stop();
            self.set_visible(true);
        } else if !self.running() {
            // SGR 5 and 6 collapse to one native flag and use one desktop rate.
            let delay =
                Duration::from_millis((self.settings.gtk_cursor_blink_time().max(2) / 2) as u64);
            let weak = Rc::downgrade(self);
            *self.source.borrow_mut() = Some(glib::timeout_add_local_once(delay, move || {
                if let Some(blink) = weak.upgrade() {
                    blink.source.borrow_mut().take();
                    if blink.eligible() {
                        blink.set_visible(!blink.visible.get());
                    }
                    blink.schedule();
                }
            }));
        }
    }
}

impl Drop for TextBlink {
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
