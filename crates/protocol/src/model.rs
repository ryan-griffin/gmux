//! Shared execution snapshots, identities, and terminal appearance values.
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
mod state;
pub use state::{DaemonId, ExecutionState, ExecutionTab, PaneRef, PaneTree, TabRef};
pub fn is_machine_directory(path: &Path) -> bool {
    path.is_absolute() || path.starts_with("~")
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MachineKind {
    #[default]
    Unknown,
    Desktop,
    Laptop,
    Convertible,
    Server,
    Tablet,
    Phone,
    Watch,
    Embedded,
    VirtualMachine,
    Container,
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

pub fn pane_directories<'de, D: serde::Deserializer<'de>>(
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    pub daemon: DaemonId,
    pub machine_kind: MachineKind,
}

pub fn validate_ids(tab: u64, ids: Vec<u64>) -> anyhow::Result<()> {
    anyhow::ensure!(
        !ids.is_empty() && ids.len() <= 24,
        "Tab must contain 1 to 24 panes"
    );
    anyhow::ensure!(tab > 0 && tab < u64::MAX, "Invalid saved tab ID");
    let mut unique = HashSet::from([tab]);
    for id in ids {
        anyhow::ensure!(id > 0 && id < u64::MAX, "Invalid saved pane ID");
        anyhow::ensure!(unique.insert(id), "Saved tab contains duplicate IDs");
    }
    Ok(())
}
