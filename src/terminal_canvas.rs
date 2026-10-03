use gtk::{glib, prelude::*, subclass::prelude::*};
use std::cell::RefCell;

type SnapshotCallback = Box<dyn Fn(&gtk::Snapshot, i32, i32)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TerminalCanvas {
        pub callback: RefCell<Option<SnapshotCallback>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TerminalCanvas {
        const NAME: &'static str = "GmuxTerminalCanvas";
        type Type = super::TerminalCanvas;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_accessible_role(gtk::AccessibleRole::Terminal);
        }
    }

    impl ObjectImpl for TerminalCanvas {}

    impl WidgetImpl for TerminalCanvas {
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
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
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
}
