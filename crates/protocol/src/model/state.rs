//! Daemon identity and persistent execution topology.
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

/// Persistent identity of one daemon's state directory, independent of address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DaemonId([u8; 16]);
impl DaemonId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Display for DaemonId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl Serialize for DaemonId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for DaemonId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut bytes = [0; 16];
        if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom("Invalid daemon identity"));
        }
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabRef {
    pub daemon: DaemonId,
    pub id: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneRef {
    pub daemon: DaemonId,
    pub id: u64,
}

/// Process topology has no desktop split proportions or selection.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum PaneTree {
    Pane(u64),
    Split {
        vertical: bool,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}
impl PaneTree {
    pub fn ids(&self) -> Vec<u64> {
        fn collect(tree: &PaneTree, ids: &mut Vec<u64>) {
            match tree {
                PaneTree::Pane(id) => ids.push(*id),
                PaneTree::Split { first, second, .. } => {
                    collect(first, ids);
                    collect(second, ids);
                }
            }
        }
        let mut ids = Vec::new();
        collect(self, &mut ids);
        ids
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTab {
    pub id: u64,
    /// Creation time in microseconds since the Unix epoch, retained across restarts.
    pub created_at: u64,
    pub name: String,
    pub custom_name: bool,
    pub layout: PaneTree,
    #[serde(deserialize_with = "super::pane_directories")]
    pub directories: HashMap<u64, PathBuf>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionState {
    /// IDs are never reused, including after a restart or deletion.
    pub last_id: u64,
    pub scrollback: HashMap<u64, String>,
    pub tabs: Vec<ExecutionTab>,
}

impl ExecutionTab {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_ids(self.id, self.layout.ids())
    }
}
impl PaneTree {
    pub fn contains(&self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }
    pub fn split(&mut self, target: u64, new_id: u64, vertical: bool) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    vertical,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new_id)),
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
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    vertical,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (first, second) => first.or(second),
            },
        }
    }
}
