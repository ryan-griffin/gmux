//! All windows share one stream-work budget on the GTK thread. Idle priority
//! lets input and frame-clock sources run between batches of queued windows.
use gmux_platform::work::Budget;
use std::{cell::RefCell, collections::VecDeque, time::Duration};

type Work = Box<dyn FnOnce(&mut Budget)>;
#[derive(Default)]
struct Dispatcher {
    pending: VecDeque<Work>,
    scheduled: bool,
}
thread_local! {
    static DISPATCHER: RefCell<Dispatcher> = RefCell::default();
}

pub(super) fn enqueue(work: impl FnOnce(&mut Budget) + 'static) {
    let schedule = DISPATCHER.with_borrow_mut(|dispatcher| {
        dispatcher.pending.push_back(Box::new(work));
        !std::mem::replace(&mut dispatcher.scheduled, true)
    });
    if schedule {
        glib::idle_add_local(|| {
            let mut budget = Budget::new(Duration::from_millis(2), 64);
            dispatch(&mut budget)
        });
    }
}

fn dispatch(budget: &mut Budget) -> glib::ControlFlow {
    loop {
        let work = DISPATCHER.with_borrow_mut(|d| d.pending.pop_front());
        if let Some(work) = work {
            work(budget);
        }
        let pending = DISPATCHER.with_borrow_mut(|d| {
            d.scheduled = !d.pending.is_empty();
            d.scheduled
        });
        if !pending {
            return glib::ControlFlow::Break;
        }
        if budget.expired() {
            return glib::ControlFlow::Continue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn windows_share_a_budget_and_requeued_work_goes_last() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        // Install directly to test dispatch without sources on the global context.
        DISPATCHER.with_borrow_mut(|d| {
            d.scheduled = true;
            let first_seen = seen.clone();
            d.pending.push_back(Box::new(move |budget| {
                assert!(budget.take());
                first_seen.borrow_mut().push(1);
                let seen = first_seen.clone();
                enqueue(move |budget| {
                    assert!(budget.take());
                    seen.borrow_mut().push(3);
                });
            }));
            let seen = seen.clone();
            d.pending.push_back(Box::new(move |budget| {
                assert!(budget.take());
                seen.borrow_mut().push(2);
            }));
        });
        for expected in [vec![1], vec![1, 2], vec![1, 2, 3]] {
            let mut budget = Budget::new(Duration::from_secs(60), 1);
            let result = dispatch(&mut budget);
            assert_eq!(*seen.borrow(), expected);
            assert_eq!(
                result,
                if expected.len() == 3 {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            );
        }
    }
}
