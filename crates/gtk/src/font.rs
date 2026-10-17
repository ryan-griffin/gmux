use crate::settings::Settings;
use gtk::{gio, pango};

pub const KEY: &str = "monospace-font-name";

pub fn desktop_settings() -> Option<gio::Settings> {
    let schema =
        gio::SettingsSchemaSource::default()?.lookup("org.gnome.desktop.interface", true)?;
    schema
        .has_key(KEY)
        .then(|| gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None))
}

pub fn description(settings: &Settings, zoom: f64) -> pango::FontDescription {
    let mut font = if settings.use_system_font {
        pango::FontDescription::from_string(&settings.system_font)
    } else {
        custom_description(settings)
    };
    if font.size() <= 0 {
        font.set_size(11 * pango::SCALE);
    }
    // Keep GNOME's point or pixel units and the full description, including style.
    // At normal zoom, use the exact system size rather than clamping it.
    if zoom != 0.0 {
        let size = (font.size() as f64 / pango::SCALE as f64 + zoom).max(1.0);
        if font.is_size_absolute() {
            font.set_absolute_size(size * pango::SCALE as f64);
        } else {
            font.set_size((size * pango::SCALE as f64).round() as i32);
        }
    }
    font
}

pub fn custom_description(settings: &Settings) -> pango::FontDescription {
    pango::FontDescription::from_string(&format!("{} {}", settings.font, settings.font_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_font_keeps_family_style_size_and_units() {
        let mut settings = Settings {
            system_font: "JetBrainsMono Nerd Font Bold 11".into(),
            ..Settings::default()
        };
        let font = description(&settings, 0.0);
        assert_eq!(font.family().as_deref(), Some("JetBrainsMono Nerd Font"));
        assert_eq!(font.weight(), pango::Weight::Bold);
        assert_eq!(font.size(), 11 * pango::SCALE);
        assert!(!font.is_size_absolute());
        assert_eq!(description(&settings, 2.0).size(), 13 * pango::SCALE);
        settings.system_font = "Monospace 14px".into();
        let zoomed = description(&settings, 2.0);
        assert!(zoomed.is_size_absolute());
        assert_eq!(zoomed.size(), 16 * pango::SCALE);
    }

    #[test]
    fn custom_font_overrides_system_font_when_enabled() {
        let mut settings = Settings {
            system_font: "Monospace 9".into(),
            font_size: 12.0,
            ..Settings::default()
        };
        assert_eq!(description(&settings, 0.0).size(), 9 * pango::SCALE);
        settings.use_system_font = false;
        assert_eq!(description(&settings, 0.0).size(), 12 * pango::SCALE);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["use_system_font"], false);
        assert!(saved.get("system_font").is_none());
    }
}
