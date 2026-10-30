//! Optional, daemon-local repository metadata. Nothing is saved with execution.
use super::{
    BranchCache, BranchWorker, Jobs, WORKERS,
    watches::{Change, Watches},
};
use gmux_protocol::transport::GitMetadata;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const RETRY: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Pane {
    directory: PathBuf,
    head: Option<PathBuf>,
    branch: String,
    requested: bool,
}

#[derive(Default, Serialize)]
pub(crate) struct Stats {
    pub panes: usize,
    pub repositories: usize,
    pub watches: usize,
    pub workers: usize,
    pub lookups: u64,
    pub pending: usize,
}

pub(crate) struct Service {
    worker: BranchWorker,
    cache: BranchCache,
    panes: HashMap<u64, Pane>,
    watches: Option<Watches>,
    missing_watches: bool,
    retry_needed: bool,
    retry_at: Instant,
    revision: u64,
}

impl Service {
    pub(crate) fn new(wake: gmux_platform::unix::ipc::Wake) -> Self {
        Self {
            worker: BranchWorker::new(wake),
            cache: BranchCache::default(),
            panes: HashMap::new(),
            watches: Watches::new().ok(),
            missing_watches: false,
            retry_needed: false,
            retry_at: Instant::now() + RETRY,
            revision: 0,
        }
    }
    pub(crate) fn fd(&self) -> Option<i32> {
        self.watches.as_ref().map(Watches::fd)
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn stats(&self) -> Stats {
        Stats {
            panes: self.panes.len(),
            repositories: self
                .panes
                .values()
                .filter_map(|pane| pane.head.as_ref())
                .collect::<HashSet<_>>()
                .len(),
            watches: self.watches.as_ref().map_or(0, Watches::len),
            workers: WORKERS,
            lookups: self.worker.lookups.get(),
            pending: self
                .worker
                .running
                .borrow()
                .iter()
                .filter(|directory| directory.is_some())
                .count()
                + self
                    .worker
                    .queued
                    .borrow()
                    .values()
                    .collect::<HashSet<_>>()
                    .len(),
        }
    }
    pub(crate) fn metadata(&self, id: u64) -> Option<GitMetadata> {
        self.panes.get(&id).map(|pane| GitMetadata {
            id,
            directory: pane.directory.clone(),
            branch: pane.branch.clone(),
        })
    }
    pub(crate) fn sync(&mut self, wanted: HashMap<u64, PathBuf>) {
        let retained: HashSet<_> = wanted.keys().copied().collect();
        let mut changed = false;
        let mut jobs = Vec::new();
        for (id, directory) in wanted {
            if self
                .panes
                .get(&id)
                .is_some_and(|pane| pane.directory == directory)
            {
                continue;
            }
            let cached = self
                .panes
                .values()
                .find(|pane| pane.directory == directory && pane.requested && pane.head.is_some())
                .cloned();
            let pane = cached.unwrap_or_else(|| {
                jobs.push((id, directory.clone()));
                Pane {
                    directory,
                    head: None,
                    branch: String::new(),
                    requested: true,
                }
            });
            self.panes.insert(id, pane);
            changed = true;
            self.revision += 1;
        }
        // Reuse a watched directory even when one pane's subscription is being
        // replaced by another's, as when a client switches active split panes.
        let before = self.panes.len();
        self.panes.retain(|id, _| retained.contains(id));
        if self.panes.len() != before {
            self.revision += 1;
            changed = true;
        }
        if !changed {
            return;
        }
        self.worker
            .retain_tabs(&self.panes.keys().copied().collect());
        jobs.extend(self.sync_watches());
        self.worker.refresh_subset(jobs);
    }

    pub(crate) fn poll(&mut self) {
        let events = self
            .watches
            .as_mut()
            .map(Watches::changes)
            .unwrap_or_default();
        let watches_changed = !events.is_empty();
        let mut invalidated = HashSet::new();
        let mut heads = HashSet::new();
        for event in events {
            match event {
                Change::Overflow => invalidated.extend(self.panes.keys().copied()),
                Change::File {
                    directory,
                    head,
                    lost,
                } => {
                    for (&id, pane) in &self.panes {
                        if lost
                            && (pane.directory.starts_with(&directory)
                                || pane
                                    .head
                                    .as_ref()
                                    .is_some_and(|path| path.starts_with(&directory)))
                            || !head && pane.directory.starts_with(&directory)
                        {
                            invalidated.insert(id);
                        } else if head
                            && pane.head.as_ref().and_then(|path| path.parent())
                                == Some(directory.as_path())
                        {
                            heads.insert(pane.head.clone().unwrap());
                        } else if head
                            && pane.head.is_none()
                            && directory.ends_with(".git")
                            && directory
                                .parent()
                                .is_some_and(|parent| pane.directory.starts_with(parent))
                        {
                            // HEAD can appear after a lookup saw an incomplete
                            // repository, before Git could resolve its path.
                            invalidated.insert(id);
                        }
                    }
                }
            }
        }
        let mut jobs = Vec::new();
        for id in invalidated {
            let pane = self.panes.get_mut(&id).unwrap();
            pane.head = None;
            if !pane.branch.is_empty() {
                self.revision += 1;
            }
            pane.branch.clear();
            pane.requested = true;
            jobs.push((id, pane.directory.clone()));
        }
        for head in heads {
            jobs.extend(self.head_job(&head));
        }
        let mut completed = false;
        for completion in self.worker.results() {
            let super::Completion {
                job,
                branch: result,
                ..
            } = completion;
            let Some(pane) = job.tabs.iter().find_map(|id| {
                self.panes
                    .get(id)
                    .filter(|pane| pane.directory == job.directory)
            }) else {
                continue;
            };
            let succeeded = result.is_some();
            let branch = result.unwrap_or_default();
            let head = branch.head.clone().or_else(|| pane.head.clone());
            let (branch, shared_changed) = match head {
                Some(head) => {
                    let previous = self
                        .cache
                        .0
                        .get(&head)
                        .map(|(_, branch)| branch.name.clone());
                    let branch = self.cache.resolve(&head, job.generation, branch);
                    let changed = previous.as_deref() != Some(branch.name.as_str());
                    (branch, changed)
                }
                None => (branch, false),
            };
            let mut ids: HashSet<_> = job
                .tabs
                .into_iter()
                .filter(|id| {
                    self.panes
                        .get(id)
                        .is_some_and(|pane| pane.directory == job.directory)
                })
                .collect();
            if shared_changed {
                ids.extend(
                    self.panes
                        .iter()
                        .filter(|(_, pane)| branch.head.is_some() && pane.head == branch.head)
                        .map(|(&id, _)| id),
                );
            }
            for id in ids {
                let pane = self.panes.get_mut(&id).unwrap();
                if pane.branch != branch.name {
                    self.revision += 1;
                }
                pane.branch.clone_from(&branch.name);
                if branch.head.is_some() {
                    pane.head.clone_from(&branch.head);
                }
                pane.requested = succeeded;
            }
            completed = true;
        }
        let retry = self.next_retry().is_some_and(|at| Instant::now() >= at);
        if retry {
            self.retry_at = Instant::now() + RETRY;
            let mut queried = HashSet::new();
            jobs.extend(
                self.panes
                    .iter()
                    .filter(|(_, pane)| !pane.requested || self.missing_watches)
                    .filter(|(_, pane)| {
                        queried.insert(pane.head.as_ref().unwrap_or(&pane.directory).clone())
                    })
                    .map(|(&id, pane)| (id, pane.directory.clone())),
            );
        }
        if completed || retry || watches_changed {
            jobs.extend(self.sync_watches());
        }
        // Coalesce events and watch-installation rechecks before starting Git.
        self.worker.refresh_subset(jobs);
    }
    fn head_job(&self, head: &Path) -> Vec<(u64, PathBuf)> {
        self.panes
            .iter()
            .find(|(_, pane)| pane.head.as_deref() == Some(head))
            .map(|(&id, pane)| vec![(id, pane.directory.clone())])
            .unwrap_or_default()
    }
    fn sync_watches(&mut self) -> Jobs {
        let heads: HashSet<_> = self
            .panes
            .values()
            .filter_map(|pane| pane.head.as_deref())
            .collect();
        self.cache.retain_heads(&heads);
        let mut wanted: HashSet<_> = self
            .panes
            .values()
            .flat_map(|pane| pane.directory.ancestors())
            .chain(heads.iter().filter_map(|head| head.parent()))
            .collect();
        let Some(watches) = &mut self.watches else {
            self.missing_watches = true;
            self.retry_needed = true;
            return Vec::new();
        };
        let mut installed: HashSet<_> = wanted
            .iter()
            .filter(|path| watches.add(path))
            .map(|path| path.to_path_buf())
            .collect();
        self.missing_watches = wanted.iter().any(|path| !watches.contains(path));
        // Git creates .git before its HEAD. Watch existing candidate directories
        // even while discovery returns no repository; absent candidates need
        // neither watches nor retries. Parent watches cover their creation.
        let repositories: Vec<_> = wanted
            .iter()
            .map(|path| path.join(".git"))
            .filter(|path| {
                if watches.add(path) {
                    installed.insert(path.clone());
                }
                watches.contains(path)
            })
            .collect();
        wanted.extend(repositories.iter().map(PathBuf::as_path));
        watches.retain(&wanted);
        self.retry_needed = self.missing_watches || self.panes.values().any(|pane| !pane.requested);
        // Recheck after installing repository watches to close the lookup gap.
        let mut jobs: Vec<_> = heads
            .iter()
            .filter(|head| {
                head.parent()
                    .is_some_and(|parent| installed.contains(parent))
            })
            .flat_map(|head| self.head_job(head))
            .collect();
        jobs.extend(
            self.panes
                .iter()
                .filter(|(_, pane)| {
                    pane.head.is_none()
                        && repositories.iter().any(|path| {
                            installed.contains(path)
                                && path
                                    .parent()
                                    .is_some_and(|parent| pane.directory.starts_with(parent))
                        })
                })
                .map(|(&id, pane)| (id, pane.directory.clone())),
        );
        jobs
    }
    pub(crate) fn next_retry(&self) -> Option<Instant> {
        self.retry_needed.then_some(self.retry_at)
    }
}
