use std::{
    cell::Cell,
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
};

type Jobs = Vec<(u64, PathBuf)>;
type Results = Vec<(u64, PathBuf, String)>;

/// Keep slow Git commands off GTK's main thread, with at most one batch in flight.
pub struct BranchWorker {
    sender: mpsc::SyncSender<Jobs>,
    receiver: mpsc::Receiver<Results>,
    pending: Cell<bool>,
}

impl BranchWorker {
    pub fn new() -> Self {
        Self::with_lookup(|directory| {
            Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(["branch", "--show-current"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .unwrap_or_default()
        })
    }

    fn with_lookup(mut lookup: impl FnMut(&Path) -> String + Send + 'static) -> Self {
        let (sender, jobs) = mpsc::sync_channel::<Jobs>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(jobs) = jobs.recv() {
                let mut branches = HashMap::new();
                let batch = jobs
                    .into_iter()
                    .map(|(id, directory)| {
                        // Tabs in the same directory share one lookup per refresh.
                        let branch = branches
                            .entry(directory.clone())
                            .or_insert_with(|| lookup(&directory));
                        (id, directory, branch.clone())
                    })
                    .collect();
                if results.send(batch).is_err() {
                    break;
                }
            }
        });
        Self {
            sender,
            receiver,
            pending: Cell::new(false),
        }
    }

    pub fn refresh(&self, jobs: Jobs) {
        if !jobs.is_empty() && !self.pending.get() && self.sender.try_send(jobs).is_ok() {
            self.pending.set(true);
        }
    }

    pub fn results(&self) -> Results {
        if let Ok(results) = self.receiver.try_recv() {
            self.pending.set(false);
            results
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn slow_lookups_do_not_overlap_and_tabs_share_directory_results() {
        let (started, lookups) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = BranchWorker::with_lookup(move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
            format!("branch-{}", directory.display())
        });
        let timeout = Duration::from_secs(2);
        worker.refresh(vec![(1, "a".into()), (2, "a".into()), (3, "b".into())]);
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("a"));
        // Repeated refreshes while Git is still running must not queue more work.
        for _ in 0..20 {
            worker.refresh(vec![(4, "ignored".into())]);
        }
        assert!(worker.results().is_empty());
        release.send(()).unwrap();
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("b"));
        release.send(()).unwrap();
        let deadline = Instant::now() + timeout;
        let results = loop {
            let results = worker.results();
            if !results.is_empty() {
                break results;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(
            results,
            vec![
                (1, "a".into(), "branch-a".into()),
                (2, "a".into(), "branch-a".into()),
                (3, "b".into(), "branch-b".into())
            ]
        );
        worker.refresh(vec![(5, "next".into())]);
        assert_eq!(
            lookups.recv_timeout(timeout).unwrap(),
            PathBuf::from("next")
        );
        release.send(()).unwrap();
    }
}
