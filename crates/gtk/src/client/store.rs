//! Durable GTK organization shared by application windows.
use super::{
    control::{BackendReply, Command, CommandOutput},
    model::*,
};
use gmux_client::ipc;
use gmux_platform::unix::storage;
use gmux_protocol::control as execution;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Organization published by the store and shared immutably between clients.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub catalog: WorkspaceCatalog,
    pub desktop: DesktopState,
}
impl Snapshot {
    pub(super) fn execution_result(
        &self,
        daemon: DaemonId,
        result: execution::CommandResult,
    ) -> super::control::CommandResult {
        let output = match result.output {
            execution::CommandOutput::New { tab, pane } => CommandOutput::New {
                tab,
                pane,
                workspace: self
                    .desktop
                    .tabs
                    .iter()
                    .find(|t| t.reference == TabRef { daemon, id: tab })
                    .and_then(|t| t.workspace),
            },
            execution::CommandOutput::Split { pane } => CommandOutput::Split { pane },
            execution::CommandOutput::Done { ok } => CommandOutput::Done { ok },
        };
        super::control::CommandResult {
            output,
            warning: result.warning,
        }
    }

    fn normalize(&mut self) {
        self.catalog.retain_valid();
        let workspaces: HashSet<_> = self.catalog.workspaces.iter().map(|w| w.id).collect();
        for tab in &mut self.desktop.tabs {
            tab.workspace = tab.workspace.filter(|id| workspaces.contains(id));
        }
        if let TabFilter::Workspace(id) = self.desktop.filter
            && !workspaces.contains(&id)
        {
            self.desktop.filter = TabFilter::All;
        }
    }
}
struct Shared {
    state: Arc<Snapshot>,
    revision: u64,
    subscribers: HashMap<u64, gmux_platform::unix::ipc::Wake>,
    next_subscriber: u64,
}
pub struct Store {
    path: PathBuf,
    shared: Mutex<Shared>,
    editing: Mutex<()>,
}
impl Store {
    /// Different clients can choose independent profiles without changing the daemon.
    pub fn open(profile: &str) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !profile.is_empty()
                && profile
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid client profile"
        );
        Self::at(
            storage::config_dir()
                .join("clients")
                .join(profile)
                .join("state.json"),
        )
    }
    pub(super) fn at(path: PathBuf) -> anyhow::Result<Arc<Self>> {
        let mut state: Snapshot = storage::load_at(&path)?;
        state.normalize();
        Ok(Arc::new(Self {
            path,
            editing: Mutex::new(()),
            shared: Mutex::new(Shared {
                state: Arc::new(state),
                revision: 0,
                subscribers: HashMap::new(),
                next_subscriber: 0,
            }),
        }))
    }
    pub fn subscribe(self: &Arc<Self>, wake: gmux_platform::unix::ipc::Wake) -> Subscription {
        let mut shared = self.shared.lock().unwrap();
        let id = shared.next_subscriber;
        shared.next_subscriber += 1;
        shared.subscribers.insert(id, wake);
        Subscription {
            store: Arc::downgrade(self),
            id,
        }
    }
    /// Share one immutable view without copying it while holding the store lock.
    pub fn snapshot(&self) -> (Arc<Snapshot>, u64) {
        let shared = self.shared.lock().unwrap();
        (Arc::clone(&shared.state), shared.revision)
    }
    pub fn revision(&self) -> u64 {
        self.shared.lock().unwrap().revision
    }
    fn edit(
        &self,
        publish: bool,
        operation: impl FnOnce(&mut Snapshot, u64) -> anyhow::Result<CommandOutput>,
    ) -> anyhow::Result<CommandOutput> {
        let _editing = self.editing.lock().unwrap();
        let revision = self.revision();
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        // Reload under the writer lock so another client process cannot lose a catalog edit.
        let (next, output) = storage::with_lock_at(&self.path, || {
            let mut next: Snapshot = storage::load_at(&self.path)?;
            next.normalize();
            let output = operation(&mut next, revision)?;
            storage::save_locked_at(&self.path, &next)?;
            Ok((next, output))
        })?;
        let mut shared = self.shared.lock().unwrap();
        shared.state = Arc::new(next);
        if publish {
            shared.revision += 1;
            for wake in shared.subscribers.values() {
                wake.notify();
            }
        }
        Ok(output)
    }
    pub fn save_desktop(&self, mut desktop: DesktopState, revision: u64) -> anyhow::Result<()> {
        self.edit(false, |state, current| {
            let previous = &state.desktop;
            let assignments: HashMap<_, _> = previous
                .tabs
                .iter()
                .map(|tab| (tab.reference, tab.workspace))
                .collect();
            let mut references = HashSet::new();
            for saved in &mut desktop.tabs {
                anyhow::ensure!(references.insert(saved.reference), "Duplicate desktop tab");
                validate_tab(saved)?;
                if let Some(&workspace) = assignments.get(&saved.reference) {
                    saved.workspace = workspace;
                }
                saved.workspace = saved
                    .workspace
                    .filter(|id| state.catalog.workspaces.iter().any(|w| w.id == *id));
            }
            if revision < current {
                desktop.tabs.extend(
                    previous
                        .tabs
                        .iter()
                        .filter(|t| !references.contains(&t.reference))
                        .cloned(),
                );
            }
            state.desktop = desktop;
            Ok(CommandOutput::Done { ok: true })
        })?;
        Ok(())
    }
    pub fn assign(&self, tab: DesktopTab) -> anyhow::Result<CommandOutput> {
        validate_tab(&tab)?;
        self.edit(true, |state, _| {
            anyhow::ensure!(
                tab.workspace
                    .is_none_or(|id| state.catalog.workspaces.iter().any(|w| w.id == id)),
                "Workspace does not exist"
            );
            if let Some(saved) = state
                .desktop
                .tabs
                .iter_mut()
                .find(|t| t.reference == tab.reference)
            {
                saved.workspace = tab.workspace;
            } else {
                state.desktop.tabs.insert(0, tab);
            }
            Ok(CommandOutput::Done { ok: true })
        })
    }
    #[cfg(test)]
    pub fn command(&self, local: DaemonId, command: Command) -> anyhow::Result<CommandOutput> {
        self.command_at(Some(local), Some(local), command)
    }
    fn command_at(
        &self,
        primary: Option<DaemonId>,
        local: Option<DaemonId>,
        command: Command,
    ) -> anyhow::Result<CommandOutput> {
        self.edit(true, |state, _| match command {
            Command::WorkspaceAdd {
                name,
                directory,
                mut directories,
                tab,
            } => {
                if let Some(directory) = directory {
                    directories.insert(
                        local.ok_or_else(|| {
                            anyhow::anyhow!(
                                "Local execution is not enabled; use a connection directory"
                            )
                        })?,
                        directory,
                    );
                }
                validate_directories(local, &mut directories)?;
                let name = state.catalog.validate_name(&name, None)?;
                let target = tab
                    .map(|id| {
                        primary
                            .map(|daemon| TabRef { daemon, id })
                            .ok_or_else(|| anyhow::anyhow!("Local execution is not enabled"))
                    })
                    .transpose()?;
                if let Some(target) = target {
                    anyhow::ensure!(
                        state.desktop.tabs.iter().any(|t| t.reference == target),
                        "The tab has been closed"
                    );
                }
                let workspace = state.catalog.reserve_id()?;
                state.catalog.workspaces.push(WorkspaceDefinition {
                    id: workspace,
                    name,
                    directories,
                });
                if let Some(target) = target {
                    state
                        .desktop
                        .tabs
                        .iter_mut()
                        .find(|t| t.reference == target)
                        .unwrap()
                        .workspace = Some(workspace);
                }
                Ok(CommandOutput::Workspace { workspace })
            }
            Command::WorkspaceEdit {
                workspace,
                name,
                directory,
                directories,
            } => {
                let name = state.catalog.validate_name(&name, Some(workspace))?;
                let definition = state
                    .catalog
                    .workspaces
                    .iter_mut()
                    .find(|w| w.id == workspace)
                    .ok_or_else(|| anyhow::anyhow!("Workspace no longer exists"))?;
                let mut directories = directories.unwrap_or_else(|| definition.directories.clone());
                if let Some(directory) = directory {
                    directories.insert(
                        local.ok_or_else(|| {
                            anyhow::anyhow!(
                                "Local execution is not enabled; use a connection directory"
                            )
                        })?,
                        directory,
                    );
                }
                validate_directories(local, &mut directories)?;
                definition.name = name;
                definition.directories = directories;
                Ok(CommandOutput::Workspace { workspace })
            }
            Command::WorkspaceRemove { workspace } => {
                anyhow::ensure!(
                    state.catalog.workspaces.iter().any(|w| w.id == workspace),
                    "Workspace {workspace} does not exist"
                );
                state.catalog.workspaces.retain(|w| w.id != workspace);
                for tab in &mut state.desktop.tabs {
                    if tab.workspace == Some(workspace) {
                        tab.workspace = None;
                    }
                }
                if state.desktop.filter == TabFilter::Workspace(workspace) {
                    state.desktop.filter = TabFilter::All;
                }
                Ok(CommandOutput::Done { ok: true })
            }
            _ => anyhow::bail!("Command requires an execution endpoint"),
        })
    }
    /// Resolve a workspace directory on the client; home expansion belongs to the destination.
    pub fn starting_directory(
        &self,
        workspace: Option<u64>,
        destination: DaemonId,
        explicit: Option<PathBuf>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let shared = self.shared.lock().unwrap();
        let configured = workspace
            .map(|id| {
                shared
                    .state
                    .catalog
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| anyhow::anyhow!("Workspace {id} does not exist"))
            })
            .transpose()?
            .and_then(|w| w.directories.get(&destination));
        Ok(explicit.or_else(|| configured.cloned()))
    }

    /// Execute on a request worker. Client-only requests never reach the daemon socket.
    #[cfg(test)]
    pub fn call(&self, endpoint: &ipc::Endpoint, body: Value) -> anyhow::Result<BackendReply> {
        self.call_inner(endpoint, body, None, None)
    }
    pub fn call_cancellable(
        &self,
        endpoint: &ipc::Endpoint,
        local: impl Into<Option<DaemonId>>,
        body: Value,
        cancellation: &gmux_platform::unix::ipc::Cancellation,
    ) -> anyhow::Result<BackendReply> {
        self.call_inner(endpoint, body, local.into(), Some(cancellation))
    }
    fn call_inner(
        &self,
        endpoint: &ipc::Endpoint,
        mut body: Value,
        local: Option<DaemonId>,
        cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
    ) -> anyhow::Result<BackendReply> {
        let call =
            |request: Value| gmux_client::connections::call_at(endpoint, request, cancellation);
        let method = body["method"].as_str().unwrap_or("").to_owned();
        let output = match method.as_str() {
            "_desktop-save" => {
                let revision = body["revision"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Desktop save requires a revision"))?;
                self.save_desktop(serde_json::from_value(body["desktop"].take())?, revision)?;
                return Ok(BackendReply::Value(json!({"ok":true})));
            }
            "_desktop-tab" => Some(self.assign(serde_json::from_value(body["tab"].take())?)?),
            "_command" if super::backend::organization_request(&body) => Some(self.command_at(
                local,
                local,
                serde_json::from_value(body["command"].take())?,
            )?),
            _ => None,
        };
        if let Some(output) = output {
            return Ok(BackendReply::Organization {
                result: output.into(),
            });
        }
        let request = if method == "_connection-call" {
            &body["request"]
        } else {
            &body
        };
        let snapshot = execution::Request::decode(request.clone())
            .is_ok_and(|request| request.returns_snapshot());
        let reply = call(body)?;
        if snapshot {
            Ok(BackendReply::Execution {
                reply: serde_json::from_value(reply)?,
            })
        } else {
            Ok(BackendReply::Value(reply))
        }
    }
}
fn validate_tab(tab: &DesktopTab) -> anyhow::Result<()> {
    anyhow::ensure!(
        tab.active.daemon == tab.reference.daemon && tab.layout.contains(tab.active.id),
        "Invalid desktop pane reference"
    );
    gmux_protocol::model::validate_ids(tab.reference.id, tab.layout.ids())
}
fn validate_directories(
    local: Option<DaemonId>,
    directories: &mut BTreeMap<DaemonId, PathBuf>,
) -> anyhow::Result<()> {
    for (&machine, directory) in directories.iter_mut() {
        if Some(machine) == local {
            *directory = std::path::absolute(expand_home(directory))?;
            anyhow::ensure!(directory.is_dir(), "Choose an existing directory");
        } else {
            anyhow::ensure!(
                is_machine_directory(directory),
                "Remote directories must be absolute paths or start with ~/"
            );
        }
    }
    directories.retain(|&machine, path| {
        path != Path::new("~") && !(Some(machine) == local && *path == home())
    });
    Ok(())
}

pub struct Subscription {
    store: std::sync::Weak<Store>,
    id: u64,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(store) = self.store.upgrade() {
            store.shared.lock().unwrap().subscribers.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        path: PathBuf,
        store: Arc<Store>,
        local: DaemonId,
        remote: DaemonId,
    }
    impl Fixture {
        fn new() -> Self {
            let local = gmux_platform::unix::identity::daemon_id().unwrap();
            let path = std::env::temp_dir().join(format!("gmux-client-store-{local}"));
            let store = Store::at(path.join("state.json")).unwrap();
            Self {
                path,
                store,
                local,
                remote: gmux_platform::unix::identity::daemon_id().unwrap(),
            }
        }
        fn workspace(&self, name: &str) -> u64 {
            let CommandOutput::Workspace { workspace } = self
                .store
                .command(
                    self.local,
                    Command::WorkspaceAdd {
                        name: name.into(),
                        directory: None,
                        directories: BTreeMap::new(),
                        tab: None,
                    },
                )
                .unwrap()
            else {
                panic!("Expected workspace")
            };
            workspace
        }
        fn tab(&self, daemon: DaemonId, workspace: Option<u64>) -> DesktopTab {
            DesktopTab {
                reference: TabRef { daemon, id: 1 },
                workspace,
                layout: Layout::Pane(2),
                active: PaneRef { daemon, id: 2 },
                name: "shell".into(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn desktop_save_requires_an_explicit_revision() {
        let f = Fixture::new();
        let mut request = json!({"method":"_desktop-save", "desktop":DesktopState::default()});
        for revision in [Value::Null, json!("zero"), json!(-1)] {
            request["revision"] = revision;
            assert!(
                f.store
                    .call(&ipc::Endpoint::local(), request.clone())
                    .is_err()
            );
            assert!(!f.store.path.exists());
        }
        request.as_object_mut().unwrap().remove("revision");
        assert!(
            f.store
                .call(&ipc::Endpoint::local(), request.clone())
                .is_err()
        );
        request["revision"] = json!(0);
        assert!(f.store.call(&ipc::Endpoint::local(), request).is_ok());
    }

    #[test]
    fn delayed_window_saves_preserve_new_assignments_and_unavailable_hosts() {
        let f = Fixture::new();
        let workspace = f.workspace("Project");
        f.store.assign(f.tab(f.local, None)).unwrap();
        let (snapshot, revision) = f.store.snapshot();
        let mut delayed = snapshot.desktop.clone();
        f.store.assign(f.tab(f.remote, Some(workspace))).unwrap();
        f.store.assign(f.tab(f.local, Some(workspace))).unwrap();
        delayed.width = 1200;
        f.store.save_desktop(delayed, revision).unwrap();
        let (saved, _) = f.store.snapshot();
        let saved = &saved.desktop;
        assert_eq!(
            saved
                .tabs
                .iter()
                .map(|t| t.reference.daemon)
                .collect::<HashSet<_>>(),
            HashSet::from([f.remote, f.local])
        );
        assert!(saved.tabs.iter().all(|t| t.workspace == Some(workspace)));
        assert_eq!(saved.width, 1200);
        f.store
            .command(f.local, Command::WorkspaceRemove { workspace })
            .unwrap();
        let reopened = Store::at(f.store.path.clone()).unwrap();
        let (snapshot, _) = reopened.snapshot();
        let catalog = &snapshot.catalog;
        let desktop = &snapshot.desktop;
        assert!(catalog.workspaces.is_empty());
        assert!(desktop.tabs.iter().all(|t| t.workspace.is_none()));
        assert!(f.workspace("Replacement") > workspace);
    }

    #[test]
    fn loading_stale_client_assignments_repairs_the_view_without_rewriting_the_file() {
        let f = Fixture::new();
        let workspace = f.workspace("Deleted externally");
        f.store.assign(f.tab(f.remote, Some(workspace))).unwrap();
        let mut saved: Snapshot = storage::load_at(&f.store.path).unwrap();
        saved.catalog.workspaces.clear();
        saved.desktop.filter = TabFilter::Workspace(workspace);
        let bytes = serde_json::to_vec(&saved).unwrap();
        std::fs::write(&f.store.path, &bytes).unwrap();
        let reopened = Store::at(f.store.path.clone()).unwrap();
        let (snapshot, _) = reopened.snapshot();
        let desktop = &snapshot.desktop;
        assert!(desktop.filter == TabFilter::All);
        assert!(desktop.tabs[0].workspace.is_none());
        assert_eq!(desktop.tabs[0].reference.daemon, f.remote);
        assert_eq!(std::fs::read(&f.store.path).unwrap(), bytes);
    }

    #[test]
    fn workspace_edits_are_atomic_and_remote_paths_are_never_expanded_on_the_receiver() {
        let f = Fixture::new();
        let workspace = f.workspace("Project");
        let directories = BTreeMap::from([
            (f.remote, PathBuf::from("~/remote-only")),
            (f.local, std::env::temp_dir()),
        ]);
        f.store
            .command(
                f.local,
                Command::WorkspaceEdit {
                    workspace,
                    name: "Project".into(),
                    directory: None,
                    directories: Some(directories.clone()),
                },
            )
            .unwrap();
        assert_eq!(
            f.store
                .starting_directory(Some(workspace), f.remote, None)
                .unwrap(),
            Some(PathBuf::from("~/remote-only"))
        );
        assert_eq!(
            f.store
                .starting_directory(
                    Some(workspace),
                    gmux_platform::unix::identity::daemon_id().unwrap(),
                    None
                )
                .unwrap(),
            None
        );
        assert_eq!(
            f.store
                .starting_directory(Some(workspace), f.remote, Some(PathBuf::from("/explicit")))
                .unwrap(),
            Some(PathBuf::from("/explicit"))
        );
        let before = std::fs::read(&f.store.path).unwrap();
        let invalid = BTreeMap::from([(f.remote, PathBuf::from("relative"))]);
        assert!(
            f.store
                .command(
                    f.local,
                    Command::WorkspaceEdit {
                        workspace,
                        name: "Invalid".into(),
                        directory: None,
                        directories: Some(invalid)
                    }
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&f.store.path).unwrap(), before);
        assert_eq!(
            f.store.snapshot().0.catalog.workspaces[0].directories,
            directories
        );
        let mut reset = directories;
        reset.insert(f.remote, PathBuf::from("~"));
        f.store
            .command(
                f.local,
                Command::WorkspaceEdit {
                    workspace,
                    name: "Project".into(),
                    directory: None,
                    directories: Some(reset),
                },
            )
            .unwrap();
        assert!(
            !f.store.snapshot().0.catalog.workspaces[0]
                .directories
                .contains_key(&f.remote)
        );
    }

    #[test]
    fn blocked_disk_writes_leave_client_snapshots_available() {
        use std::{
            fs::OpenOptions,
            os::fd::AsRawFd,
            sync::mpsc,
            time::{Duration, Instant},
        };
        let f = Fixture::new();
        std::fs::create_dir_all(&f.path).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(f.path.join(".state.json.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| f.workspace("Waiting for disk"));
            let deadline = Instant::now() + Duration::from_secs(2);
            while f.store.editing.try_lock().is_ok() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let (sender, reply) = mpsc::channel();
            let store = &f.store;
            scope.spawn(move || {
                sender
                    .send(store.snapshot().0.catalog.workspaces.len())
                    .unwrap()
            });
            let result = reply.recv_timeout(Duration::from_millis(200));
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
            assert_eq!(result.unwrap(), 0);
            writer.join().unwrap();
        });
        assert_eq!(f.store.snapshot().0.catalog.workspaces.len(), 1);
    }

    #[test]
    fn concurrent_catalog_writers_keep_every_edit_and_release_window_subscriptions() {
        let f = Fixture::new();
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|i| {
                    let f = &f;
                    scope.spawn(move || f.workspace(&format!("Project {i}")))
                })
                .collect();
            let ids: HashSet<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert_eq!(ids.len(), 8);
        });
        assert_eq!(
            Store::at(f.store.path.clone())
                .unwrap()
                .snapshot()
                .0
                .catalog
                .workspaces
                .len(),
            8
        );
        let subscription = f
            .store
            .subscribe(gmux_platform::unix::ipc::Wake::new().unwrap());
        assert_eq!(f.store.shared.lock().unwrap().subscribers.len(), 1);
        drop(subscription);
        assert!(f.store.shared.lock().unwrap().subscribers.is_empty());
        // Presentation defaults are private to a window and cannot cause idle save loops.
        let revision = f.store.revision();
        f.store
            .save_desktop(DesktopState::default(), revision)
            .unwrap();
        assert_eq!(f.store.revision(), revision);
    }
}
