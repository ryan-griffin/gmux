//! Effects of owner-thread mutations, separate from control reply delivery.
use super::*;
use crate::persistence::SaveMode;
use gmux_protocol::{
    control::{PaneFailure, ScrollbackReply, StopReply},
    retention::ScrollbackLimits,
};

#[must_use]
pub(super) struct Mutation<T> {
    pub(super) value: T,
    pub(super) effects: Effects,
}
impl<T> Mutation<T> {
    pub(super) fn unchanged(value: T) -> Self {
        Self {
            value,
            effects: Effects::None,
        }
    }
}

// These variants specify the permitted order, rather than independent flags
// that could accidentally apply destructive settings before saving them.
#[must_use]
pub(super) enum Effects {
    None,
    /// The process/topology change is already live. Publish even if saving fails.
    Execution(SavedExecution),
    /// Save first, then publish the policy and prune history on the owner thread.
    ScrollbackSettings(ScrollbackLimits),
    /// Preserve topology while shutting down; checkpoint completion tracks durability.
    Shutdown(Save),
}
enum AfterSave {
    None,
    ScrollbackSettings(ScrollbackLimits),
    Shutdown,
}
pub(super) enum Completion {
    Saved,
    SaveFailed(String),
    Scrollback(ScrollbackReply),
}
pub(super) enum Progress {
    Complete(Completion),
    Saving(PendingEffects),
}
pub(super) struct PendingEffects {
    pub(super) id: u64,
    after_save: AfterSave,
}
impl Effects {
    pub(super) fn start(self, daemon: &mut Daemon, worker: &Worker, mode: SaveMode) -> Progress {
        let (save, after_save) = match self {
            Self::None => return Progress::Complete(Completion::Saved),
            Self::Execution(execution) => {
                // Revision identifies live topology, not disk completion or reply
                // delivery. Reads and streams must never see new topology at an
                // older revision while a command save is pending.
                daemon.revision += 1;
                (Save::Execution(execution), AfterSave::None)
            }
            Self::ScrollbackSettings(limits) => (
                Save::Settings(limits),
                AfterSave::ScrollbackSettings(limits),
            ),
            Self::Shutdown(save) => {
                daemon.quitting = true;
                (save, AfterSave::Shutdown)
            }
        };
        match worker.submit(save, mode) {
            Ok(id) => Progress::Saving(PendingEffects { id, after_save }),
            Err(error) => Progress::Complete(after_save.complete(daemon, Err(error.to_string()))),
        }
    }
}
impl PendingEffects {
    pub(super) fn complete(self, daemon: &mut Daemon, result: Result<(), String>) -> Completion {
        self.after_save.complete(daemon, result)
    }
}
impl AfterSave {
    fn complete(self, daemon: &mut Daemon, result: Result<(), String>) -> Completion {
        match self {
            Self::ScrollbackSettings(limits) => Completion::Scrollback(match result {
                Ok(()) => daemon.apply_scrollback_limits(limits),
                Err(error) => ScrollbackReply {
                    limits: daemon.limits,
                    saved: Some(false),
                    applied: Some(false),
                    failures: vec![],
                    error: Some(format!(
                        "Could not durably save scrollback settings; running limits and history are unchanged: {error}"
                    )),
                },
            }),
            Self::None | Self::Shutdown => {
                if matches!(self, Self::Shutdown) {
                    // Failed explicit checkpoints get another attempt at final
                    // shutdown, after preceding writes have drained.
                    daemon.checkpointed = result.is_ok();
                }
                match result {
                    Ok(()) => Completion::Saved,
                    Err(error) => Completion::SaveFailed(error),
                }
            }
        }
    }
}
impl Daemon {
    fn apply_scrollback_limits(
        &mut self,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> ScrollbackReply {
        // Publication precedes irreversible pruning. Never roll back a policy
        // after applying it to only part of the history.
        self.limits = limits;
        let mut failures = Vec::new();
        for (&id, session) in &mut self.sessions {
            if let Err(error) = session.set_scrollback_limits(limits) {
                failures.push(PaneFailure {
                    pane: id,
                    error: error.to_string(),
                });
            }
            self.changed.insert(id);
        }
        let applied = failures.is_empty();
        ScrollbackReply {
            limits, saved: Some(true), applied: Some(applied), failures,
            error: (!applied).then(|| "Settings were saved, but some terminals could not apply them. Removed history cannot be restored".into()),
        }
    }

    pub(super) fn execution_mutation<T>(&mut self, value: T) -> Mutation<T> {
        Mutation {
            value,
            effects: Effects::Execution(self.saved_execution()),
        }
    }

    pub(super) fn process_exits(&mut self, panes: impl Iterator<Item = u64>) -> Mutation<()> {
        // Shutdown checkpoints retain shells for restart, even when they exit
        // while a write is pending. No topology change means no new revision.
        if self.quitting {
            return Mutation::unchanged(());
        }
        let mut changed = false;
        for pane in panes {
            if self.sessions.contains_key(&pane) {
                self.close(pane);
                changed = true;
            }
        }
        if changed {
            self.execution_mutation(())
        } else {
            Mutation::unchanged(())
        }
    }

    pub(super) fn shutdown(&mut self) -> Mutation<StopReply> {
        let (save, warning) = match self.prepare_checkpoint() {
            Ok(checkpoint) => (Save::Checkpoint(checkpoint), None),
            Err(error) => (
                self.state_save(),
                Some(format!("Could not capture scrollback: {error}")),
            ),
        };
        Mutation {
            value: StopReply { ok: true, warning },
            effects: Effects::Shutdown(save),
        }
    }
}
