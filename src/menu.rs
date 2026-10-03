use gtk::{gdk, prelude::*};
use std::collections::HashSet;

pub fn setup_menu_button(button: &gtk::MenuButton, label: &'static str) {
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    let setup = move |button: &gtk::MenuButton| {
        if let Some(popover) = button.popover() {
            setup_popover(&popover, label);
        }
    };
    setup(button);
    button.connect_popover_notify(setup);
}

pub fn setup_split_button(button: &adw::SplitButton, label: &'static str) {
    let setup = move |button: &adw::SplitButton| {
        if let Some(popover) = button.popover() {
            setup_popover(&popover, label);
        }
    };
    setup(button);
    button.connect_popover_notify(setup);
}

pub fn setup_popover(popover: &impl IsA<gtk::Popover>, label: &str) {
    let popover = popover.as_ref();
    popover.update_property(&[gtk::accessible::Property::Label(label)]);
    popover.connect_show(|popover| label_items(popover.upcast_ref()));
}

pub fn context_menu_shortcut(key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
    key == gdk::Key::Menu
        || key == gdk::Key::F10 && modifiers.contains(gdk::ModifierType::SHIFT_MASK)
}

pub fn mnemonic_label(label: &str, used: &mut HashSet<char>) -> String {
    let available = |character: char| {
        character.is_alphanumeric() && !used.contains(&character.to_lowercase().next().unwrap())
    };
    let mnemonic = label
        .char_indices()
        .find(|&(_, character)| {
            // Prefer letters with a clear underline, as the HIG recommends.
            available(character) && !matches!(character.to_ascii_lowercase(), 'i' | 'l' | 'g' | 'y')
        })
        .or_else(|| {
            label
                .char_indices()
                .find(|&(_, character)| available(character))
        })
        .map(|(index, character)| {
            used.insert(character.to_lowercase().next().unwrap());
            index
        });
    let mut result = String::with_capacity(label.len() + 1);
    for (index, character) in label.char_indices() {
        if Some(index) == mnemonic || character == '_' {
            result.push('_');
        }
        result.push(character);
    }
    result
}

fn label_items(widget: &gtk::Widget) {
    // GTK 4.22 can leave model buttons with names derived from inaccessible
    // presentation labels. Use each native item's displayed text directly.
    if matches!(
        widget.accessible_role(),
        gtk::AccessibleRole::MenuItem
            | gtk::AccessibleRole::MenuItemCheckbox
            | gtk::AccessibleRole::MenuItemRadio
    ) && let Some(label) = item_label(widget)
    {
        widget.reset_relation(gtk::AccessibleRelation::LabelledBy);
        widget.update_property(&[gtk::accessible::Property::Label(&label)]);
        return;
    }
    let mut child = widget.first_child();
    while let Some(item) = child {
        child = item.next_sibling();
        label_items(&item);
    }
}

fn item_label(widget: &gtk::Widget) -> Option<glib::GString> {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        return Some(label.text());
    }
    let mut child = widget.first_child();
    while let Some(item) = child {
        if let Some(label) = item_label(&item) {
            return Some(label);
        }
        child = item.next_sibling();
    }
    None
}
