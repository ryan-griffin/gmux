//! Cooperative event-loop work limits. A work item is atomic, so time limits
//! are soft; callers must also keep individual items small.
use std::{
    collections::{HashSet, VecDeque},
    hash::Hash,
    time::{Duration, Instant},
};

pub struct Budget {
    deadline: Instant,
    remaining: usize,
}
impl Budget {
    pub fn new(time: Duration, items: usize) -> Self {
        Self {
            deadline: Instant::now() + time,
            remaining: items,
        }
    }
    pub fn expired(&self) -> bool {
        self.remaining == 0 || Instant::now() >= self.deadline
    }
    pub fn take(&mut self) -> bool {
        if self.expired() {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// Readiness discovered again on the next poll cannot jump ahead of work left
/// from this turn. Membership is removed before servicing an item.
pub struct Queue<T> {
    items: VecDeque<T>,
    queued: HashSet<T>,
}
impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            queued: HashSet::new(),
        }
    }
}
impl<T: Copy + Eq + Hash> Queue<T> {
    pub fn push(&mut self, item: T) {
        if self.queued.insert(item) {
            self.items.push_back(item);
        }
    }
    pub fn pop(&mut self) -> Option<T> {
        let item = self.items.pop_front()?;
        self.queued.remove(&item);
        Some(item)
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_readiness_does_not_overtake_deferred_work() {
        let mut queue = Queue::default();
        for id in 0..4 {
            queue.push(id);
        }
        assert_eq!(queue.pop(), Some(0));
        for id in 0..4 {
            queue.push(id);
        }
        assert_eq!(
            (0..4).map(|_| queue.pop().unwrap()).collect::<Vec<_>>(),
            [1, 2, 3, 0]
        );
        assert!(queue.is_empty());
    }
}
