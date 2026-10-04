//! Layout and workspace commands have one implementation in the daemon.
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
}

// Missing means inherit the current filter; explicit null means unassigned.
fn workspace_selection<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<u64>>, D::Error> {
    Option::<u64>::deserialize(deserializer).map(Some)
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
    fn typed_results_preserve_existing_wire_fields_and_save_warnings() {
        for wire in [
            json!({"tab":1,"pane":2,"workspace":null}),
            json!({"pane":3}),
            json!({"workspace":4}),
            json!({"ok":true,"warning":"Could not save layout"}),
        ] {
            let result: CommandResult = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(result).unwrap(), wire);
        }
    }
}
