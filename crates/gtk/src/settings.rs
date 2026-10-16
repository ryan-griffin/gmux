//! Preferences for the GTK frontend, independent of client organization.
use gmux_client::model::CursorStyle;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub local_daemon_enabled: bool,
    pub use_system_font: bool,
    #[serde(skip)]
    pub system_font: String,
    pub font: String,
    pub font_size: f64,
    pub appearance: String,
    pub show_sidebar: bool,
    pub show_pane_titles: bool,
    pub cursor_style: CursorStyle,
    pub desktop_notifications: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            local_daemon_enabled: false,
            use_system_font: true,
            system_font: "Monospace 11".into(),
            font: "Monospace".into(),
            font_size: 11.0,
            appearance: "system".into(),
            show_sidebar: true,
            show_pane_titles: false,
            cursor_style: CursorStyle::default(),
            desktop_notifications: true,
        }
    }
}
