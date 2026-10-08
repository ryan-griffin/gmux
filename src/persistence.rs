//! Ordered disk writes outside the daemon's PTY owner thread.
use crate::{
    ipc,
    model::{Session, WorkspaceCatalog},
    scrollback::Checkpoint,
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
    Checkpoint(Checkpoint),
    Prune,
    Catalog(WorkspaceCatalog),
}
impl Save {
    fn write(self) -> anyhow::Result<()> {
        match self {
            Self::Session(value) => crate::scrollback::save_session(&value),
            Self::Checkpoint(value) => value.write(),
            Self::Prune => crate::scrollback::prune_committed(),
            Self::Catalog(value) => storage::save("workspaces.json", &value),
        }
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
                    let mut jobs = lock.lock().unwrap();
                    while jobs.queue.is_empty() && !jobs.closed {
                        jobs = ready.wait(jobs).unwrap();
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
        let mut jobs = lock.lock().unwrap();
        anyhow::ensure!(!jobs.closed, "Persistence worker is closed");
        if automatic
            && let Some(last) = jobs.queue.back_mut()
            && last.automatic
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
        lock.lock().unwrap().closed = true;
        ready.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
                panic!("Expected a session")
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
}
