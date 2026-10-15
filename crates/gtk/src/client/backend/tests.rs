use super::*;
use gmux_client::{control, model::MachineKind};
use serde_json::json;
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Fixture {
    path: PathBuf,
    local: DaemonId,
    store: Arc<Store>,
    endpoint: ipc::Endpoint,
    server: ipc::Server,
}
impl Fixture {
    fn new() -> Self {
        let local = DaemonId::generate().unwrap();
        let path = std::env::temp_dir().join(format!("gmux-backend-{local}"));
        std::fs::create_dir_all(&path).unwrap();
        let control = path.join("control.sock");
        let endpoint = ipc::Endpoint::unix(control.clone(), path.join("stream.sock"));
        let server = ipc::Server::start_at(control, ipc::Wake::new().unwrap()).unwrap();
        let store = Store::at(path.join("state.json")).unwrap();
        Self {
            path,
            local,
            store,
            endpoint,
            server,
        }
    }
    fn backend(&self) -> Backend {
        Backend::new(
            self.endpoint.clone(),
            ipc::Wake::new().unwrap(),
            self.store.clone(),
            self.local,
        )
    }
    fn incoming(&self) -> ipc::Request {
        self.server
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    }
    fn answer(&self, request: ipc::Request, daemon: DaemonId) {
        let reply = control::CommandReply {
            daemon,
            home_directory: "/home/test".into(),
            machine_kind: MachineKind::Server,
            machine_name: "test-host".into(),
            revision: 1,
            result: control::CommandOutput::Done { ok: true }.into(),
            session: Default::default(),
            metadata: Vec::new(),
        };
        request
            .reply
            .send(serde_json::to_value(reply).unwrap())
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn remote(name: &str) -> Value {
    json!({"method":"_state", "test_destination":name})
}
fn collect(backend: &Backend, count: usize) -> Vec<(u64, anyhow::Result<Value>)> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut replies = Vec::new();
    while replies.iter().filter(|(id, _)| *id != 0).count() < count {
        replies.extend(backend.replies());
        assert!(
            Instant::now() < deadline,
            "Backend replies did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    replies
}

#[test]
fn stalled_remote_does_not_block_other_connections_or_organization_and_preserves_its_order() {
    let f = Fixture::new();
    let backend = f.backend();
    let a = DaemonId::generate().unwrap();
    let b = DaemonId::generate().unwrap();
    let first = backend.request(a, remote("a")).unwrap();
    let held = f.incoming();
    assert_eq!(held.body["test_destination"], "a");
    let second = backend.request(a, remote("renamed-a")).unwrap();
    let local = backend
        .request(f.local, json!({"method":"_state"}))
        .unwrap();
    let local_request = f.incoming();
    assert_eq!(local_request.body["method"], "_state");
    f.answer(local_request, f.local);
    let other = backend.request(b, remote("b")).unwrap();
    let other_request = f.incoming();
    assert_eq!(other_request.body["test_destination"], "b");
    f.answer(other_request, b);
    let workspace = backend
        .request(
            f.local,
            json!({"method":"_command", "command":{"method":"workspace-add", "name":"Project"}}),
        )
        .unwrap();
    backend
        .save_desktop(
            DesktopState {
                width: 1234,
                ..Default::default()
            },
            f.store.revision(),
        )
        .unwrap();
    let replies = collect(&backend, 3);
    let completed: HashSet<_> = replies
        .iter()
        .filter(|(_, result)| result.is_ok())
        .map(|(id, _)| *id)
        .collect();
    assert!(completed.is_superset(&HashSet::from([local, other, workspace])));
    assert!(!completed.contains(&first) && !completed.contains(&second));
    let deadline = Instant::now() + Duration::from_secs(2);
    while f.store.snapshot().0.desktop.width != 1234 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    f.answer(held, a);
    let next = f.incoming();
    assert_eq!(next.body["test_destination"], "renamed-a");
    f.answer(next, a);
    let replies = collect(&backend, 2);
    assert_eq!(
        replies
            .iter()
            .filter(|(id, _)| *id != 0)
            .map(|(id, result)| {
                result.as_ref().unwrap();
                *id
            })
            .collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(backend.outstanding.get(), 0);
}

#[test]
fn removing_a_connection_cancels_its_requests_and_shutdown_flushes_the_latest_save() {
    let f = Fixture::new();
    let backend = f.backend();
    let a = DaemonId::generate().unwrap();
    let first = backend.request(a, remote("a")).unwrap();
    let held = f.incoming();
    let second = backend.request(a, remote("a")).unwrap();
    let before = Instant::now();
    backend.retain_connections(|daemon| daemon == f.local);
    assert!(
        before.elapsed() < Duration::from_secs(1),
        "Removal waited for a network timeout"
    );
    let replies = collect(&backend, 2);
    assert_eq!(
        replies.iter().map(|(id, _)| *id).collect::<HashSet<_>>(),
        HashSet::from([first, second])
    );
    assert!(
        replies
            .iter()
            .all(|(_, result)| result.as_ref().unwrap_err().to_string() == "Request canceled")
    );
    assert!(backend.workers.borrow().is_empty());
    assert!(
        f.server.receiver.try_recv().is_err(),
        "A queued remote request was sent after cancellation"
    );
    drop(held);
    // A new queue can use the same daemon after removal without reusing request IDs.
    let third = backend.request(a, remote("a")).unwrap();
    assert!(third > second);
    let held = f.incoming();
    backend.request(a, remote("a")).unwrap();
    backend.request(f.local, json!({"method":"_command", "command":{"method":"workspace-add", "name":"Before close"}})).unwrap();
    backend.request(f.local, json!({"method":"_command", "command":{"method":"workspace-add", "name":"Queued edit"}})).unwrap();
    backend
        .save_desktop(
            DesktopState {
                width: 1300,
                ..Default::default()
            },
            f.store.revision(),
        )
        .unwrap();
    backend
        .save_desktop(
            DesktopState {
                width: 1400,
                ..Default::default()
            },
            f.store.revision(),
        )
        .unwrap();
    let before = Instant::now();
    backend.shutdown();
    assert!(
        backend
            .request(f.local, json!({"method":"_state"}))
            .is_err()
    );
    drop(backend);
    assert!(
        before.elapsed() < Duration::from_secs(1),
        "Closing waited for a network timeout"
    );
    let reopened = Store::at(f.path.join("state.json")).unwrap();
    assert_eq!(reopened.snapshot().0.desktop.width, 1400);
    assert_eq!(
        reopened
            .snapshot()
            .0
            .catalog
            .workspaces
            .iter()
            .map(|workspace| workspace.name.as_str())
            .collect::<Vec<_>>(),
        ["Before close", "Queued edit"]
    );
    assert!(f.server.receiver.try_recv().is_err());
    drop(held);
}

#[test]
fn removing_the_default_remote_cancels_its_execution_queue() {
    let f = Fixture::new();
    let backend = f.backend();
    let request = backend.request(f.local, remote("default")).unwrap();
    let held = f.incoming();
    let before = Instant::now();
    backend.retain_connections(|_| false);
    let replies = collect(&backend, 1);
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(replies[0].0, request);
    assert_eq!(
        replies[0].1.as_ref().unwrap_err().to_string(),
        "Request canceled"
    );
    assert!(backend.workers.borrow().is_empty());
    drop(held);
}
