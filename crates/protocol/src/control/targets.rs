//! Type-preserving target conversion at client and transport boundaries.
use super::*;

impl<P, T> ExecutionRequest<P, T> {
    pub fn map_ids<Q, U>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
        map_tab: &mut impl FnMut(T) -> anyhow::Result<U>,
    ) -> anyhow::Result<ExecutionRequest<Q, U>> {
        Ok(match self {
            Self::State => ExecutionRequest::State,
            Self::List => ExecutionRequest::List,
            Self::Status => ExecutionRequest::Status,
            Self::Scrollback {
                max_bytes,
                max_lines,
            } => ExecutionRequest::Scrollback {
                max_bytes,
                max_lines,
            },
            Self::Stop => ExecutionRequest::Stop,
            Self::New { directory, name } => ExecutionRequest::New { directory, name },
            Self::Split { pane, down } => ExecutionRequest::Split {
                pane: map_pane(pane)?,
                down,
            },
            Self::Close { pane } => ExecutionRequest::Close {
                pane: map_pane(pane)?,
            },
            Self::CloseTab { tab } => ExecutionRequest::CloseTab { tab: map_tab(tab)? },
            Self::Rename { tab, name } => ExecutionRequest::Rename {
                tab: map_tab(tab)?,
                name,
            },
            Self::ResetName { tab } => ExecutionRequest::ResetName { tab: map_tab(tab)? },
            Self::Read { pane } => ExecutionRequest::Read {
                pane: map_pane(pane)?,
            },
            Self::Links { pane } => ExecutionRequest::Links {
                pane: map_pane(pane)?,
            },
            Self::Send {
                pane,
                text,
                paste,
                enter,
            } => ExecutionRequest::Send {
                pane: map_pane(pane)?,
                text,
                paste,
                enter,
            },
            Self::Notify { pane, message } => ExecutionRequest::Notify {
                pane: map_pane(pane)?,
                message,
            },
            Self::Busy { panes } => ExecutionRequest::Busy {
                panes: panes
                    .into_iter()
                    .map(&mut *map_pane)
                    .collect::<anyhow::Result<_>>()?,
            },
            Self::Metadata { pane } => ExecutionRequest::Metadata {
                pane: map_pane(pane)?,
            },
            Self::CommandOutput {
                pane,
                point,
                formats,
            } => ExecutionRequest::CommandOutput {
                pane: map_pane(pane)?,
                point,
                formats,
            },
            Self::Copy { pane, formats } => ExecutionRequest::Copy {
                pane: map_pane(pane)?,
                formats,
            },
        })
    }
}

impl<P, T> Command<P, T> {
    pub fn map_ids<Q, U>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
        map_tab: &mut impl FnMut(T) -> anyhow::Result<U>,
    ) -> anyhow::Result<Command<Q, U>> {
        Ok(match self {
            Self::New { directory, name } => Command::New { directory, name },
            Self::Tab(value) => Command::Tab(value.map_ids(map_pane, map_tab)?),
        })
    }
}

impl<P, T> TabCommand<P, T> {
    pub fn map_ids<Q, U>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
        map_tab: &mut impl FnMut(T) -> anyhow::Result<U>,
    ) -> anyhow::Result<TabCommand<Q, U>> {
        Ok(match self {
            Self::Split { pane, down } => TabCommand::Split {
                pane: pane.map(&mut *map_pane).transpose()?,
                down,
            },
            Self::Close { pane } => TabCommand::Close {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
            Self::CloseTab { tab } => TabCommand::CloseTab { tab: map_tab(tab)? },
            Self::Rename { tab, name } => TabCommand::Rename {
                tab: map_tab(tab)?,
                name,
            },
            Self::ResetName { tab } => TabCommand::ResetName { tab: map_tab(tab)? },
        })
    }
}

impl<P, T> InternalRequest<P, T> {
    pub fn map_ids<Q, U>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
        map_tab: &mut impl FnMut(T) -> anyhow::Result<U>,
    ) -> anyhow::Result<InternalRequest<Q, U>> {
        Ok(match self {
            Self::Hello => InternalRequest::Hello,
            Self::Execution { request } => InternalRequest::Execution {
                request: request.map_ids(map_pane, map_tab)?,
            },
            Self::Busy { panes } => InternalRequest::Busy {
                panes: panes
                    .into_iter()
                    .map(&mut *map_pane)
                    .collect::<anyhow::Result<_>>()?,
            },
            Self::Command { command } => InternalRequest::Command {
                command: command.map_ids(map_pane, map_tab)?,
            },
            Self::State => InternalRequest::State,
            Self::Metadata { pane } => InternalRequest::Metadata {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
            Self::Copy { pane, formats } => InternalRequest::Copy {
                pane: pane.map(&mut *map_pane).transpose()?,
                formats,
            },
            Self::Read { pane } => InternalRequest::Read {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
        })
    }
}

impl<P> PublicRequest<P> {
    pub fn map_ids<Q>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
    ) -> anyhow::Result<PublicRequest<Q>> {
        Ok(match self {
            Self::Status => PublicRequest::Status,
            Self::Stop => PublicRequest::Stop,
            Self::List => PublicRequest::List,
            Self::Pane(value) => PublicRequest::Pane(value.map_ids(map_pane)?),
        })
    }
}

impl<P> PaneRequest<P> {
    pub fn map_ids<Q>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
    ) -> anyhow::Result<PaneRequest<Q>> {
        Ok(match self {
            Self::Metadata { pane } => PaneRequest::Metadata {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
            Self::CommandOutput {
                pane,
                point,
                formats,
            } => PaneRequest::CommandOutput {
                pane: pane.map(&mut *map_pane).transpose()?,
                point,
                formats,
            },
            Self::Read { pane } => PaneRequest::Read {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
            Self::Links { pane } => PaneRequest::Links {
                pane: pane.map(&mut *map_pane).transpose()?,
            },
            Self::Send {
                pane,
                text,
                paste,
                enter,
            } => PaneRequest::Send {
                pane: pane.map(&mut *map_pane).transpose()?,
                text,
                paste,
                enter,
            },
            Self::Notify { pane, message } => PaneRequest::Notify {
                pane: pane.map(&mut *map_pane).transpose()?,
                message,
            },
        })
    }
}

impl<P, T> Request<P, T> {
    pub fn map_ids<Q, U>(
        self,
        map_pane: &mut impl FnMut(P) -> anyhow::Result<Q>,
        map_tab: &mut impl FnMut(T) -> anyhow::Result<U>,
    ) -> anyhow::Result<Request<Q, U>> {
        Ok(match self {
            Self::Layout(value) => Request::Layout(value.map_ids(map_pane, map_tab)?),
            Self::Execution(value) => Request::Execution(value.map_ids(map_pane, map_tab)?),
            Self::Internal(value) => Request::Internal(value.map_ids(map_pane, map_tab)?),
            Self::Public(value) => Request::Public(value.map_ids(map_pane)?),
        })
    }
}
