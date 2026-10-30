//! Execution requests independent of client organization and toolkit.
use crate::{model::ExecutionState, transport::Metadata};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Public commands whose CLI pane selector can be inherited from GMUX_PANE.
pub fn pane_method(method: &str) -> bool {
    matches!(
        method,
        "split" | "close" | "send" | "read" | "links" | "notify" | "metadata" | "command-output"
    )
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutionRequest {
    State,
    List,
    Status,
    Scrollback {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::retention::update_field"
        )]
        max_bytes: Option<crate::retention::Limit>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::retention::update_field"
        )]
        max_lines: Option<crate::retention::Limit>,
    },
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
    Links {
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
    CommandOutput {
        pane: u64,
        point: Option<(u16, u32)>,
        #[serde(default)]
        formats: crate::clipboard::Formats,
    },
    Copy {
        pane: u64,
        #[serde(default)]
        formats: crate::clipboard::Formats,
    },
}
impl ExecutionRequest {
    pub fn mutates_state(&self) -> bool {
        self.changes_layout()
            || matches!(
                self,
                Self::Stop
                    | Self::Scrollback {
                        max_bytes: Some(_),
                        ..
                    }
                    | Self::Scrollback {
                        max_lines: Some(_),
                        ..
                    }
            )
    }
    pub fn changes_layout(&self) -> bool {
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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Command {
    New {
        directory: Option<PathBuf>,
        name: Option<String>,
    },
    #[serde(untagged)]
    Tab(TabCommand),
}

/// Commands that act on an existing tab or one of its panes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TabCommand {
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
#[serde(tag = "method", deny_unknown_fields)]
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
    Copy {
        pane: Option<u64>,
        #[serde(default)]
        formats: crate::clipboard::Formats,
    },
    #[serde(rename = "_read")]
    Read { pane: Option<u64> },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum PublicRequest {
    Status,
    Stop,
    List,
    #[serde(untagged)]
    Pane(PaneRequest),
}

/// Input and inspection requests with the same options in every client.
#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum PaneRequest {
    Metadata {
        pane: Option<u64>,
    },
    CommandOutput {
        pane: Option<u64>,
        point: Option<(u16, u32)>,
        #[serde(default)]
        formats: crate::clipboard::Formats,
    },
    Read {
        pane: Option<u64>,
    },
    Links {
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
    pub fn returns_snapshot(&self) -> bool {
        match self {
            Self::Internal(InternalRequest::Command { .. } | InternalRequest::State) => true,
            Self::Execution(request) | Self::Internal(InternalRequest::Execution { request }) => {
                matches!(request, ExecutionRequest::State) || request.changes_layout()
            }
            _ => false,
        }
    }
    /// Normalize control requests, resolving and validating pane targets through
    /// the destination's policy. The boolean selects a public command result
    /// rather than a full execution snapshot.
    pub fn into_execution(
        self,
        mut resolve_pane: impl FnMut(Option<u64>) -> anyhow::Result<u64>,
    ) -> anyhow::Result<(ExecutionRequest, bool)> {
        let mut command = |command: Command| -> anyhow::Result<ExecutionRequest> {
            Ok(match command {
                Command::New { directory, name } => ExecutionRequest::New { directory, name },
                Command::Tab(command) => command.into_execution(&mut resolve_pane)?,
            })
        };
        Ok(match self {
            Self::Layout(value) => (command(value)?, true),
            Self::Internal(InternalRequest::Command { command: value }) => (command(value)?, false),
            Self::Execution(mut request)
            | Self::Internal(InternalRequest::Execution { mut request }) => {
                if let ExecutionRequest::New {
                    directory: Some(directory),
                    ..
                } = &request
                {
                    anyhow::ensure!(
                        crate::model::is_machine_directory(directory),
                        "Remote directories must be absolute paths or start with ~/ on the remote machine"
                    );
                }
                match &mut request {
                    ExecutionRequest::Split { pane: id, .. }
                    | ExecutionRequest::Close { pane: id }
                    | ExecutionRequest::Read { pane: id }
                    | ExecutionRequest::Links { pane: id }
                    | ExecutionRequest::Send { pane: id, .. }
                    | ExecutionRequest::Notify { pane: id, .. }
                    | ExecutionRequest::Metadata { pane: id }
                    | ExecutionRequest::CommandOutput { pane: id, .. }
                    | ExecutionRequest::Copy { pane: id, .. } => *id = resolve_pane(Some(*id))?,
                    _ => (),
                }
                (request, false)
            }
            Self::Internal(InternalRequest::State) => (ExecutionRequest::State, false),
            Self::Internal(InternalRequest::Busy { panes }) => {
                (ExecutionRequest::Busy { panes }, false)
            }
            Self::Internal(InternalRequest::Metadata { pane: id }) => (
                ExecutionRequest::Metadata {
                    pane: resolve_pane(id)?,
                },
                false,
            ),
            Self::Internal(InternalRequest::Copy { pane: id, formats }) => (
                ExecutionRequest::Copy {
                    pane: resolve_pane(id)?,
                    formats,
                },
                false,
            ),
            Self::Internal(InternalRequest::Read { pane: id }) => (
                ExecutionRequest::Read {
                    pane: resolve_pane(id)?,
                },
                false,
            ),
            Self::Internal(InternalRequest::Hello) => {
                anyhow::bail!("Hello is part of connection setup")
            }
            Self::Public(PublicRequest::Status) => (ExecutionRequest::Status, false),
            Self::Public(PublicRequest::Stop) => (ExecutionRequest::Stop, false),
            Self::Public(PublicRequest::List) => (ExecutionRequest::List, false),
            Self::Public(PublicRequest::Pane(request)) => {
                (request.into_execution(resolve_pane)?, false)
            }
        })
    }
    pub fn decode(value: serde_json::Value) -> anyhow::Result<Self> {
        let method = value["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Request requires a method"))?;
        Ok(if Command::supports(method) {
            Self::Layout(serde_json::from_value(value)?)
        } else if matches!(method, "state" | "busy" | "copy" | "scrollback") {
            Self::Execution(serde_json::from_value(value)?)
        } else if method.starts_with('_') {
            Self::Internal(serde_json::from_value(value)?)
        } else {
            Self::Public(serde_json::from_value(value)?)
        })
    }
    pub fn mutates_state(&self) -> bool {
        match self {
            Self::Execution(request) | Self::Internal(InternalRequest::Execution { request }) => {
                request.mutates_state()
            }
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

impl Command {
    pub fn supports(method: &str) -> bool {
        method == "new" || TabCommand::supports(method)
    }
}
impl TabCommand {
    pub fn supports(method: &str) -> bool {
        matches!(
            method,
            "split" | "close" | "close-tab" | "rename" | "reset-name"
        )
    }
    pub fn into_execution(
        self,
        mut resolve_pane: impl FnMut(Option<u64>) -> anyhow::Result<u64>,
    ) -> anyhow::Result<ExecutionRequest> {
        Ok(match self {
            Self::Split { pane, down } => ExecutionRequest::Split {
                pane: resolve_pane(pane)?,
                down,
            },
            Self::Close { pane } => ExecutionRequest::Close {
                pane: resolve_pane(pane)?,
            },
            Self::CloseTab { tab } => ExecutionRequest::CloseTab { tab },
            Self::Rename { tab, name } => ExecutionRequest::Rename { tab, name },
            Self::ResetName { tab } => ExecutionRequest::ResetName { tab },
        })
    }
}

impl PaneRequest {
    pub fn into_execution(
        self,
        mut resolve_pane: impl FnMut(Option<u64>) -> anyhow::Result<u64>,
    ) -> anyhow::Result<ExecutionRequest> {
        Ok(match self {
            Self::Metadata { pane } => ExecutionRequest::Metadata {
                pane: resolve_pane(pane)?,
            },
            Self::CommandOutput {
                pane,
                point,
                formats,
            } => ExecutionRequest::CommandOutput {
                pane: resolve_pane(pane)?,
                point,
                formats,
            },
            Self::Links { pane } => ExecutionRequest::Links {
                pane: resolve_pane(pane)?,
            },
            Self::Read { pane } => ExecutionRequest::Read {
                pane: resolve_pane(pane)?,
            },
            Self::Send {
                pane,
                text,
                paste,
                enter,
            } => ExecutionRequest::Send {
                pane: resolve_pane(pane)?,
                text,
                paste,
                enter,
            },
            Self::Notify { pane, message } => ExecutionRequest::Notify {
                pane: resolve_pane(pane)?,
                message,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_resolves_local_targets_and_requires_remote_targets() {
        for body in [
            json!({"method":"split"}),
            json!({"method":"_command","command":{"method":"split"}}),
            json!({"method":"read"}),
            json!({"method":"_read"}),
            json!({"method":"_execution","request":{"method":"read","pane":7}}),
        ] {
            let (request, _) = Request::decode(body.clone())
                .unwrap()
                .into_execution(|pane| Ok(pane.unwrap_or(7)))
                .unwrap();
            assert!(matches!(
                request,
                ExecutionRequest::Split { pane: 7, .. } | ExecutionRequest::Read { pane: 7 }
            ));
            let remote = Request::decode(body.clone())
                .unwrap()
                .into_execution(|pane| {
                    pane.ok_or_else(|| anyhow::anyhow!("Explicit pane required"))
                });
            assert_eq!(remote.is_ok(), body["method"] == "_execution");
        }
        for method in ["new", "_command", "_execution"] {
            let command = json!({"method":"new","directory":"relative"});
            let body = match method {
                "new" => command,
                "_command" => json!({"method":method,"command":command}),
                _ => json!({"method":method,"request":command}),
            };
            let normalized = Request::decode(body)
                .unwrap()
                .into_execution(|_| unreachable!());
            assert_eq!(normalized.is_ok(), method != "_execution");
        }
    }

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
