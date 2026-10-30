//! Typed execution results. Their serialization preserves the control JSON API.
use super::*;

/// The request selects the reply shape at the socket boundary. Diagnostic status
/// fields remain dynamic because they expose daemon and engine instrumentation.
#[derive(Serialize)]
#[serde(untagged)]
pub enum ExecutionReply {
    Snapshot(Box<CommandReply>),
    Outcome(CommandOutcome),
    List(ExecutionList),
    Status(serde_json::Value),
    Scrollback(ScrollbackReply),
    Stopped(StopReply),
    Done(Acknowledgement),
    Busy(BusyReply),
    Metadata(Box<PaneMetadata>),
    Content(crate::clipboard::Content),
    Links(PaneLinks),
}

#[derive(Serialize, Deserialize)]
pub struct Acknowledgement {
    pub ok: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct StopReply {
    pub ok: bool,
    pub warning: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct BusyReply {
    pub busy: bool,
}
#[derive(Serialize, Deserialize)]
pub struct PaneMetadata {
    pub meta: Metadata,
    pub program_status_records: Vec<crate::program_status::Record>,
}
#[derive(Serialize, Deserialize)]
pub struct PaneLinks {
    pub frame: crate::Frame,
    pub hyperlinks: crate::hyperlinks::Hyperlinks,
}
#[derive(Serialize, Deserialize)]
pub struct ScrollbackReply {
    pub limits: crate::retention::ScrollbackLimits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<PaneFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct PaneFailure {
    pub pane: u64,
    pub error: String,
}
#[derive(Serialize, Deserialize)]
pub struct ExecutionList {
    pub daemon: DaemonId,
    pub tabs: Vec<ListTab>,
}
#[derive(Serialize, Deserialize)]
pub struct ListTab {
    pub id: u64,
    pub created_at: u64,
    pub reference: TabRef,
    pub name: String,
    pub unread: bool,
    pub progress: Option<crate::progress::PaneProgress>,
    pub program_status: Option<crate::program_status::PaneStatus>,
    pub layout: PaneTree,
    pub panes: Vec<ListPane>,
}
#[derive(Serialize, Deserialize)]
pub struct ListPane {
    pub id: u64,
    pub reference: PaneRef,
    pub title: String,
    pub directory: PathBuf,
    pub exited: bool,
    pub busy: bool,
    pub command: crate::shell::CommandState,
    pub progress: crate::progress::Progress,
    pub program_status: Option<crate::program_status::Summary>,
    pub unread: bool,
}

impl ExecutionReply {
    pub fn into_snapshot(self) -> anyhow::Result<CommandReply> {
        match self {
            Self::Snapshot(reply) => Ok(*reply),
            _ => anyhow::bail!("Expected an execution snapshot reply"),
        }
    }
    pub fn into_stopped(self) -> anyhow::Result<StopReply> {
        match self {
            Self::Stopped(reply) => Ok(reply),
            _ => anyhow::bail!("Expected a daemon stop reply"),
        }
    }
    pub fn into_busy(self) -> anyhow::Result<bool> {
        match self {
            Self::Busy(reply) => Ok(reply.busy),
            _ => anyhow::bail!("Expected a busy reply"),
        }
    }
}

impl ExecutionRequest {
    /// Decode into the request's concrete reply, without constructing a JSON
    /// object tree for snapshots, clipboard content, or acknowledgements.
    pub fn decode_reply(&self, bytes: &[u8]) -> anyhow::Result<ExecutionReply> {
        self.decode_reply_from(bytes, None)
    }
    /// TCP peers must also match the identity established during authentication.
    pub fn decode_reply_from(
        &self,
        bytes: &[u8],
        expected: Option<DaemonId>,
    ) -> anyhow::Result<ExecutionReply> {
        crate::network::check_response_from(bytes, expected)?;
        self.decode_payload(bytes)
    }
    fn decode_payload(&self, bytes: &[u8]) -> anyhow::Result<ExecutionReply> {
        use ExecutionReply as Reply;
        Ok(match self {
            Self::State
            | Self::New { .. }
            | Self::Split { .. }
            | Self::Close { .. }
            | Self::CloseTab { .. }
            | Self::Rename { .. }
            | Self::ResetName { .. } => Reply::Snapshot(serde_json::from_slice(bytes)?),
            Self::List => Reply::List(serde_json::from_slice(bytes)?),
            Self::Status => Reply::Status(serde_json::from_slice(bytes)?),
            Self::Scrollback { .. } => Reply::Scrollback(serde_json::from_slice(bytes)?),
            Self::Stop => Reply::Stopped(serde_json::from_slice(bytes)?),
            Self::Send { .. } | Self::Notify { .. } => Reply::Done(serde_json::from_slice(bytes)?),
            Self::Busy { .. } => Reply::Busy(serde_json::from_slice(bytes)?),
            Self::Metadata { .. } => Reply::Metadata(serde_json::from_slice(bytes)?),
            Self::Read { .. } | Self::CommandOutput { .. } | Self::Copy { .. } => {
                Reply::Content(serde_json::from_slice(bytes)?)
            }
            Self::Links { .. } => Reply::Links(serde_json::from_slice(bytes)?),
        })
    }
}

impl Request {
    /// Select the control reply without resolving implicit targets. The daemon
    /// owns Unix target resolution; reply decoding never needs a fabricated ID.
    pub fn decode_reply(&self, bytes: &[u8]) -> anyhow::Result<ExecutionReply> {
        crate::network::check_response(bytes)?;
        use ExecutionReply as Reply;
        Ok(match self {
            Self::Execution(request) | Self::Internal(InternalRequest::Execution { request }) => {
                return request.decode_payload(bytes);
            }
            Self::Layout(_) => Reply::Outcome(serde_json::from_slice(bytes)?),
            Self::Internal(InternalRequest::Command { .. } | InternalRequest::State) => {
                Reply::Snapshot(serde_json::from_slice(bytes)?)
            }
            Self::Internal(InternalRequest::Busy { .. }) => {
                Reply::Busy(serde_json::from_slice(bytes)?)
            }
            Self::Internal(InternalRequest::Metadata { .. })
            | Self::Public(PublicRequest::Pane(PaneRequest::Metadata { .. })) => {
                Reply::Metadata(serde_json::from_slice(bytes)?)
            }
            Self::Internal(InternalRequest::Read { .. } | InternalRequest::Copy { .. })
            | Self::Public(PublicRequest::Pane(
                PaneRequest::Read { .. } | PaneRequest::CommandOutput { .. },
            )) => Reply::Content(serde_json::from_slice(bytes)?),
            Self::Public(PublicRequest::Pane(PaneRequest::Links { .. })) => {
                Reply::Links(serde_json::from_slice(bytes)?)
            }
            Self::Public(PublicRequest::Pane(
                PaneRequest::Send { .. } | PaneRequest::Notify { .. },
            )) => Reply::Done(serde_json::from_slice(bytes)?),
            Self::Public(PublicRequest::Status) => Reply::Status(serde_json::from_slice(bytes)?),
            Self::Public(PublicRequest::List) => Reply::List(serde_json::from_slice(bytes)?),
            Self::Public(PublicRequest::Stop) => Reply::Stopped(serde_json::from_slice(bytes)?),
            Self::Internal(InternalRequest::Hello) => {
                anyhow::bail!("Hello is part of connection setup")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_acknowledgements_are_not_daemon_stop_replies() {
        let reply = ExecutionReply::Done(Acknowledgement { ok: true });
        assert_eq!(
            reply.into_stopped().unwrap_err().to_string(),
            "Expected a daemon stop reply"
        );
    }
}
