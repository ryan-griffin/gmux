//! Daemon identity and client-facing execution topology.
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
        text.parse().map_err(serde::de::Error::custom)
    }
}
impl std::str::FromStr for DaemonId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 16];
        anyhow::ensure!(
            text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid daemon identity"
        );
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)?;
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
    pub fn max_id(&self) -> u64 {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, second, .. } => first.max_id().max(second.max_id()),
        }
    }

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
/// Live topology sent to clients. Persistence metadata belongs to the daemon.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSnapshot {
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

macro_rules! reference_text {
    ($reference:ty) => {
        impl std::fmt::Display for $reference {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}:{}", self.daemon, self.id)
            }
        }
        impl std::str::FromStr for $reference {
            type Err = anyhow::Error;
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                let (daemon, id) = text
                    .split_once(':')
                    .ok_or_else(|| anyhow::anyhow!("Expected daemon:id"))?;
                Ok(Self {
                    daemon: daemon.parse()?,
                    id: id.parse()?,
                })
            }
        }
    };
}
reference_text!(PaneRef);
reference_text!(TabRef);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_text_targets_preserve_identity_and_reject_malformed_selectors() {
        for (daemon, bytes) in [
            (
                "00112233445566778899aabbccddeeff",
                [
                    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                    0xdd, 0xee, 0xff,
                ],
            ),
            (
                "FFEEDDCCBBAA99887766554433221100",
                [
                    0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33,
                    0x22, 0x11, 0x00,
                ],
            ),
        ] {
            let text = format!("{daemon}:42");
            let pane: PaneRef = text.parse().unwrap();
            let tab: TabRef = text.parse().unwrap();
            let from_json: DaemonId = serde_json::from_value(serde_json::json!(daemon)).unwrap();
            let expected = DaemonId::from_bytes(bytes);
            assert_eq!(
                pane,
                PaneRef {
                    daemon: expected,
                    id: 42
                }
            );
            assert_eq!(
                tab,
                TabRef {
                    daemon: expected,
                    id: 42
                }
            );
            assert_eq!(from_json, expected);
            assert_eq!(pane.to_string().parse::<PaneRef>().unwrap(), pane);
            assert_eq!(tab.to_string().parse::<TabRef>().unwrap(), tab);
        }
        for text in [
            "42",
            ":42",
            "001122:42",
            "gg112233445566778899aabbccddeeff:42",
            "00112233445566778899aabbccddeeff:",
            "00112233445566778899aabbccddeeff:-1",
            "00112233445566778899aabbccddeeff:18446744073709551616",
            "00112233445566778899aabbccddeeff:42:1",
        ] {
            assert!(text.parse::<PaneRef>().is_err(), "{text}");
            assert!(text.parse::<TabRef>().is_err(), "{text}");
        }
    }
}
