use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

pub use gmux_client::model::{
    CursorStyle, DaemonId, ExecutionState, ExecutionTab, Machine, MachineKind, PaneRef, PaneTree,
    TabRef, directory_name, display_path, expand_home, home, is_machine_directory,
};
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
    fn first_pane(&self) -> u64 {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }
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
#[derive(Clone, Serialize, Deserialize)]
pub struct TabState {
    pub daemon: DaemonId,
    pub id: u64,
    pub created_at: u64,
    pub name: String,
    pub custom_name: bool,
    pub workspace: Option<u64>,
    pub layout: Layout,
    #[serde(deserialize_with = "gmux_client::model::pane_directories")]
    pub directories: HashMap<u64, PathBuf>,
    pub active: u64,
}

impl TabState {
    pub fn validate(&self) -> anyhow::Result<()> {
        gmux_client::model::validate_ids(self.id, self.layout.ids())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    // A client view assembled from execution and organization.
    pub tabs: Vec<TabState>,
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
    Workspace(u64),
}
impl TabFilter {
    pub fn matches(self, workspace: Option<u64>) -> bool {
        match self {
            Self::All => true,
            Self::Workspace(id) => workspace == Some(id),
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDefinition {
    pub id: u64,
    pub name: String,
    pub directories: BTreeMap<DaemonId, PathBuf>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalog {
    pub last_id: u64,
    pub workspaces: Vec<WorkspaceDefinition>,
}
impl WorkspaceCatalog {
    pub fn reserve_id(&mut self) -> anyhow::Result<u64> {
        let last = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .chain(std::iter::once(self.last_id))
            .max()
            .unwrap_or(0);
        self.last_id = last
            .checked_add(1)
            .filter(|id| *id < u64::MAX)
            .ok_or_else(|| anyhow::anyhow!("No workspace IDs remain"))?;
        Ok(self.last_id)
    }
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
            directories: BTreeMap::new(),
        };
        let mut catalog = WorkspaceCatalog {
            last_id: 0,
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
    fn tab_states_reject_duplicate_and_overflowing_ids() {
        let mut saved = TabState {
            daemon: DaemonId::generate().unwrap(),
            id: 1,
            created_at: 1,
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopTab {
    pub reference: TabRef,
    pub workspace: Option<u64>,
    pub layout: Layout,
    pub active: PaneRef,
    pub name: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopState {
    pub tabs: Vec<DesktopTab>,
    pub filter: TabFilter,
    pub active: Option<TabRef>,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}
impl TabState {
    /// Saved presentation can decorate execution, but cannot create panes or change topology.
    pub fn from_execution(
        daemon: DaemonId,
        tab: &ExecutionTab,
        saved: Option<&DesktopTab>,
    ) -> Self {
        let mut layout = Layout::from(&tab.layout);
        if let Some(saved) = saved {
            layout.update_ratios(&saved.layout);
        }
        let active = saved
            .filter(|saved| saved.active.daemon == daemon && layout.contains(saved.active.id))
            .map_or_else(|| layout.first_pane(), |saved| saved.active.id);
        Self {
            daemon,
            id: tab.id,
            created_at: tab.created_at,
            name: if tab.custom_name {
                tab.name.clone()
            } else {
                saved.map_or_else(|| tab.name.clone(), |saved| saved.name.clone())
            },
            custom_name: tab.custom_name,
            workspace: saved.and_then(|saved| saved.workspace),
            layout,
            directories: tab.directories.clone(),
            active,
        }
    }
}

impl From<&PaneTree> for Layout {
    fn from(tree: &PaneTree) -> Self {
        match tree {
            PaneTree::Pane(id) => Self::Pane(*id),
            PaneTree::Split {
                vertical,
                first,
                second,
            } => Self::Split {
                vertical: *vertical,
                first: Box::new(Self::from(first.as_ref())),
                second: Box::new(Self::from(second.as_ref())),
                ratio: 0.5,
            },
        }
    }
}
impl From<&Layout> for PaneTree {
    fn from(layout: &Layout) -> Self {
        match layout {
            Layout::Pane(id) => Self::Pane(*id),
            Layout::Split {
                vertical,
                first,
                second,
                ..
            } => Self::Split {
                vertical: *vertical,
                first: Box::new(Self::from(first.as_ref())),
                second: Box::new(Self::from(second.as_ref())),
            },
        }
    }
}
