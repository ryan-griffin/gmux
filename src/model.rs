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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    pub fn update_ratios(&mut self, saved: &Self) {
        if let (
            Self::Split {
                vertical,
                first,
                second,
                ratio,
            },
            Self::Split {
                vertical: saved_vertical,
                first: saved_first,
                second: saved_second,
                ratio: saved_ratio,
            },
        ) = (self, saved)
            && vertical == saved_vertical
            && first.ids() == saved_first.ids()
            && second.ids() == saved_second.ids()
        {
            if saved_ratio.is_finite() {
                *ratio = saved_ratio.clamp(0.05, 0.95);
            }
            first.update_ratios(saved_first);
            second.update_ratios(saved_second);
        }
    }
    pub fn ids(&self) -> Vec<u64> {
        fn collect(layout: &Layout, ids: &mut Vec<u64>) {
            match layout {
                Layout::Pane(id) => ids.push(*id),
                Layout::Split { first, second, .. } => {
                    collect(first, ids);
                    collect(second, ids);
                }
            }
        }
        let mut ids = Vec::new();
        collect(self, &mut ids);
        ids
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
    pub show_sidebar: bool,
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
            show_sidebar: true,
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
#[derive(Clone, Default, Serialize, Deserialize)]
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
impl WorkspaceCatalog {
    pub fn validate_name(&self, name: &str, editing: Option<u64>) -> anyhow::Result<String> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "Enter a workspace name");
        anyhow::ensure!(
            !self
                .workspaces
                .iter()
                .any(|w| Some(w.id) != editing && w.name.eq_ignore_ascii_case(name)),
            "A workspace with that name already exists"
        );
        Ok(name.to_owned())
    }
}
pub fn load<T: serde::de::DeserializeOwned + Default>(name: &str) -> anyhow::Result<T> {
    load_at(&config_dir().join(name))
}
fn load_at<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    use anyhow::Context;
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!("Could not load {}. The file is unchanged. Repair it or move it aside to start with defaults", path.display())
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}
pub fn save<T: Serialize + serde::de::DeserializeOwned + Default>(
    name: &str,
    value: &T,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    let path = config_dir().join(name);
    save_at(&path, value)
}
fn save_at<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    value: &T,
) -> anyhow::Result<()> {
    // An autosave must not replace a file that became unreadable or invalid
    // after startup. Missing files are the only case that starts from defaults.
    let _: T = load_at(path)?;
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_state_uses_defaults_but_invalid_and_unreadable_state_is_preserved() {
        let path = std::env::temp_dir().join(format!(
            "gmux-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let file = path.join("session.json");
        assert!(load_at::<Session>(&file).unwrap().tabs.is_empty());
        assert!(load_at::<Session>(&path).is_err());
        for bytes in [b"{broken".as_slice(), b"{\"tabs\":\"invalid\"}".as_slice()] {
            std::fs::write(&file, bytes).unwrap();
            assert!(load_at::<Session>(&file).is_err());
            assert!(save_at(&file, &Session::default()).is_err());
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            assert!(!file.with_extension("tmp").exists());
        }
        std::fs::remove_dir_all(path).unwrap();
    }
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
