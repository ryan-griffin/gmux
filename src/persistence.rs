//! Ordered disk writes outside the daemon's PTY owner thread.
use crate::{
    ipc,
    model::{Session, WorkspaceCatalog},
    storage,
};
use std::{
    cell::Cell,
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::JoinHandle,
};

pub(crate) enum Save {
    Session(Session),
    Catalog(WorkspaceCatalog),
}
impl Save {
    fn write(self) -> anyhow::Result<()> {
        match self {
            Self::Session(value) => storage::save("session.json", &value),
            Self::Catalog(value) => storage::save("workspaces.json", &value),
        }
    }
    fn same_target(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Session(_), Self::Session(_)) | (Self::Catalog(_), Self::Catalog(_))
        )
    }
}
struct Job {
    id: u64,
    save: Save,
    automatic: bool,
}
#[derive(Default)]
struct Jobs {
    queue: VecDeque<Job>,
    closed: bool,
}
pub(crate) struct Worker {
    jobs: Arc<(Mutex<Jobs>, Condvar)>,
    completed: mpsc::Receiver<(u64, Result<(), String>)>,
    next: Cell<u64>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub(crate) fn new(wake: ipc::Wake) -> Self {
        Self::with_save(wake, Save::write)
    }
    fn with_save(
        wake: ipc::Wake,
        save: impl Fn(Save) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        let jobs = Arc::new((Mutex::new(Jobs::default()), Condvar::new()));
        let worker = jobs.clone();
        let (sender, completed) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            loop {
                let job = {
                    let (lock, ready) = &*worker;
                    // Recover from a poisoned mutex: a prior panic while holding
                    // the lock still leaves the queue usable.
                    let mut jobs = lock.lock().unwrap_or_else(|error| error.into_inner());
                    while jobs.queue.is_empty() && !jobs.closed {
                        jobs = ready.wait(jobs).unwrap_or_else(|error| error.into_inner());
                    }
                    let Some(job) = jobs.queue.pop_front() else {
                        break;
                    };
                    job
                };
                let result = save(job.save).map_err(|error| error.to_string());
                if sender.send((job.id, result)).is_err() {
                    break;
                }
                wake.notify();
            }
        });
        Self {
            jobs,
            completed,
            next: Cell::new(1),
            thread: Some(thread),
        }
    }
    pub(crate) fn submit(&self, save: Save, automatic: bool) -> anyhow::Result<u64> {
        let (lock, ready) = &*self.jobs;
        // Recover from a poisoned mutex: a prior panic while holding
        // the lock still leaves the queue usable.
        let mut jobs = lock.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(!jobs.closed, "Persistence worker is closed");
        // Only trailing automatic saves for the same file coalesce.
        // Explicit saves back control replies keyed by id, so each keeps
        // its own job. The target check stops a session autosave from
        // replacing a queued catalog save (and vice versa).
        if automatic
            && let Some(last) = jobs.queue.back_mut()
            && last.automatic
            && last.save.same_target(&save)
        {
            last.save = save;
            return Ok(last.id);
        }
        anyhow::ensure!(jobs.queue.len() < 32, "Persistence queue is full");
        let id = self.next.get();
        self.next.set(
            id.checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("No save IDs remain"))?,
        );
        jobs.queue.push_back(Job {
            id,
            save,
            automatic,
        });
        ready.notify_one();
        Ok(id)
    }
    pub(crate) fn results(&self) -> impl Iterator<Item = (u64, Result<(), String>)> + '_ {
        self.completed.try_iter()
    }
    pub(crate) fn finish(&mut self) {
        let (lock, ready) = &*self.jobs;
        // Recover from a poisoned mutex: a prior panic while holding
        // the lock still leaves the queue usable.
        lock.lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        ready.notify_one();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("Persistence worker thread panicked");
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slow_saves_leave_the_caller_free_and_coalesce_only_automatic_snapshots() {
        let (started, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (seen, writes) = mpsc::channel();
        let mut worker = Worker::with_save(ipc::Wake::new().unwrap(), move |save| {
            let Save::Session(session) = save else {
                return Err(anyhow::anyhow!("expected session save"));
            };
            if session.width == 1 {
                started.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            seen.send(session.width).unwrap();
            Ok(())
        });
        let session = |width| {
            Save::Session(Session {
                width,
                ..Session::default()
            })
        };
        worker.submit(session(1), false).unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.submit(session(2), true).unwrap();
        worker.submit(session(3), true).unwrap();
        worker.submit(session(4), false).unwrap();
        worker.submit(session(5), true).unwrap();
        release.send(()).unwrap();
        worker.finish();
        assert_eq!(writes.try_iter().collect::<Vec<_>>(), vec![1, 3, 4, 5]);
        assert_eq!(worker.results().count(), 4);
    }

    #[test]
    fn automatic_coalescing_respects_target() {
        let (started, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (seen, writes) = mpsc::channel();
        let mut worker = Worker::with_save(ipc::Wake::new().unwrap(), move |save| match save {
            Save::Session(session) => {
                if session.width == 1 {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                seen.send(format!("session:{}", session.width)).unwrap();
                Ok(())
            }
            Save::Catalog(_) => {
                seen.send("catalog".to_owned()).unwrap();
                Ok(())
            }
        });
        let session = |width| {
            Save::Session(Session {
                width,
                ..Session::default()
            })
        };
        let catalog = || Save::Catalog(WorkspaceCatalog::default());
        worker.submit(session(1), false).unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.submit(session(2), true).unwrap();
        worker.submit(catalog(), true).unwrap();
        worker.submit(session(3), true).unwrap();
        release.send(()).unwrap();
        worker.finish();
        assert_eq!(
            writes.try_iter().collect::<Vec<_>>(),
            vec!["session:1", "session:2", "catalog", "session:3"]
        );
    }

    #[test]
    fn explicit_saves_never_coalesce_so_control_replies_keep_their_ids() {
        let (started, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (seen, writes) = mpsc::channel();
        let mut worker = Worker::with_save(ipc::Wake::new().unwrap(), move |save| match save {
            Save::Session(session) => {
                if session.width == 1 {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                seen.send(format!("session:{}", session.width)).unwrap();
                Ok(())
            }
            Save::Catalog(_) => {
                seen.send("catalog".to_owned()).unwrap();
                Ok(())
            }
        });
        let session = |width| {
            Save::Session(Session {
                width,
                ..Session::default()
            })
        };
        let catalog = || Save::Catalog(WorkspaceCatalog::default());
        worker.submit(session(1), false).unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        let second = worker.submit(session(2), false).unwrap();
        let third = worker.submit(session(3), false).unwrap();
        assert_ne!(second, third);
        let catalog_id = worker.submit(catalog(), false).unwrap();
        assert_ne!(third, catalog_id);
        let fourth = worker.submit(session(4), false).unwrap();
        assert_ne!(fourth, catalog_id);
        assert_ne!(fourth, second);
        release.send(()).unwrap();
        worker.finish();
        assert_eq!(
            writes.try_iter().collect::<Vec<_>>(),
            vec![
                "session:1",
                "session:2",
                "session:3",
                "catalog",
                "session:4"
            ]
        );
        assert_eq!(worker.results().count(), 5);
    }
}
