use gtk::{glib, prelude::*, subclass::prelude::*};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FindField;

    #[glib::object_subclass]
    impl ObjectSubclass for FindField {
        const NAME: &'static str = "GmuxFindField";
        type Type = super::FindField;
        type ParentType = gtk::Box;

        fn class_init(class: &mut Self::Class) {
            // Use Adwaita's entry styling for the whole compound field,
            // including its native focus ring and error state.
            class.set_css_name("entry");
        }
    }

    impl ObjectImpl for FindField {}
    impl WidgetImpl for FindField {}
    impl BoxImpl for FindField {}
}

glib::wrapper! {
    pub struct FindField(ObjectSubclass<imp::FindField>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl FindField {
    pub fn new(text: &gtk::Text, count: &gtk::Label) -> Self {
        let field: Self = glib::Object::new();
        field.set_hexpand(true);
        field.add_css_class("terminal-find");
        field.append(&gtk::Image::from_icon_name("edit-find-symbolic"));
        field.append(text);
        field.append(count);
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text("Clear search")
            .focus_on_click(false)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        clear.add_css_class("flat");
        field.append(&clear);
        let entry = text.clone();
        clear.connect_clicked(move |_| {
            entry.set_text("");
            entry.grab_focus();
        });
        let weak = clear.downgrade();
        text.connect_changed(move |text| {
            if let Some(clear) = weak.upgrade() {
                clear.set_visible(!text.text().is_empty());
            }
        });
        field
    }
}
