//! Window command state machine. No widgets, callbacks, I/O, or duplicate execution cache.
//!
//! Connections projects accepted daemon revisions and current organization. The adapter
//! supplies a small selection at command/snapshot boundaries and executes these effects
//! through Backend. Viewport ticks do not build or update a window model.
use super::{
    connections::{Assignment, Route},
    control::{Command, CommandOutput, CommandReply, CommandResult, TabCommand},
    model::{DaemonId, DesktopTab, PaneRef, Session, TabFilter, TabRef, WorkspaceCatalog},
    request::RoutedRequest,
};
use std::collections::{HashMap, HashSet};
mod reconcile;
pub use reconcile::{Selection, TabSelection};
use reconcile::{preserve_window_state, remove_exited_panes};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandId(u64);

struct Pending {
    reveal: Option<u64>,
    creating: Option<DaemonId>,
    phase: Phase,
}
enum Phase {
    Executing(Option<Assignment>),
    Assigning(CommandResult),
    Applying,
}

/// A reply may need an organization save before the command can finish.
pub enum Effect {
    Assign {
        id: CommandId,
        tab: DesktopTab,
    },
    Finished {
        id: CommandId,
        update: anyhow::Result<Update>,
        refresh_on_failure: bool,
    },
}

/// An accepted presentation change. A stale reply still completes its command,
/// but must not replace newer rows. Revealing its surviving output is safe.
pub struct Update {
    pub result: CommandResult,
    pub state: Option<(Session, WorkspaceCatalog)>,
    pub reveal: bool,
    pub previous: Selection,
}

pub struct Controller {
    next: u64,
    revision: u64,
    focus_serial: u64,
    pending: HashMap<CommandId, Pending>,
    creating: HashMap<DaemonId, usize>,
    exits: HashSet<PaneRef>,
    restore: Option<(TabRef, Option<TabRef>, TabFilter)>,
    closed: bool,
}
impl Controller {
    pub fn new(revision: u64) -> Self {
        Self {
            next: 0,
            revision,
            focus_serial: 0,
            pending: HashMap::new(),
            creating: HashMap::new(),
            exits: HashSet::new(),
            restore: None,
            closed: false,
        }
    }

    /// Resolve native actions with the same qualified window targets as socket actions.
    pub fn resolve(
        command: &mut Command,
        active_tab: Option<TabRef>,
        active_pane: Option<PaneRef>,
        filter: TabFilter,
    ) -> anyhow::Result<()> {
        match command {
            Command::New { workspace, .. } if workspace.is_none() => {
                *workspace = Some(match filter {
                    TabFilter::All => None,
                    TabFilter::Workspace(id) => Some(id),
                });
            }
            Command::Tab(TabCommand::Split { pane, .. } | TabCommand::Close { pane })
                if pane.is_none() =>
            {
                *pane = Some(active_pane.ok_or_else(|| anyhow::anyhow!("No active terminal"))?);
            }
            Command::Move { tab, .. } if tab.is_none() => {
                *tab = Some(active_tab.ok_or_else(|| anyhow::anyhow!("No active tab"))?);
            }
            _ => (),
        }
        Ok(())
    }

    pub fn begin(&mut self, route: Route) -> anyhow::Result<(CommandId, RoutedRequest)> {
        anyhow::ensure!(!self.closed, "Window is closed");
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("No command IDs remain"))?;
        let id = CommandId(self.next);
        use gmux_protocol::control::{Command as E, InternalRequest, Request};
        let (reveal, creating) = match &route.request {
            RoutedRequest::Execution {
                target,
                request: Request::Internal(InternalRequest::Command { command }),
            } => match command {
                E::New { .. } => (true, Some(target.daemon())),
                E::Tab(gmux_protocol::control::TabCommand::Split { .. }) => (true, None),
                _ => (false, None),
            },
            _ => (false, None),
        };
        if let Some(daemon) = creating {
            *self.creating.entry(daemon).or_default() += 1;
        }
        let reveal = reveal.then(|| {
            self.focus_serial += 1;
            self.focus_serial
        });
        self.pending.insert(
            id,
            Pending {
                reveal,
                creating,
                phase: Phase::Executing(route.assign),
            },
        );
        Ok((id, route.request))
    }

    /// Both execution and assignment replies enter here, including dispatch failures.
    /// Unknown replies after teardown have no effects.
    pub fn reply(
        &mut self,
        id: CommandId,
        reply: anyhow::Result<CommandReply>,
        current: Selection,
    ) -> Option<Effect> {
        let pending = self.pending.get_mut(&id)?;
        if matches!(pending.phase, Phase::Applying) {
            return None;
        }
        let phase = std::mem::replace(&mut pending.phase, Phase::Applying);
        let reply = match (phase, reply) {
            (Phase::Executing(assign), Ok(mut reply)) => {
                if let Some((target, workspace)) =
                    assign.and_then(|a| a.resolve(&reply.result.output))
                    && let Some(saved) = reply.session.tabs.iter().find(|t| t.id == target)
                {
                    let tab = DesktopTab {
                        reference: saved.id,
                        workspace,
                        layout: saved.layout.clone(),
                        active: saved.active,
                        name: saved.name.clone(),
                    };
                    if let CommandOutput::New {
                        workspace: assigned,
                        ..
                    } = &mut reply.result.output
                    {
                        *assigned = workspace;
                    }
                    pending.phase = Phase::Assigning(reply.result);
                    return Some(Effect::Assign { id, tab });
                }
                Ok(reply)
            }
            (Phase::Assigning(output), Ok(mut reply)) => {
                reply.result = output;
                Ok(reply)
            }
            (_, Err(error)) => Err(error),
            (Phase::Applying, _) => unreachable!(),
        };
        let creating = pending.creating.take();
        let reveal = pending.reveal == Some(self.focus_serial);
        if let Some(daemon) = creating {
            let count = self.creating.get_mut(&daemon).unwrap();
            *count -= 1;
            if *count == 0 {
                self.creating.remove(&daemon);
            }
        }
        let update = reply.map(|reply| self.prepare(reply, reveal, current));
        let refresh_on_failure = creating.is_some();
        Some(Effect::Finished {
            id,
            update,
            refresh_on_failure,
        })
    }

    /// Acknowledge widget application and any refresh after a presentation failure.
    /// Keep exits until then so even a failed final command cannot resurrect panes
    /// from the cached execution used to release deferred rows.
    pub fn settled(&mut self, id: CommandId) {
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| matches!(pending.phase, Phase::Applying))
        {
            self.pending.remove(&id);
        }
        if self.pending.is_empty() {
            self.exits.clear();
        }
    }

    pub fn shared(&mut self, reply: CommandReply, current: Selection) -> Option<Update> {
        if self.closed || reply.revision <= self.revision {
            return None;
        }
        // Shared snapshots never reveal a command's output.
        Some(self.prepare(reply, false, current))
    }

    fn prepare(&mut self, mut reply: CommandReply, reveal: bool, mut current: Selection) -> Update {
        if reply.revision < self.revision {
            return Update {
                result: reply.result,
                state: None,
                reveal,
                previous: current,
            };
        }
        self.revision = reply.revision;
        remove_exited_panes(&mut reply, &self.exits);
        let revealed = match reply.result.output {
            CommandOutput::New { tab, .. } if reveal => Some(tab),
            _ => None,
        };
        reply.session.tabs.retain(|tab| {
            current.tabs.contains_key(&tab.id)
                || revealed == Some(tab.id)
                || !self.creating.contains_key(&tab.id.daemon)
        });
        if let Some((desired, fallback, filter)) = self.restore {
            if current.active != fallback || current.filter != filter {
                self.restore = None;
            } else if reply.session.tabs.iter().any(|t| t.id == desired) {
                current.active = Some(desired);
                self.restore = None;
            }
        }
        preserve_window_state(&mut reply.session, &reply.catalog, &current);
        Update {
            result: reply.result,
            state: Some((reply.session, reply.catalog)),
            reveal,
            previous: current,
        }
    }

    /// Explicit navigation invalidates pending reveals even if the user returns to
    /// their original selection. Adapter-driven focus restoration is not navigation.
    pub fn navigated(&mut self) {
        self.focus_serial += 1;
        self.restore = None;
    }
    pub fn restore_selection(
        &mut self,
        desired: TabRef,
        fallback: Option<TabRef>,
        filter: TabFilter,
    ) {
        self.restore = Some((desired, fallback, filter));
    }
    pub fn exited(&mut self, pane: PaneRef) {
        if !self.pending.is_empty() {
            self.exits.insert(pane);
        }
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.pending.clear();
        self.creating.clear();
        self.exits.clear();
        self.restore = None;
    }
}

#[cfg(test)]
mod tests;
