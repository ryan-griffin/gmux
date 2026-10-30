use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStringExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

mod service;
mod watches;
pub(crate) use service::{Service, Stats};

type Jobs = Vec<(u64, PathBuf)>;
type Results = Vec<Completion>;
const WORKERS: usize = 2;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Branch {
    pub name: String,
    pub head: Option<PathBuf>,
}

/// Order completed lookups by request generation, including missing HEADs.
#[derive(Default)]
struct BranchCache(HashMap<PathBuf, (u64, Branch)>);
impl BranchCache {
    fn resolve(&mut self, head: &Path, generation: u64, mut branch: Branch) -> Branch {
        if let Some((newer, branch)) = self.0.get(head)
            && *newer > generation
        {
            return branch.clone();
        }
        // Keep the parent watch through deletion so creation can wake us again.
        branch.head = Some(head.to_path_buf());
        self.0
            .insert(head.to_path_buf(), (generation, branch.clone()));
        branch
    }
    fn retain_heads(&mut self, heads: &HashSet<&Path>) {
        self.0.retain(|head, _| heads.contains(head.as_path()));
    }
}

fn bounded_output(
    command: &mut Command,
    timeout: Duration,
    cancel: Option<&gmux_platform::unix::ipc::Wake>,
) -> io::Result<Output> {
    if let Some(cancel) = cancel {
        let mut fd = libc::pollfd {
            fd: cancel.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 0) } > 0 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Git lookup cancelled",
            ));
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("Git stdout is piped");
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut eof = false;
        loop {
            let mut buffer = [0; 4096];
            while !eof {
                match stdout.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        if bytes.len() + n > MAX_OUTPUT {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Git output is too large",
                            ));
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            if let Some(status) = child.try_wait()?
                && eof
            {
                return Ok(Output {
                    status,
                    stdout: bytes,
                    stderr: Vec::new(),
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Git lookup timed out",
                ));
            }
            // Normally the stdout pipe wakes us on exit. A process that closes
            // stdout early still gets a bounded wait for its exit status.
            let wait = if eof {
                remaining.min(Duration::from_millis(10))
            } else {
                remaining
            };
            let mut pollfds = [
                libc::pollfd {
                    fd: if eof { -1 } else { fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cancel.map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe {
                libc::poll(
                    pollfds.as_mut_ptr(),
                    2,
                    wait.as_nanos().div_ceil(1_000_000) as i32,
                )
            } < 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            if pollfds[1].revents != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Git lookup cancelled",
                ));
            }
        }
    })();
    if result.is_err() {
        // Also stop subprocesses started by Git or a PATH wrapper.
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn lookup(directory: &Path, cancel: Option<&gmux_platform::unix::ipc::Wake>) -> Option<Branch> {
    let output = bounded_output(
        Command::new("git").arg("-C").arg(directory).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "HEAD",
            "--symbolic-full-name",
            "HEAD",
        ]),
        LOOKUP_TIMEOUT,
        cancel,
    )
    .ok()?;
    let stdout = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    // Split from the end because directory names can contain newlines.
    let Some(split) = stdout.iter().rposition(|&byte| byte == b'\n') else {
        return Some(Branch::default());
    };
    let head = Some(PathBuf::from(OsString::from_vec(stdout[..split].to_vec())));
    let name = if output.status.success() {
        stdout[split + 1..]
            .strip_prefix(b"refs/heads/")
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default()
    } else {
        // An unborn branch has no commit for rev-parse to resolve.
        let output = bounded_output(
            Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(["branch", "--show-current"]),
            LOOKUP_TIMEOUT,
            cancel,
        )
        .ok()?;
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            String::new()
        }
    };
    Some(Branch { name, head })
}

#[derive(Clone)]
struct Job {
    directory: PathBuf,
    tabs: Vec<u64>,
    generation: u64,
}
struct Completion {
    worker: usize,
    job: Job,
    branch: Option<Branch>,
}

/// Two bounded workers publish independently. Refreshes for the same directory
/// share a lookup and coalesce into one follow-up while that lookup is running.
struct BranchWorker {
    senders: Vec<mpsc::SyncSender<Job>>,
    receiver: mpsc::Receiver<Completion>,
    running: RefCell<[Option<PathBuf>; WORKERS]>,
    queued: RefCell<BTreeMap<u64, PathBuf>>,
    generation: Cell<u64>,
    last_dispatched: Cell<u64>,
    lookups: Cell<u64>,
    cancel: Option<gmux_platform::unix::ipc::Wake>,
}

impl BranchWorker {
    fn new(wake: gmux_platform::unix::ipc::Wake) -> Self {
        let cancel = gmux_platform::unix::ipc::Wake::new().ok();
        let stop = cancel.clone();
        let mut worker = Self::with_lookup(wake, move |directory| lookup(directory, stop.as_ref()));
        worker.cancel = cancel;
        worker
    }

    fn with_lookup(
        wake: gmux_platform::unix::ipc::Wake,
        lookup: impl Fn(&Path) -> Option<Branch> + Send + Sync + 'static,
    ) -> Self {
        let lookup = Arc::new(lookup);
        let (results, receiver) = mpsc::sync_channel(WORKERS);
        let mut senders = Vec::new();
        for worker in 0..WORKERS {
            let (sender, jobs) = mpsc::sync_channel::<Job>(1);
            senders.push(sender);
            let lookup = lookup.clone();
            let results = results.clone();
            let wake = wake.clone();
            std::thread::spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let branch = lookup(&job.directory);
                    if results
                        .send(Completion {
                            worker,
                            job,
                            branch,
                        })
                        .is_err()
                    {
                        break;
                    }
                    wake.notify();
                }
            });
        }
        Self {
            senders,
            receiver,
            running: RefCell::new([None, None]),
            queued: RefCell::new(BTreeMap::new()),
            generation: Cell::new(0),
            last_dispatched: Cell::new(0),
            lookups: Cell::new(0),
            cancel: None,
        }
    }

    fn retain_tabs(&self, tabs: &HashSet<u64>) {
        self.queued.borrow_mut().retain(|id, _| tabs.contains(id));
    }
    fn refresh_subset(&self, jobs: Jobs) {
        self.queued.borrow_mut().extend(jobs);
        self.dispatch();
    }

    fn dispatch(&self) {
        let mut running = self.running.borrow_mut();
        let mut queued = self.queued.borrow_mut();
        for worker in 0..WORKERS {
            if running[worker].is_some() {
                continue;
            }
            // Rotate through tab IDs so repeated refreshes of a low-ID tab
            // cannot indefinitely delay another directory's queued lookup.
            let last = self.last_dispatched.get();
            let Some((&id, directory)) = queued
                .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
                .chain(queued.range(..=last))
                .find(|(_, directory)| {
                    !running
                        .iter()
                        .any(|active| active.as_ref() == Some(directory))
                })
            else {
                break;
            };
            let directory = directory.clone();
            self.last_dispatched.set(id);
            let tabs: Vec<_> = queued
                .iter()
                .filter(|(_, path)| **path == directory)
                .map(|(&id, _)| id)
                .collect();
            for id in &tabs {
                queued.remove(id);
            }
            running[worker] = Some(directory.clone());
            let generation = self
                .generation
                .get()
                .checked_add(1)
                .expect("Git generation overflow");
            self.generation.set(generation);
            if self.senders[worker]
                .try_send(Job {
                    directory,
                    tabs,
                    generation,
                })
                .is_err()
            {
                running[worker] = None;
            } else {
                self.lookups.set(self.lookups.get() + 1);
            }
        }
    }

    fn results(&self) -> Results {
        let mut results = Vec::new();
        for completion in self.receiver.try_iter() {
            self.running.borrow_mut()[completion.worker] = None;
            results.push(completion);
        }
        self.dispatch();
        results
    }
}

impl Drop for BranchWorker {
    fn drop(&mut self) {
        // Leave the descriptor readable so every worker wakes and stops its Git
        // process group before exiting. Idle workers stop when senders close.
        if let Some(cancel) = &self.cancel {
            cancel.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, sync::Mutex};

    fn wait_results(
        worker: &BranchWorker,
        wake: &gmux_platform::unix::ipc::Wake,
    ) -> Vec<(u64, PathBuf, u64, Option<Branch>)> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let results = worker.results();
            if !results.is_empty() {
                return results
                    .into_iter()
                    .flat_map(|completion| {
                        completion.job.tabs.into_iter().map(move |id| {
                            (
                                id,
                                completion.job.directory.clone(),
                                completion.job.generation,
                                completion.branch.clone(),
                            )
                        })
                    })
                    .collect();
            }
            let mut fd = libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Timed out waiting for branches");
            assert_eq!(
                unsafe { libc::poll(&mut fd, 1, remaining.as_nanos().div_ceil(1_000_000) as i32) },
                1
            );
            wake.drain();
        }
    }

    #[test]
    fn fast_results_share_a_lookup_and_do_not_wait_for_a_slow_directory() {
        let (started, lookups) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        let worker = BranchWorker::with_lookup(wake.clone(), move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            if directory == Path::new("slow") {
                gate.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            Some(Branch {
                name: directory.display().to_string(),
                head: None,
            })
        });
        worker.refresh_subset(vec![
            (1, "slow".into()),
            (2, "fast".into()),
            (3, "fast".into()),
        ]);
        let started: HashSet<_> = (0..2)
            .map(|_| lookups.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect();
        assert_eq!(
            started,
            HashSet::from([PathBuf::from("slow"), PathBuf::from("fast")])
        );
        assert_eq!(
            wait_results(&worker, &wake)
                .iter()
                .map(|(id, _, _, _)| *id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(lookups.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(wait_results(&worker, &wake)[0].0, 1);
    }

    #[test]
    fn queued_refreshes_keep_latest_paths_preserve_other_tabs_and_drop_closed_tabs() {
        let (started, lookups) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        let worker = BranchWorker::with_lookup(wake.clone(), move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            if directory.starts_with("held-a") || directory.starts_with("held-b") {
                gate.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            Some(Branch::default())
        });
        worker.refresh_subset(vec![(1, "held-a".into()), (2, "held-b".into())]);
        for _ in 0..2 {
            lookups.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        worker.refresh_subset(vec![
            (3, "old".into()),
            (4, "other".into()),
            (5, "closed".into()),
        ]);
        for _ in 0..20 {
            worker.refresh_subset(vec![(3, "latest".into())]);
        }
        worker.retain_tabs(&HashSet::from([1, 2, 3, 4]));
        assert!(lookups.try_recv().is_err());
        release.send(()).unwrap();
        release.send(()).unwrap();
        let mut results = Vec::new();
        while results.len() < 4 {
            results.extend(wait_results(&worker, &wake));
        }
        let started: HashSet<_> = (0..2)
            .map(|_| lookups.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect();
        assert_eq!(
            started,
            HashSet::from([PathBuf::from("latest"), PathBuf::from("other")])
        );
        assert!(lookups.try_recv().is_err());
    }

    #[test]
    fn repeated_refreshes_do_not_jump_ahead_of_other_queued_directories() {
        let (started, lookups) = mpsc::channel();
        let (release_a, gate_a) = mpsc::channel();
        let (release_b, gate_b) = mpsc::channel();
        let gate_a = Mutex::new(Some(gate_a));
        let gate_b = Mutex::new(Some(gate_b));
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        let worker = BranchWorker::with_lookup(wake.clone(), move |directory| {
            started.send(directory.to_path_buf()).unwrap();
            let gate = if directory == Path::new("a") {
                gate_a.lock().unwrap().take()
            } else if directory == Path::new("b") {
                gate_b.lock().unwrap().take()
            } else {
                None
            };
            if let Some(gate) = gate {
                gate.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            Some(Branch::default())
        });
        worker.refresh_subset(vec![(1, "a".into()), (2, "b".into())]);
        for _ in 0..2 {
            lookups.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        worker.refresh_subset(vec![(3, "c".into()), (1, "a".into())]);
        release_a.send(()).unwrap();
        let mut results = wait_results(&worker, &wake);
        let next = lookups.recv_timeout(Duration::from_secs(2)).unwrap();
        release_b.send(()).unwrap();
        while results.len() < 4 {
            results.extend(wait_results(&worker, &wake));
        }
        assert_eq!(next, Path::new("c"));
    }

    #[test]
    fn old_results_cannot_revert_newer_branches_or_a_missing_head() {
        let head = Path::new("repo/.git/HEAD");
        let mut cache = BranchCache::default();
        let named = |name: &str| Branch {
            name: name.into(),
            head: Some(head.into()),
        };
        assert_eq!(cache.resolve(head, 2, named("new")).name, "new");
        assert_eq!(cache.resolve(head, 1, named("old")).name, "new");
        let missing = cache.resolve(head, 3, Branch::default());
        assert!(missing.name.is_empty());
        assert_eq!(
            missing.head.as_deref(),
            Some(head),
            "Keep watching for HEAD creation"
        );
        assert!(cache.resolve(head, 2, named("old")).name.is_empty());
        assert_eq!(cache.resolve(head, 4, named("restored")).name, "restored");
    }

    #[test]
    fn commands_that_hang_after_closing_stdout_are_killed_and_reaped() {
        let began = Instant::now();
        let error = bounded_output(
            Command::new("/bin/sh").args(["-c", "exec 1>&-; sleep 30"]),
            Duration::from_millis(50),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(1));
        let output = bounded_output(
            Command::new("/bin/sh").args(["-c", "printf branch"]),
            Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"branch");
    }
}
