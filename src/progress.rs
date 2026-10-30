//! One latest value and one deadline per execution. No history or frontend timer.
use gmux_protocol::progress::{Progress, ProgressState};
use std::time::{Duration, Instant};

const STALE_AFTER: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(crate) struct RetainedProgress {
    pub(crate) value: Progress,
    pub(crate) deadline: Option<Instant>,
    changed: bool,
}
impl RetainedProgress {
    pub(crate) fn report(&mut self, state: ProgressState, percentage: Option<u8>, now: Instant) {
        let previous = self.value;
        self.value.apply(state, percentage);
        self.changed |= previous != self.value;
        self.deadline = self.value.active().then_some(now + STALE_AFTER);
    }
    pub(crate) fn clear(&mut self) {
        self.report(ProgressState::Inactive, None, Instant::now());
    }
    pub(crate) fn take_changed(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|at| now >= at) {
            self.clear();
        }
        std::mem::take(&mut self.changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_reports_refresh_the_deadline_without_changes_and_expire_once() {
        let start = Instant::now();
        let mut p = RetainedProgress::default();
        assert!(!p.take_changed(start));
        p.report(ProgressState::Determinate, Some(42), start);
        assert!(p.take_changed(start));
        p.report(
            ProgressState::Determinate,
            Some(42),
            start + Duration::from_secs(14),
        );
        assert!(!p.take_changed(start + Duration::from_secs(15)));
        assert_eq!(p.deadline, Some(start + Duration::from_secs(29)));
        assert!(p.take_changed(start + Duration::from_secs(29)));
        assert!(!p.value.active());
        assert!(p.deadline.is_none());
        assert!(!p.take_changed(start + Duration::from_secs(30)));
        p.report(ProgressState::Error, Some(9), start);
        p.clear();
        assert_eq!(p.value, Progress::default());
        assert!(p.deadline.is_none());
    }
}
