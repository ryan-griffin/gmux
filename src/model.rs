use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "gmux")
        .unwrap()
        .config_dir()
        .to_owned()
}
pub fn home() -> PathBuf {
    directories::BaseDirs::new().unwrap().home_dir().to_owned()
}
pub fn display_path(path: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home()) {
        if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        path.display().to_string()
    }
}
pub fn directory_name(path: &Path) -> String {
    if path == home() {
        return "~".into();
    }
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Layout {
    Pane(u64),
    Split {
        vertical: bool,
        first: Box<Layout>,
        second: Box<Layout>,
        ratio: f64,
    },
}
impl Layout {
    pub fn ids(&self) -> Vec<u64> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.ids();
                ids.extend(second.ids());
                ids
            }
        }
    }
    pub fn split(&mut self, target: u64, new_id: u64, vertical: bool) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    vertical,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new_id)),
                    ratio: 0.5,
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new_id, vertical) || second.split(target, new_id, vertical)
            }
        }
    }
    pub fn remove(self, target: u64) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                vertical,
                first,
                second,
                ratio,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    vertical,
                    first: Box::new(first),
                    second: Box::new(second),
                    ratio,
                }),
                (first, second) => first.or(second),
            },
        }
    }
}
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum CursorStyle {
    #[default]
    Bar = 0,
    Block = 1,
    Underline = 2,
    HollowBlock = 3,
}
impl CursorStyle {
    pub const ALL: [Self; 4] = [Self::Bar, Self::Block, Self::Underline, Self::HollowBlock];
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub use_system_font: bool,
    #[serde(skip)]
    pub system_font: String,
    pub font: String,
    pub font_size: f64,
    pub appearance: String,
    pub show_pane_titles: bool,
    pub cursor_style: CursorStyle,
    pub restore: bool,
    pub desktop_notifications: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            use_system_font: true,
            system_font: "Monospace 11".into(),
            font: "Monospace".into(),
            font_size: 11.0,
            appearance: "system".into(),
            show_pane_titles: true,
            cursor_style: CursorStyle::default(),
            restore: true,
            desktop_notifications: true,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub id: u64,
    pub name: String,
    pub custom_name: bool,
    pub workspace: Option<u64>,
    pub layout: Layout,
    pub directories: HashMap<u64, PathBuf>,
    pub active: u64,
}
impl SavedTab {
    pub fn validate(&self) -> anyhow::Result<()> {
        let ids = self.layout.ids();
        anyhow::ensure!(ids.len() <= 24, "Tab has too many panes");
        anyhow::ensure!(self.id > 0 && self.id < u64::MAX, "Invalid saved tab ID");
        let mut unique = HashSet::from([self.id]);
        for id in ids {
            anyhow::ensure!(id > 0 && id < u64::MAX, "Invalid saved pane ID");
            anyhow::ensure!(unique.insert(id), "Saved tab contains duplicate IDs");
        }
        Ok(())
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    pub tabs: Vec<SavedTab>,
    pub filter: TabFilter,
    pub active: u64,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabFilter {
    #[default]
    All,
    Unassigned,
    Workspace(u64),
}
impl TabFilter {
    pub fn matches(self, workspace: Option<u64>) -> bool {
        match self {
            Self::All => true,
            Self::Unassigned => workspace.is_none(),
            Self::Workspace(id) => workspace == Some(id),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct WorkspaceDefinition {
    pub id: u64,
    pub name: String,
    pub directory: PathBuf,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceCatalog {
    pub workspaces: Vec<WorkspaceDefinition>,
}
pub fn load<T: serde::de::DeserializeOwned + Default>(name: &str) -> T {
    match std::fs::read(config_dir().join(name)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            eprintln!("Could not read {name}: {error}");
            T::default()
        }),
        Err(_) => T::default(),
    }
}
pub fn save<T: Serialize>(name: &str, value: &T) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    let path = config_dir().join(name);
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unassigned_filter_matches_only_unassigned_tabs() {
        assert!(TabFilter::All.matches(None));
        assert!(TabFilter::All.matches(Some(2)));
        assert!(TabFilter::Unassigned.matches(None));
        assert!(!TabFilter::Unassigned.matches(Some(2)));
        assert!(TabFilter::Workspace(2).matches(Some(2)));
        assert!(!TabFilter::Workspace(2).matches(Some(3)));
        assert!(!TabFilter::Workspace(2).matches(None));
    }
    #[test]
    fn cursor_defaults_to_bar_and_choices_round_trip() {
        let settings = Settings::default();
        assert_eq!(settings.cursor_style as u8, CursorStyle::Bar as u8);
        for style in CursorStyle::ALL {
            let settings = Settings {
                cursor_style: style,
                ..Settings::default()
            };
            let json = serde_json::to_string(&settings).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.cursor_style as u8, style as u8);
        }
    }
    #[test]
    fn removing_a_nested_pane_keeps_its_sibling() {
        let mut layout = Layout::Pane(1);
        assert!(layout.split(1, 2, false));
        assert!(layout.split(2, 3, true));
        assert_eq!(layout.ids(), vec![1, 2, 3]);
        let layout = layout.remove(2).unwrap();
        assert_eq!(layout.ids(), vec![1, 3]);
        assert_eq!(layout.remove(1).unwrap().ids(), vec![3]);
    }
    #[test]
    fn saved_tabs_reject_duplicate_and_overflowing_ids() {
        let mut saved = SavedTab {
            id: 1,
            name: "Project".into(),
            workspace: None,
            custom_name: false,
            layout: Layout::Pane(2),
            directories: HashMap::new(),
            active: 2,
        };
        assert!(saved.validate().is_ok());
        for id in [0, 2, u64::MAX] {
            saved.id = id;
            assert!(saved.validate().is_err());
        }
        saved.id = 1;
        saved.layout = Layout::Split {
            vertical: false,
            first: Box::new(Layout::Pane(2)),
            second: Box::new(Layout::Pane(2)),
            ratio: 0.5,
        };
        assert!(saved.validate().is_err());
        saved.layout = Layout::Pane(u64::MAX);
        assert!(saved.validate().is_err());
    }
}
