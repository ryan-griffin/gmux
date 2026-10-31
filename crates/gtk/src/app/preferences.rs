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
    pending: RefCell<Option<PendingPreferenceSave>>,
    desktop: Option<gio::Settings>,
    desktop_handler: RefCell<Option<glib::SignalHandlerId>>,
    source: RefCell<Option<glib::SourceId>>,
}

struct PendingPreferenceSave {
    local_daemon_enabled: bool,
    dirty: bool,
    callback: Box<dyn FnOnce(anyhow::Result<()>)>,
}

impl AppPreferences {
    pub fn new(owner: &Rc<Windows>) -> anyhow::Result<Rc<Self>> {
        let mut settings = storage::load::<Settings>("settings.json")?;
        let desktop = font::desktop_settings();
        if let Some(desktop) = &desktop {
            settings.system_font = desktop.string(font::KEY).to_string();
        }
        apply_appearance(&settings);
        let wake = gmux_platform::unix::ipc::Wake::new()?;
        let preferences = Rc::new(Self {
            settings: Rc::new(RefCell::new(settings)),
            writer: SettingsWriter::new(wake.clone()),
            pending: RefCell::new(None),
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
                for (confirmed, result) in preferences.writer.results.try_iter() {
                    if confirmed {
                        preferences.finish_save(result);
                    } else if let Err(error) = result {
                        if let Some(ui) = windows.upgrade().and_then(|owner| owner.active()) {
                            ui.message(&error);
                        } else {
                            eprintln!("{error}");
                        }
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

    fn persist(&self) {
        if let Some(pending) = self.pending.borrow_mut().as_mut() {
            // A later snapshot must include the outcome of the confirmed save.
            pending.dirty = true;
        } else {
            self.writer.persist(self.settings.borrow().clone(), false);
        }
    }

    pub(super) fn save_local_daemon_enabled(
        &self,
        enabled: bool,
        callback: impl FnOnce(anyhow::Result<()>) + 'static,
    ) {
        if self.pending.borrow().is_some() {
            callback(Err(anyhow::anyhow!("The local connection is changing")));
            return;
        }
        let mut settings = self.settings.borrow().clone();
        settings.local_daemon_enabled = enabled;
        *self.pending.borrow_mut() = Some(PendingPreferenceSave {
            local_daemon_enabled: enabled,
            dirty: false,
            callback: Box::new(callback),
        });
        self.writer.persist(settings, true);
    }

    fn finish_save(&self, result: Result<(), String>) {
        let pending = self.pending.borrow_mut().take().unwrap();
        if result.is_ok() {
            self.settings.borrow_mut().local_daemon_enabled = pending.local_daemon_enabled;
        }
        if pending.dirty {
            self.persist();
        }
        (pending.callback)(result.map_err(anyhow::Error::msg));
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
        if let Some(pending) = self.pending.get_mut().take()
            && pending.dirty
        {
            // The main loop may stop before the save completes. Flush other
            // preference changes with its outcome before the writer shuts down.
            for (confirmed, result) in &self.writer.results {
                if confirmed {
                    if result.is_ok() {
                        self.settings.borrow_mut().local_daemon_enabled =
                            pending.local_daemon_enabled;
                    }
                    self.writer.persist(self.settings.borrow().clone(), false);
                }
                if let Err(error) = result {
                    eprintln!("{error}");
                }
                if confirmed {
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
struct PendingSettings {
    snapshot: Option<(Settings, bool)>,
    closed: bool,
}

struct SettingsWriter {
    pending: Arc<(Mutex<PendingSettings>, Condvar)>,
    results: mpsc::Receiver<(bool, Result<(), String>)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SettingsWriter {
    fn new(wake: gmux_platform::unix::ipc::Wake) -> Self {
        let pending = Arc::new((Mutex::new(PendingSettings::default()), Condvar::new()));
        let writing = pending.clone();
        let (results, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                let (settings, confirmed) = {
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
                let result = storage::save("settings.json", &settings)
                    .map_err(|error| format!("Could not save preferences: {error}"));
                if confirmed || result.is_err() {
                    let _ = results.send((confirmed, result));
                    wake.notify();
                }
            }
        });
        Self {
            pending,
            results: receiver,
            worker: Some(worker),
        }
    }
    fn persist(&self, settings: Settings, confirmed: bool) {
        let (lock, ready) = &*self.pending;
        let mut pending = lock.lock().unwrap();
        // Keep the in-flight save and only the latest pending snapshot.
        pending.snapshot = Some((settings, confirmed));
        ready.notify_one();
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
        for (_, result) in self.results.try_iter() {
            if let Err(error) = result {
                eprintln!("{error}");
            }
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
        let themes = gtk::StringList::new(&["Follow System", "Light", "Dark"]);
        let theme = adw::ComboRow::builder()
            .title("Color Scheme")
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
            .subtitle("Show a title and close button above each pane")
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
            .title("Default Cursor Style")
            .subtitle("Shell prompts use a bar; applications may choose another style")
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
            .title("Desktop Notifications")
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
        {
            preferences.persist();
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
