use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
};

type Jobs = Vec<(u64, PathBuf)>;
type Results = Vec<(u64, PathBuf, Branch)>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub head: Option<PathBuf>,
}

fn lookup(directory: &Path) -> Branch {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "HEAD",
            "--symbolic-full-name",
            "HEAD",
        ])
        .output()
        .ok();
    let Some(output) = output else {
        return Branch::default();
    };
    let stdout = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    // Split from the end because directory names can contain newlines.
    let Some(split) = stdout.iter().rposition(|&byte| byte == b'\n') else {
        return Branch::default();
    };
    let head = Some(PathBuf::from(OsString::from_vec(stdout[..split].to_vec())));
    let name = if output.status.success() {
        stdout[split + 1..]
            .strip_prefix(b"refs/heads/")
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default()
    } else {
        // An unborn branch has no commit for rev-parse to resolve.
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["branch", "--show-current"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    Branch { name, head }
}

/// Keep slow Git commands off GTK's main thread, with at most one batch in flight.
pub struct BranchWorker {
    sender: mpsc::SyncSender<Jobs>,
    receiver: mpsc::Receiver<Results>,
    pending: Cell<bool>,
    queued: RefCell<BTreeMap<u64, PathBuf>>,
}

impl BranchWorker {
    pub fn new(wake: crate::ipc::Wake) -> Self {
        Self::with_lookup(wake, lookup)
    }

    fn with_lookup(
        wake: crate::ipc::Wake,
        mut lookup: impl FnMut(&Path) -> Branch + Send + 'static,
    ) -> Self {
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
                wake.notify();
            }
        });
        Self {
            sender,
            receiver,
            pending: Cell::new(false),
            queued: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn refresh(&self, jobs: Jobs) {
        // A full snapshot also removes queued work for tabs that were closed.
        self.queued.borrow_mut().clear();
        self.refresh_subset(jobs);
    }

    pub fn refresh_subset(&self, jobs: Jobs) {
        if self.pending.get() {
            self.queued.borrow_mut().extend(jobs);
        } else if !jobs.is_empty() && self.sender.try_send(jobs).is_ok() {
            self.pending.set(true);
        }
    }

    pub fn results(&self) -> Results {
        if let Ok(results) = self.receiver.try_recv() {
            self.pending.set(false);
            let jobs = std::mem::take(&mut *self.queued.borrow_mut());
            self.refresh_subset(jobs.into_iter().collect());
            results
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::fd::AsRawFd, time::Duration};

    fn wait_results(worker: &BranchWorker, wake: &crate::ipc::Wake) -> Results {
        let mut fd = libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 2000) }, 1);
        wake.drain();
        worker.results()
    }

    #[test]
    fn slow_lookups_coalesce_refreshes_share_directories_and_wake_frontend() {
        let (started, lookups) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let wake = crate::ipc::Wake::new().unwrap();
        let worker = BranchWorker::with_lookup(wake.clone(), move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
            Branch {
                name: format!("branch-{}", directory.display()),
                head: None,
            }
        });
        let timeout = Duration::from_secs(2);
        worker.refresh(vec![(1, "a".into()), (2, "a".into()), (3, "b".into())]);
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("a"));
        // Requests during a lookup coalesce into one follow-up batch.
        for _ in 0..20 {
            worker.refresh(vec![(4, "ignored".into())]);
        }
        assert!(worker.results().is_empty());
        release.send(()).unwrap();
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("b"));
        worker.refresh(vec![(5, "next".into())]);
        release.send(()).unwrap();
        let results = wait_results(&worker, &wake);
        assert_eq!(
            results,
            vec![
                (
                    1,
                    "a".into(),
                    Branch {
                        name: "branch-a".into(),
                        head: None
                    }
                ),
                (
                    2,
                    "a".into(),
                    Branch {
                        name: "branch-a".into(),
                        head: None
                    }
                ),
                (
                    3,
                    "b".into(),
                    Branch {
                        name: "branch-b".into(),
                        head: None
                    }
                )
            ]
        );
        assert_eq!(
            lookups.recv_timeout(timeout).unwrap(),
            PathBuf::from("next")
        );
        release.send(()).unwrap();
    }

    #[test]
    fn partial_refreshes_preserve_other_repositories_and_keep_the_latest_directory() {
        let (started, lookups) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let wake = crate::ipc::Wake::new().unwrap();
        let timeout = Duration::from_secs(2);
        let worker = BranchWorker::with_lookup(wake.clone(), move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            gate.recv_timeout(timeout).unwrap();
            Branch::default()
        });
        worker.refresh(vec![(1, "a".into())]);
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("a"));
        worker.refresh_subset(vec![(2, "b".into())]);
        worker.refresh_subset(vec![(3, "c".into())]);
        worker.refresh_subset(vec![(2, "new-b".into())]);
        assert!(lookups.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(wait_results(&worker, &wake)[0].0, 1);
        assert_eq!(
            lookups.recv_timeout(timeout).unwrap(),
            PathBuf::from("new-b")
        );
        release.send(()).unwrap();
        assert_eq!(lookups.recv_timeout(timeout).unwrap(), PathBuf::from("c"));
        release.send(()).unwrap();
        let results = wait_results(&worker, &wake);
        assert_eq!(
            results
                .iter()
                .map(|(id, directory, _)| (*id, directory.clone()))
                .collect::<Vec<_>>(),
            vec![(2, "new-b".into()), (3, "c".into())]
        );
    }
}
