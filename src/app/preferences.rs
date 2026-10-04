//! Preferences and terminal settings.
use super::*;

impl Ui {
    pub(super) fn preferences(self: &Rc<Self>) {
        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        let page = adw::PreferencesPage::new();
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let themes = gtk::StringList::new(&["Follow system", "Light", "Dark"]);
        let theme = adw::ComboRow::builder()
            .title("Style")
            .model(&themes)
            .selected(match self.settings.borrow().appearance.as_str() {
                "light" => 1,
                "dark" => 2,
                _ => 0,
            })
            .build();
        let weak = Rc::downgrade(self);
        theme.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().appearance =
                    ["system", "light", "dark"][row.selected() as usize].into();
                ui.save_settings();
            }
        });
        appearance.add(&theme);
        let pane_titles = adw::SwitchRow::builder()
            .title("Pane Title Bars")
            .subtitle("Show a title and close button above each terminal")
            .active(self.settings.borrow().show_pane_titles)
            .build();
        let weak = Rc::downgrade(self);
        pane_titles.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().show_pane_titles = row.is_active();
                ui.save_settings();
            }
        });
        appearance.add(&pane_titles);
        let cursor = adw::ComboRow::builder()
            .title("Cursor Style")
            .subtitle("Applications may choose a different cursor style")
            .model(&gtk::StringList::new(&[
                "Bar",
                "Block",
                "Underline",
                "Hollow Block",
            ]))
            .selected(self.settings.borrow().cursor_style as u32)
            .build();
        let weak = Rc::downgrade(self);
        cursor.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade()
                && let Some(style) = model::CursorStyle::ALL.get(row.selected() as usize)
            {
                ui.settings.borrow_mut().cursor_style = *style;
                ui.save_settings();
            }
        });
        appearance.add(&cursor);
        page.add(&appearance);
        let font_group = adw::PreferencesGroup::new();
        let custom_font = adw::ExpanderRow::builder()
            .title("Custom Font")
            .show_enable_switch(true)
            .enable_expansion(!self.settings.borrow().use_system_font)
            .expanded(!self.settings.borrow().use_system_font)
            .build();
        let font_row = adw::ActionRow::builder()
            .title(font::custom_description(&self.settings.borrow()).to_string())
            .use_markup(false)
            .activatable(true)
            .build();
        font_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        custom_font.add_row(&font_row);
        let weak = Rc::downgrade(self);
        custom_font.connect_enable_expansion_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().use_system_font = !row.enables_expansion();
                ui.save_settings();
            }
        });
        let weak = Rc::downgrade(self);
        font_row.connect_activated(move |row| {
            if let Some(ui) = weak.upgrade() {
                let initial = font::custom_description(&ui.settings.borrow());
                let font_dialog = gtk::FontDialog::builder().title("Custom Font").build();
                let weak = Rc::downgrade(&ui);
                let row = row.downgrade();
                font_dialog.choose_font(
                    Some(&ui.window),
                    Some(&initial),
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let (Some(ui), Some(row), Ok(mut desc)) =
                            (weak.upgrade(), row.upgrade(), result)
                        {
                            let mut settings = ui.settings.borrow_mut();
                            settings.font_size =
                                (desc.size() as f64 / pango::SCALE as f64).clamp(6.0, 40.0);
                            desc.unset_fields(pango::FontMask::SIZE);
                            settings.font = desc.to_string().to_string();
                            row.set_title(&font::custom_description(&settings).to_string());
                            drop(settings);
                            ui.save_settings();
                        }
                    },
                );
            }
        });
        font_group.add(&custom_font);
        page.add(&font_group);
        let behavior = adw::PreferencesGroup::builder()
            .title("Tabs")
            .description("Reopening the window reconnects to running terminals. Restore tabs and directories with fresh shells after the background daemon stops.")
            .build();
        let restore = adw::SwitchRow::builder()
            .title("Restore tabs")
            .active(self.settings.borrow().restore)
            .build();
        let weak = Rc::downgrade(self);
        restore.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().restore = row.is_active();
                ui.save_settings();
            }
        });
        behavior.add(&restore);
        let desktop = adw::SwitchRow::builder()
            .title("Desktop notifications")
            .subtitle("Send alerts from unattended terminals to the desktop")
            .active(self.settings.borrow().desktop_notifications)
            .build();
        let weak = Rc::downgrade(self);
        desktop.connect_active_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                ui.settings.borrow_mut().desktop_notifications = row.is_active();
                ui.save_settings();
            }
        });
        behavior.add(&desktop);
        page.add(&behavior);
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }
    pub(super) fn save_settings(&self) {
        apply_appearance(&self.settings.borrow());
        self.persist_settings();
        self.redraw_terminals();
    }
    pub(super) fn persist_settings(&self) {
        if let Err(e) = self.backend.persist(self.settings.borrow().clone()) {
            self.message(&e.to_string());
        }
    }
    pub(super) fn redraw_terminals(&self) {
        for w in self.tabs.borrow().iter() {
            for p in w.panes.values() {
                p.refresh_settings();
            }
        }
    }
}

pub(super) fn apply_appearance(settings: &Settings) {
    adw::StyleManager::default().set_color_scheme(match settings.appearance.as_str() {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
}
