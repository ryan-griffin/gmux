use gtk::{glib, prelude::*, subclass::prelude::*};
use std::cell::{Cell, OnceCell};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SplitPane {
        pub paned: OnceCell<gtk::Paned>,
        pub separator: OnceCell<gtk::Widget>,
        pub ratio: Cell<f64>,
        pub available: Cell<i32>,
        pub allocating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SplitPane {
        const NAME: &'static str = "GmuxSplitPane";
        type Type = super::SplitPane;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for SplitPane {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SplitPane {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.paned.get().unwrap().measure(orientation, for_size)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let paned = self.paned.get().unwrap();
            let orientation = paned.orientation();
            let dimension = if orientation == gtk::Orientation::Vertical {
                height
            } else {
                width
            };
            let (_, handle, _, _) = self.separator.get().unwrap().measure(orientation, -1);
            let available = (dimension - handle).max(1);
            self.available.set(available);
            // Always derive the divider from the user's proportion. Scaling the
            // previous integer position accumulates rounding during small resizes.
            self.allocating.set(true);
            paned.set_position((available as f64 * self.ratio.get()).round() as i32);
            paned.allocate(width, height, baseline, None);
            self.allocating.set(false);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj()
                .snapshot_child(self.paned.get().unwrap(), snapshot);
        }
    }
}

glib::wrapper! {
    pub struct SplitPane(ObjectSubclass<imp::SplitPane>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SplitPane {
    pub fn new(
        orientation: gtk::Orientation,
        ratio: f64,
        on_change: impl Fn(f64) + 'static,
    ) -> Self {
        let split: Self = glib::Object::new();
        let imp = split.imp();
        imp.ratio.set(ratio.clamp(0.1, 0.9));
        let paned = gtk::Paned::new(orientation);
        // We set the start size before allocation; GTK must not scale it again.
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.add_css_class("tab-split");
        // Preserve the terminal cards' shadows on the split's outer edges.
        paned.set_overflow(gtk::Overflow::Visible);
        // Narrow handles extend their hit area into adjacent scrollbars.
        // Keep input within the 6px separator styled in CSS.
        paned.set_wide_handle(true);
        imp.separator
            .set(paned.first_child().expect("GtkPaned has a separator"))
            .unwrap();
        paned.set_parent(&split);
        let weak = split.downgrade();
        paned.connect_position_notify(move |paned| {
            let Some(split) = weak.upgrade() else {
                return;
            };
            let imp = split.imp();
            // Allocation and minimum-size clamping do not change the user's ratio.
            if imp.allocating.get() || imp.available.get() <= 0 {
                return;
            }
            let ratio = (paned.position() as f64 / imp.available.get() as f64).clamp(0.1, 0.9);
            if imp.ratio.replace(ratio) != ratio {
                on_change(ratio);
            }
        });
        imp.paned.set(paned).unwrap();
        split.set_hexpand(true);
        split.set_vexpand(true);
        split.set_overflow(gtk::Overflow::Visible);
        split
    }

    pub fn paned(&self) -> &gtk::Paned {
        self.imp().paned.get().unwrap()
    }
}
