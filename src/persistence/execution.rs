//! Daemon-owned execution.json record, also used by the runtime and disk worker.
use gmux_protocol::model::{ExecutionSnapshot, ExecutionTab};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedExecution {
    /// IDs are never reused, including after deletion and restart.
    pub(crate) last_id: u64,
    /// Pane IDs mapped to committed checkpoint filenames.
    pub(crate) scrollback: HashMap<u64, String>,
    pub(crate) tabs: Vec<ExecutionTab>,
}

impl SavedExecution {
    pub(crate) fn snapshot(&self) -> ExecutionSnapshot {
        ExecutionSnapshot {
            tabs: self.tabs.clone(),
        }
    }
}
