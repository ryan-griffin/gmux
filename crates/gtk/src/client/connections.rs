//! Client-local streams, qualified handles, and command routing across daemons.
use super::{
    Snapshot, Store, Subscription,
    control::{
        BackendReply, Command, CommandOutput, CommandReply, CommandResult, InternalRequest,
        TabCommand,
    },
    model::{
        DaemonId, DesktopState, DesktopTab, ExecutionState, Layout, Machine, MachineKind, PaneRef,
        Session, TabRef, TabState,
    },
};
use gmux_client::{
    control as execution, ipc, remote::SessionHandle, session::SessionUpdate, theme::Theme,
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gmux_client::connection::Connection;
pub use gmux_client::connection::Status as ConnectionStatus;

struct Origin {
    machine: Machine,
    home_directory: Option<PathBuf>,
    link: Connection,
    execution: ExecutionState,
    revision: u64,
}
impl Origin {
    fn new(machine: Machine, wake: ipc::Wake) -> Self {
        let mut link = Connection::new(
            machine.daemon,
            ipc::Endpoint::remote(machine.name.clone(), Some(machine.daemon)),
            wake,
        );
        link.set_auto_reconnect(true);
        Self {
            machine,
            home_directory: None,
            link,
            execution: ExecutionState::default(),
            revision: 0,
        }
    }
}
pub struct Connections {
    primary: Option<DaemonId>,
    local: Option<DaemonId>,
    local_enabled: bool,
    preferred: Option<String>,
    definitions: Vec<gmux_client::connections::Remote>,
    watcher: gmux_client::connections::Watch,
    store: Arc<Store>,
    client_revision: u64,
    _subscription: Subscription,
    origins: HashMap<DaemonId, Origin>,
    handles: Handles,
    revision: u64,
    desktop: DesktopState,
    organization: Arc<Snapshot>,
    wake: ipc::Wake,
    snapshot: Option<CommandReply>,
    attached: HashSet<u64>,
    focused: Cell<bool>,
    frame_interval: Duration,
    status_changed: bool,
}
pub struct Route {
    pub destination: Option<DaemonId>,
    pub body: Value,
    pub assign: Option<Assignment>,
}
pub enum Assignment {
    CreatedTab { workspace: Option<u64> },
    CreatedWorkspace { tab: u64 },
}
impl Assignment {
    pub fn resolve(self, output: &CommandOutput) -> Option<(u64, Option<u64>)> {
        match (self, output) {
            (Self::CreatedTab { workspace }, CommandOutput::New { tab, .. }) => {
                Some((*tab, workspace))
            }
            (Self::CreatedWorkspace { tab }, CommandOutput::Workspace { workspace }) => {
                Some((tab, Some(*workspace)))
            }
            _ => None,
        }
    }
}
#[derive(Default)]
struct Handles {
    ids: HashMap<(DaemonId, u64), u64>,
    references: HashMap<u64, (DaemonId, u64)>,
    next: u64,
    dirty: bool,
    execution_count: usize,
}
impl Handles {
    fn get(&mut self, daemon: DaemonId, id: u64) -> u64 {
        if id == 0 {
            return 0;
        }
        *self.ids.entry((daemon, id)).or_insert_with(|| {
            self.next += 1;
            self.references.insert(self.next, (daemon, id));
            self.dirty = true;
            self.next
        })
    }
}
fn layout_ids(layout: &mut Layout, map: &mut impl FnMut(u64) -> u64) {
    match layout {
        Layout::Pane(id) => *id = map(*id),
        Layout::Split { first, second, .. } => {
            layout_ids(first, map);
            layout_ids(second, map);
        }
    }
}
impl Connections {
    pub fn new(
        wake: ipc::Wake,
        store: Arc<Store>,
        local_enabled: bool,
        preferred: Option<String>,
    ) -> anyhow::Result<(Self, CommandReply)> {
        // Observe edits made while loading the initial catalog, too.
        let watcher = gmux_client::connections::Watch::new(wake.clone())?;
        let catalog = gmux_client::connections::Catalog::load()?;
        let subscription = store.subscribe(wake.clone());
        let (organization, client_revision) = store.snapshot();
        let primary = match preferred.as_deref() {
            Some("local") => None,
            Some(name) => Some(catalog.remote(name, None)?.daemon),
            None if local_enabled => None,
            None => catalog.remotes.first().map(|remote| remote.daemon),
        };
        let origins = catalog
            .connections()
            .into_iter()
            .map(|machine| (machine.daemon, Origin::new(machine, wake.clone())))
            .collect();
        let mut hub = Self {
            primary,
            local: None,
            local_enabled,
            preferred,
            definitions: catalog.remotes,
            watcher,
            store: store.clone(),
            client_revision,
            _subscription: subscription,
            origins,
            handles: Handles::default(),
            revision: 0,
            desktop: organization.desktop.clone(),
            organization,
            wake,
            snapshot: None,
            attached: HashSet::new(),
            focused: Cell::new(false),
            frame_interval: gmux_client::transport::DEFAULT_FRAME_INTERVAL,
            status_changed: false,
        };
        let initial = hub.combined(CommandOutput::Done { ok: true }.into());
        Ok((hub, initial))
    }
    pub fn local_daemon(&self) -> Option<DaemonId> {
        self.local
    }
    pub fn enable_local(&mut self, machine: Machine) {
        let daemon = machine.daemon;
        if self.local.is_some_and(|id| id != daemon) {
            self.disable_local();
        }
        self.local_enabled = true;
        self.local = Some(daemon);
        self.origins
            .entry(daemon)
            .or_insert_with(|| Origin::new(machine, self.wake.clone()))
            .link
            .set_auto_reconnect(false);
        if self.preferred.as_deref().is_none_or(|name| name == "local") || self.primary.is_none() {
            self.primary = Some(daemon);
        }
        self.status_changed = true;
        self.revision += 1;
        self.connect(daemon);
    }
    pub fn disable_local(&mut self) {
        self.local_enabled = false;
        if let Some(id) = self.local.take() {
            self.origins.remove(&id);
            if self.primary == Some(id) {
                self.primary = None;
            }
        }
        if self.primary.is_none() {
            self.primary = self.fallback_daemon();
        }
        self.status_changed = true;
        self.revision += 1;
        self.wake.notify();
    }
    fn fallback_daemon(&self) -> Option<DaemonId> {
        // An enabled local daemon may still be starting and have no identity yet.
        if self.local_enabled || self.preferred.as_deref() == Some("local") {
            self.local
        } else {
            self.local
                .or_else(|| self.definitions.first().map(|remote| remote.daemon))
        }
    }
    pub fn snapshot(&mut self) -> CommandReply {
        self.refresh_organization();
        let mut reply = self.combined(CommandOutput::Done { ok: true }.into());
        reply.desktop = Some(self.desktop.clone());
        reply
    }
    fn required_default(&self) -> anyhow::Result<DaemonId> {
        self.primary.ok_or_else(|| {
            anyhow::anyhow!("Enable the Local Daemon or add a connection in Manage Connections")
        })
    }
    fn handle(&mut self, daemon: DaemonId, id: u64) -> u64 {
        self.handles.get(daemon, id)
    }
    pub fn default_daemon(&self) -> Option<DaemonId> {
        self.primary
    }
    pub fn preferred_connection(&self) -> Option<String> {
        match self.primary {
            Some(id) if Some(id) == self.local => Some("local".into()),
            Some(id) => self
                .origins
                .get(&id)
                .map(|origin| origin.machine.name.clone()),
            None => self.preferred.clone(),
        }
    }
    pub fn select_default(&mut self, name: String) -> anyhow::Result<()> {
        self.refresh_definitions()?;
        self.primary = if name == "local" {
            self.local
        } else {
            Some(self.connection_id(&name)?)
        };
        self.preferred = Some(name);
        self.status_changed = true;
        self.wake.notify();
        Ok(())
    }
    pub fn connection_revision(&self) -> u64 {
        self.revision
    }
    pub fn desktop_revision(&self) -> u64 {
        self.client_revision
    }
    pub fn tab_ref(&self, id: u64) -> TabRef {
        let &(daemon, id) = self.handles.references.get(&id).expect("Known tab handle");
        TabRef { daemon, id }
    }
    fn checked_ref(&self, id: u64) -> anyhow::Result<TabRef> {
        let &(daemon, id) = self
            .handles
            .references
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("The tab or pane has been closed"))?;
        Ok(TabRef { daemon, id })
    }
    pub fn pane_ref(&self, id: u64) -> PaneRef {
        let tab = self.tab_ref(id);
        PaneRef {
            daemon: tab.daemon,
            id: tab.id,
        }
    }
    pub fn lookup(&self, daemon: DaemonId, id: u64) -> Option<u64> {
        self.handles.ids.get(&(daemon, id)).copied()
    }
    pub fn default_handle(&self, id: u64) -> anyhow::Result<u64> {
        self.lookup(self.required_default()?, id)
            .ok_or_else(|| anyhow::anyhow!("ID {id} does not exist on the default connection"))
    }
    pub fn is_local(&self, id: u64) -> bool {
        self.handles
            .references
            .get(&id)
            .is_some_and(|r| Some(r.0) == self.local)
    }
    pub fn connection_name(&self, id: u64) -> String {
        self.handles
            .references
            .get(&id)
            .and_then(|r| self.origins.get(&r.0))
            .map(|o| o.machine.name.clone())
            .unwrap_or_default()
    }
    pub fn has_connection(&self, daemon: DaemonId) -> bool {
        self.origins.contains_key(&daemon)
    }
    pub fn connections(&self) -> Vec<Machine> {
        let mut machines: Vec<_> = self.origins.values().map(|o| o.machine.clone()).collect();
        machines.sort_by(|a, b| {
            (Some(a.daemon) != self.local, &a.name).cmp(&(Some(b.daemon) != self.local, &b.name))
        });
        machines
    }
    pub fn machine_kind(&self, id: u64) -> MachineKind {
        self.kind(self.tab_ref(id).daemon)
    }
    pub fn display_directory(&self, daemon: DaemonId, path: &Path) -> String {
        self.origins
            .get(&daemon)
            .and_then(|origin| origin.home_directory.as_ref())
            .map_or_else(
                || path.display().to_string(),
                |home| gmux_client::model::display_path_with_home(path, home),
            )
    }
    pub fn kind(&self, daemon: DaemonId) -> MachineKind {
        self.origins
            .get(&daemon)
            .map_or(MachineKind::Unknown, |origin| origin.machine.machine_kind)
    }
    pub fn connection_id(&self, name: &str) -> anyhow::Result<DaemonId> {
        if name == "local" {
            return self
                .local
                .ok_or_else(|| anyhow::anyhow!("Local execution is not enabled"));
        }
        self.origins
            .values()
            .find(|o| o.machine.name == name)
            .map(|o| o.machine.daemon)
            .ok_or_else(|| anyhow::anyhow!("Connection {name} does not exist"))
    }
    pub fn external_handle(&self, connection: Option<&str>, id: u64) -> anyhow::Result<u64> {
        let daemon = connection
            .map(|name| self.connection_id(name))
            .transpose()?
            .or(self.primary)
            .ok_or_else(|| anyhow::anyhow!("No connection is selected"))?;
        self.lookup(daemon, id)
            .ok_or_else(|| anyhow::anyhow!("ID {id} does not exist on the selected connection"))
    }
    pub fn connected(&self, daemon: DaemonId) -> bool {
        self.origins
            .get(&daemon)
            .is_some_and(|o| o.link.status() == ConnectionStatus::Connected)
    }
    pub fn connection_status(&self, daemon: DaemonId) -> ConnectionStatus<'_> {
        self.origins
            .get(&daemon)
            .map_or(ConnectionStatus::Disconnected { error: None }, |origin| {
                origin.link.status()
            })
    }
    pub fn connect_remotes(&mut self) {
        let ids: Vec<_> = self.origins.keys().copied().collect();
        for id in ids {
            self.connect(id);
        }
    }
    pub fn next_retry(&self) -> Option<Instant> {
        self.origins
            .values()
            .filter_map(|origin| origin.link.next_retry())
            .min()
    }
    /// Earliest deadline among attached connections' coalesced scroll targets.
    pub fn next_scroll(&self) -> Option<Instant> {
        self.origins
            .values()
            .filter_map(|origin| origin.link.next_scroll())
            .min()
    }
    pub fn connect(&mut self, id: DaemonId) {
        let Some(origin) = self.origins.get(&id) else {
            return;
        };
        let endpoint = if Some(id) == self.local {
            ipc::Endpoint::local()
        } else {
            ipc::Endpoint::remote(origin.machine.name.clone(), Some(id))
        };
        self.connect_endpoint(id, endpoint);
    }
    fn connect_endpoint(&mut self, id: DaemonId, endpoint: ipc::Endpoint) {
        let Some(origin) = self.origins.get_mut(&id) else {
            return;
        };
        origin.link.set_endpoint(endpoint);
        if origin.link.connect() {
            // Every attempt creates a new session manager. Existing terminal
            // widgets must attach to it, including after a catalog edit cancels
            // the old link before poll can observe its disconnection.
            self.attached
                .retain(|handle| self.handles.references[handle].0 != id);
            self.status_changed = true;
        }
    }
    /// Combine an execution snapshot with this client's organization and other connections.
    #[cfg(test)]
    pub fn project_execution(
        &mut self,
        reply: execution::CommandReply,
    ) -> anyhow::Result<CommandReply> {
        self.project(BackendReply::Execution { reply })
    }
    pub fn project(&mut self, reply: BackendReply) -> anyhow::Result<CommandReply> {
        self.refresh_organization();
        let result = match reply {
            BackendReply::Execution { reply } => {
                // Control replies can create panes before their stream snapshot arrives.
                // Stream metadata is installed by Connection::poll instead.
                if let Some(origin) = self.origins.get_mut(&reply.daemon)
                    && reply.revision >= origin.revision
                    && let Some(manager) = origin.link.manager_mut()
                {
                    manager.install_metadata(&reply.metadata, reply.revision);
                }
                self.ingest_execution(reply)?
            }
            BackendReply::Organization { result } => result,
            BackendReply::Value(_) => anyhow::bail!("Expected an execution or organization reply"),
        };
        Ok(self.combined(result))
    }
    fn refresh_definitions(&mut self) -> anyhow::Result<()> {
        let catalog = gmux_client::connections::Catalog::load()?;
        let keep: HashSet<_> = catalog
            .remotes
            .iter()
            .map(|r| r.daemon)
            .chain(self.local)
            .collect();
        self.origins.retain(|id, _| keep.contains(id));
        let mut reconnect = Vec::new();
        for remote in &catalog.remotes {
            let previous = self.definitions.iter().find(|r| r.daemon == remote.daemon);
            let connection_changed = previous.is_none_or(|old| {
                old.address != remote.address || old.authentication != remote.authentication
            });
            let renamed = previous.is_some_and(|old| old.name != remote.name);
            let origin = self.origins.entry(remote.daemon).or_insert_with(|| {
                Origin::new(
                    Machine {
                        name: remote.name.clone(),
                        daemon: remote.daemon,
                        machine_kind: remote.machine_kind,
                    },
                    self.wake.clone(),
                )
            });
            origin.machine.name = remote.name.clone();
            origin.machine.machine_kind = remote.machine_kind;
            if connection_changed
                || (renamed
                    && matches!(
                        origin.link.status(),
                        ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. }
                    ))
            {
                origin.link.cancel();
                reconnect.push(remote.daemon);
            } else if renamed {
                origin.link.set_endpoint(ipc::Endpoint::remote(
                    remote.name.clone(),
                    Some(remote.daemon),
                ));
            }
        }
        if let Some(local) = self.local {
            self.origins.get_mut(&local).unwrap().machine.name = catalog.local_name().into();
        }
        self.definitions = catalog.remotes;
        if self
            .primary
            .is_none_or(|id| !self.origins.contains_key(&id))
        {
            self.primary = self.fallback_daemon();
        }
        self.status_changed = true;
        self.revision += 1;
        for id in reconnect {
            self.connect(id);
        }
        Ok(())
    }
    fn refresh_organization(&mut self) {
        let (organization, revision) = self.store.snapshot();
        if !Arc::ptr_eq(&self.organization, &organization) {
            self.desktop = organization.desktop.clone();
            self.organization = organization;
        }
        self.client_revision = revision;
    }
    fn ingest_execution(
        &mut self,
        reply: execution::CommandReply,
    ) -> anyhow::Result<CommandResult> {
        let mut result = self
            .organization
            .execution_result(reply.daemon, reply.result);
        let daemon = reply.daemon;
        let origin = self
            .origins
            .get_mut(&daemon)
            .ok_or_else(|| anyhow::anyhow!("Unknown daemon"))?;
        if reply.revision >= origin.revision {
            origin.machine.machine_kind = reply.machine_kind;
            origin.home_directory = Some(reply.home_directory);
            origin.revision = reply.revision;
            origin.execution = reply.execution;
        }
        match &mut result.output {
            CommandOutput::New { tab, pane, .. } => {
                *tab = self.handle(daemon, *tab);
                *pane = self.handle(daemon, *pane);
            }
            CommandOutput::Split { pane } => *pane = self.handle(daemon, *pane),
            _ => (),
        }
        Ok(result)
    }
    fn combined(&mut self, result: CommandResult) -> CommandReply {
        let desktop_indices: HashMap<_, _> = self
            .desktop
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| (tab.reference, index))
            .collect();
        let mut tabs = Vec::new();
        let handles = &mut self.handles;
        let mut execution_count = 0;
        for (&daemon, origin) in &self.origins {
            for tab in &origin.execution.tabs {
                let reference = TabRef { daemon, id: tab.id };
                let saved = desktop_indices
                    .get(&reference)
                    .map(|&index| &self.desktop.tabs[index]);
                let mut tab = TabState::from_execution(daemon, tab, saved);
                tab.id = handles.get(daemon, tab.id);
                tab.active = handles.get(daemon, tab.active);
                execution_count += 1;
                layout_ids(&mut tab.layout, &mut |id| {
                    execution_count += 1;
                    handles.get(daemon, id)
                });
                tab.directories = tab
                    .directories
                    .into_iter()
                    .map(|(id, path)| (handles.get(daemon, id), path))
                    .collect();
                tabs.push(tab);
            }
        }
        // Insertions mark themselves dirty. A lower count detects removals without
        // scanning the maps for ordinary title, directory, or metadata updates.
        handles.dirty |= handles.execution_count != execution_count;
        handles.execution_count = execution_count;
        tabs.sort_unstable_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.daemon.cmp(&b.daemon))
                .then_with(|| self.tab_ref(b.id).id.cmp(&self.tab_ref(a.id).id))
        });
        let active = self
            .desktop
            .active
            .map_or(0, |reference| self.handle(reference.daemon, reference.id));
        self.revision += 1;
        CommandReply {
            daemon: self.primary,
            revision: self.revision,
            result,
            session: Session {
                tabs,
                active,
                filter: self.desktop.filter,
                width: self.desktop.width,
                height: self.desktop.height,
                maximized: self.desktop.maximized,
            },
            catalog: self.organization.catalog.clone(),
            desktop: None,
        }
    }
    /// Apply local definitions before remote snapshots and produce one view for the batch.
    fn project_execution_batch(
        &mut self,
        mut replies: Vec<execution::CommandReply>,
    ) -> anyhow::Result<CommandReply> {
        self.refresh_organization();
        replies.sort_by_key(|reply| (Some(reply.daemon) != self.primary, reply.revision));
        for reply in replies {
            if self.origins.contains_key(&reply.daemon) {
                self.ingest_execution(reply)?;
            }
        }
        Ok(self.combined(CommandOutput::Done { ok: true }.into()))
    }
    pub fn validate_state(&self, reply: &CommandReply) -> anyhow::Result<()> {
        anyhow::ensure!(
            reply.daemon == self.primary,
            "State belongs to a different desktop"
        );
        Ok(())
    }
    pub fn set_focused(&self, active: bool) -> anyhow::Result<()> {
        self.focused.set(active);
        for o in self.origins.values() {
            if let Some(m) = o.link.manager()
                && let Err(error) = m.set_focused(active)
                && Some(o.machine.daemon) == self.local
            {
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn set_frame_interval(&mut self, interval: Duration) {
        if self.frame_interval == interval {
            return;
        }
        self.frame_interval = interval;
        for origin in self.origins.values_mut() {
            if let Some(manager) = origin.link.manager_mut() {
                manager.set_frame_interval(interval);
            }
        }
    }
    pub fn frame_interval(&self) -> Duration {
        self.frame_interval
    }
    pub fn attach(&mut self, id: u64, theme: &Theme) -> anyhow::Result<SessionHandle> {
        let reference = self.pane_ref(id);
        let manager = self
            .origins
            .get_mut(&reference.daemon)
            .and_then(|o| o.link.manager_mut())
            .ok_or_else(|| anyhow::anyhow!("Connection is disconnected"))?;
        manager.set_clipboard_writes(true)?;
        manager.set_clipboard_reads(true)?;
        let handle = manager.attach(reference.id, theme)?;
        self.attached.insert(id);
        Ok(handle)
    }
    pub fn has_handle(&self, id: u64) -> bool {
        self.attached.contains(&id)
    }
    pub fn set_visible(&mut self, ids: HashSet<u64>) -> anyhow::Result<()> {
        let references: Vec<_> = ids.iter().map(|id| self.pane_ref(*id)).collect();
        for (&daemon, o) in &mut self.origins {
            if let Some(m) = o.link.manager_mut() {
                let result = m.set_visible(
                    references
                        .iter()
                        .filter(|r| r.daemon == daemon)
                        .map(|r| r.id)
                        .collect(),
                );
                if Some(daemon) == self.local {
                    result?;
                }
            }
        }
        Ok(())
    }
    pub fn is_visible(&self, id: u64) -> bool {
        let r = self.pane_ref(id);
        self.origins
            .get(&r.daemon)
            .and_then(|o| o.link.manager())
            .is_some_and(|m| m.is_visible(r.id))
    }
    pub fn set_git_panes(&mut self, ids: HashSet<u64>) -> anyhow::Result<()> {
        let references: Vec<_> = ids.iter().map(|id| self.pane_ref(*id)).collect();
        for (&daemon, origin) in &mut self.origins {
            if let Some(manager) = origin.link.manager_mut() {
                let result = manager.set_git_panes(
                    references
                        .iter()
                        .filter(|pane| pane.daemon == daemon)
                        .map(|pane| pane.id)
                        .collect(),
                );
                if Some(daemon) == self.local {
                    result?;
                }
            }
        }
        Ok(())
    }
    pub fn mark_read(&mut self, id: u64) {
        let r = self.pane_ref(id);
        if let Some(m) = self
            .origins
            .get_mut(&r.daemon)
            .and_then(|o| o.link.manager_mut())
        {
            m.mark_read(r.id);
        }
    }
    pub fn close(&mut self, id: u64) {
        let r = self.pane_ref(id);
        self.attached.remove(&id);
        self.handles.dirty = true;
        if let Some(m) = self
            .origins
            .get_mut(&r.daemon)
            .and_then(|o| o.link.manager_mut())
        {
            m.close(r.id);
        }
    }
    /// Call after reconciling views and completing outstanding command callbacks.
    /// Retain any additional handles still referenced by client presentation.
    pub fn prune_handles(&mut self, retained: impl IntoIterator<Item = u64>) {
        if !std::mem::take(&mut self.handles.dirty) {
            return;
        }
        let mut live = HashSet::new();
        for (&daemon, origin) in &self.origins {
            for tab in &origin.execution.tabs {
                live.insert((daemon, tab.id));
                live.extend(tab.layout.ids().into_iter().map(|id| (daemon, id)));
            }
        }
        let handles = &mut self.handles;
        let mut retain = |handle| {
            if let Some(&reference) = handles.references.get(&handle) {
                // Sweep again when a temporary presentation reference is released.
                handles.dirty |= live.insert(reference);
            }
        };
        for handle in retained.into_iter().chain(self.attached.iter().copied()) {
            retain(handle);
        }
        // A projected snapshot may still be waiting for the client to consume it.
        if let Some(snapshot) = &self.snapshot {
            for tab in &snapshot.session.tabs {
                for handle in std::iter::once(tab.id).chain(tab.layout.ids()) {
                    retain(handle);
                }
            }
            match snapshot.result.output {
                CommandOutput::New { tab, pane, .. } => {
                    retain(tab);
                    retain(pane);
                }
                CommandOutput::Split { pane } => retain(pane),
                _ => (),
            }
        }
        handles.ids.retain(|reference, _| live.contains(reference));
        handles
            .references
            .retain(|_, reference| live.contains(reference));
        // Reclaim a former peak without shrinking on every closed pane.
        if handles.ids.capacity() > 64 && handles.ids.len() < handles.ids.capacity() / 4 {
            handles.ids.shrink_to_fit();
            handles.references.shrink_to_fit();
        }
    }
    pub fn clear(&mut self) {
        for o in self.origins.values_mut() {
            self.status_changed |= o.link.cancel();
        }
        self.attached.clear();
        self.handles.dirty = true;
    }
    pub fn take_state(&mut self) -> Option<CommandReply> {
        self.snapshot.take()
    }
    pub fn take_status_changed(&mut self) -> bool {
        std::mem::take(&mut self.status_changed)
    }
    #[cfg(test)]
    pub fn poll(&mut self) -> HashMap<u64, SessionUpdate> {
        self.poll_with_clipboard(|_| {})
    }
    #[cfg(test)]
    pub fn poll_with_clipboard(
        &mut self,
        mut clipboard: impl FnMut(gmux_client::clipboard::Write),
    ) -> HashMap<u64, SessionUpdate> {
        self.poll_with_clipboard_events(|event, _| {
            if let gmux_client::clipboard::Event::Write(write) = event {
                clipboard(write);
            }
        })
    }
    pub fn poll_with_clipboard_events(
        &mut self,
        mut clipboard: impl FnMut(gmux_client::clipboard::Event, Option<String>),
    ) -> HashMap<u64, SessionUpdate> {
        if self.watcher.changed()
            && let Err(error) = self.refresh_definitions()
        {
            eprintln!("Could not refresh connections: {error}");
        }
        let mut states = Vec::new();
        let mut updates = HashMap::new();
        let mut lost = Vec::new();
        let now = Instant::now();
        for (&daemon, origin) in &mut self.origins {
            let was_connected = origin.link.status() == ConnectionStatus::Connected;
            let label = (Some(daemon) != self.local).then(|| origin.machine.name.clone());
            let update = origin.link.poll_at_with_clipboard_events(now, |event| {
                let event = match event {
                    gmux_client::clipboard::Event::Read { pane, event } => {
                        gmux_client::clipboard::Event::Read {
                            pane: self.handles.get(daemon, pane),
                            event,
                        }
                    }
                    other => other,
                };
                clipboard(event, label.clone());
            });
            if update.status_changed {
                self.status_changed = true;
                if let Some(manager) = origin.link.manager() {
                    if let Err(error) = manager.set_focused(self.focused.get()) {
                        eprintln!("Could not set connection focus: {error}");
                    }
                    origin.revision = 0;
                }
                if let Some(manager) = origin.link.manager_mut() {
                    manager.set_frame_interval(self.frame_interval);
                }
            }
            for (id, update) in update.panes {
                let handle = self.handles.get(daemon, id);
                updates.insert(handle, update);
            }
            if let Some(state) = update.state {
                states.push(state);
            }
            if was_connected && origin.link.status() != ConnectionStatus::Connected {
                lost.push(daemon);
            }
        }
        if !states.is_empty()
            || self.status_changed
            || self.client_revision != self.store.revision()
        {
            match self.project_execution_batch(states) {
                Ok(reply) => self.snapshot = Some(reply),
                Err(error) => eprintln!("{error}"),
            }
        }
        for daemon in lost {
            let Some(origin) = self.origins.get(&daemon) else {
                continue;
            };
            let name = origin.machine.name.clone();
            let panes: Vec<_> = origin
                .execution
                .tabs
                .iter()
                .flat_map(|tab| tab.layout.ids())
                .collect();
            for id in panes {
                let handle = self.handle(daemon, id);
                self.attached.remove(&handle);
                updates
                    .entry(handle)
                    .or_insert_with(SessionUpdate::default)
                    .error
                    .get_or_insert_with(|| format!("Connection to {name} lost"));
            }
        }
        updates
    }
    pub fn desktop(&mut self, session: &Session) -> DesktopState {
        let mut desktop = self.desktop.clone();
        let current_refs: HashSet<_> = session.tabs.iter().map(|t| self.tab_ref(t.id)).collect();
        let mut live_tabs: HashMap<DaemonId, HashSet<u64>> = HashMap::new();
        desktop.tabs.retain(|t| {
            !current_refs.contains(&t.reference)
                && self.origins.get(&t.reference.daemon).is_none_or(|origin| {
                    // Organization can arrive before execution. Only an ID already
                    // allocated in a snapshot can be confirmed closed by its absence.
                    origin.link.manager().is_none()
                        || t.reference.id > origin.execution.last_id
                        || live_tabs
                            .entry(t.reference.daemon)
                            .or_insert_with(|| {
                                origin.execution.tabs.iter().map(|tab| tab.id).collect()
                            })
                            .contains(&t.reference.id)
                })
        });
        let mut current: Vec<_> = session
            .tabs
            .iter()
            .map(|tab| self.desktop_tab(tab, tab.workspace))
            .collect();
        current.extend(desktop.tabs);
        desktop.tabs = current;
        desktop.active = (session.active != 0).then(|| self.tab_ref(session.active));
        desktop.filter = session.filter;
        desktop.width = session.width;
        desktop.height = session.height;
        desktop.maximized = session.maximized;
        self.desktop = desktop.clone();
        desktop
    }
    pub fn desktop_tab(&self, tab: &TabState, workspace: Option<u64>) -> DesktopTab {
        let reference = self.tab_ref(tab.id);
        let mut layout = tab.layout.clone();
        layout_ids(&mut layout, &mut |id| self.pane_ref(id).id);
        DesktopTab {
            reference,
            workspace,
            layout,
            active: self.pane_ref(tab.active),
            name: tab.name.clone(),
        }
    }
    pub fn raw_layout(&self, layout: &Layout) -> Layout {
        let mut layout = layout.clone();
        layout_ids(&mut layout, &mut |id| self.pane_ref(id).id);
        layout
    }
    pub fn route_body(&self, mut value: Value) -> anyhow::Result<(Option<DaemonId>, Value)> {
        if matches!(
            value["method"].as_str(),
            Some("connection-edit" | "connection-remove")
        ) {
            let target = value["connection"]
                .as_str()
                .and_then(|name| {
                    self.origins.values().find(|origin| {
                        Some(origin.machine.daemon) != self.local && origin.machine.name == name
                    })
                })
                .map(|origin| origin.machine.daemon);
            if let Some(daemon) = target {
                let expected: Option<DaemonId> = serde_json::from_value(value["daemon"].clone())?;
                anyhow::ensure!(
                    expected.is_none_or(|expected| expected == daemon),
                    "Remote connection identity changed"
                );
                value["daemon"] = json!(daemon);
            }
            // Editing credentials can wait on the remote. Removal stays in the
            // configuration queue so it can cancel a stalled edit for that daemon.
            return Ok((
                target.filter(|_| value["method"] == "connection-edit"),
                value,
            ));
        }
        if gmux_client::connections::handles(value["method"].as_str().unwrap_or(""))
            && value["method"] != "_connection-call"
            || super::backend::organization_request(&value)
        {
            return Ok((None, value));
        }
        if value["method"] == "_connection-call" {
            let name = value["connection"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing connection name"))?;
            let daemon = self.connection_id(name)?;
            let expected: Option<DaemonId> = serde_json::from_value(value["daemon"].clone())?;
            anyhow::ensure!(
                expected.is_none_or(|expected| expected == daemon),
                "Remote connection identity changed"
            );
            value["daemon"] = json!(daemon);
            return Ok((Some(daemon), value));
        }
        let mut daemon = None;
        let mut translate = |pane: &mut Value| -> anyhow::Result<()> {
            let id = pane
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Invalid pane ID"))?;
            let r = self.checked_ref(id)?;
            anyhow::ensure!(
                daemon.is_none_or(|daemon| daemon == r.daemon),
                "Panes belong to different connections"
            );
            daemon = Some(r.daemon);
            *pane = json!(r.id);
            Ok(())
        };
        if let Some(pane) = value.get_mut("pane").filter(|pane| !pane.is_null()) {
            translate(pane)?;
        }
        if let Some(panes) = value.get_mut("panes") {
            let panes = panes
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("Invalid pane list"))?;
            for pane in panes {
                translate(pane)?;
            }
        }
        let daemon = daemon
            .or(self.primary)
            .ok_or_else(|| anyhow::anyhow!("No connection is selected"))?;
        Ok((Some(daemon), self.wrap(daemon, value)?))
    }
    /// Resolve socket IDs and implicit window targets before dispatch. A selector
    /// constrains the target even when it is also the window's default connection.
    pub fn incoming_command(
        &self,
        command: &mut Command,
        connection: Option<&str>,
        active_pane: Option<u64>,
        active_tab: Option<u64>,
    ) -> anyhow::Result<Option<DaemonId>> {
        let selected = connection
            .map(|name| self.connection_id(name))
            .transpose()?;
        let mut destination = selected.or(self.primary);
        let target = |id: Option<u64>, active: Option<u64>, kind: &str| {
            let handle = match id {
                Some(id) => self.external_handle(connection, id)?,
                None => active.ok_or_else(|| anyhow::anyhow!("No active {kind}"))?,
            };
            let reference = self.checked_ref(handle)?;
            anyhow::ensure!(
                id.is_some() || selected.is_none_or(|daemon| daemon == reference.daemon),
                "Specify a {kind} on the selected connection"
            );
            Ok::<_, anyhow::Error>((handle, reference.daemon))
        };
        match command {
            Command::Tab(TabCommand::Split { pane, .. })
            | Command::Tab(TabCommand::Close { pane }) => {
                let (handle, daemon) = target(*pane, active_pane, "pane")?;
                *pane = Some(handle);
                destination = Some(daemon);
            }
            Command::Move { tab, .. } => {
                let (handle, daemon) = target(*tab, active_tab, "tab")?;
                *tab = Some(handle);
                destination = Some(daemon);
            }
            Command::Tab(TabCommand::Rename { tab: id, .. })
            | Command::Tab(TabCommand::CloseTab { tab: id })
            | Command::Tab(TabCommand::ResetName { tab: id }) => {
                let (handle, daemon) = target(Some(*id), None, "tab")?;
                *id = handle;
                destination = Some(daemon);
            }
            Command::WorkspaceAdd { .. }
            | Command::WorkspaceEdit { .. }
            | Command::WorkspaceRemove { .. } => {
                anyhow::ensure!(
                    connection.is_none(),
                    "Workspace commands control the GTK app; omit connection"
                );
                if let Command::WorkspaceAdd { tab: Some(id), .. } = command {
                    *id = self.default_handle(*id)?;
                }
            }
            Command::New { .. } => (),
        }
        Ok(destination)
    }
    pub fn external_result(
        &self,
        mut result: CommandResult,
        mut daemon: Option<DaemonId>,
    ) -> anyhow::Result<Value> {
        let pane = match &mut result.output {
            CommandOutput::New { tab, pane, .. } => {
                *tab = self.tab_ref(*tab).id;
                Some(pane)
            }
            CommandOutput::Split { pane } => Some(pane),
            CommandOutput::Workspace { .. } | CommandOutput::Done { .. } => None,
        };
        if let Some(pane) = pane {
            let reference = self.pane_ref(*pane);
            *pane = reference.id;
            daemon = Some(reference.daemon);
        }
        let mut value = serde_json::to_value(result)?;
        value["daemon"] = json!(daemon);
        Ok(value)
    }
    fn wrap(&self, daemon: DaemonId, value: Value) -> anyhow::Result<Value> {
        if super::backend::organization_request(&value) {
            return Ok(value);
        }
        let origin = self
            .origins
            .get(&daemon)
            .ok_or_else(|| anyhow::anyhow!("Connection no longer exists"))?;
        Ok(if Some(daemon) == self.local {
            value
        } else {
            json!({"method":"_connection-call","connection":origin.machine.name,"daemon":daemon,"request":value})
        })
    }
    pub fn route_command(
        &self,
        mut command: Command,
        destination: Option<DaemonId>,
    ) -> anyhow::Result<Route> {
        let mut daemon = destination;
        command.resolve_directory(daemon.is_some() && daemon == self.local)?;
        let mut assign = None;
        let creates_pane = matches!(
            command,
            Command::New { .. } | Command::Tab(TabCommand::Split { .. })
        );
        match &mut command {
            Command::New {
                workspace,
                directory,
                ..
            } => {
                let workspace = workspace.flatten();
                *directory = self.store.starting_directory(
                    workspace,
                    daemon.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Enable the Local Daemon or add a connection in Manage Connections"
                        )
                    })?,
                    directory.take(),
                )?;
                assign = Some(Assignment::CreatedTab { workspace });
            }
            Command::Tab(TabCommand::Split { pane: Some(id), .. })
            | Command::Tab(TabCommand::Close { pane: Some(id) }) => {
                let r = self.checked_ref(*id)?;
                daemon = Some(r.daemon);
                *id = r.id;
            }
            Command::Tab(TabCommand::CloseTab { tab })
            | Command::Tab(TabCommand::Rename { tab, .. })
            | Command::Tab(TabCommand::ResetName { tab }) => {
                let r = self.checked_ref(*tab)?;
                daemon = Some(r.daemon);
                *tab = r.id;
            }
            Command::WorkspaceAdd { tab, .. } => {
                assign = tab.take().map(|tab| Assignment::CreatedWorkspace { tab });
                daemon = self.primary;
            }
            Command::WorkspaceEdit { .. }
            | Command::WorkspaceRemove { .. }
            | Command::Move { .. } => daemon = self.primary,
            _ => (),
        }
        let body = match command.into_execution() {
            Ok(command) => serde_json::to_value(execution::InternalRequest::Command { command })?,
            Err(command) => serde_json::to_value(InternalRequest::Command { command })?,
        };
        let body = if super::backend::organization_request(&body) {
            body
        } else {
            self.wrap(
                daemon.ok_or_else(|| anyhow::anyhow!("No connection is selected"))?,
                body,
            )?
        };
        if creates_pane {
            anyhow::ensure!(
                daemon.is_some_and(|id| self.connected(id)),
                "Connection is disconnected or connecting. Wait for reconnection or check Manage Connections before creating a tab or pane"
            );
        }
        Ok(Route {
            destination: daemon,
            body,
            assign,
        })
    }
}

#[cfg(test)]
mod tests;
