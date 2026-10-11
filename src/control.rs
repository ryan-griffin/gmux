//! Control requests shared by the CLI, frontend, and daemon.
use crate::{
    model::{Session, WorkspaceCatalog},
    transport::Metadata,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Command {
    New {
        directory: Option<PathBuf>,
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
    WorkspaceNew {
        name: String,
        directory: Option<PathBuf>,
        tab: Option<u64>,
    },
    WorkspaceEdit {
        workspace: u64,
        name: String,
        directory: PathBuf,
    },
    WorkspaceDelete {
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
    #[serde(rename = "_busy")]
    Busy { panes: Vec<u64> },
    #[serde(rename = "_command")]
    Command { command: Command },
    #[serde(rename = "_focus")]
    Focus { tab: u64, pane: Option<u64> },
    #[serde(rename = "_presentation")]
    Presentation { session: Session },
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
    Focus {
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
    pub(crate) fn mutates_state(&self) -> bool {
        matches!(
            self,
            Self::Layout(_)
                | Self::Internal(
                    InternalRequest::Command { .. }
                        | InternalRequest::Focus { .. }
                        | InternalRequest::Presentation { .. }
                )
                | Self::Public(PublicRequest::Focus { .. } | PublicRequest::Quit)
        )
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
#[derive(Serialize, Deserialize)]
pub struct CommandReply {
    pub revision: u64,
    pub result: CommandResult,
    pub session: Session,
    pub catalog: WorkspaceCatalog,
    pub metadata: Vec<Metadata>,
}

impl Command {
    pub fn supports(method: &str) -> bool {
        matches!(
            method,
            "new"
                | "split"
                | "close"
                | "close-tab"
                | "workspace-new"
                | "workspace-edit"
                | "workspace-delete"
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
    fn presentation_requests_round_trip_sessions_with_pane_directory_keys() {
        use crate::model::{Layout, SavedTab};
        use std::collections::HashMap;
        let session = Session {
            tabs: vec![SavedTab {
                id: 1,
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
        let wire = json!({"method":"_presentation","session":session});
        let request: InternalRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), wire);
        let Request::Internal(InternalRequest::Presentation { session }) =
            Request::decode(wire.clone()).unwrap()
        else {
            panic!("Expected presentation");
        };
        assert_eq!(session.tabs[0].directories[&2], PathBuf::from("/tmp"));
        let mut invalid = wire;
        invalid["session"]["tabs"][0]["directories"] = json!({"invalid":"/tmp"});
        assert!(Request::decode(invalid).is_err());
    }

    #[test]
    fn control_requests_classify_mutations_and_reject_invalid_fields() {
        for (wire, mutates) in [
            (json!({"method":"_hello"}), false),
            (json!({"method":"_read","pane":7}), false),
            (json!({"method":"send","pane":7,"text":"hello"}), false),
            (json!({"method":"notify","pane":7,"message":"hello"}), false),
            (json!({"method":"close","pane":7}), true),
            (
                json!({"method":"_command","command":{"method":"close","pane":7}}),
                true,
            ),
            (json!({"method":"_focus","tab":0}), true),
            (
                json!({"method":"_presentation","session":Session::default()}),
                true,
            ),
            (json!({"method":"focus","workspace":7}), true),
            (json!({"method":"quit"}), true),
            (
                json!({"method":"screenshot","path":"/tmp/window.png"}),
                false,
            ),
            (json!({"method":"resize","width":800,"height":600}), false),
        ] {
            assert_eq!(Request::decode(wire).unwrap().mutates_state(), mutates);
        }
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
