//! Typed operations and destinations retained from the window to its request worker.
use super::{
    control::{Command, InternalRequest},
    model::{DaemonId, DesktopState, DesktopTab, PaneRef, TabRef},
};
use gmux_client::{connections, ipc};
use gmux_protocol::control as execution;

pub enum OrganizationRequest {
    Command(Command),
    Save {
        desktop: DesktopState,
        revision: u64,
    },
    Assign(DesktopTab),
}

/// Qualified identities stay intact until routing selects one daemon.
pub enum Request {
    Execution(execution::Request<PaneRef, TabRef>),
    Organization(OrganizationRequest),
    Configuration(connections::ConfigurationRequest),
}
impl From<InternalRequest> for Request {
    fn from(request: InternalRequest) -> Self {
        use execution::InternalRequest as E;
        let request = match request {
            InternalRequest::DesktopSave { desktop, revision } => {
                return Self::Organization(OrganizationRequest::Save { desktop, revision });
            }
            InternalRequest::DesktopTab { tab } => {
                return Self::Organization(OrganizationRequest::Assign(tab));
            }
            InternalRequest::Command { command } => match command.into_execution() {
                Ok(command) => E::Command { command },
                Err(command) => return Self::Organization(OrganizationRequest::Command(command)),
            },
            InternalRequest::Hello => E::Hello,
            InternalRequest::Execution { request } => E::Execution { request },
            InternalRequest::Busy { panes } => E::Busy { panes },
            InternalRequest::State => E::State,
            InternalRequest::Metadata { pane } => E::Metadata { pane },
            InternalRequest::Copy { pane } => E::Copy {
                pane,
                formats: Default::default(),
            },
            InternalRequest::Read { pane } => E::Read { pane },
        };
        Self::Execution(execution::Request::Internal(request))
    }
}
impl From<execution::PaneRequest<PaneRef>> for Request {
    fn from(request: execution::PaneRequest<PaneRef>) -> Self {
        Self::Execution(execution::Request::Public(execution::PublicRequest::Pane(
            request,
        )))
    }
}
impl From<connections::ConfigurationRequest> for Request {
    fn from(request: connections::ConfigurationRequest) -> Self {
        Self::Configuration(request)
    }
}

/// Remote names are selectors; a queued operation always pins their daemon identity.
pub enum Target {
    Local(DaemonId),
    Remote { name: String, daemon: DaemonId },
}
impl Target {
    pub fn daemon(&self) -> DaemonId {
        match self {
            Self::Local(id) | Self::Remote { daemon: id, .. } => *id,
        }
    }
    pub fn into_endpoint(self, local: ipc::Endpoint) -> ipc::Endpoint {
        match self {
            Self::Local(_) => local,
            Self::Remote { name, daemon } => ipc::Endpoint::remote(name, Some(daemon)),
        }
    }
}

pub enum RoutedRequest {
    Execution {
        target: Target,
        request: execution::Request,
    },
    Organization(OrganizationRequest),
    Configuration(connections::ConfigurationRequest),
}
