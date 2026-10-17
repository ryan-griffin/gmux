//! Ordered disk writes outside the daemon's PTY owner thread.
use crate::scrollback::Checkpoint;
use gmux_client::{
    ipc,
    model::{DaemonId, ExecutionState},
    storage,
};
use std::{
    cell::Cell,
    collections::VecDeque,
    path::Path,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::JoinHandle,
};

pub(crate) fn daemon_id() -> anyhow::Result<DaemonId> {
    storage::with_state_lock("daemon-id.json", identity_at)
}
fn identity_at(path: &Path) -> anyhow::Result<DaemonId> {
    let saved: Option<DaemonId> = storage::load_at(path)?;
    if let Some(id) = saved {
        return Ok(id);
    }
    anyhow::ensure!(
        !path.exists(),
        "Invalid daemon identity in {}",
        path.display()
    );
    let id = DaemonId::generate()?;
    storage::save_locked_at(path, &Some(id))?;
    Ok(id)
}
pub(crate) enum Save {
    Execution(ExecutionState),
    Checkpoint(Checkpoint),
    Prune,
}
impl Save {
    fn write(self) -> anyhow::Result<()> {
        match self {
            Self::Execution(value) => crate::scrollback::save_execution(&value),
            Self::Checkpoint(value) => value.write(),
            Self::Prune => crate::scrollback::prune_committed(),
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
            && std::mem::discriminant(&last.save) == std::mem::discriminant(&save)
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
    fn daemon_identity_is_durable_and_concurrent_initialization_keeps_one_id() {
        let directory = std::env::temp_dir().join(format!(
            "gmux-identity-{}-{}",
            std::process::id(),
            DaemonId::generate().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon-id.json");
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..8)
                .map(|_| {
                    let path = &path;
                    scope.spawn(move || storage::with_lock_at(path, || identity_at(path)).unwrap())
                })
                .collect();
            let ids: Vec<_> = writers
                .into_iter()
                .map(|writer| writer.join().unwrap())
                .collect();
            assert!(ids.iter().all(|id| *id == ids[0]));
            assert_eq!(identity_at(&path).unwrap(), ids[0]);
        });
        for invalid in ["null", "\"bad-id\"", "{broken"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(storage::with_lock_at(&path, || identity_at(&path)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn slow_saves_leave_the_caller_free_and_coalesce_only_automatic_snapshots() {
        let (started, began) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (seen, writes) = mpsc::channel();
        let mut worker = Worker::with_save(ipc::Wake::new().unwrap(), move |save| {
            match save {
                Save::Execution(value) => {
                    if value.last_id == 1 {
                        started.send(()).unwrap();
                        gate.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                    seen.send(value.last_id).unwrap();
                }
                Save::Prune => seen.send(6).unwrap(),
                _ => panic!("Expected execution"),
            }
            Ok(())
        });
        let session = |last_id| {
            Save::Execution(ExecutionState {
                last_id,
                ..Default::default()
            })
        };
        worker.submit(session(1), false).unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.submit(session(2), true).unwrap();
        worker.submit(session(3), true).unwrap();
        worker.submit(session(4), false).unwrap();
        worker.submit(session(5), true).unwrap();
        worker.submit(Save::Prune, true).unwrap();
        worker.submit(session(7), true).unwrap();
        release.send(()).unwrap();
        worker.finish();
        assert_eq!(
            writes.try_iter().collect::<Vec<_>>(),
            vec![1, 3, 4, 5, 6, 7]
        );
        assert_eq!(worker.results().count(), 6);
    }
}
