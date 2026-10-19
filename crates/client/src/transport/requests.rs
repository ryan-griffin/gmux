//! Typed commands for the frontend stream.
use crate::{model::CursorStyle, theme::Theme};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize)]
pub struct StreamRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(flatten)]
    pub command: StreamCommand,
}

impl StreamRequest {
    pub fn connection(command: StreamCommand) -> Self {
        Self {
            pane: None,
            command,
        }
    }
    pub fn pane(pane: u64, command: StreamCommand) -> Self {
        Self {
            pane: Some(pane),
            command,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::connection(StreamCommand::Invalid {
            message: message.into(),
            attach: false,
        })
    }
    pub fn decode(bytes: &[u8]) -> Self {
        match serde_json::from_slice(bytes) {
            Ok(request) => request,
            Err(error) => {
                // Invalid requests still need their pane for error delivery, and
                // a malformed attach must disconnect just like a version mismatch.
                let raw: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
                Self {
                    pane: raw["pane"].as_u64(),
                    command: StreamCommand::Invalid {
                        message: format!("Invalid frontend command: {error}"),
                        attach: raw["method"] == "attach",
                    },
                }
            }
        }
    }
    pub fn is_attach(&self) -> bool {
        matches!(
            self.command,
            StreamCommand::Attach { .. } | StreamCommand::Invalid { attach: true, .. }
        )
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct PasteOptions {
    #[serde(default)]
    pub clipboard: bool,
    #[serde(default)]
    pub enter: bool,
    #[serde(default)]
    pub bottom: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum SelectionMode {
    #[default]
    Cell,
    Word,
    Line,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum StreamCommand {
    Attach {
        version: u32,
        daemon: crate::model::DaemonId,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        remote: bool,
    },
    Ping,
    Focused {
        active: bool,
    },
    Subscribe {
        panes: HashSet<u64>,
    },
    GitSubscribe {
        panes: HashSet<u64>,
    },
    MarkRead {
        through: u64,
    },
    Copy,
    Find {
        #[serde(default)]
        query: String,
    },
    FindAdvance {
        #[serde(default)]
        previous: bool,
    },
    Select {
        start: (u16, u32),
        end: (u16, u32),
        #[serde(default)]
        mode: SelectionMode,
        #[serde(default)]
        serial: u64,
    },
    ClearSelection {
        #[serde(default)]
        serial: u64,
    },
    ClearScreen,
    Input {
        bytes: Vec<u8>,
    },
    Paste {
        text: String,
        #[serde(flatten)]
        options: PasteOptions,
    },
    PasteBegin {
        size: usize,
        #[serde(flatten)]
        options: PasteOptions,
    },
    PasteData {
        text: String,
    },
    PasteEnd,
    Key {
        #[serde(default)]
        name: String,
        #[serde(default)]
        cp: u32,
        #[serde(default)]
        mods: u16,
        #[serde(default)]
        release: bool,
        #[serde(default)]
        text: String,
    },
    Mouse {
        position: (f32, f32),
        metrics: (u32, u32),
        #[serde(default)]
        action: i32,
        #[serde(default)]
        button: i32,
        #[serde(default)]
        mods: u16,
        #[serde(default)]
        pressed: bool,
    },
    Scroll {
        #[serde(default)]
        delta: isize,
        #[serde(default)]
        bottom: bool,
    },
    ScrollTo {
        row: u64,
        serial: u64,
    },
    Theme {
        theme: Theme,
    },
    Style {
        style: CursorStyle,
    },
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cw: u32,
        #[serde(default)]
        ch: u32,
        #[serde(default)]
        width: u16,
        #[serde(default)]
        height: u16,
    },
    #[serde(skip)]
    Invalid {
        message: String,
        attach: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_commands_keep_routing_and_reject_numeric_truncation() {
        for wire in [
            json!({"method":"key","pane":7,"mods":65536}),
            json!({"method":"key","pane":7,"cp":4294967296u64}),
            json!({"method":"resize","pane":7,"cols":65537,"rows":24}),
            json!({"method":"paste-data","pane":7,"text":false}),
            json!({"method":"scroll-to","pane":7,"row":-1,"serial":1}),
        ] {
            let request = StreamRequest::decode(&serde_json::to_vec(&wire).unwrap());
            assert_eq!(request.pane, Some(7));
            assert!(matches!(request.command, StreamCommand::Invalid { .. }));
        }
        assert!(StreamRequest::decode(br#"{"method":"attach","version":"six"}"#).is_attach());
        assert!(!StreamRequest::decode(b"invalid json").is_attach());
    }
}
