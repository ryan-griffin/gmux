//! Caller-driven compression deadlines. No timer survives a completed pass.
use crate::engine::CompressionResult;
use std::time::{Duration, Instant};

pub(crate) const IDLE_DELAY: Duration = Duration::from_secs(5);
pub(crate) const STEP_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Default)]
pub(crate) struct IdleCompression {
    activity: Option<u64>,
    pub deadline: Option<Instant>,
    pub stats: CompressionStats,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct CompressionStats {
    pub steps: u64,
    pub work_ns: u64,
    pub max_step_ns: u64,
}

impl IdleCompression {
    pub fn observe(&mut self, activity: u64, now: Instant) {
        // Tokens can wrap or change in either direction. Never order them.
        if self.activity != Some(activity) {
            self.activity = Some(activity);
            self.deadline = Some(now + IDLE_DELAY);
        }
    }

    pub fn finish(&mut self, result: Option<CompressionResult>, started: Instant, now: Instant) {
        let elapsed = now.duration_since(started).as_nanos().min(u64::MAX as u128) as u64;
        self.stats.steps += 1;
        self.stats.work_ns += elapsed;
        self.stats.max_step_ns = self.stats.max_step_ns.max(elapsed);
        // Errors also stop until activity changes, avoiding a failing timer loop.
        self.deadline = (result == Some(CompressionResult::Pending)).then_some(now + STEP_INTERVAL);
    }
}

/// The limit applies across all panes, including overdue panes after a stall.
/// Oldest deadlines go first, so a pending pane cannot starve other panes.
pub(crate) struct CompressionBudget {
    not_before: Instant,
}

impl CompressionBudget {
    pub fn new(now: Instant) -> Self {
        Self { not_before: now }
    }

    pub fn deadline(&self, earliest: Option<Instant>) -> Option<Instant> {
        earliest.map(|at| at.max(self.not_before))
    }

    pub fn ready(&self, earliest: Option<Instant>, now: Instant) -> bool {
        self.deadline(earliest).is_some_and(|at| at <= now)
    }

    pub fn used(&mut self, now: Instant) {
        self.not_before = now + STEP_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_restarts_delay_and_completion_removes_deadlines() {
        let now = Instant::now();
        let mut idle = IdleCompression::default();
        idle.observe(u64::MAX, now);
        assert_eq!(idle.deadline, Some(now + IDLE_DELAY));
        idle.observe(u64::MAX, now + Duration::from_secs(1));
        assert_eq!(idle.deadline, Some(now + IDLE_DELAY));
        // A wrapped or decreasing token is activity too.
        let later = now + IDLE_DELAY;
        idle.observe(0, later);
        assert_eq!(idle.deadline, Some(later + IDLE_DELAY));
        idle.finish(Some(CompressionResult::Pending), later, later);
        assert_eq!(idle.deadline, Some(later + STEP_INTERVAL));
        idle.observe(7, later);
        assert_eq!(idle.deadline, Some(later + IDLE_DELAY));
        for result in [
            Some(CompressionResult::Complete),
            Some(CompressionResult::Unsupported),
            None,
        ] {
            idle.finish(result, later, later);
            idle.observe(7, later + Duration::from_secs(60));
            assert_eq!(idle.deadline, None);
            idle.observe(6, later);
            assert_eq!(idle.deadline, Some(later + IDLE_DELAY));
            idle.observe(7, later);
        }
    }

    #[test]
    fn overdue_panes_share_one_step_per_interval_and_teardown_leaves_no_timer() {
        let now = Instant::now();
        let mut panes: Vec<_> = (0..24).map(|_| IdleCompression::default()).collect();
        for pane in &mut panes {
            pane.observe(0, now);
        }
        let mut budget = CompressionBudget::new(now);
        let mut at = now + IDLE_DELAY + Duration::from_secs(1);
        for expected in 0..panes.len() {
            let (id, deadline) = panes
                .iter()
                .enumerate()
                .filter_map(|(id, p)| p.deadline.map(|at| (id, at)))
                .min_by_key(|&(id, at)| (at, id))
                .unwrap();
            assert_eq!(id, expected);
            assert!(budget.ready(Some(deadline), at));
            panes[id].finish(Some(CompressionResult::Pending), at, at);
            budget.used(at);
            // Another overdue pane must still wait for the global interval.
            assert!(!budget.ready(Some(deadline), at));
            assert_eq!(budget.deadline(Some(deadline)), Some(at + STEP_INTERVAL));
            at += STEP_INTERVAL;
        }
        for pane in &mut panes {
            pane.finish(Some(CompressionResult::Complete), at, at);
        }
        assert_eq!(
            budget.deadline(panes.iter().filter_map(|p| p.deadline).min()),
            None
        );
        panes[0].observe(1, at);
        panes.clear();
        assert_eq!(
            budget.deadline(panes.iter().filter_map(|p| p.deadline).min()),
            None
        );
    }
}
