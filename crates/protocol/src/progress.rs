//! Execution-owned OSC 9;4 progress, independent of terminal cells and toolkits.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    #[default]
    Inactive,
    Determinate,
    Error,
    Indeterminate,
    Paused,
}

/// Resolved current state. Indeterminate retains the preceding percentage for
/// a later error/paused report, but must not present it as a current estimate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireProgress")]
pub struct Progress {
    state: ProgressState,
    percentage: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProgress {
    state: ProgressState,
    percentage: u8,
}
impl TryFrom<WireProgress> for Progress {
    type Error = &'static str;
    fn try_from(value: WireProgress) -> Result<Self, Self::Error> {
        if value.percentage > 100 {
            return Err("Progress percentage exceeds 100");
        }
        if value.state == ProgressState::Inactive && value.percentage != 0 {
            return Err("Inactive progress must have zero percentage");
        }
        Ok(Self {
            state: value.state,
            percentage: value.percentage,
        })
    }
}
impl Progress {
    pub fn state(self) -> ProgressState {
        self.state
    }
    pub fn percentage(self) -> u8 {
        self.percentage
    }
    pub fn active(self) -> bool {
        self.state != ProgressState::Inactive
    }

    /// Apply a parsed upstream report. Values above 100 are clamped, as in
    /// Ghostty's parser. An omitted determinate value starts at zero.
    pub fn apply(&mut self, state: ProgressState, percentage: Option<u8>) {
        self.state = state;
        self.percentage = match state {
            ProgressState::Inactive => 0,
            ProgressState::Determinate => percentage.unwrap_or(0).min(100),
            ProgressState::Error | ProgressState::Paused => {
                percentage.map_or(self.percentage, |p| p.min(100))
            }
            ProgressState::Indeterminate => self.percentage,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProgress {
    pub pane: u64,
    pub progress: Progress,
}

/// Select an actual reporting pane, never an average. Error precedes paused,
/// determinate, and indeterminate. Ties use the lowest daemon-local pane ID.
pub fn representative(panes: impl IntoIterator<Item = (u64, Progress)>) -> Option<PaneProgress> {
    panes
        .into_iter()
        .filter(|(_, p)| p.active())
        .min_by_key(|(id, p)| {
            (
                match p.state {
                    ProgressState::Error => 0,
                    ProgressState::Paused => 1,
                    ProgressState::Determinate => 2,
                    ProgressState::Indeterminate => 3,
                    ProgressState::Inactive => unreachable!(),
                },
                *id,
            )
        })
        .map(|(pane, progress)| PaneProgress { pane, progress })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transitions_resolve_percentages_and_validate_wire_values() {
        use ProgressState::*;
        let mut p = Progress::default();
        for (state, value, expected) in [
            (Determinate, Some(42), 42),
            (Error, None, 42),
            (Paused, Some(81), 81),
            (Indeterminate, Some(12), 81),
            (Paused, None, 81),
            (Inactive, Some(99), 0),
            (Error, None, 0),
            (Determinate, None, 0),
            (Determinate, Some(255), 100),
            (Error, Some(0), 0),
            (Paused, Some(100), 100),
        ] {
            p.apply(state, value);
            assert_eq!((p.state(), p.percentage()), (state, expected));
            assert_eq!(
                serde_json::from_value::<Progress>(serde_json::to_value(p).unwrap()).unwrap(),
                p
            );
        }
        for invalid in [
            r#"{"state":"wat","percentage":0}"#,
            r#"{"state":"error","percentage":101}"#,
            r#"{"state":"inactive","percentage":1}"#,
            r#"{"state":"error","percentage":-1}"#,
            r#"{"state":"error"}"#,
            r#"{"state":2,"percentage":0}"#,
        ] {
            assert!(
                serde_json::from_str::<Progress>(invalid).is_err(),
                "{invalid}"
            );
        }
    }
    #[test]
    fn representative_is_stable_and_never_averages() {
        use ProgressState::*;
        let progress = |state, n| {
            let mut p = Progress::default();
            p.apply(state, Some(n));
            p
        };
        let panes = [
            (8, progress(Determinate, 80)),
            (3, progress(Indeterminate, 0)),
            (7, progress(Error, 12)),
            (6, progress(Paused, 23)),
            (4, progress(Error, 91)),
        ];
        assert_eq!(representative(panes).unwrap().pane, 4);
        assert_eq!(
            representative(panes.into_iter().rev())
                .unwrap()
                .progress
                .percentage(),
            91
        );
        assert_eq!(representative([(2, Progress::default())]), None);
    }
}
