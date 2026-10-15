//! Organization and navigation requests served by execution clients.
use crate::client::model::{DaemonId, Session, WorkspaceCatalog};
use gmux_client::transport::Metadata;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Command {
    New {
        directory: Option<PathBuf>,
        /// Resolve relative paths against the CLI's directory only for local execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory_base: Option<PathBuf>,
        name: Option<String>,
        #[serde(
            default,
            deserialize_with = "workspace_selection",
            skip_serializing_if = "Option::is_none"
        )]
        workspace: Option<Option<u64>>,
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
    WorkspaceAdd {
        name: String,
        directory: Option<PathBuf>,
        #[serde(default)]
        directories: BTreeMap<DaemonId, PathBuf>,
        tab: Option<u64>,
    },
    WorkspaceEdit {
        workspace: u64,
        name: String,
        directory: Option<PathBuf>,
        directories: Option<BTreeMap<DaemonId, PathBuf>>,
    },
    WorkspaceRemove {
        workspace: u64,
    },
    Move {
        tab: Option<u64>,
        #[serde(deserialize_with = "Option::deserialize")]
        workspace: Option<u64>,
    },
    Rename {
        tab: u64,
        name: String,
    },
    RemoveTabName {
        tab: u64,
    },
}

// Missing means inherit the current filter; explicit null means unassigned.
fn workspace_selection<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<u64>>, D::Error> {
    Option::<u64>::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum InternalRequest {
    #[serde(rename = "_hello")]
    Hello,
    #[serde(rename = "_execution")]
    Execution {
        request: gmux_client::control::ExecutionRequest,
    },
    #[serde(rename = "_busy")]
    Busy { panes: Vec<u64> },
    #[serde(rename = "_command")]
    Command { command: Command },
    #[serde(rename = "_desktop-save")]
    DesktopSave {
        desktop: crate::client::model::DesktopState,
        revision: u64,
    },
    #[serde(rename = "_desktop-tab")]
    DesktopTab {
        tab: crate::client::model::DesktopTab,
    },
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
    DaemonInfo,
    Quit,
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
    Launch {
        connection: Option<String>,
        directory: Option<PathBuf>,
        directory_base: Option<PathBuf>,
        #[serde(default)]
        new_window: bool,
    },
    Focus {
        connection: Option<String>,
        pane: Option<u64>,
        tab: Option<u64>,
        workspace: Option<u64>,
    },
    Windows,
    NewWindow,
    Detach,
    RendererInfo {
        pane: Option<u64>,
    },
    SearchTabs {
        query: String,
    },
    Filter {
        workspace: Option<u64>,
        value: Option<String>,
    },
    Screenshot {
        path: PathBuf,
    },
    Resize {
        width: i32,
        height: i32,
    },
    Action {
        name: String,
    },
}

pub enum Request {
    Layout(Command),
    Internal(InternalRequest),
    Public(PublicRequest),
}
impl Request {
    pub fn decode(value: serde_json::Value) -> anyhow::Result<Self> {
        let method = value["method"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Request requires a method"))?;
        Ok(if Command::supports(method) {
            Self::Layout(serde_json::from_value(value)?)
        } else if method.starts_with('_') {
            Self::Internal(serde_json::from_value(value)?)
        } else {
            Self::Public(serde_json::from_value(value)?)
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandOutput {
    New {
        tab: u64,
        pane: u64,
        workspace: Option<u64>,
    },
    Split {
        pane: u64,
    },
    Workspace {
        workspace: u64,
    },
    Done {
        ok: bool,
    },
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

/// Worker results keep execution separate from GTK organization until the window combines them.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BackendReply {
    Execution {
        reply: gmux_client::control::CommandReply,
    },
    Organization {
        result: CommandResult,
    },
}

#[derive(Serialize, Deserialize)]
pub struct CommandReply {
    pub daemon: Option<crate::model::DaemonId>,
    pub home_directory: PathBuf,
    pub machine_kind: crate::model::MachineKind,
    pub machine_name: String,
    pub revision: u64,
    /// Highest ID allocated by the reply's daemon, including closed tabs and panes.
    pub last_id: u64,
    pub result: CommandResult,
    pub session: Session,
    pub catalog: WorkspaceCatalog,
    pub metadata: Vec<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<crate::client::model::DesktopState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<crate::model::Machine>,
}

impl Command {
    pub fn resolve_directory(&mut self, local: bool) -> anyhow::Result<()> {
        if let Self::New {
            directory,
            directory_base,
            ..
        } = self
        {
            if local && let Some(path) = directory {
                let expanded = gmux_client::model::expand_home(path);
                *path = if let Some(base) = directory_base.as_ref() {
                    std::path::absolute(base.join(expanded))?
                } else {
                    std::path::absolute(expanded)?
                };
            }
            *directory_base = None;
        }
        Ok(())
    }
    /// Convert after the client resolves workspace membership and its destination directory.
    pub fn into_execution(self) -> Result<gmux_client::control::Command, Self> {
        use gmux_client::control::Command as Execution;
        Ok(match self {
            Self::New {
                directory, name, ..
            } => Execution::New { directory, name },
            Self::Split { pane, down } => Execution::Split { pane, down },
            Self::Close { pane } => Execution::Close { pane },
            Self::CloseTab { tab } => Execution::CloseTab { tab },
            Self::Rename { tab, name } => Execution::Rename { tab, name },
            Self::RemoveTabName { tab } => Execution::RemoveTabName { tab },
            organization => return Err(organization),
        })
    }
    pub fn supports(method: &str) -> bool {
        matches!(
            method,
            "new"
                | "split"
                | "close"
                | "close-tab"
                | "workspace-add"
                | "workspace-edit"
                | "workspace-remove"
                | "move"
                | "rename"
                | "remove-tab-name"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_selection_distinguishes_inheritance_from_explicit_assignment() {
        for (request, expected) in [
            (json!({"method":"new"}), None),
            (json!({"method":"new","workspace":null}), Some(None)),
            (json!({"method":"new","workspace":7}), Some(Some(7))),
        ] {
            let command: Command = serde_json::from_value(request.clone()).unwrap();
            let Command::New { workspace, .. } = command else {
                panic!("Expected new")
            };
            assert_eq!(workspace, expected);
            let encoded = serde_json::to_value(Command::New {
                directory_base: None,
                directory: None,
                name: None,
                workspace,
            })
            .unwrap();
            assert_eq!(encoded.get("workspace"), request.get("workspace"));
        }
        assert!(serde_json::from_value::<Command>(json!({"method":"move"})).is_err());
        assert!(
            serde_json::from_value::<Command>(json!({"method":"new","workspace":"invalid"}))
                .is_err()
        );
    }
    #[test]
    fn client_sessions_round_trip_pane_directory_keys() {
        use crate::client::model::{Layout, TabState};
        use std::collections::HashMap;
        let session = Session {
            tabs: vec![TabState {
                daemon: DaemonId::generate().unwrap(),
                id: 1,
                created_at: 1,
                name: "Terminal".into(),
                custom_name: false,
                workspace: None,
                layout: Layout::Pane(2),
                directories: HashMap::from([(2, PathBuf::from("/tmp"))]),
                active: 2,
            }],
            active: 1,
            ..Session::default()
        };
        let wire = serde_json::to_value(&session).unwrap();
        let decoded: Session = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(decoded.tabs[0].directories[&2], PathBuf::from("/tmp"));
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        let mut unqualified = wire.clone();
        unqualified["tabs"][0]
            .as_object_mut()
            .unwrap()
            .remove("daemon");
        assert!(serde_json::from_value::<Session>(unqualified).is_err());
        let mut invalid = wire;
        invalid["tabs"][0]["directories"] = json!({"invalid":"/tmp"});
        assert!(serde_json::from_value::<Session>(invalid).is_err());
    }

    #[test]
    fn control_requests_reject_invalid_fields() {
        for wire in [
            json!({"method":"_focus","tab":"invalid"}),
            json!({"method":"_read","pane":-1}),
            json!({"method":"_busy","panes":["invalid"]}),
            json!({"method":"_command","command":{"method":"move"}}),
            json!({"method":"send","text":false}),
            json!({"method":"notify"}),
            json!({"method":"read","pane":"invalid"}),
            json!({"method":"send","text":"hello","paste":1}),
            json!({"method":"renderer-info","pane":-1}),
            json!({"method":"resize","width":800,"height":false}),
            json!({"method":"screenshot"}),
            json!({"method":"search-tabs","query":null}),
            json!({"method":"action","name":7}),
        ] {
            assert!(Request::decode(wire).is_err());
        }
    }
}
