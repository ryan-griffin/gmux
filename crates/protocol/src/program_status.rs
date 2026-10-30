//! Retained OSC 7501 metadata. Parsing belongs to libghostty in the owner.
use serde::{Deserialize, Serialize};

pub const MAX_RECORDS: usize = 64;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_APP_BYTES: usize = 32;
pub const MAX_TITLE_BYTES: usize = 192;
pub const MAX_MESSAGE_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}
impl State {
    pub fn priority(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Error => 1,
            Self::Done => 2,
            Self::Working => 3,
            Self::Idle => 4,
        }
    }
    pub fn transient(self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }
    pub fn result(self) -> bool {
        matches!(self, Self::Done | Self::Error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub state: State,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: String,
    pub message: String,
}
impl Record {
    pub fn heap_bytes(&self) -> usize {
        self.id.capacity()
            + self.app.as_ref().map_or(0, String::capacity)
            + self.title.capacity()
            + self.message.capacity()
    }
    pub fn validate(&self) -> bool {
        self.id.len() <= MAX_ID_BYTES
            && self.app.as_ref().is_none_or(|s| s.len() <= MAX_APP_BYTES)
            && self.title.len() <= MAX_TITLE_BYTES
            && self.message.len() <= MAX_MESSAGE_BYTES
            && self.progress.is_none_or(|p| p <= 100)
            && (self.state.transient() || self.progress.is_none())
            && (self.state == State::Blocked || self.kind.is_none())
    }
}

/// One representative, with its app resolved through the nearest ancestor.
/// Complete records are available from the explicit metadata request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireSummary")]
pub struct Summary {
    pub record: Record,
    pub count: usize,
}
#[derive(Deserialize)]
struct WireSummary {
    record: Record,
    count: usize,
}
impl TryFrom<WireSummary> for Summary {
    type Error = &'static str;
    fn try_from(value: WireSummary) -> Result<Self, Self::Error> {
        if !(1..=MAX_RECORDS).contains(&value.count) || !value.record.validate() {
            return Err("Invalid program status summary");
        }
        Ok(Self {
            record: value.record,
            count: value.count,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PaneStatus {
    pub pane: u64,
    pub status: Summary,
}

/// Stable pane attribution. Borrow candidates and copy only the selected record.
pub fn representative<S: std::ops::Deref<Target = Summary>>(
    panes: impl IntoIterator<Item = (u64, Option<S>)>,
) -> Option<PaneStatus> {
    panes
        .into_iter()
        .filter_map(|(pane, status)| status.map(|status| (pane, status)))
        .min_by_key(|(pane, status)| (status.record.state.priority(), *pane))
        .map(|(pane, status)| PaneStatus {
            pane,
            status: (*status).clone(),
        })
}

/// Reports are plain text. Disarm Unicode formatting outside the terminal grid,
/// including bidi controls, zero-width characters, fillers and variation selectors.
pub fn display_text(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            let formatting = matches!(c as u32,
            0x00ad | 0x034f | 0x0600..=0x0605 | 0x061c | 0x06dd | 0x070f
            | 0x0890..=0x0891 | 0x08e2 | 0x115f..=0x1160 | 0x17b4..=0x17b5
            | 0x180b..=0x180f | 0x200b..=0x200f | 0x2028..=0x202e | 0x2060..=0x206f
            | 0x3164 | 0xfe00..=0xfe0f | 0xfeff | 0xffa0 | 0xfff9..=0xfffb
            | 0x110bd | 0x110cd | 0x13430..=0x1343f | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a | 0xe0000..=0xe0fff);
            !c.is_control() && !formatting
        })
        .collect()
}

/// Plain presentation text; pane identity remains in structured metadata.
pub fn description(summary: &Summary) -> String {
    let record = &summary.record;
    let state = match (record.state, record.kind) {
        (State::Idle, _) => "Idle",
        (State::Working, _) => "Working",
        (State::Done, _) => "Finished",
        (State::Error, _) => "Failed",
        (State::Blocked, Some(Kind::Permission)) => "Approval needed",
        (State::Blocked, Some(Kind::Question)) => "Answer needed",
        (State::Blocked, Some(Kind::Auth)) => "Authentication needed",
        (State::Blocked, None) => "Blocked",
    };
    let mut text = state.to_owned();
    if let Some(p) = record.progress {
        text.push_str(&format!(", {p}%"));
    }
    for value in [
        record.app.as_deref().unwrap_or(""),
        if record.title.is_empty() {
            &record.id
        } else {
            &record.title
        },
        &record.message,
    ] {
        let value = display_text(value);
        if !value.is_empty() {
            text.push_str(" · ");
            text.push_str(&value);
        }
    }
    if summary.count > 1 {
        text.push_str(&format!(" · {} records", summary.count));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(state: State) -> Summary {
        Summary {
            count: 1,
            record: Record {
                id: "worker".into(),
                state,
                kind: None,
                progress: None,
                app: None,
                title: String::new(),
                message: String::new(),
            },
        }
    }
    #[test]
    fn aggregation_selects_actionable_source_deterministically_and_disarms_text() {
        let items = [
            (2, Some(summary(State::Idle))),
            (9, Some(summary(State::Working))),
            (7, Some(summary(State::Done))),
            (6, Some(summary(State::Error))),
            (5, Some(summary(State::Blocked))),
            (3, Some(summary(State::Blocked))),
        ];
        let selected = representative(items.iter().map(|(id, s)| (*id, s.as_ref()))).unwrap();
        assert_eq!(selected.pane, 3);
        assert_eq!(
            representative(items.iter().rev().map(|(id, s)| (*id, s.as_ref()))),
            Some(selected)
        );
        let mut s = summary(State::Blocked);
        s.record.kind = Some(Kind::Auth);
        s.record.message = "<b>Login</b>\u{202e}\u{200b}\u{2066}\u{fe0f}".into();
        let text = description(&s);
        assert!(text.starts_with("Authentication needed"));
        assert!(text.ends_with("<b>Login</b>"));
        assert!(representative([(1, None::<&Summary>)]).is_none());
    }
    #[test]
    fn wire_summary_bounds_record_count_text_and_percentage() {
        let good = serde_json::to_value(summary(State::Working)).unwrap();
        assert!(serde_json::from_value::<Summary>(good.clone()).is_ok());
        for count in [0, MAX_RECORDS + 1] {
            let mut bad = good.clone();
            bad["count"] = count.into();
            assert!(serde_json::from_value::<Summary>(bad).is_err());
        }
        let mut bad = good.clone();
        bad["record"]["message"] = "x".repeat(MAX_MESSAGE_BYTES + 1).into();
        assert!(serde_json::from_value::<Summary>(bad).is_err());
        let mut bad = good;
        bad["record"]["progress"] = 101.into();
        assert!(serde_json::from_value::<Summary>(bad).is_err());
    }
}
