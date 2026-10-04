//! Repository watches and branch labels.
use super::*;

impl Ui {
    pub(super) fn update_branches(self: &Rc<Self>) {
        let results = self.branches.results();
        if results.is_empty() {
            return;
        }
        let mut changed = false;
        for (id, directory, generation, result) in results {
            let mut tabs = self.tabs.borrow_mut();
            let Some(branch) = result else {
                if let Some(tab) = tabs
                    .iter_mut()
                    .find(|tab| tab.id == id && tab.branch_directory == directory)
                {
                    tab.branch_requested = false;
                }
                continue;
            };
            let head = branch.head.clone().or_else(|| {
                tabs.iter()
                    .find(|tab| tab.id == id && tab.branch_directory == directory)
                    .and_then(|tab| tab.branch_head.clone())
            });
            let branch = if let Some(head) = head {
                self.branch_cache
                    .borrow_mut()
                    .resolve(&head, generation, branch)
            } else {
                branch
            };
            for w in tabs.iter_mut() {
                let direct = w.id == id && w.branch_directory == directory;
                let shared = branch.head.is_some() && w.branch_head == branch.head;
                if !(direct || shared)
                    || !w
                        .panes
                        .get(&w.active)
                        .is_some_and(|pane| pane.directory() == w.branch_directory)
                {
                    continue;
                }
                if w.branch_label.text() != branch.name {
                    w.branch_label.set_label(&branch.name);
                    w.branch_box.set_tooltip_text(Some(&branch.name));
                    changed = true;
                }
                let visible = !branch.name.is_empty();
                if w.branch_box.get_visible() != visible {
                    w.branch_box.set_visible(visible);
                    changed = true;
                }
                // Keep the watch through a temporarily missing HEAD. A later
                // creation must still wake us without waiting for a fallback.
                if branch.head.is_some() {
                    w.branch_head = branch.head.clone();
                }
                w.branch_requested = true;
            }
        }
        if changed {
            self.refresh_sidebar();
        }
        let heads = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|tab| tab.branch_head.clone())
            .collect::<HashSet<_>>();
        self.branch_cache.borrow_mut().retain_heads(&heads);
        let mut monitors = self.branch_monitors.borrow_mut();
        monitors.retain(|head, monitor| {
            if heads.contains(head) {
                true
            } else {
                monitor.cancel();
                false
            }
        });
        let mut installed = HashSet::new();
        for head in heads {
            if monitors.contains_key(&head) {
                continue;
            }
            // Watch the parent so Git's atomic HEAD.lock -> HEAD rename is reported.
            let Some(parent) = head.parent() else {
                continue;
            };
            let Ok(monitor) = gio::File::for_path(parent)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };
            monitor.set_rate_limit(50);
            let weak = Rc::downgrade(self);
            let watched = head.clone();
            let watched_parent = parent.to_path_buf();
            monitor.connect_changed(move |_, file, other, event| {
                if event == gio::FileMonitorEvent::Unmounted
                    || (event == gio::FileMonitorEvent::Deleted
                        && file.path().as_ref() == Some(&watched_parent))
                {
                    if let Some(ui) = weak.upgrade()
                        && let Some(monitor) = ui.branch_monitors.borrow_mut().remove(&watched)
                    {
                        monitor.cancel();
                    }
                    return;
                }
                if (file.path().as_ref() == Some(&watched)
                    || other.and_then(gio::File::path).as_ref() == Some(&watched))
                    && let Some(ui) = weak.upgrade()
                {
                    ui.refresh_branch_head(&watched);
                }
            });
            installed.insert(head.clone());
            monitors.insert(head, monitor);
        }
        drop(monitors);
        // Close the gap between the first lookup and installing its monitor.
        for head in installed {
            self.refresh_branch_head(&head);
        }
    }
    pub(super) fn refresh_branch_head(&self, head: &Path) {
        // One query supplies every tab using this HEAD, including subdirectories.
        let jobs = self
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.branch_head.as_deref() == Some(head))
            .map(|tab| vec![(tab.id, tab.branch_directory.clone())])
            .unwrap_or_default();
        self.branches.refresh_subset(jobs);
    }
    pub(super) fn refresh_repository_monitors(self: &Rc<Self>) {
        let directories: HashSet<_> = self
            .tabs
            .borrow()
            .iter()
            .flat_map(|tab| tab.branch_directory.ancestors().map(Path::to_path_buf))
            .collect();
        let mut monitors = self.repository_monitors.borrow_mut();
        monitors.retain(|directory, monitor| {
            if directories.contains(directory) {
                true
            } else {
                monitor.cancel();
                false
            }
        });
        for directory in directories {
            if monitors.contains_key(&directory) {
                continue;
            }
            let Ok(monitor) = gio::File::for_path(&directory)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };
            monitor.set_rate_limit(50);
            let watched = directory.clone();
            let weak = Rc::downgrade(self);
            monitor.connect_changed(move |_, file, other, event| {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let git_changed = file
                    .basename()
                    .is_some_and(|name| name == Path::new(".git"))
                    || other
                        .and_then(gio::File::basename)
                        .is_some_and(|name| name == Path::new(".git"));
                let lost = event == gio::FileMonitorEvent::Unmounted
                    || (event == gio::FileMonitorEvent::Deleted
                        && file.path().as_ref() == Some(&watched));
                if lost && let Some(monitor) = ui.repository_monitors.borrow_mut().remove(&watched)
                {
                    monitor.cancel();
                }
                if git_changed || lost {
                    for tab in ui
                        .tabs
                        .borrow_mut()
                        .iter_mut()
                        .filter(|tab| tab.branch_directory.starts_with(&watched))
                    {
                        tab.branch_requested = false;
                        tab.branch_head = None;
                        tab.branch_box.set_visible(false);
                    }
                    ui.refresh_branches();
                }
            });
            monitors.insert(directory, monitor);
        }
    }
    pub(super) fn refresh_unwatched_branches(self: &Rc<Self>) {
        self.refresh_repository_monitors();
        let heads = self.branch_monitors.borrow();
        let directories = self.repository_monitors.borrow();
        let mut jobs = Vec::new();
        for tab in self.tabs.borrow_mut().iter_mut() {
            let unwatched = tab
                .branch_head
                .as_ref()
                .is_some_and(|head| !heads.contains_key(head))
                || tab
                    .branch_directory
                    .ancestors()
                    .any(|directory| !directories.contains_key(directory));
            if unwatched || !tab.branch_requested {
                tab.branch_requested = true;
                jobs.push((tab.id, tab.branch_directory.clone()));
            }
        }
        self.branches.refresh_subset(jobs);
    }
    pub(super) fn refresh_branches(self: &Rc<Self>) {
        let mut jobs = Vec::new();
        let mut ids = HashSet::new();
        for tab in self.tabs.borrow_mut().iter_mut() {
            ids.insert(tab.id);
            if let Some(pane) = tab.panes.get(&tab.active) {
                let directory = pane.directory();
                if tab.branch_directory != directory {
                    tab.branch_box.set_visible(false);
                    tab.branch_head = None;
                    tab.branch_requested = false;
                }
                tab.branch_directory = directory.clone();
                if !tab.branch_requested {
                    tab.branch_requested = true;
                    jobs.push((tab.id, directory));
                }
            }
        }
        self.refresh_repository_monitors();
        self.branches.retain_tabs(&ids);
        self.branches.refresh_subset(jobs);
        if ids.is_empty() {
            self.branch_cache.borrow_mut().retain_heads(&HashSet::new());
            for monitor in self.branch_monitors.borrow_mut().drain().map(|(_, m)| m) {
                monitor.cancel();
            }
        }
    }
}
