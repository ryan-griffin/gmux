use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

pub use gmux_platform::unix::paths::{expand_home, home};
pub use gmux_protocol::model::{
    CursorStyle, DaemonId, ExecutionSnapshot, ExecutionTab, Machine, MachineKind, PaneRef,
    PaneTree, TabRef, is_machine_directory,
};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Layout {
    Pane(PaneRef),
    Split {
        vertical: bool,
        first: Box<Layout>,
        second: Box<Layout>,
        ratio: f64,
    },
}
impl Layout {
    fn first_pane(&self) -> PaneRef {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }
    pub fn contains(&self, target: PaneRef) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }
    pub fn update_ratios(&mut self, saved: &Self) {
        // For unchanged topology, verify pane identity and grouping once, then
        // copy ratios without repeatedly collecting every subtree's panes.
        fn same_panes(layout: &Layout, saved: &Layout) -> bool {
            match (layout, saved) {
                (Layout::Pane(id), Layout::Pane(saved_id)) => id == saved_id,
                (
                    Layout::Split { first, second, .. },
                    Layout::Split {
                        first: saved_first,
                        second: saved_second,
                        ..
                    },
                ) => same_panes(first, saved_first) && same_panes(second, saved_second),
                _ => false,
            }
        }

        // Record each split's first-branch leaf count in preorder. A branch with
        // n leaves occupies n - 1 entries, so copying can skip mismatched branches.
        fn index(layout: &Layout, splits: &mut Vec<usize>) -> usize {
            match layout {
                Layout::Pane(_) => 1,
                Layout::Split { first, second, .. } => {
                    let split = splits.len();
                    splits.push(0);
                    let first_len = index(first, splits);
                    splits[split] = first_len;
                    first_len + index(second, splits)
                }
            }
        }

        fn copy(layout: &mut Layout, saved: &Layout, spans: Option<(&[usize], &[usize])>) {
            let (
                Layout::Split {
                    vertical,
                    first,
                    second,
                    ratio,
                },
                Layout::Split {
                    vertical: saved_vertical,
                    first: saved_first,
                    second: saved_second,
                    ratio: saved_ratio,
                },
            ) = (layout, saved)
            else {
                return;
            };
            if vertical != saved_vertical {
                return;
            }
            let (first_spans, second_spans) = if let Some((splits, saved_splits)) = spans {
                if splits[0] != saved_splits[0] {
                    return;
                }
                let (first, second) = splits[1..].split_at(splits[0] - 1);
                let (saved_first, saved_second) = saved_splits[1..].split_at(saved_splits[0] - 1);
                (Some((first, saved_first)), Some((second, saved_second)))
            } else {
                (None, None)
            };
            if saved_ratio.is_finite() {
                *ratio = saved_ratio.clamp(0.05, 0.95);
            }
            copy(first, saved_first, first_spans);
            copy(second, saved_second, second_spans);
        }

        let (
            Self::Split { vertical, .. },
            Self::Split {
                vertical: saved_vertical,
                ..
            },
        ) = (&*self, saved)
        else {
            return;
        };
        if vertical != saved_vertical {
            return;
        }
        if same_panes(self, saved) {
            copy(self, saved, None);
        } else {
            // Different grouping can still have matching ancestors. Compare the
            // full leaf order once, then use branch boundaries to check each split.
            let split_count = {
                let ids = self.ids();
                if ids != saved.ids() {
                    return;
                }
                ids.len() - 1
            };
            let mut splits = Vec::with_capacity(split_count);
            let mut saved_splits = Vec::with_capacity(split_count);
            index(self, &mut splits);
            index(saved, &mut saved_splits);
            copy(self, saved, Some((&splits, &saved_splits)));
        }
    }
    pub fn ids(&self) -> Vec<PaneRef> {
        fn collect(layout: &Layout, ids: &mut Vec<PaneRef>) {
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
    #[cfg(test)]
    pub fn split(&mut self, target: PaneRef, new_id: PaneRef, vertical: bool) -> bool {
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
    pub fn remove(self, target: PaneRef) -> Option<Self> {
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
    pub id: TabRef,
    pub created_at: u64,
    pub name: String,
    pub custom_name: bool,
    pub workspace: Option<u64>,
    pub layout: Layout,
    #[serde(with = "directories")]
    pub directories: HashMap<PaneRef, PathBuf>,
    pub active: PaneRef,
}

impl TabState {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_layout(self.id, &self.layout)?;
        anyhow::ensure!(self.layout.contains(self.active), "Invalid active pane");
        anyhow::ensure!(
            self.directories.keys().all(|id| self.layout.contains(*id)),
            "Invalid pane directory"
        );
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    // A client view assembled from execution and organization.
    pub tabs: Vec<TabState>,
    pub filter: TabFilter,
    pub active: Option<TabRef>,
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
        let mut layout = Layout::from_execution(daemon, &tab.layout);
        if let Some(saved) = saved {
            layout.update_ratios(&saved.layout);
        }
        let active = saved
            .filter(|saved| layout.contains(saved.active))
            .map_or_else(|| layout.first_pane(), |saved| saved.active);
        Self {
            id: TabRef { daemon, id: tab.id },
            created_at: tab.created_at,
            name: if tab.custom_name {
                tab.name.clone()
            } else {
                saved.map_or_else(|| tab.name.clone(), |saved| saved.name.clone())
            },
            custom_name: tab.custom_name,
            workspace: saved.and_then(|saved| saved.workspace),
            layout,
            directories: tab
                .directories
                .iter()
                .map(|(&id, path)| (PaneRef { daemon, id }, path.clone()))
                .collect(),
            active,
        }
    }
}

impl Layout {
    fn from_execution(daemon: DaemonId, tree: &PaneTree) -> Self {
        match tree {
            PaneTree::Pane(id) => Self::Pane(PaneRef { daemon, id: *id }),
            PaneTree::Split {
                vertical,
                first,
                second,
            } => Self::Split {
                vertical: *vertical,
                first: Box::new(Self::from_execution(daemon, first.as_ref())),
                second: Box::new(Self::from_execution(daemon, second.as_ref())),
                ratio: 0.5,
            },
        }
    }
}
/// A desktop layout belongs to exactly one daemon and cannot alias its tab ID.
pub fn validate_layout(tab: TabRef, layout: &Layout) -> anyhow::Result<()> {
    let ids = layout.ids();
    anyhow::ensure!(
        ids.iter().all(|pane| pane.daemon == tab.daemon),
        "Layout contains panes from another daemon"
    );
    gmux_protocol::model::validate_ids(tab.id, ids.into_iter().map(|pane| pane.id).collect())
}

mod directories {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &HashMap<PaneRef, PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(value.len()))?;
        for entry in value {
            seq.serialize_element(&entry)?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<PaneRef, PathBuf>, D::Error> {
        let entries = Vec::<(PaneRef, PathBuf)>::deserialize(deserializer)?;
        let mut result = HashMap::with_capacity(entries.len());
        for (pane, path) in entries {
            if result.insert(pane, path).is_some() {
                return Err(serde::de::Error::custom("Duplicate pane directory"));
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pane(id: u64) -> Layout {
        Layout::Pane(p(id))
    }
    fn p(id: u64) -> PaneRef {
        PaneRef {
            daemon: DaemonId::from_bytes([1; 16]),
            id,
        }
    }
    fn t(id: u64) -> TabRef {
        TabRef {
            daemon: DaemonId::from_bytes([1; 16]),
            id,
        }
    }

    fn split(first: Layout, second: Layout, vertical: bool, ratio: f64) -> Layout {
        Layout::Split {
            first: Box::new(first),
            second: Box::new(second),
            vertical,
            ratio,
        }
    }

    #[test]
    fn ratio_updates_require_exact_pane_identity_order_and_branch_boundaries() {
        let current = split(split(pane(1), pane(2), true, 0.4), pane(3), false, 0.3);
        let foreign = |id| {
            Layout::Pane(PaneRef {
                daemon: DaemonId::from_bytes([2; 16]),
                id,
            })
        };
        for saved in [
            split(
                split(foreign(1), foreign(2), true, 0.8),
                foreign(3),
                false,
                0.7,
            ),
            split(split(pane(1), pane(9), true, 0.8), pane(3), false, 0.7),
            split(split(pane(2), pane(1), true, 0.8), pane(3), false, 0.7),
            split(pane(1), split(pane(2), pane(3), true, 0.8), false, 0.7),
            split(split(pane(1), pane(2), true, 0.8), pane(3), true, 0.7),
            split(split(pane(1), pane(2), true, 0.8), pane(4), false, 0.7),
        ] {
            let mut updated = current.clone();
            updated.update_ratios(&saved);
            assert_eq!(updated, current, "saved: {saved:?}");
        }
    }

    #[test]
    fn regrouped_children_keep_matching_ancestor_and_sibling_ratios() {
        let changed = split(
            split(split(pane(1), pane(2), false, 0.5), pane(3), false, 0.5),
            pane(4),
            false,
            0.5,
        );
        let mut current = split(
            changed.clone(),
            split(pane(5), split(pane(6), pane(7), true, 0.5), false, 0.5),
            true,
            0.5,
        );
        let saved = split(
            split(
                pane(1),
                split(pane(2), split(pane(3), pane(4), false, 0.8), false, 0.8),
                false,
                0.8,
            ),
            split(pane(5), split(pane(6), pane(7), true, 0.6), false, 0.7),
            true,
            0.9,
        );
        current.update_ratios(&saved);
        assert_eq!(
            current,
            split(
                changed,
                split(pane(5), split(pane(6), pane(7), true, 0.6), false, 0.7),
                true,
                0.9,
            )
        );
    }

    #[test]
    fn differing_orientation_blocks_descendants_but_not_siblings() {
        let blocked = split(split(pane(1), pane(2), false, 0.4), pane(3), true, 0.3);
        let mut current = split(
            blocked.clone(),
            split(pane(4), pane(5), true, 0.5),
            false,
            0.5,
        );
        let saved = split(
            split(split(pane(1), pane(2), false, 0.8), pane(3), false, 0.7),
            split(pane(4), pane(5), true, 0.6),
            false,
            0.9,
        );
        current.update_ratios(&saved);
        assert_eq!(
            current,
            split(blocked, split(pane(4), pane(5), true, 0.6), false, 0.9)
        );
    }

    #[test]
    fn saved_ratios_clamp_finite_values_and_ignore_nonfinite_values() {
        for (saved_ratio, expected) in [
            (f64::NAN, 0.3),
            (f64::INFINITY, 0.3),
            (f64::NEG_INFINITY, 0.3),
            (-1.0, 0.05),
            (0.05, 0.05),
            (0.4, 0.4),
            (0.95, 0.95),
            (2.0, 0.95),
        ] {
            let mut current = split(split(pane(1), pane(2), true, 0.5), pane(3), false, 0.3);
            let saved = split(
                split(pane(1), pane(2), true, 0.8),
                pane(3),
                false,
                saved_ratio,
            );
            current.update_ratios(&saved);
            assert_eq!(
                current,
                split(split(pane(1), pane(2), true, 0.8), pane(3), false, expected),
                "saved ratio: {saved_ratio}"
            );
        }
    }

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
        let mut layout = pane(1);
        assert!(layout.split(p(1), p(2), false));
        assert!(layout.split(p(2), p(3), true));
        assert_eq!(layout.ids(), vec![p(1), p(2), p(3)]);
        let layout = layout.remove(p(2)).unwrap();
        assert_eq!(layout.ids(), vec![p(1), p(3)]);
        assert_eq!(layout.remove(p(1)).unwrap().ids(), vec![p(3)]);
    }
    #[test]
    fn tab_states_reject_duplicate_and_overflowing_ids() {
        let mut saved = TabState {
            id: t(1),
            created_at: 1,
            name: "Project".into(),
            workspace: None,
            custom_name: false,
            layout: pane(2),
            directories: HashMap::new(),
            active: p(2),
        };
        assert!(saved.validate().is_ok());
        for id in [0, 2, u64::MAX] {
            saved.id = t(id);
            assert!(saved.validate().is_err());
        }
        saved.id = t(1);
        saved.layout = Layout::Split {
            vertical: false,
            first: Box::new(pane(2)),
            second: Box::new(pane(2)),
            ratio: 0.5,
        };
        assert!(saved.validate().is_err());
        saved.layout = pane(u64::MAX);
        assert!(saved.validate().is_err());
    }
}
