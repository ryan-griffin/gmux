use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

mod machine;
pub use machine::{MachineKind, hostname};
mod state;
pub use state::{DaemonId, ExecutionState, ExecutionTab, PaneRef, PaneTree, TabRef};

pub fn home() -> PathBuf {
    directories::BaseDirs::new().unwrap().home_dir().to_owned()
}
/// Expand home paths on the machine that will run the terminal.
pub fn expand_home(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        home()
    } else if let Ok(relative) = path.strip_prefix("~") {
        home().join(relative)
    } else {
        path.to_owned()
    }
}
pub fn is_machine_directory(path: &Path) -> bool {
    path.is_absolute() || path.starts_with("~")
}
pub fn display_path(path: &Path) -> String {
    display_path_with_home(path, &home())
}
pub fn display_path_with_home(path: &Path, home: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home) {
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
