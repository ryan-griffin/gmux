use super::*;
use gmux_client::{control as execution, model::MachineKind, remote::SessionManager};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, mpsc},
};

struct Fixture {
    hub: Connections,
    path: PathBuf,
    ids: Vec<DaemonId>,
}
impl Fixture {
    fn new(hosts: usize) -> Self {
        let ids: Vec<_> = (0..hosts).map(|_| DaemonId::generate().unwrap()).collect();
        let local = ids[0];
        let path = std::env::temp_dir().join(format!("gmux-connections-{local}"));
        let store = Store::at(path.join("state.json")).unwrap();
        let wake = ipc::Wake::new().unwrap();
        let mut origins: HashMap<_, _> = ids
            .iter()
            .enumerate()
            .map(|(index, &daemon)| {
                (
                    daemon,
                    Origin::new(
                        Machine {
                            daemon,
                            name: format!("host-{index}"),
                            kind: MachineKind::Server,
                        },
                        wake.clone(),
                    ),
                )
            })
            .collect();
        origins.get_mut(&local).unwrap().home_directory = Some("/home/host-0".into());
        let hub = Connections {
            primary: Some(local),
            local: Some(local),
            preferred: None,
            definitions: Vec::new(),
            watcher: gmux_client::connections::Watch::new(wake.clone()).unwrap(),
            _subscription: store.subscribe(wake.clone()),
            store: Arc::clone(&store),
            client_revision: 0,
            origins,
            handles: Handles::default(),
            revision: 0,
            desktop: DesktopState::default(),
            organization: store.snapshot().0,
            wake,
            snapshot: None,
            attached: HashSet::new(),
            focused: Cell::new(false),
            status_changed: false,
        };
        Self { hub, path, ids }
    }
    fn execution(&self, host: usize, tabs: usize, revision: u64) -> execution::CommandReply {
        let machine = &self.hub.origins[&self.ids[host]].machine;
        let last_id = tabs as u64 * 3;
        let tabs = (0..tabs)
            .map(|index| {
                let id = 1 + index as u64 * 3;
                gmux_client::model::ExecutionTab {
                    id,
                    created_at: id,
                    name: format!("tab-{index}"),
                    custom_name: false,
                    layout: crate::model::PaneTree::Split {
                        vertical: false,
                        first: Box::new(crate::model::PaneTree::Pane(id + 1)),
                        second: Box::new(crate::model::PaneTree::Pane(id + 2)),
                    },
                    directories: HashMap::from([
                        (id + 1, PathBuf::from("/home/test/project")),
                        (id + 2, PathBuf::from("/home/test/other")),
                    ]),
                }
            })
            .collect();
        execution::CommandReply {
            daemon: machine.daemon,
            home_directory: format!("/home/host-{host}").into(),
            machine_name: machine.name.clone(),
            machine_kind: machine.kind,
            revision,
            result: execution::CommandOutput::Done { ok: true }.into(),
            session: crate::model::ExecutionState {
                tabs,
                last_id,
                ..Default::default()
            },
            metadata: Vec::new(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn directory_labels_use_each_daemons_home_without_changing_paths_or_trusting_stale_metadata() {
    let mut f = Fixture::new(2);
    let mut local = f.execution(0, 1, 1);
    local.home_directory = "/home/desktop".into();
    f.hub.project_execution(local).unwrap();
    let mut remote = f.execution(1, 1, 2);
    remote.home_directory = "/root".into();
    remote.machine_kind = MachineKind::VirtualMachine;
    remote.session.tabs[0]
        .directories
        .insert(2, "/root/project".into());
    let reply = f.hub.project_execution(remote).unwrap();
    let local_pane = f.hub.lookup(f.ids[0], 2).unwrap();
    let remote_pane = f.hub.lookup(f.ids[1], 2).unwrap();
    let display = |hub: &Connections, id, path| {
        hub.display_directory(hub.pane_ref(id).daemon, Path::new(path))
    };
    assert_eq!(
        display(&f.hub, local_pane, "/home/desktop/project"),
        "~/project"
    );
    assert_eq!(display(&f.hub, remote_pane, "/root"), "~");
    assert_eq!(display(&f.hub, remote_pane, "/root/project"), "~/project");
    assert_eq!(
        display(&f.hub, remote_pane, "/root-other/project"),
        "/root-other/project"
    );
    assert_eq!(
        display(&f.hub, remote_pane, "/home/desktop/project"),
        "/home/desktop/project"
    );
    assert_eq!(
        display(&f.hub, local_pane, "/root/project"),
        "/root/project"
    );
    let tab = reply
        .session
        .tabs
        .iter()
        .find(|tab| f.hub.tab_ref(tab.id).daemon == f.ids[1])
        .unwrap();
    assert_eq!(tab.directories[&remote_pane], Path::new("/root/project"));

    let mut stale = f.execution(1, 1, 1);
    stale.home_directory = "/home/desktop".into();
    stale.machine_kind = MachineKind::Desktop;
    f.hub.project_execution(stale).unwrap();
    assert_eq!(display(&f.hub, remote_pane, "/root/project"), "~/project");
    assert_eq!(f.hub.kind(f.ids[1]), MachineKind::VirtualMachine);
    // Rows can still emit metadata updates while their removed host is reconciled.
    f.hub.origins.remove(&f.ids[1]);
    assert_eq!(
        f.hub
            .display_directory(f.ids[1], Path::new("/root/project")),
        "/root/project"
    );
}

#[test]
fn connection_names_remain_routable_independently_of_live_status() {
    let mut f = Fixture::new(2);
    let remote = f.ids[1];
    let reply = f.execution(1, 1, 1);
    f.hub.project_execution(reply).unwrap();
    let tab = f.hub.lookup(remote, 1).unwrap();
    let name = f.hub.connection_name(tab);
    f.hub.connect(remote);
    assert_eq!(f.hub.connection_name(tab), name);
    f.hub.origins.get_mut(&remote).unwrap().link.cancel();
    f.hub.poll();
    assert_eq!(f.hub.connection_name(tab), name);
    assert_eq!(f.hub.connection_id(&name).unwrap(), remote);
}

#[test]
fn control_and_stream_snapshots_allow_immediate_attachment_without_accepting_stale_metadata() {
    use gmux_client::transport::{Header, Metadata, Packet};
    use std::{
        io::Write,
        time::{Duration, Instant},
    };

    let mut f = Fixture::new(2);
    for host in 0..2 {
        let daemon = f.ids[host];
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let manager =
            SessionManager::from_stream(ipc::Endpoint::local(), f.hub.wake.clone(), daemon, socket)
                .unwrap();
        f.hub.origins.get_mut(&daemon).unwrap().link =
            Connection::from_session(manager, f.hub.wake.clone());
        let with_metadata = |mut reply: execution::CommandReply, title: &str| {
            reply.metadata = reply
                .session
                .tabs
                .iter()
                .flat_map(|tab| tab.layout.ids())
                .map(|id| Metadata {
                    id,
                    title: title.into(),
                    directory: "/home/test/project".into(),
                    size: (80, 24),
                    ..Default::default()
                })
                .collect();
            reply
        };
        f.hub
            .project_execution(with_metadata(f.execution(host, 1, 2), "control"))
            .unwrap();
        let pane = f.hub.lookup(daemon, 2).unwrap();
        let handle = f.hub.attach(pane, &Theme::adwaita(true)).unwrap();
        assert_eq!(handle.borrow().title(), "control");
        f.hub
            .project_execution(with_metadata(f.execution(host, 1, 1), "stale"))
            .unwrap();
        assert_eq!(handle.borrow().title(), "control");

        let packet = Packet {
            header: Header {
                state: Some(with_metadata(f.execution(host, 2, 3), "stream")),
                ..Default::default()
            },
            cells: Vec::new(),
        };
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        peer.write_all(&bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while f.hub.origins[&daemon].revision < 3 {
            f.hub.poll();
            assert!(Instant::now() < deadline, "Stream snapshot did not arrive");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(handle.borrow().title(), "stream");
        let new_pane = f.hub.lookup(daemon, 5).unwrap();
        let new_handle = f.hub.attach(new_pane, &Theme::adwaita(true)).unwrap();
        assert_eq!(new_handle.borrow().title(), "stream");
    }
}

#[test]
fn stream_loss_updates_local_and_remote_status_without_discarding_cached_execution() {
    use std::time::{Duration, Instant};
    let mut f = Fixture::new(2);
    let mut peers = Vec::new();
    for host in 0..2 {
        let daemon = f.ids[host];
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
        let (socket, peer) = UnixStream::pair().unwrap();
        let endpoint = ipc::Endpoint::unix(f.path.join("control"), f.path.join("stream"));
        let manager =
            SessionManager::from_stream(endpoint, f.hub.wake.clone(), daemon, socket).unwrap();
        f.hub.origins.get_mut(&daemon).unwrap().link =
            Connection::from_session(manager, f.hub.wake.clone());
        peers.push(peer);
    }
    for host in [1, 0] {
        let daemon = f.ids[host];
        peers[host].shutdown(std::net::Shutdown::Both).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let updates = f.hub.poll();
            if !f.hub.connected(daemon) {
                assert!(f.hub.take_status_changed());
                assert!(matches!(
                    f.hub.connection_status(daemon),
                    ConnectionStatus::Disconnected { error: Some(_) }
                ));
                assert_eq!(f.hub.origins[&daemon].state.tabs.len(), 1);
                if host != 0 {
                    let pane = f.hub.lookup(daemon, 2).unwrap();
                    assert!(updates[&pane].error.is_some());
                    assert!(!updates[&pane].exited);
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Closed stream remained connected"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn abandoned_connection_setup_releases_sockets_in_both_phases_and_retries_cleanly() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    for waiting_for_state in [false, true] {
        let mut f = Fixture::new(2);
        let daemon = f.ids[1];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = ipc::Endpoint::tcp(
            listener.local_addr().unwrap().to_string(),
            gmux_client::network::Credentials::default(),
            Some(daemon),
        );
        let (started, ready) = mpsc::channel();
        let (finished, closed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let hello = json!({"version":gmux_client::transport::VERSION,"daemon":daemon,"hostname":"test","home_directory":"/home/test","machine_kind":"server"});
            for retry in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                BufReader::with_capacity(1, &stream)
                    .read_line(&mut line)
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["channel"],
                    "stream"
                );
                let mut control = if waiting_for_state {
                    writeln!(stream, "{hello}").unwrap();
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    line.clear();
                    BufReader::with_capacity(1, &socket)
                        .read_line(&mut line)
                        .unwrap();
                    writeln!(socket, "{hello}").unwrap();
                    line.clear();
                    BufReader::with_capacity(1, &socket)
                        .read_line(&mut line)
                        .unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&line).unwrap()["method"],
                        "state"
                    );
                    Some(socket)
                } else {
                    None
                };
                started.send(retry).unwrap();
                if let Some(socket) = &mut control {
                    assert_eq!(socket.read(&mut [0]).unwrap(), 0);
                }
                assert_eq!(stream.read(&mut [0]).unwrap(), 0);
                finished.send(retry).unwrap();
            }
        });
        for retry in [false, true] {
            f.hub.connect_endpoint(daemon, endpoint.clone());
            assert_eq!(ready.recv_timeout(Duration::from_secs(2)).unwrap(), retry);
            let start = Instant::now();
            if retry {
                f.hub.origins.remove(&daemon);
            } else {
                f.hub.clear();
            }
            assert_eq!(closed.recv_timeout(Duration::from_secs(1)).unwrap(), retry);
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        server.join().unwrap();
    }
}

#[test]
fn pruning_preserves_disconnected_execution_and_references_until_views_release_them() {
    let mut f = Fixture::new(2);
    for host in 0..2 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let local = f.hub.lookup(f.ids[0], 1).unwrap();
    let pane = f.hub.lookup(f.ids[0], 2).unwrap();
    let remote = f.hub.lookup(f.ids[1], 1).unwrap();
    assert_ne!(local, remote);
    f.hub.attached.insert(pane);
    f.hub.project_execution(f.execution(0, 0, 2)).unwrap();
    f.hub.prune_handles([local]);
    assert_eq!(f.hub.tab_ref(local).daemon, f.ids[0]);
    assert_eq!(f.hub.pane_ref(pane).daemon, f.ids[0]);
    assert_eq!(f.hub.tab_ref(remote).daemon, f.ids[1]);
    f.hub.close(pane);
    f.hub.prune_handles([]);
    assert_eq!(f.hub.lookup(f.ids[0], 1), None);
    assert_eq!(f.hub.lookup(f.ids[0], 2), None);
    assert_eq!(f.hub.lookup(f.ids[1], 1), Some(remote));
    assert_eq!(f.hub.handles.ids.len(), 3);
    f.hub.origins.remove(&f.ids[1]);
    f.hub.handles.dirty = true;
    f.hub.prune_handles([]);
    assert!(f.hub.handles.ids.is_empty());
    assert!(f.hub.handles.references.is_empty());
    assert!(
        f.hub
            .route_command(Command::CloseTab { tab: local }, Some(f.ids[0]))
            .is_err()
    );
    assert!(
        f.hub
            .route_body(json!({"method":"_read", "pane":pane}))
            .is_err()
    );
}

#[test]
fn routing_rejects_removed_connections_while_their_views_still_hold_handles() {
    let mut f = Fixture::new(2);
    f.hub.project_execution(f.execution(1, 1, 1)).unwrap();
    let remote = f.ids[1];
    let tab = f.hub.lookup(remote, 1).unwrap();
    let pane = f.hub.lookup(remote, 2).unwrap();
    f.hub.origins.remove(&remote);
    // Presentation and pending callbacks can keep these handles past removal.
    for command in [
        Command::CloseTab { tab },
        Command::New {
            directory_base: None,
            directory: None,
            name: None,
            workspace: None,
        },
    ] {
        assert_eq!(
            f.hub
                .route_command(command, Some(remote))
                .err()
                .unwrap()
                .to_string(),
            "Connection no longer exists"
        );
    }
    assert_eq!(
        f.hub
            .route_body(json!({"method":"_read", "pane":pane}))
            .unwrap_err()
            .to_string(),
        "Connection no longer exists"
    );
}

#[test]
fn pane_routing_requires_one_connection_and_valid_qualified_ids() {
    let mut f = Fixture::new(2);
    for host in 0..2 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let local = f.hub.lookup(f.ids[0], 2).unwrap();
    let remote = f.hub.lookup(f.ids[1], 2).unwrap();
    let other = f.hub.lookup(f.ids[1], 3).unwrap();
    assert_eq!(
        f.hub
            .route_body(json!({"method":"_busy", "panes":[remote, other]}))
            .unwrap(),
        (
            Some(f.ids[1]),
            json!({"method":"_connection-call", "connection":"host-1", "daemon":f.ids[1],
                   "request":{"method":"_busy", "panes":[2, 3]}})
        )
    );
    assert_eq!(
        f.hub
            .route_body(json!({"method":"_busy", "panes":[]}))
            .unwrap(),
        (Some(f.ids[0]), json!({"method":"_busy", "panes":[]}))
    );
    for request in [
        json!({"method":"_busy", "panes":[local, remote]}),
        json!({"method":"_busy", "pane":local, "panes":[remote]}),
    ] {
        assert_eq!(
            f.hub.route_body(request).unwrap_err().to_string(),
            "Panes belong to different connections"
        );
    }
    for request in [
        json!({"method":"_read", "pane":"invalid"}),
        json!({"method":"_busy", "panes":[remote, "invalid"]}),
        json!({"method":"_busy", "panes":[null]}),
        json!({"method":"_busy", "panes":null}),
        json!({"method":"_busy", "panes":42}),
    ] {
        assert!(f.hub.route_body(request).is_err());
    }
}

#[test]
fn connection_edits_use_the_daemons_queue_and_removal_can_cancel_them() {
    let f = Fixture::new(3);
    for host in 1..3 {
        let edit = json!({"method":"connection-edit","connection":format!("host-{host}"),"address":"127.0.0.1:1"});
        let (destination, body) = f.hub.route_body(edit.clone()).unwrap();
        assert_eq!(destination, Some(f.ids[host]));
        assert_eq!(body["daemon"], json!(f.ids[host]));

        let mut stale = edit;
        stale["daemon"] = json!(f.ids[0]);
        assert_eq!(
            f.hub.route_body(stale).unwrap_err().to_string(),
            "Remote connection identity changed"
        );

        let (destination, body) = f
            .hub
            .route_body(json!({"method":"connection-remove","connection":format!("host-{host}")}))
            .unwrap();
        assert_eq!(destination, None);
        assert_eq!(body["daemon"], json!(f.ids[host]));
    }
    assert_eq!(
        f.hub
            .route_body(json!({"method":"connection-edit","connection":"local","name":"desktop"}))
            .unwrap()
            .0,
        None
    );
}

#[test]
fn prepared_remote_commands_keep_their_identity_when_names_are_reassigned() {
    let mut f = Fixture::new(3);
    for host in 0..3 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let original = f.ids[1];
    let tab = f.hub.lookup(original, 1).unwrap();
    let route = f
        .hub
        .route_command(Command::CloseTab { tab }, Some(original))
        .unwrap();
    assert_eq!(
        f.hub.route_body(route.body.clone()).unwrap().0,
        Some(original)
    );
    f.hub.origins.get_mut(&original).unwrap().machine.name = "renamed".into();
    f.hub.origins.get_mut(&f.ids[2]).unwrap().machine.name = "host-1".into();
    assert_eq!(
        f.hub.route_body(route.body).unwrap_err().to_string(),
        "Remote connection identity changed"
    );
}

#[test]
fn closed_results_remain_translatable_and_handle_storage_is_bounded_under_churn() {
    let mut f = Fixture::new(1);
    f.hub.project_execution(f.execution(0, 256, 1)).unwrap();
    assert!(f.hub.handles.ids.capacity() > 64);
    f.hub.project_execution(f.execution(0, 0, 2)).unwrap();
    f.hub.prune_handles([]);
    assert!(f.hub.handles.ids.is_empty());
    assert!(f.hub.handles.ids.capacity() <= 64);
    let mut previous = f.hub.handles.next;
    for id in 1000..2000 {
        let mut raw = f.execution(0, 0, id);
        raw.result = execution::CommandOutput::New {
            tab: id * 2,
            pane: id * 2 + 1,
        }
        .into();
        // A newer shared snapshot can already have removed the just-created tab.
        let projected = f.hub.project_execution(raw).unwrap();
        let CommandOutput::New { tab, pane, .. } = projected.result.output else {
            panic!("New result expected")
        };
        assert!(tab > previous);
        previous = pane;
        f.hub.snapshot = Some(projected);
        f.hub.prune_handles([]);
        let mut result = json!({"tab":tab, "pane":pane});
        f.hub.external_result(&mut result);
        assert_eq!(
            result,
            json!({"tab":id * 2, "pane":id * 2 + 1, "daemon":f.ids[0]})
        );
        f.hub.take_state();
        f.hub.prune_handles([]);
        assert!(f.hub.handles.ids.is_empty());
        assert!(f.hub.handles.references.is_empty());
    }
    assert!(f.hub.handles.ids.capacity() <= 64);
    assert!(f.hub.handles.references.capacity() <= 64);
}

#[test]
fn execution_projection_uses_current_organization_without_reusing_ids_across_hosts() {
    let mut f = Fixture::new(2);
    let CommandOutput::Workspace { workspace } = f
        .hub
        .store
        .command(
            f.ids[0],
            Command::WorkspaceAdd {
                name: "Project".into(),
                directory: None,
                directories: Default::default(),
                tab: None,
            },
        )
        .unwrap()
    else {
        panic!("Workspace expected")
    };
    let before = f.hub.store.snapshot().0;
    assert!(Arc::ptr_eq(&before, &f.hub.store.snapshot().0));
    f.hub
        .store
        .assign(DesktopTab {
            reference: TabRef {
                daemon: f.ids[1],
                id: 1,
            },
            workspace: Some(workspace),
            layout: Layout::Split {
                vertical: false,
                first: Box::new(Layout::Pane(2)),
                second: Box::new(Layout::Pane(3)),
                ratio: 0.7,
            },
            active: PaneRef {
                daemon: f.ids[1],
                id: 3,
            },
            name: "saved title".into(),
        })
        .unwrap();
    assert!(
        before.desktop.tabs.is_empty(),
        "Published snapshots must remain immutable after edits"
    );
    f.hub.project_execution(f.execution(0, 1, 2)).unwrap();
    let reply = f.hub.project_execution(f.execution(1, 1, 2)).unwrap();
    assert_eq!(reply.session.tabs.len(), 2);
    let remote = reply
        .session
        .tabs
        .iter()
        .find(|tab| f.hub.tab_ref(tab.id).daemon == f.ids[1])
        .unwrap();
    assert_eq!(
        f.hub.tab_ref(remote.id),
        TabRef {
            daemon: f.ids[1],
            id: 1
        }
    );
    assert_eq!(remote.workspace, Some(workspace));
    assert_eq!(remote.name, "saved title");
    assert_eq!(
        f.hub.pane_ref(remote.active),
        PaneRef {
            daemon: f.ids[1],
            id: 3
        }
    );
    assert!(matches!(remote.layout, Layout::Split { ratio: 0.7, .. }));
    assert!(remote.directories.contains_key(&remote.active));
    assert_eq!(reply.catalog.workspaces[0].name, "Project");
    // A late execution snapshot cannot erase current topology or organization.
    let stale = f.hub.project_execution(f.execution(1, 0, 1)).unwrap();
    assert_eq!(stale.session.tabs.len(), 2);
    assert_eq!(
        stale
            .session
            .tabs
            .iter()
            .find(|tab| f.hub.tab_ref(tab.id).daemon == f.ids[1])
            .unwrap()
            .workspace,
        Some(workspace)
    );
}

#[test]
fn saves_preserve_assignments_ahead_of_execution_and_prune_only_confirmed_closed_tabs() {
    for host in 0..2 {
        let mut f = Fixture::new(2);
        for origin in 0..2 {
            f.hub
                .project(BackendReply::Execution {
                    reply: f.execution(origin, 1, 1),
                })
                .unwrap();
        }
        let daemon = f.ids[host];
        let (socket, _peer) = UnixStream::pair().unwrap();
        let manager =
            SessionManager::from_stream(ipc::Endpoint::local(), f.hub.wake.clone(), daemon, socket)
                .unwrap();
        f.hub.origins.get_mut(&daemon).unwrap().link =
            Connection::from_session(manager, f.hub.wake.clone());
        let CommandOutput::Workspace { workspace } = f
            .hub
            .store
            .command(
                f.ids[0],
                Command::WorkspaceAdd {
                    name: "Project".into(),
                    directory: None,
                    directories: Default::default(),
                    tab: None,
                },
            )
            .unwrap()
        else {
            panic!("Workspace expected")
        };
        let reference = TabRef { daemon, id: 4 };
        let assignment = |store: &Store| {
            store
                .snapshot()
                .0
                .desktop
                .tabs
                .iter()
                .find(|t| t.reference == reference)
                .map(|t| t.workspace)
        };
        f.hub
            .store
            .assign(DesktopTab {
                reference,
                workspace: Some(workspace),
                layout: Layout::Pane(5),
                active: PaneRef { daemon, id: 5 },
                name: "New tab in another window".into(),
            })
            .unwrap();
        // The store wakes this window before its stream delivers the new tab.
        f.hub.poll();
        let delayed = f.hub.take_state().unwrap();
        assert!(
            delayed
                .session
                .tabs
                .iter()
                .all(|t| f.hub.tab_ref(t.id) != reference)
        );
        let desktop = f.hub.desktop(&delayed.session);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        assert_eq!(assignment(&f.hub.store), Some(Some(workspace)));

        // Even a stale reply with a higher ID cannot confirm that the tab closed.
        let mut stale = f.execution(host, 0, 0);
        stale.session.last_id = 100;
        let delayed = f.hub.project_execution(stale).unwrap();
        let desktop = f.hub.desktop(&delayed.session);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        assert_eq!(assignment(&f.hub.store), Some(Some(workspace)));

        let caught_up = f.hub.project_execution(f.execution(host, 2, 2)).unwrap();
        let tab = caught_up
            .session
            .tabs
            .iter()
            .find(|t| f.hub.tab_ref(t.id) == reference)
            .unwrap();
        assert_eq!(tab.workspace, Some(workspace));
        // A tab awaiting its widgets is still live in the daemon snapshot.
        let mut presentation = caught_up.session;
        presentation
            .tabs
            .retain(|t| f.hub.tab_ref(t.id) != reference);
        let desktop = f.hub.desktop(&presentation);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        assert_eq!(assignment(&f.hub.store), Some(Some(workspace)));

        let mut closed = f.execution(host, 1, 3);
        closed.session.last_id = 6;
        let closed = f.hub.project_execution(closed).unwrap();
        let desktop = f.hub.desktop(&closed.session);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        let reopened = Store::at(f.path.join("state.json")).unwrap();
        assert_eq!(assignment(&reopened), None);
    }
}

#[test]
fn creation_times_merge_remote_imports_and_external_tabs_without_saved_order_overrides() {
    let mut f = Fixture::new(3);
    let mut local = f.execution(0, 3, 1);
    for (tab, created_at) in local.session.tabs.iter_mut().zip([100, 300, 500]) {
        tab.created_at = created_at;
    }
    let reply = f.hub.project_execution(local).unwrap();
    let desktop = f.hub.desktop(&reply.session);
    f.hub
        .store
        .save_desktop(desktop, f.hub.desktop_revision())
        .unwrap();

    let mut remote = f.execution(1, 2, 1);
    for (tab, created_at) in remote.session.tabs.iter_mut().zip([200, 400]) {
        tab.created_at = created_at;
    }
    f.hub.project_execution(remote).unwrap();
    let mut other = f.execution(2, 1, 1);
    other.session.tabs[0].created_at = 350;
    let reply = f.hub.project_execution(other).unwrap();
    let reference = |host, id| TabRef {
        daemon: f.ids[host],
        id,
    };
    let expected = vec![
        reference(0, 7),
        reference(1, 4),
        reference(2, 1),
        reference(0, 4),
        reference(1, 1),
        reference(0, 1),
    ];
    let references = |hub: &Connections, reply: &CommandReply| {
        reply
            .session
            .tabs
            .iter()
            .map(|tab| hub.tab_ref(tab.id))
            .collect::<Vec<_>>()
    };
    assert_eq!(references(&f.hub, &reply), expected);

    // Consecutive discoveries on different hosts must precede saved tabs even
    // before the receiving window has saved either discovery.
    let mut remote = f.execution(1, 3, 2);
    for (tab, created_at) in remote.session.tabs.iter_mut().zip([200, 400, 600]) {
        tab.created_at = created_at;
    }
    let reconnect = remote.session.clone();
    let reply = f.hub.project_execution(remote).unwrap();
    assert_eq!(references(&f.hub, &reply)[0], reference(1, 7));
    let mut other = f.execution(2, 2, 2);
    for (tab, created_at) in other.session.tabs.iter_mut().zip([350, 700]) {
        tab.created_at = created_at;
    }
    let reply = f.hub.project_execution(other).unwrap();
    let mut expected = expected;
    expected.splice(0..0, [reference(2, 4), reference(1, 7)]);
    assert_eq!(references(&f.hub, &reply), expected);

    // A delayed presentation save and a reconnect cannot replace event ordering.
    let mut desktop = f.hub.desktop(&reply.session);
    desktop.tabs.reverse();
    f.hub
        .store
        .save_desktop(desktop, f.hub.desktop_revision())
        .unwrap();
    f.hub.origins.get_mut(&f.ids[1]).unwrap().state = Session::default();
    let mut remote = f.execution(1, 3, 3);
    remote.session = reconnect;
    let reply = f.hub.project_execution(remote).unwrap();
    assert_eq!(references(&f.hub, &reply), expected);

    // Another receiver's subset retains the same relative order for shared tabs.
    f.hub.origins.remove(&f.ids[2]);
    let reply = f.hub.combined(CommandOutput::Done { ok: true }.into());
    expected.retain(|tab| tab.daemon != f.ids[2]);
    assert_eq!(references(&f.hub, &reply), expected);
}

#[test]
fn snapshot_bursts_advance_the_view_once_and_removed_connections_cannot_reappear() {
    let mut f = Fixture::new(3);
    let before = f.hub.revision;
    let inputs = vec![
        f.execution(2, 1, 1),
        f.execution(1, 1, 1),
        f.execution(0, 1, 1),
    ];
    let reply = f.hub.project_execution_batch(inputs).unwrap();
    assert_eq!(reply.revision, before + 1);
    assert_eq!(reply.session.tabs.len(), 3);
    let refs: HashSet<_> = reply
        .session
        .tabs
        .iter()
        .map(|tab| f.hub.tab_ref(tab.id))
        .collect();
    assert_eq!(
        refs,
        f.ids
            .iter()
            .map(|&daemon| TabRef { daemon, id: 1 })
            .collect()
    );
    let local = f.execution(0, 1, 2);
    let removed = f.execution(2, 2, 3);
    f.hub.origins.remove(&f.ids[2]);
    // Put the removed host first to catch ordering dependence in a poll batch.
    let inputs = vec![removed, local, f.execution(1, 2, 2), f.execution(1, 0, 1)];
    let reply = f.hub.project_execution_batch(inputs).unwrap();
    assert_eq!(reply.revision, before + 2);
    assert_eq!(reply.session.tabs.len(), 3);
    assert!(
        reply
            .session
            .tabs
            .iter()
            .all(|tab| f.hub.tab_ref(tab.id).daemon != f.ids[2])
    );
    assert_eq!(f.hub.origins[&f.ids[1]].revision, 2);
    assert_eq!(f.hub.origins[&f.ids[1]].state.tabs.len(), 2);
    f.hub.prune_handles([]);
    assert_eq!(f.hub.lookup(f.ids[2], 1), None);
    let before = f.hub.revision;
    f.hub
        .store
        .command(
            f.ids[0],
            Command::WorkspaceAdd {
                name: "Fresh organization".into(),
                directory: None,
                directories: Default::default(),
                tab: None,
            },
        )
        .unwrap();
    f.hub.poll();
    let reply = f.hub.take_state().unwrap();
    assert_eq!(reply.revision, before + 1);
    assert_eq!(reply.catalog.workspaces[0].name, "Fresh organization");
    assert_eq!(reply.session.tabs.len(), 3);
}
