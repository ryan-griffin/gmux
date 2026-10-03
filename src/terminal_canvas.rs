use gmux::Scrollback;
use gtk::{glib, prelude::*, subclass::prelude::*};
use std::{
    cell::{Cell, RefCell},
    sync::OnceLock,
};

type SnapshotCallback = Box<dyn Fn(&gtk::Snapshot, i32, i32)>;
type ScrollCallback = Box<dyn Fn(u64)>;

mod imp {
    use super::*;

    pub struct TerminalCanvas {
        pub callback: RefCell<Option<SnapshotCallback>>,
        pub scroll_callback: RefCell<Option<ScrollCallback>>,
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub adjustment_handler: RefCell<Option<glib::SignalHandlerId>>,
        pub scrollback: Cell<Scrollback>,
        pub scroll_pending: Cell<bool>,
        pub syncing: Cell<bool>,
        pub scroll_row: Cell<u64>,
        pub row_height: Cell<f64>,
    }

    impl Default for TerminalCanvas {
        fn default() -> Self {
            Self {
                callback: RefCell::default(),
                scroll_callback: RefCell::default(),
                hadjustment: RefCell::default(),
                vadjustment: RefCell::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                adjustment_handler: RefCell::default(),
                scrollback: Cell::default(),
                scroll_pending: Cell::default(),
                syncing: Cell::default(),
                scroll_row: Cell::default(),
                row_height: Cell::new(22.0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TerminalCanvas {
        const NAME: &'static str = "GmuxTerminalCanvas";
        type Type = super::TerminalCanvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_accessible_role(gtk::AccessibleRole::Terminal);
        }
    }

    impl ObjectImpl for TerminalCanvas {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                [
                    "hadjustment",
                    "vadjustment",
                    "hscroll-policy",
                    "vscroll-policy",
                ]
                .map(glib::ParamSpecOverride::for_interface::<gtk::Scrollable>)
                .into()
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "hadjustment" => {
                    let adjustment: Option<gtk::Adjustment> = value.get().unwrap();
                    if let Some(adjustment) = &adjustment {
                        adjustment.configure(0.0, 0.0, 0.0, 1.0, 1.0, 0.0);
                    }
                    self.hadjustment.replace(adjustment);
                }
                "vadjustment" => self.set_vadjustment(value.get().unwrap()),
                "hscroll-policy" => self.hscroll_policy.set(value.get().unwrap()),
                "vscroll-policy" => self.vscroll_policy.set(value.get().unwrap()),
                name => unreachable!("Unknown property {name}"),
            }
            self.obj().queue_allocate();
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self.hadjustment.borrow().to_value(),
                "vadjustment" => self.vadjustment.borrow().to_value(),
                "hscroll-policy" => self.hscroll_policy.get().to_value(),
                "vscroll-policy" => self.vscroll_policy.get().to_value(),
                name => unreachable!("Unknown property {name}"),
            }
        }

        fn dispose(&self) {
            self.set_vadjustment(None);
            self.hadjustment.borrow_mut().take();
            self.callback.borrow_mut().take();
            self.scroll_callback.borrow_mut().take();
        }
    }

    impl TerminalCanvas {
        fn set_vadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            if let Some(previous) = self.vadjustment.borrow_mut().take()
                && let Some(handler) = self.adjustment_handler.borrow_mut().take()
            {
                previous.disconnect(handler);
            }
            if let Some(adjustment) = &adjustment {
                let weak = self.obj().downgrade();
                let handler = adjustment.connect_value_changed(move |adjustment| {
                    let Some(canvas) = weak.upgrade() else {
                        return;
                    };
                    let imp = canvas.imp();
                    if imp.syncing.get() {
                        return;
                    }
                    let row = (adjustment.value() / imp.row_height.get()).round() as u64;
                    if imp.scroll_row.replace(row) != row
                        && let Some(callback) = &*imp.scroll_callback.borrow()
                    {
                        imp.scroll_pending.set(true);
                        callback(row);
                    }
                });
                self.adjustment_handler.replace(Some(handler));
            }
            self.vadjustment.replace(adjustment);
            self.obj().sync_adjustment();
        }
    }

    impl ScrollableImpl for TerminalCanvas {}

    impl WidgetImpl for TerminalCanvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().sync_adjustment();
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.present();
                }
            }
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(callback) = &*self.callback.borrow() {
                let widget = self.obj();
                callback(snapshot, widget.width(), widget.height());
            }
        }
    }
}

glib::wrapper! {
    pub struct TerminalCanvas(ObjectSubclass<imp::TerminalCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl TerminalCanvas {
    pub fn new() -> Self {
        let canvas: Self = glib::Object::new();
        canvas.set_focusable(true);
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas.set_overflow(gtk::Overflow::Hidden);
        canvas
    }

    pub fn set_snapshot_func(&self, callback: impl Fn(&gtk::Snapshot, i32, i32) + 'static) {
        *self.imp().callback.borrow_mut() = Some(Box::new(callback));
        self.queue_draw();
    }

    pub fn set_scroll_func(&self, callback: impl Fn(u64) + 'static) {
        *self.imp().scroll_callback.borrow_mut() = Some(Box::new(callback));
    }

    pub fn set_scrollback(&self, scrollback: Scrollback, pending: bool) {
        self.imp().scrollback.set(scrollback);
        self.imp().scroll_pending.set(pending);
        self.sync_adjustment();
    }

    pub fn set_row_height(&self, height: f64) {
        let height = height.max(1.0);
        let previous = self.imp().row_height.replace(height);
        if previous != height {
            self.configure_adjustment(previous);
        }
    }

    fn sync_adjustment(&self) {
        self.configure_adjustment(self.imp().row_height.get());
    }

    fn configure_adjustment(&self, previous_row_height: f64) {
        let imp = self.imp();
        let Some(adjustment) = imp.vadjustment.borrow().clone() else {
            return;
        };
        let scrollback = imp.scrollback.get();
        let height = imp.row_height.get();
        // Preserve fractional rows through acknowledgements and font changes.
        // GTK handles touchpad deltas and momentum in logical pixels.
        let value = if imp.scroll_pending.get() || scrollback.offset == imp.scroll_row.get() {
            adjustment.value() / previous_row_height * height
        } else {
            scrollback.offset as f64 * height
        };
        imp.syncing.set(true);
        adjustment.configure(
            value,
            0.0,
            scrollback.total as f64 * height,
            height,
            scrollback.len.saturating_sub(1).max(1) as f64 * height,
            scrollback.len as f64 * height,
        );
        imp.scroll_row
            .set((adjustment.value() / height).round() as u64);
        imp.syncing.set(false);
    }
}
