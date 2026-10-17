//! Management lists with separate, temporary action dialogs for their editors.
use super::*;

pub(super) fn error_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    label.add_css_class("error");
    label
}

pub(super) fn show_error(label: &gtk::Label, message: impl std::fmt::Display) {
    let message = message.to_string();
    label.set_text(&message);
    label.set_visible(true);
    label.announce(&message, gtk::AccessibleAnnouncementPriority::High);
}

pub(super) fn list_dialog(
    title: &str,
    content: &adw::ToolbarView,
    focus: &impl IsA<gtk::Widget>,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(480)
        .content_height(480)
        .child(content)
        .build();
    dialog.set_focus(Some(focus));
    dialog
}

/// Track one editor independently of whether its management list is open.
#[derive(Default)]
pub(super) struct EditorDialog {
    dialog: Rc<glib::WeakRef<adw::Dialog>>,
}
impl EditorDialog {
    pub fn present(&self, parent: &impl IsA<gtk::Widget>) -> bool {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.present(Some(parent));
            true
        } else {
            false
        }
    }
    pub fn create(&self, title: &str, parent: &impl IsA<gtk::Widget>) -> Option<adw::Dialog> {
        if self.present(parent) {
            return None;
        }
        let dialog = adw::Dialog::builder()
            .title(title)
            .content_width(480)
            .content_height(480)
            .build();
        self.dialog.set(Some(&dialog));
        let current = self.dialog.clone();
        dialog.connect_closed(move |_| current.set(None));
        Some(dialog)
    }
}

/// Share request feedback without keeping a closed editor or its widgets alive.
#[derive(Clone)]
pub(super) struct EditorRequest {
    dialog: glib::WeakRef<adw::Dialog>,
    view: glib::WeakRef<adw::ToolbarView>,
    submit: glib::WeakRef<gtk::Button>,
    error: glib::WeakRef<gtk::Label>,
    label: glib::GString,
}
impl EditorRequest {
    pub fn new(
        dialog: &adw::Dialog,
        view: &adw::ToolbarView,
        submit: &gtk::Button,
        error: &gtk::Label,
    ) -> Self {
        Self {
            dialog: dialog.downgrade(),
            view: view.downgrade(),
            submit: submit.downgrade(),
            error: error.downgrade(),
            label: submit.label().unwrap_or_default(),
        }
    }
    pub fn begin(&self, label: Option<&str>) {
        if let Some(error) = self.error.upgrade() {
            error.set_visible(false);
        }
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_can_close(false);
        }
        if let Some(view) = self.view.upgrade() {
            view.set_sensitive(false);
        }
        if let Some(submit) = self.submit.upgrade()
            && let Some(label) = label
        {
            submit.set_label(label);
        }
    }
    pub fn restore(&self) {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_can_close(true);
        }
        if let Some(view) = self.view.upgrade() {
            view.set_sensitive(true);
        }
        if let Some(submit) = self.submit.upgrade() {
            submit.set_label(&self.label);
        }
    }
    pub fn finish(&self, result: anyhow::Result<()>) {
        self.restore();
        match result {
            Ok(()) => {
                if let Some(dialog) = self.dialog.upgrade() {
                    dialog.close();
                }
            }
            Err(message) => {
                if let Some(error) = self.error.upgrade() {
                    show_error(&error, message);
                }
            }
        }
    }
}

pub(super) fn editor_header(dialog: &adw::Dialog, label: &str) -> (adw::HeaderBar, gtk::Button) {
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let dialog = dialog.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(dialog) = dialog.upgrade() {
            dialog.close();
        }
    });
    header.pack_start(&cancel);
    let submit = gtk::Button::with_label(label);
    submit.add_css_class("suggested-action");
    header.pack_end(&submit);
    (header, submit)
}

pub(super) fn remove_action(page: &adw::PreferencesPage, label: &str) -> adw::ButtonRow {
    let group = adw::PreferencesGroup::new();
    let remove = adw::ButtonRow::builder().title(label).build();
    group.add(&remove);
    page.add(&group);
    remove
}
