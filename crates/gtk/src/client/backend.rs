//! Ordered organization work and independent execution queues outside the UI thread.
use super::{
    Store,
    model::{DaemonId, DesktopState},
};
use gmux_client::ipc;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{Arc, Mutex},
};

struct Worker {
    client: ipc::Client,
    cancellation: ipc::Cancellation,
    execution: bool,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Lane {
    Execution(DaemonId),
    AddConnection,
    Configuration,
}
impl Worker {
    fn new(
        endpoint: ipc::Endpoint,
        wake: ipc::Wake,
        store: Arc<Store>,
        local: Arc<Mutex<Option<DaemonId>>>,
        execution: bool,
    ) -> Self {
        let cancellation = ipc::Cancellation::default();
        let calls = cancellation.clone();
        let client = ipc::Client::with_handler(wake, move |body| {
            let local = *local.lock().unwrap();
            store.call_cancellable(&endpoint, local, body, &calls)
        });
        Self {
            client,
            cancellation,
            execution,
        }
    }
    fn stop(&self) {
        self.cancellation.cancel();
        if self.execution {
            self.client.cancel_requests();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        self.client.join();
    }
}

pub struct Backend {
    organization: Worker,
    workers: RefCell<HashMap<Lane, Worker>>,
    endpoint: ipc::Endpoint,
    local: Arc<Mutex<Option<DaemonId>>>,
    wake: ipc::Wake,
    store: Arc<Store>,
    next: Cell<u64>,
    outstanding: Cell<usize>,
    closed: Cell<bool>,
    ready: RefCell<Vec<(u64, anyhow::Result<Value>)>>,
}
impl Backend {
    pub fn new(
        endpoint: ipc::Endpoint,
        wake: ipc::Wake,
        store: Arc<Store>,
        local: impl Into<Option<DaemonId>>,
    ) -> Self {
        let local = Arc::new(Mutex::new(local.into()));
        Self {
            organization: Worker::new(
                endpoint.clone(),
                wake.clone(),
                store.clone(),
                local.clone(),
                false,
            ),
            workers: RefCell::new(HashMap::new()),
            endpoint,
            local,
            wake,
            store,
            next: Cell::new(1),
            outstanding: Cell::new(0),
            closed: Cell::new(false),
            ready: RefCell::new(Vec::new()),
        }
    }
    pub fn set_local(&self, local: Option<DaemonId>) {
        *self.local.lock().unwrap() = local;
    }
    pub fn request(
        &self,
        destination: impl Into<Option<DaemonId>>,
        body: Value,
    ) -> anyhow::Result<u64> {
        anyhow::ensure!(!self.closed.get(), "Request worker is closed");
        anyhow::ensure!(self.outstanding.get() < 128, "Request queue is full");
        let id = self.next.get();
        self.next.set(
            id.checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("No request IDs remain"))?,
        );
        if organization_request(&body) {
            self.organization.client.request_with_id(id, body)?;
        } else {
            let mut workers = self.workers.borrow_mut();
            // Adding a connection has no pinned daemon ID yet and can wait on a
            // network handshake. Keep it out of the local execution queue.
            let lane = if body["method"] == "connection-add" {
                Lane::AddConnection
            } else {
                destination
                    .into()
                    .map_or(Lane::Configuration, Lane::Execution)
            };
            if !workers.contains_key(&lane) {
                // One lane per remote, plus local execution, configuration, and connection setup.
                anyhow::ensure!(
                    workers.len() < gmux_client::connections::MAX_REMOTES + 3,
                    "Too many execution queues"
                );
                workers.insert(
                    lane,
                    Worker::new(
                        self.endpoint.clone(),
                        self.wake.clone(),
                        self.store.clone(),
                        self.local.clone(),
                        true,
                    ),
                );
            }
            workers[&lane].client.request_with_id(id, body)?;
        }
        self.outstanding.set(self.outstanding.get() + 1);
        Ok(id)
    }
    pub fn save_desktop(&self, desktop: DesktopState, revision: u64) -> anyhow::Result<()> {
        anyhow::ensure!(!self.closed.get(), "Request worker is closed");
        let store = self.store.clone();
        self.organization.client.update_with("desktop", move || {
            store.save_desktop(desktop, revision)?;
            Ok(serde_json::json!({"ok":true}))
        })
    }
    pub fn replies(&self) -> Vec<(u64, anyhow::Result<Value>)> {
        let mut replies = std::mem::take(&mut *self.ready.borrow_mut());
        replies.extend(self.organization.client.replies());
        for worker in self.workers.borrow().values() {
            replies.extend(worker.client.replies());
        }
        self.outstanding
            .set(self.outstanding.get() - replies.iter().filter(|(id, _)| *id != 0).count());
        replies
    }
    /// Release queues for removed connections, returning cancellations through normal replies.
    pub fn retain_connections(&self, retained: impl Fn(DaemonId) -> bool) {
        let removed: Vec<_> = self
            .workers
            .borrow_mut()
            .extract_if(|lane, _| matches!(lane, Lane::Execution(id) if !retained(*id)))
            .map(|(_, worker)| worker)
            .collect();
        for mut worker in removed {
            worker.stop();
            worker.client.join();
            self.ready.borrow_mut().extend(worker.client.replies());
        }
    }
    /// Stop execution requests and interrupt socket waits; organization edits and saves still flush.
    pub fn shutdown(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.organization.stop();
        for worker in self.workers.borrow().values() {
            worker.stop();
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub(super) fn organization_request(body: &Value) -> bool {
    matches!(
        body["method"].as_str(),
        Some("_desktop-save" | "_desktop-tab")
    ) || body["method"] == "_command"
        && matches!(
            body["command"]["method"].as_str(),
            Some("workspace-add" | "workspace-edit" | "workspace-remove")
        )
}

#[cfg(test)]
mod tests;
