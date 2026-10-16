//! Execution requests independent of client organization and toolkit.
use crate::{model::ExecutionState, transport::Metadata};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutionRequest {
    State,
    List,
    Status,
    Stop,
    New {
        directory: Option<PathBuf>,
        name: Option<String>,
    },
    Split {
        pane: u64,
        #[serde(default)]
        down: bool,
    },
    Close {
        pane: u64,
    },
    CloseTab {
        tab: u64,
    },
    Rename {
        tab: u64,
        name: String,
    },
    ResetName {
        tab: u64,
    },
    Read {
        pane: u64,
    },
    Send {
        pane: u64,
        text: String,
        #[serde(default)]
        paste: bool,
        #[serde(default)]
        enter: bool,
    },
    Notify {
        pane: u64,
        message: String,
    },
    Busy {
        panes: Vec<u64>,
    },
    Metadata {
        pane: u64,
    },
    Copy {
        pane: u64,
    },
}
impl ExecutionRequest {
    pub fn mutates_state(&self) -> bool {
        matches!(
            self,
            Self::New { .. }
                | Self::Split { .. }
                | Self::Close { .. }
                | Self::CloseTab { .. }
                | Self::Rename { .. }
                | Self::ResetName { .. }
        )
    }
    pub fn command(self) -> Result<Command, Self> {
        Ok(match self {
            Self::New { directory, name } => Command::New { directory, name },
            Self::Split { pane, down } => Command::Split {
                pane: Some(pane),
                down,
            },
            Self::Close { pane } => Command::Close { pane: Some(pane) },
            Self::CloseTab { tab } => Command::CloseTab { tab },
            Self::Rename { tab, name } => Command::Rename { tab, name },
            Self::ResetName { tab } => Command::ResetName { tab },
            request => return Err(request),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Command {
    New {
        directory: Option<PathBuf>,
        name: Option<String>,
    },
    Split {
        pane: Option<u64>,
        #[serde(default)]
        down: bool,
    },
    Close {
        pane: Option<u64>,
    },
    CloseTab {
        tab: u64,
    },
    Rename {
        tab: u64,
        name: String,
    },
    ResetName {
        tab: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum InternalRequest {
    #[serde(rename = "_hello")]
    Hello,
    #[serde(rename = "_execution")]
    Execution { request: ExecutionRequest },
    #[serde(rename = "_busy")]
    Busy { panes: Vec<u64> },
    #[serde(rename = "_command")]
    Command { command: Command },
    #[serde(rename = "_state")]
    State,
    #[serde(rename = "_metadata")]
    Metadata { pane: Option<u64> },
    #[serde(rename = "_copy")]
    Copy { pane: Option<u64> },
    #[serde(rename = "_read")]
    Read { pane: Option<u64> },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum PublicRequest {
    Status,
    Stop,
    List,
    Read {
        pane: Option<u64>,
    },
    Send {
        pane: Option<u64>,
        text: String,
        #[serde(default)]
        paste: bool,
        #[serde(default)]
        enter: bool,
    },
    Notify {
        pane: Option<u64>,
        message: String,
    },
}

pub enum Request {
    Layout(Command),
    Execution(ExecutionRequest),
    Internal(InternalRequest),
    Public(PublicRequest),
}
impl Request {
    /// Convert local control conveniences to execution commands. The boolean
    /// selects the public command result rather than a full execution snapshot.
    pub(crate) fn execution(self) -> anyhow::Result<(ExecutionRequest, bool)> {
        let pane = |pane: Option<u64>| {
            pane.ok_or_else(|| {
                anyhow::anyhow!("Remote pane requests require an explicit pane field")
            })
        };
        let command = |command: Command| -> anyhow::Result<ExecutionRequest> {
            Ok(match command {
                Command::New { directory, name } => ExecutionRequest::New { directory, name },
                Command::Split { pane: id, down } => ExecutionRequest::Split {
                    pane: pane(id)?,
                    down,
                },
                Command::Close { pane: id } => ExecutionRequest::Close { pane: pane(id)? },
                Command::CloseTab { tab } => ExecutionRequest::CloseTab { tab },
                Command::Rename { tab, name } => ExecutionRequest::Rename { tab, name },
                Command::ResetName { tab } => ExecutionRequest::ResetName { tab },
            })
        };
        Ok(match self {
            Self::Execution(request) => (request, false),
            Self::Layout(value) => (command(value)?, true),
            Self::Internal(InternalRequest::Command { command: value }) => (command(value)?, false),
            Self::Internal(InternalRequest::Execution { request }) => (request, false),
            Self::Internal(InternalRequest::State) => (ExecutionRequest::State, false),
            Self::Internal(InternalRequest::Busy { panes }) => {
                (ExecutionRequest::Busy { panes }, false)
            }
            Self::Internal(InternalRequest::Metadata { pane: id }) => {
                (ExecutionRequest::Metadata { pane: pane(id)? }, false)
            }
            Self::Internal(InternalRequest::Copy { pane: id }) => {
                (ExecutionRequest::Copy { pane: pane(id)? }, false)
            }
            Self::Internal(InternalRequest::Read { pane: id }) => {
                (ExecutionRequest::Read { pane: pane(id)? }, false)
            }
            Self::Internal(InternalRequest::Hello) => {
                anyhow::bail!("Hello is part of connection setup")
            }
            Self::Public(PublicRequest::Status) => (ExecutionRequest::Status, false),
            Self::Public(PublicRequest::Stop) => (ExecutionRequest::Stop, false),
            Self::Public(PublicRequest::List) => (ExecutionRequest::List, false),
            Self::Public(PublicRequest::Read { pane: id }) => {
                (ExecutionRequest::Read { pane: pane(id)? }, false)
            }
            Self::Public(PublicRequest::Send {
                pane: id,
                text,
                paste,
                enter,
            }) => (
                ExecutionRequest::Send {
                    pane: pane(id)?,
                    text,
                    paste,
                    enter,
                },
                false,
            ),
            Self::Public(PublicRequest::Notify { pane: id, message }) => (
                ExecutionRequest::Notify {
                    pane: pane(id)?,
                    message,
                },
                false,
            ),
        })
    }
    pub fn decode(value: serde_json::Value) -> anyhow::Result<Self> {
        let method = value["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Request requires a method"))?;
        Ok(if Command::supports(method) {
            Self::Layout(serde_json::from_value(value)?)
        } else if matches!(method, "state" | "busy" | "metadata" | "copy") {
            Self::Execution(serde_json::from_value(value)?)
        } else if method.starts_with('_') {
            Self::Internal(serde_json::from_value(value)?)
        } else {
            Self::Public(serde_json::from_value(value)?)
        })
    }
    pub fn mutates_state(&self) -> bool {
        match self {
            Self::Execution(request) => request.mutates_state(),
            Self::Internal(InternalRequest::Execution {
                request: ExecutionRequest::Stop,
            }) => true,
            Self::Internal(InternalRequest::Execution { request }) => request.mutates_state(),
            Self::Layout(_)
            | Self::Internal(InternalRequest::Command { .. })
            | Self::Public(PublicRequest::Stop) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandOutput {
    New { tab: u64, pane: u64 },
    Split { pane: u64 },
    Done { ok: bool },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResult {
    #[serde(flatten)]
    pub output: CommandOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
impl From<CommandOutput> for CommandResult {
    fn from(output: CommandOutput) -> Self {
        Self {
            output,
            warning: None,
        }
    }
}
#[derive(Serialize, Deserialize)]
pub struct CommandReply {
    pub daemon: crate::model::DaemonId,
    pub home_directory: PathBuf,
    pub machine_kind: crate::model::MachineKind,
    pub hostname: String,
    pub revision: u64,
    pub result: CommandResult,
    pub execution: ExecutionState,
    pub metadata: Vec<Metadata>,
}

/// A command result without an execution snapshot, for clients that only need IDs.
#[derive(Serialize, Deserialize)]
pub struct CommandOutcome {
    pub daemon: crate::model::DaemonId,
    #[serde(flatten)]
    pub result: CommandResult,
}

impl CommandOutcome {
    pub(crate) fn from_execution(mut reply: serde_json::Value) -> anyhow::Result<Self> {
        Ok(Self {
            daemon: serde_json::from_value(reply["daemon"].take())?,
            result: serde_json::from_value(reply["result"].take())?,
        })
    }
}

impl Command {
    pub fn supports(method: &str) -> bool {
        matches!(
            method,
            "new" | "split" | "close" | "close-tab" | "rename" | "reset-name"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn daemon_requests_reject_client_organization_and_presentation() {
        for body in [
            json!({"method":"workspace-add","name":"Project"}),
            json!({"method":"move","tab":1,"workspace":1}),
            json!({"method":"focus","pane":2}),
            json!({"method":"_desktop-save","desktop":{},"revision":0}),
            json!({"method":"_presentation","session":{}}),
            json!({"method":"new","workspace":1}),
            json!({"method":"_command","command":{"method":"new","workspace":null}}),
        ] {
            assert!(Request::decode(body).is_err());
        }
        assert!(Request::decode(json!({"method":"new","directory":"/tmp"})).is_ok());
        assert!(
            serde_json::from_value::<crate::model::PaneTree>(json!({"Split":{
            "vertical":false,"first":{"Pane":2},"second":{"Pane":3},"ratio":0.5}}))
            .is_err()
        );
    }
}
