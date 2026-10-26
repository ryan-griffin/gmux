//! Shared presentation of independent OSC 7501 status and OSC 9;4 progress.
//! Selection borrows metadata; it never changes reports or their lifetimes.
use crate::{
    program_status::{self, Summary},
    progress::{Progress, ProgressState},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bar {
    #[default]
    Hidden,
    Working(Option<u8>),
    Paused(Option<u8>),
    Error(Option<u8>),
}
impl Bar {
    pub fn percentage(self) -> Option<u8> {
        match self {
            Self::Hidden => None,
            Self::Working(p) | Self::Paused(p) | Self::Error(p) => p,
        }
    }
    pub fn description(self) -> String {
        let state = match self {
            Self::Hidden => return String::new(),
            Self::Working(_) => "In progress",
            Self::Paused(_) => "Paused",
            Self::Error(_) => "Error",
        };
        match self.percentage() {
            Some(p) => format!("{state}, {p}%"),
            None => format!("{state}, percentage unknown"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity<'a> {
    Program(&'a Summary),
    Progress(Progress),
}
impl<'a> Activity<'a> {
    /// A retained program status takes precedence within its own pane, including
    /// idle/results. Legacy progress must not obscure it or resurrect old work.
    #[inline]
    pub fn select(status: Option<&'a Summary>, progress: Progress) -> Option<Self> {
        status
            .map(Self::Program)
            .or_else(|| progress.active().then_some(Self::Progress(progress)))
    }
    #[inline]
    pub fn state(self) -> State {
        match self {
            Self::Program(status) => match status.record.state {
                program_status::State::Idle => State::Idle,
                program_status::State::Working => State::Working,
                program_status::State::Done => State::Done,
                program_status::State::Blocked => State::Blocked,
                program_status::State::Error => State::Error,
            },
            Self::Progress(p) => match p.state() {
                ProgressState::Determinate | ProgressState::Indeterminate => State::Working,
                ProgressState::Paused => State::Paused,
                ProgressState::Error => State::Error,
                ProgressState::Inactive => State::Idle,
            },
        }
    }
    #[inline]
    pub fn bar(self) -> Bar {
        match self {
            Self::Program(status) => match status.record.state {
                program_status::State::Working => Bar::Working(status.record.progress),
                program_status::State::Blocked => Bar::Paused(status.record.progress),
                program_status::State::Error => Bar::Error(None),
                program_status::State::Idle | program_status::State::Done => Bar::Hidden,
            },
            Self::Progress(p) => match p.state() {
                ProgressState::Inactive => Bar::Hidden,
                ProgressState::Determinate => Bar::Working(Some(p.percentage())),
                ProgressState::Indeterminate => Bar::Working(None),
                ProgressState::Error => Bar::Error(Some(p.percentage())),
                ProgressState::Paused => Bar::Paused(Some(p.percentage())),
            },
        }
    }
    pub fn description(self) -> String {
        match self {
            Self::Program(status) => program_status::description(status),
            Self::Progress(_) => self.bar().description(),
        }
    }
    #[inline]
    fn priority(self) -> (u8, bool) {
        let state = match self.state() {
            State::Blocked => 0,
            State::Error => 1,
            State::Paused => 2,
            State::Done => 3,
            State::Working => 4,
            State::Idle => 5,
        };
        // Preserve OSC 9;4's preference for determinate over indeterminate work.
        (state, matches!(self.bar(), Bar::Working(None)))
    }
}

pub struct PaneActivity<S> {
    pub pane: u64,
    status: Option<S>,
    progress: Progress,
}
impl<S: std::ops::Deref<Target = Summary>> PaneActivity<S> {
    #[inline]
    pub fn activity(&self) -> Activity<'_> {
        Activity::select(self.status.as_deref(), self.progress).unwrap()
    }
}

/// Select across both protocols with stable source attribution. The selected
/// status stays borrowed, even with many panes and maximum-size messages.
pub fn representative<S: std::ops::Deref<Target = Summary>>(
    panes: impl IntoIterator<Item = (u64, Option<S>, Progress)>,
) -> Option<PaneActivity<S>> {
    panes
        .into_iter()
        .filter_map(|(pane, status, progress)| {
            if status.is_none() && !progress.active() {
                return None;
            }
            Some(PaneActivity {
                pane,
                status,
                progress,
            })
        })
        .min_by_key(|p| (p.activity().priority(), p.pane))
}

#[cfg(test)]
mod tests {
    use super::*;
    use program_status::{Record, State as ProgramState};
    fn status(state: ProgramState, progress: Option<u8>) -> Summary {
        Summary {
            count: 1,
            record: Record {
                id: String::new(),
                state,
                kind: None,
                progress,
                app: None,
                title: String::new(),
                message: "Approve deployment?".into(),
            },
        }
    }
    fn progress(state: ProgressState, percentage: u8) -> Progress {
        let mut p = Progress::default();
        p.apply(state, Some(percentage));
        p
    }
    #[test]
    fn both_protocols_share_states_and_bar_without_inventing_completion_or_percentages() {
        for (state, bar, icon_state) in [
            (
                ProgressState::Determinate,
                Bar::Working(Some(100)),
                State::Working,
            ),
            (
                ProgressState::Indeterminate,
                Bar::Working(None),
                State::Working,
            ),
            (ProgressState::Paused, Bar::Paused(Some(100)), State::Paused),
            (ProgressState::Error, Bar::Error(Some(100)), State::Error),
        ] {
            let activity = Activity::select(None, progress(state, 100)).unwrap();
            assert_eq!((activity.state(), activity.bar()), (icon_state, bar));
        }
        for (state, percentage, bar) in [
            (ProgramState::Working, Some(42), Bar::Working(Some(42))),
            (ProgramState::Working, None, Bar::Working(None)),
            (ProgramState::Blocked, Some(37), Bar::Paused(Some(37))),
            (ProgramState::Blocked, None, Bar::Paused(None)),
            (ProgramState::Error, None, Bar::Error(None)),
            (ProgramState::Done, None, Bar::Hidden),
            (ProgramState::Idle, None, Bar::Hidden),
        ] {
            let s = status(state, percentage);
            let activity = Activity::select(Some(&s), progress(ProgressState::Error, 99)).unwrap();
            assert_eq!(activity.bar(), bar);
            assert!(!activity.description().contains("Pane "));
        }
        assert!(Activity::select(None, Progress::default()).is_none());
    }
    #[test]
    fn selection_preserves_sources_and_returns_to_legacy_when_program_records_clear() {
        let idle = status(ProgramState::Idle, None);
        let blocked = status(ProgramState::Blocked, None);
        let done = status(ProgramState::Done, None);
        let error = progress(ProgressState::Error, 30);
        let paused = progress(ProgressState::Paused, 40);
        let normal = progress(ProgressState::Determinate, 50);
        let panes = [
            (1, Some(&idle), error),
            (8, None, error),
            (5, Some(&blocked), normal),
            (3, None, paused),
            (4, Some(&done), normal),
        ];
        assert_eq!(representative(panes).unwrap().pane, 5);
        assert_eq!(representative(panes.into_iter().rev()).unwrap().pane, 5);
        assert_eq!(
            representative([panes[0], panes[1], panes[3], panes[4]])
                .unwrap()
                .pane,
            8
        );
        assert_eq!(
            representative([panes[0], panes[3], panes[4]]).unwrap().pane,
            3
        );
        let chosen = representative([(9, None::<&Summary>, normal), (2, None, normal)]).unwrap();
        assert_eq!(chosen.pane, 2);
        let unknown = progress(ProgressState::Indeterminate, 91);
        assert_eq!(
            representative([(1, None::<&Summary>, unknown), (7, None, normal)])
                .unwrap()
                .pane,
            7
        );
        let failed = status(ProgramState::Error, None);
        assert_eq!(
            representative([(9, Some(&failed), normal), (2, None, error)])
                .unwrap()
                .pane,
            2
        );
        assert_eq!(
            Activity::select(Some(&idle), error).unwrap().state(),
            State::Idle
        );
        assert_eq!(Activity::select(None, error).unwrap().state(), State::Error);
        assert_eq!(
            Activity::select(Some(&blocked), Progress::default())
                .unwrap()
                .state(),
            State::Blocked
        );
        assert!(representative([(1, None::<&Summary>, Progress::default())]).is_none());
    }
}
