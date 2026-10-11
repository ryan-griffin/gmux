use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

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
    pub fn contains(&self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }
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
#[serde(default, deny_unknown_fields)]
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
            show_pane_titles: false,
            cursor_style: CursorStyle::default(),
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
    #[serde(deserialize_with = "pane_directories")]
    pub directories: HashMap<u64, PathBuf>,
    pub active: u64,
}

fn pane_directories<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<u64, PathBuf>, D::Error> {
    // JSON object keys are strings. Parse IDs explicitly so this also works
    // when a tagged request buffers the session before deserializing it.
    HashMap::<String, PathBuf>::deserialize(deserializer)?
        .into_iter()
        .map(|(id, path)| {
            id.parse()
                .map(|id| (id, path))
                .map_err(serde::de::Error::custom)
        })
        .collect()
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
#[serde(deny_unknown_fields)]
pub struct Session {
    /// Daemon-owned, opaque checkpoint filenames.
    pub scrollback: HashMap<u64, String>,
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
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalog {
    pub workspaces: Vec<WorkspaceDefinition>,
}
impl WorkspaceCatalog {
    /// Keep the first valid definition for each ID without rewriting the file.
    pub fn retain_valid(&mut self) {
        let mut ids = HashSet::new();
        self.workspaces.retain(|workspace| {
            workspace.id > 0
                && workspace.id < u64::MAX
                && !workspace.name.trim().is_empty()
                && ids.insert(workspace.id)
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_validation_keeps_first_valid_ids_and_preserves_definition_order() {
        let definition = |id, name: &str| WorkspaceDefinition {
            id,
            name: name.into(),
            directory: PathBuf::from("/tmp"),
        };
        let mut catalog = WorkspaceCatalog {
            workspaces: vec![
                definition(0, "Zero"),
                definition(1, " "),
                definition(1, "First"),
                definition(2, "Second"),
                definition(1, "Duplicate"),
                definition(u64::MAX, "Overflow"),
            ],
        };
        catalog.retain_valid();
        assert_eq!(
            catalog
                .workspaces
                .iter()
                .map(|w| (w.id, w.name.as_str()))
                .collect::<Vec<_>>(),
            [(1, "First"), (2, "Second")]
        );
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
