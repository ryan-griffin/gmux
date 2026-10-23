//! Shell-reported state. Absent command text and exit codes remain unknown.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandPhase {
    #[default]
    Unknown,
    Prompt,
    Input,
    Running,
    Complete,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandState {
    pub phase: CommandPhase,
    pub at_prompt: bool,
    /// Supplied by the shell, never inferred from terminal text or process names.
    pub command: Option<String>,
    pub command_truncated: bool,
    /// Exit code of the last completed command, when that command reported one.
    pub last_exit_code: Option<i32>,
}
