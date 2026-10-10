//! Preferences and terminal settings.
use super::*;
use std::{
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
};

/// One settings snapshot and ordered writer for the entire application.
pub(super) struct AppPreferences {
    pub settings: Rc<RefCell<Settings>>,
    writer: SettingsWriter,
    desktop: Option<gio::Settings>,
    desktop_handler: RefCell<Option<glib::SignalHandlerId>>,
    source: RefCell<Option<glib::SourceId>>,
}

impl AppPreferences {
    pub fn new(owner: &Rc<Windows>) -> anyhow::Result<Rc<Self>> {
        let mut settings = storage::load::<Settings>("settings.json")?;
        let desktop = font::desktop_settings();
        if let Some(desktop) = &desktop {
            settings.system_font = desktop.string(font::KEY).to_string();
        }
        apply_appearance(&settings);
        let wake = ipc::Wake::new()?;
        let preferences = Rc::new(Self {
            settings: Rc::new(RefCell::new(settings)),
            writer: SettingsWriter::new(wake.clone()),
            desktop,
            desktop_handler: RefCell::new(None),
            source: RefCell::new(None),
        });
        let weak = Rc::downgrade(&preferences);
        let windows = Rc::downgrade(owner);
        *preferences.source.borrow_mut() = Some(glib_unix::unix_fd_add_local(
            wake.as_raw_fd(),
            glib::IOCondition::IN,
            move |_, _| {
                wake.drain();
                let Some(preferences) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                for error in preferences.writer.errors.try_iter() {
                    if let Some(ui) = windows.upgrade().and_then(|owner| owner.active()) {
                        ui.message(&error);
                    } else {
                        eprintln!("Could not save preferences: {error}");
                    }
                }
                glib::ControlFlow::Continue
            },
        ));
        if let Some(desktop) = &preferences.desktop {
            let weak = Rc::downgrade(&preferences);
            let windows = Rc::downgrade(owner);
            let handler = desktop.connect_changed(Some(font::KEY), move |desktop, _| {
                if let Some(preferences) = weak.upgrade() {
                    preferences.settings.borrow_mut().system_font =
                        desktop.string(font::KEY).to_string();
                    if preferences.settings.borrow().use_system_font
                        && let Some(owner) = windows.upgrade()
                    {
                        for ui in owner.windows.borrow().iter() {
                            ui.redraw_terminals();
                        }
                    }
                }
            });
            *preferences.desktop_handler.borrow_mut() = Some(handler);
        }
        Ok(preferences)
    }
}

impl Drop for AppPreferences {
    fn drop(&mut self) {
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
        if let Some(desktop) = &self.desktop
            && let Some(handler) = self.desktop_handler.get_mut().take()
        {
            desktop.disconnect(handler);
        }
    }
}

#[derive(Default)]
struct PendingSettings {
    snapshot: Option<Settings>,
    closed: bool,
}

struct SettingsWriter {
    pending: Arc<(Mutex<PendingSettings>, Condvar)>,
    errors: mpsc::Receiver<String>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SettingsWriter {
    fn new(wake: ipc::Wake) -> Self {
        let pending = Arc::new((Mutex::new(PendingSettings::default()), Condvar::new()));
        let writing = pending.clone();
        let (errors, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                let settings = {
                    let (lock, ready) = &*writing;
                    let mut pending = lock.lock().unwrap();
                    while pending.snapshot.is_none() && !pending.closed {
                        pending = ready.wait(pending).unwrap();
                    }
                    let Some(settings) = pending.snapshot.take() else {
                        break;
                    };
                    settings
                };
                if let Err(error) = storage::save("settings.json", &settings) {
                    let _ = errors.send(error.to_string());
                    wake.notify();
                }
            }
        });
        Self {
            pending,
            errors: receiver,
            worker: Some(worker),
        }
    }
    fn persist(&self, settings: Settings) -> anyhow::Result<()> {
        let (lock, ready) = &*self.pending;
        let mut pending = lock.lock().unwrap();
        anyhow::ensure!(!pending.closed, "Preferences writer is closed");
        // Keep the in-flight save and only the latest pending snapshot.
        pending.snapshot = Some(settings);
        ready.notify_one();
        Ok(())
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        // Finish queued saves before exiting, including after the last window closes.
        let (lock, ready) = &*self.pending;
        lock.lock().unwrap().closed = true;
        ready.notify_one();
        if self.worker.take().unwrap().join().is_err() {
            eprintln!("Preferences writer stopped unexpectedly");
        }
        for error in self.errors.try_iter() {
            eprintln!("Could not save preferences: {error}");
        }
    }
}

pub(super) struct PreferenceControls {
    dialog: adw::PreferencesDialog,
    updating: Rc<Cell<bool>>,
    theme: adw::ComboRow,
    pane_titles: adw::SwitchRow,
    cursor: adw::ComboRow,
    custom_font: adw::ExpanderRow,
    font: adw::ActionRow,
    desktop: adw::SwitchRow,
}

impl PreferenceControls {
    fn refresh(&self, settings: &Settings) {
        // Updating a row emits the same notifications as a user edit.
        self.updating.set(true);
        self.theme.set_selected(appearance_index(settings));
        self.pane_titles.set_active(settings.show_pane_titles);
        self.cursor.set_selected(settings.cursor_style as u32);
        self.custom_font
            .set_enable_expansion(!settings.use_system_font);
        self.font
            .set_title(&font::custom_description(settings).to_string());
        self.desktop.set_active(settings.desktop_notifications);
        self.updating.set(false);
    }
}

fn appearance_index(settings: &Settings) -> u32 {
    match settings.appearance.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    }
}

impl Ui {
    pub(super) fn preferences(self: &Rc<Self>) {
        if let Some(controls) = self.preferences_dialog.borrow().as_ref() {
            controls.dialog.present(Some(&self.window));
            return;
        }
        let updating = Rc::new(Cell::new(false));
        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        let page = adw::PreferencesPage::new();
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let themes = gtk::StringList::new(&["Follow system", "Light", "Dark"]);
        let theme = adw::ComboRow::builder()
            .title("Style")
            .model(&themes)
            .selected(appearance_index(&self.settings.borrow()))
            .build();
        let weak = Rc::downgrade(self);
        let syncing = updating.clone();
        theme.connect_selected_notify(move |row| {
            if !syncing.get()
                && let Some(ui) = weak.upgrade()
            {
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
        let syncing = updating.clone();
        pane_titles.connect_active_notify(move |row| {
            if !syncing.get()
                && let Some(ui) = weak.upgrade()
            {
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
        let syncing = updating.clone();
        cursor.connect_selected_notify(move |row| {
            if !syncing.get()
                && let Some(ui) = weak.upgrade()
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
        let syncing = updating.clone();
        custom_font.connect_enable_expansion_notify(move |row| {
            if !syncing.get()
                && let Some(ui) = weak.upgrade()
            {
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
        let notifications = adw::PreferencesGroup::builder()
            .title("Notifications")
            .build();
        let desktop = adw::SwitchRow::builder()
            .title("Desktop notifications")
            .subtitle("Send alerts from unattended terminals to the desktop")
            .active(self.settings.borrow().desktop_notifications)
            .build();
        let weak = Rc::downgrade(self);
        let syncing = updating.clone();
        desktop.connect_active_notify(move |row| {
            if !syncing.get()
                && let Some(ui) = weak.upgrade()
            {
                ui.settings.borrow_mut().desktop_notifications = row.is_active();
                ui.save_settings();
            }
        });
        notifications.add(&desktop);
        page.add(&notifications);
        dialog.add(&page);
        *self.preferences_dialog.borrow_mut() = Some(Rc::new(PreferenceControls {
            dialog: dialog.clone(),
            updating,
            theme,
            pane_titles,
            cursor,
            custom_font,
            font: font_row,
            desktop,
        }));
        let weak = Rc::downgrade(self);
        dialog.connect_closed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.preferences_dialog.borrow_mut().take();
            }
        });
        dialog.present(Some(&self.window));
    }
    pub(super) fn save_settings(&self) {
        apply_appearance(&self.settings.borrow());
        self.persist_settings();
        if let Some(owner) = self.owner.upgrade() {
            for ui in owner.windows.borrow().iter() {
                ui.redraw_terminals();
                if let Some(controls) = ui.preferences_dialog.borrow().as_ref() {
                    controls.refresh(&ui.settings.borrow());
                }
            }
        }
    }
    pub(super) fn persist_settings(&self) {
        if let Some(owner) = self.owner.upgrade()
            && let Some(preferences) = owner.preferences.borrow().as_ref()
            && let Err(error) = preferences.writer.persist(self.settings.borrow().clone())
        {
            self.message(&error.to_string());
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
