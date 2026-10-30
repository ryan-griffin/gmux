use super::*;
use gmux_protocol::{control, model::MachineKind};
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
    server: gmux_platform::unix::ipc::Server,
}
impl Fixture {
    fn new() -> Self {
        let local = gmux_platform::unix::identity::daemon_id().unwrap();
        let path = std::env::temp_dir().join(format!("gmux-backend-{local}"));
        std::fs::create_dir_all(&path).unwrap();
        let control = path.join("control.sock");
        let endpoint = ipc::Endpoint::unix(control.clone(), path.join("stream.sock"));
        let server = gmux_platform::unix::ipc::Server::start_at(
            control,
            gmux_platform::unix::ipc::Wake::new().unwrap(),
        )
        .unwrap();
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
            gmux_platform::unix::ipc::Wake::new().unwrap(),
            self.store.clone(),
            self.local,
        )
    }
    fn incoming(&self) -> gmux_platform::unix::ipc::Request {
        self.server
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
    }
    fn answer(&self, request: gmux_platform::unix::ipc::Request, daemon: DaemonId) {
        let reply = control::CommandReply {
            daemon,
            home_directory: "/home/test".into(),
            machine_kind: MachineKind::Server,
            hostname: "test-host".into(),
            revision: 1,
            result: control::CommandOutput::Done { ok: true }.into(),
            execution: Default::default(),
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
    json!({"method":"_command", "command":{"method":"rename", "tab":1, "name":name}})
}
fn collect(backend: &Backend, count: usize) -> Vec<(u64, anyhow::Result<BackendReply>)> {
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
fn workers_decode_snapshots_by_request_and_preserve_ordinary_response_fields() {
    let f = Fixture::new();
    let backend = f.backend();
    backend
        .request(f.local, json!({"method":"status"}))
        .unwrap();
    f.incoming()
        .reply
        .send(json!({"execution":"diagnostic"}))
        .unwrap();
    let replies = collect(&backend, 1);
    assert!(
        matches!(&replies[0].1, Ok(BackendReply::Execution(control::ExecutionReply::Status(value))) if value["execution"] == "diagnostic")
    );

    backend
        .request(f.local, json!({"method":"_state"}))
        .unwrap();
    f.incoming().reply.send(json!({"execution":{}})).unwrap();
    assert!(
        collect(&backend, 1)[0].1.is_err(),
        "Malformed snapshots must fail on the worker"
    );

    backend
        .request(f.local, json!({"method":"_state"}))
        .unwrap();
    f.answer(f.incoming(), f.local);
    let replies = collect(&backend, 1);
    assert!(
        matches!(&replies[0].1, Ok(BackendReply::Execution(control::ExecutionReply::Snapshot(reply))) if reply.daemon == f.local)
    );

    backend
        .request(f.local, json!({"method":"_busy","panes":[7]}))
        .unwrap();
    f.incoming().reply.send(json!({"busy":false})).unwrap();
    let replies = collect(&backend, 1);
    assert!(matches!(&replies[0].1,
        Ok(BackendReply::Execution(control::ExecutionReply::Busy(reply))) if !reply.busy));

    backend
        .request(f.local, json!({"method":"_busy","panes":[7]}))
        .unwrap();
    f.incoming()
        .reply
        .send(json!({"error":"Daemon is shutting down","busy":false}))
        .unwrap();
    assert_eq!(
        collect(&backend, 1)[0]
            .1
            .as_ref()
            .err()
            .unwrap()
            .to_string(),
        "Daemon is shutting down"
    );
}

#[test]
fn full_remote_capacity_leaves_room_for_local_daemon_and_configuration() {
    let f = Fixture::new();
    let backend = f.backend();
    // Invalid setup still allocates its independent lane without starting a handshake.
    let setup = backend
        .request(None, json!({"method":"connection-add"}))
        .unwrap();
    let replies = collect(&backend, 1);
    assert!(
        replies
            .iter()
            .any(|(id, result)| *id == setup && result.is_err())
    );
    backend.request(None, remote("configuration")).unwrap();
    f.answer(f.incoming(), f.local);
    assert!(
        collect(&backend, 1)
            .iter()
            .all(|(_, result)| result.is_ok())
    );

    let daemons: Vec<_> = std::iter::once(f.local)
        .chain(
            (0..gmux_client::connections::MAX_REMOTES)
                .map(|_| gmux_platform::unix::identity::daemon_id().unwrap()),
        )
        .collect();
    for &daemon in &daemons {
        let id = backend.request(daemon, remote("execution")).unwrap();
        f.answer(f.incoming(), daemon);
        let replies = collect(&backend, 1);
        assert!(
            replies
                .iter()
                .any(|(reply, result)| *reply == id && result.is_ok())
        );
    }
    let replacement = gmux_platform::unix::identity::daemon_id().unwrap();
    assert!(backend.request(replacement, remote("overflow")).is_err());
    let removed = daemons[1];
    backend.retain_connections(|daemon| daemon != removed);
    backend.request(replacement, remote("replacement")).unwrap();
    f.answer(f.incoming(), replacement);
    assert!(
        collect(&backend, 1)
            .iter()
            .all(|(_, result)| result.is_ok())
    );
    assert_eq!(backend.outstanding.get(), 0);
}

#[test]
fn stalled_remote_does_not_block_other_connections_or_organization_and_preserves_its_order() {
    let f = Fixture::new();
    let backend = f.backend();
    let a = gmux_platform::unix::identity::daemon_id().unwrap();
    let b = gmux_platform::unix::identity::daemon_id().unwrap();
    let first = backend.request(a, remote("a")).unwrap();
    let held = f.incoming();
    assert_eq!(held.body["command"]["name"], "a");
    let second = backend.request(a, remote("renamed-a")).unwrap();
    let local = backend
        .request(f.local, json!({"method":"_state"}))
        .unwrap();
    let local_request = f.incoming();
    assert_eq!(local_request.body["method"], "_state");
    f.answer(local_request, f.local);
    let other = backend.request(b, remote("b")).unwrap();
    let other_request = f.incoming();
    assert_eq!(other_request.body["command"]["name"], "b");
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
    assert_eq!(next.body["command"]["name"], "renamed-a");
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
    let a = gmux_platform::unix::identity::daemon_id().unwrap();
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
            .all(|(_, result)| result.as_ref().err().unwrap().to_string() == "Request canceled")
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
        replies[0].1.as_ref().err().unwrap().to_string(),
        "Request canceled"
    );
    assert!(backend.workers.borrow().is_empty());
    drop(held);
}
