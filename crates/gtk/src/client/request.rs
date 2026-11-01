//! Typed operations and destinations retained from the window to its request worker.
use super::{
    control::{Command, InternalRequest},
    model::{DaemonId, DesktopState, DesktopTab},
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

/// Requests here still use this window's handles. Routing resolves them once.
pub enum Request {
    Execution(execution::Request),
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
impl From<execution::PaneRequest> for Request {
    fn from(request: execution::PaneRequest) -> Self {
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

/// Exhaustively translate execution selectors before they leave the window.
pub fn translate_targets(
    request: &mut execution::Request,
    mut translate: impl FnMut(u64) -> anyhow::Result<u64>,
) -> anyhow::Result<()> {
    use execution::{
        Command, ExecutionRequest as E, InternalRequest as I, PaneRequest as P, PublicRequest as U,
        Request as R, TabCommand,
    };
    fn optional(
        id: &mut Option<u64>,
        translate: &mut impl FnMut(u64) -> anyhow::Result<u64>,
    ) -> anyhow::Result<()> {
        if let Some(id) = id {
            *id = translate(*id)?;
        }
        Ok(())
    }
    match request {
        R::Layout(Command::Tab(command))
        | R::Internal(I::Command {
            command: Command::Tab(command),
        }) => match command {
            TabCommand::Split { pane, .. } | TabCommand::Close { pane } => {
                optional(pane, &mut translate)?
            }
            TabCommand::CloseTab { tab }
            | TabCommand::Rename { tab, .. }
            | TabCommand::ResetName { tab } => *tab = translate(*tab)?,
        },
        R::Layout(Command::New { .. })
        | R::Internal(I::Command {
            command: Command::New { .. },
        }) => (),
        R::Execution(request) | R::Internal(I::Execution { request }) => match request {
            E::Split { pane, .. }
            | E::Close { pane }
            | E::Read { pane }
            | E::Links { pane }
            | E::Send { pane, .. }
            | E::Notify { pane, .. }
            | E::Metadata { pane }
            | E::CommandOutput { pane, .. }
            | E::Copy { pane, .. } => *pane = translate(*pane)?,
            E::CloseTab { tab } | E::Rename { tab, .. } | E::ResetName { tab } => {
                *tab = translate(*tab)?
            }
            E::Busy { panes } => {
                for pane in panes {
                    *pane = translate(*pane)?;
                }
            }
            E::New { .. } | E::State | E::List | E::Status | E::Scrollback { .. } | E::Stop => (),
        },
        R::Internal(I::Busy { panes }) => {
            for pane in panes {
                *pane = translate(*pane)?;
            }
        }
        R::Internal(I::Metadata { pane } | I::Copy { pane, .. } | I::Read { pane }) => {
            optional(pane, &mut translate)?
        }
        R::Public(U::Pane(request)) => match request {
            P::Metadata { pane }
            | P::CommandOutput { pane, .. }
            | P::Read { pane }
            | P::Links { pane }
            | P::Send { pane, .. }
            | P::Notify { pane, .. } => optional(pane, &mut translate)?,
        },
        R::Internal(I::Hello | I::State) | R::Public(U::Status | U::Stop | U::List) => (),
    }
    Ok(())
}
