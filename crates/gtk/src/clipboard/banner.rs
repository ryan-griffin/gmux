use super::prompt::Prompt;
use gtk::{glib, prelude::*, subclass::prelude::*};
use std::cell::OnceCell;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Banner {
        pub revealer: OnceCell<gtk::Revealer>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Banner {
        const NAME: &'static str = "GmuxClipboardBanner";
        type Type = super::Banner;
        type ParentType = gtk::Box;

        fn class_init(class: &mut Self::Class) {
            // Keep AdwBanner's CSS structure so the installed theme supplies
            // its colors, padding and backdrop treatment without a CSS fork.
            class.set_css_name("banner");
            class.set_accessible_role(gtk::AccessibleRole::Group);
        }
    }

    impl ObjectImpl for Banner {}
    impl WidgetImpl for Banner {}
    impl BoxImpl for Banner {}
}

glib::wrapper! {
    pub struct Banner(ObjectSubclass<imp::Banner>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Banner {
    pub fn new() -> Self {
        let banner: Self = glib::Object::new();
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .hexpand(true)
            .build();
        revealer.connect_child_revealed_notify(|revealer| {
            if !revealer.reveals_child() && !revealer.is_child_revealed() {
                revealer.set_child(gtk::Widget::NONE);
                if let Some(banner) = revealer.parent() {
                    banner.update_relation(&[gtk::accessible::Relation::LabelledBy(&[])]);
                }
            }
        });
        banner.append(&revealer);
        banner.imp().revealer.set(revealer).unwrap();
        banner
    }

    pub fn present(&self, prompt: &Prompt) {
        self.imp().revealer.get().unwrap().set_child(Some(prompt));
        self.update_relation(&[gtk::accessible::Relation::LabelledBy(&[prompt
            .first_child()
            .unwrap()
            .upcast_ref()])]);
        self.imp().revealer.get().unwrap().set_reveal_child(true);
    }

    pub fn dismiss(&self) {
        self.imp().revealer.get().unwrap().set_reveal_child(false);
    }
}
