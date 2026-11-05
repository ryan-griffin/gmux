use super::super::control::InternalRequest;
use super::*;
use crate::model::Layout;
use gmux_client::remote::SessionManager;
use gmux_protocol::{control as execution, model::MachineKind};
use serde_json::{Value, json};
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
        let ids: Vec<_> = (0..hosts)
            .map(|_| gmux_platform::unix::identity::daemon_id().unwrap())
            .collect();
        let local = ids[0];
        let path = std::env::temp_dir().join(format!("gmux-connections-{local}"));
        let store = Store::at(path.join("state.json")).unwrap();
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
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
                            machine_kind: MachineKind::Server,
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
            local_enabled: true,
            preferred: None,
            definitions: Vec::new(),
            watcher: gmux_client::unix::Watch::new(wake.clone()).unwrap(),
            _subscription: store.subscribe(wake.clone()),
            store: Arc::clone(&store),
            client_revision: 0,
            origins,
            poll_cursor: None,
            revision: 0,
            desktop: DesktopState::default(),
            organization: store.snapshot().0,
            wake,
            snapshot: None,
            attached: HashSet::new(),
            focused: Cell::new(false),
            frame_interval: gmux_protocol::transport::DEFAULT_FRAME_INTERVAL,
            status_changed: false,
        };
        Self { hub, path, ids }
    }
    fn execution(&self, host: usize, tabs: usize, revision: u64) -> execution::CommandReply {
        let machine = &self.hub.origins[&self.ids[host]].machine;
        let tabs = (0..tabs)
            .map(|index| {
                let id = 1 + index as u64 * 3;
                gmux_protocol::model::ExecutionTab {
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
            hostname: machine.name.clone(),
            machine_kind: machine.machine_kind,
            revision,
            result: execution::CommandOutput::Done { ok: true }.into(),
            execution: crate::model::ExecutionSnapshot { tabs },
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
    remote.execution.tabs[0]
        .directories
        .insert(2, "/root/project".into());
    let reply = f.hub.project_execution(remote).unwrap();
    let local_pane = PaneRef {
        daemon: f.ids[0],
        id: 2,
    };
    let remote_pane = PaneRef {
        daemon: f.ids[1],
        id: 2,
    };
    let display =
        |hub: &Connections, id: PaneRef, path| hub.display_directory(id.daemon, Path::new(path));
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
        .find(|tab| tab.id.daemon == f.ids[1])
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
    let tab = TabRef {
        daemon: remote,
        id: 1,
    };
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
    use gmux_protocol::transport::{Header, Metadata, Packet};
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
                .execution
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
        let pane = PaneRef { daemon, id: 2 };
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
        let new_pane = PaneRef { daemon, id: 5 };
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
                assert_eq!(f.hub.origins[&daemon].execution.tabs.len(), 1);
                if host != 0 {
                    let pane = PaneRef { daemon, id: 2 };
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
fn rejected_stream_state_preserves_cached_views_and_the_disconnect_reason() {
    use gmux_protocol::transport::{Header, Metadata, Packet};
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    let mut f = Fixture::new(2);
    let daemon = f.ids[1];
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let manager =
        SessionManager::from_stream(ipc::Endpoint::local(), f.hub.wake.clone(), daemon, socket)
            .unwrap();
    let link = &mut f.hub.origins.get_mut(&daemon).unwrap().link;
    *link = Connection::from_session(manager, f.hub.wake.clone());
    link.set_auto_reconnect(true);
    let mut snapshot = f.execution(1, 1, 1);
    snapshot.metadata.push(Metadata {
        id: 2,
        title: "retained".into(),
        size: (80, 24),
        ..Default::default()
    });
    f.hub.project_execution(snapshot).unwrap();
    let pane = PaneRef { daemon, id: 2 };
    let view = f.hub.attach(pane, &Theme::adwaita(true)).unwrap();
    let packet = Packet {
        header: Header {
            state: Some(f.execution(0, 1, 2)),
            ..Default::default()
        },
        cells: vec![],
    };
    let mut bytes = Vec::new();
    packet.encode_into(&mut bytes).unwrap();
    peer.write_all(&bytes).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let updates = f.hub.poll();
        if !f.hub.connected(daemon) {
            assert!(
                updates[&pane]
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("different daemon")
            );
            assert!(matches!(
                f.hub.connection_status(daemon),
                ConnectionStatus::Disconnected { error: Some(_) }
            ));
            assert!(f.hub.next_retry().is_none());
            assert!(!f.hub.is_attached(pane));
            assert_eq!(view.borrow().title(), "retained");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Invalid stream state remained connected"
        );
        std::thread::sleep(Duration::from_millis(1));
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
            let hello = json!({"version":gmux_protocol::transport::VERSION,"daemon":daemon,"hostname":"test","home_directory":"/home/test","machine_kind":"server"});
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
                    line.clear();
                    BufReader::with_capacity(1, &stream)
                        .read_line(&mut line)
                        .unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&line).unwrap()["method"],
                        "attach"
                    );
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
fn routing_rejects_removed_connections_while_views_retain_references() {
    let mut f = Fixture::new(2);
    f.hub.project_execution(f.execution(1, 1, 1)).unwrap();
    let remote = f.ids[1];
    let tab = TabRef {
        daemon: remote,
        id: 1,
    };
    let pane = PaneRef {
        daemon: remote,
        id: 2,
    };
    f.hub.origins.remove(&remote);
    // Presentation and pending callbacks can keep these references past removal.
    for command in [
        Command::Tab(TabCommand::CloseTab { tab }),
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
            .route_request(InternalRequest::Read { pane: Some(pane) })
            .err()
            .unwrap()
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
    let local = PaneRef {
        daemon: f.ids[0],
        id: 2,
    };
    let remote = PaneRef {
        daemon: f.ids[1],
        id: 2,
    };
    let other = PaneRef {
        daemon: f.ids[1],
        id: 3,
    };
    let route = f
        .hub
        .route_request(InternalRequest::Busy {
            panes: vec![remote, other],
        })
        .unwrap();
    assert!(matches!(route, RoutedRequest::Execution {
        target: Target::Remote { name, daemon },
        request: execution::Request::Internal(execution::InternalRequest::Busy { panes }),
    } if name == "host-1" && daemon == f.ids[1] && panes == [2, 3]));
    let route = f
        .hub
        .route_request(InternalRequest::Busy { panes: vec![] })
        .unwrap();
    assert!(matches!(route, RoutedRequest::Execution {
        target: Target::Local(daemon),
        request: execution::Request::Internal(execution::InternalRequest::Busy { panes }),
    } if daemon == f.ids[0] && panes.is_empty()));
    assert_eq!(
        f.hub
            .route_request(InternalRequest::Busy {
                panes: vec![local, remote]
            })
            .err()
            .unwrap()
            .to_string(),
        "Panes belong to different connections"
    );
    // Explicit execution requests must resolve their nested selectors, too.
    let route = f
        .hub
        .route_request(InternalRequest::Execution {
            request: execution::ExecutionRequest::Read { pane: remote },
        })
        .unwrap();
    assert!(matches!(route, RoutedRequest::Execution {
        target: Target::Remote { daemon, .. },
        request: execution::Request::Internal(execution::InternalRequest::Execution {
            request: execution::ExecutionRequest::Read { pane: 2 },
        }),
    } if daemon == f.ids[1]));
}

#[test]
fn connection_configuration_pins_the_selected_daemon() {
    use gmux_client::connections::ConfigurationRequest as C;
    let f = Fixture::new(3);
    for host in 1..3 {
        let edit = || C::ConnectionEdit {
            connection: format!("host-{host}"),
            daemon: None,
            name: None,
            address: Some("127.0.0.1:1".into()),
            authentication: None,
            remote_access: None,
        };
        let route = f.hub.route_request(edit()).unwrap();
        assert!(
            matches!(route, RoutedRequest::Configuration(C::ConnectionEdit {
            daemon: Some(id), ..
        }) if id == f.ids[host])
        );
        let mut stale = edit();
        if let C::ConnectionEdit { daemon, .. } = &mut stale {
            *daemon = Some(f.ids[0]);
        }
        assert_eq!(
            f.hub.route_request(stale).err().unwrap().to_string(),
            "Remote connection identity changed"
        );
        let route = f
            .hub
            .route_request(C::ConnectionRemove {
                connection: format!("host-{host}"),
                daemon: None,
            })
            .unwrap();
        assert!(
            matches!(route, RoutedRequest::Configuration(C::ConnectionRemove {
            daemon: Some(id), ..
        }) if id == f.ids[host])
        );
    }
    let route = f
        .hub
        .route_request(C::ConnectionEdit {
            connection: "local".into(),
            daemon: None,
            name: Some(Some("desktop".into())),
            address: None,
            authentication: None,
            remote_access: None,
        })
        .unwrap();
    assert!(matches!(
        route,
        RoutedRequest::Configuration(C::ConnectionEdit { daemon: None, .. })
    ));
}

#[test]
fn prepared_remote_commands_keep_their_identity_when_names_are_reassigned() {
    let mut f = Fixture::new(3);
    for host in 0..3 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let original = f.ids[1];
    let tab = TabRef {
        daemon: original,
        id: 1,
    };
    let route = f
        .hub
        .route_command(Command::Tab(TabCommand::CloseTab { tab }), Some(original))
        .unwrap();
    f.hub.origins.get_mut(&original).unwrap().machine.name = "renamed".into();
    f.hub.origins.get_mut(&f.ids[2]).unwrap().machine.name = "host-1".into();
    let RoutedRequest::Execution { target, .. } = route.request else {
        panic!("Expected execution");
    };
    let ipc::Endpoint::Saved { name, daemon } = target.into_endpoint(ipc::Endpoint::local()) else {
        panic!("Expected a saved endpoint");
    };
    assert_eq!(name, "host-1");
    assert_eq!(daemon, Some(original));
    assert_ne!(
        daemon,
        f.hub
            .origins
            .values()
            .find(|o| o.machine.name == name)
            .map(|o| o.machine.daemon)
    );
}

#[test]
fn organization_changes_reproject_cached_execution_without_changing_its_topology() {
    let mut f = Fixture::new(2);
    for host in 0..2 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let daemon = f.ids[1];
    let reply = f.hub.snapshot();
    let mut desktop = f.hub.desktop(&reply.session);
    let saved = desktop
        .tabs
        .iter_mut()
        .find(|t| t.reference.daemon == daemon)
        .unwrap();
    saved.name = "Updated presentation".into();
    saved.active.id = 3;
    let Layout::Split { ratio, .. } = &mut saved.layout else {
        panic!("Expected split")
    };
    *ratio = 0.8;
    f.hub
        .store
        .save_desktop(desktop, f.hub.desktop_revision())
        .unwrap();
    let view = f.hub.snapshot();
    let tab = view
        .session
        .tabs
        .iter()
        .find(|t| t.id.daemon == daemon)
        .unwrap();
    assert_eq!(tab.name, "Updated presentation");
    assert!(matches!(tab.layout, Layout::Split { ratio: 0.8, .. }));
    assert_eq!(tab.active.id, 3);
    assert_eq!(f.hub.origins[&daemon].revision, 1);

    let mut desktop = f.hub.store.snapshot().0.desktop.clone();
    let saved = desktop
        .tabs
        .iter_mut()
        .find(|t| t.reference.daemon == daemon)
        .unwrap();
    saved.layout = Layout::Pane(PaneRef { daemon, id: 99 });
    saved.active.id = 99;
    f.hub
        .store
        .save_desktop(desktop, f.hub.desktop_revision())
        .unwrap();
    let view = f.hub.snapshot();
    let tab = view
        .session
        .tabs
        .iter()
        .find(|t| t.id.daemon == daemon)
        .unwrap();
    assert_eq!(
        tab.layout
            .ids()
            .into_iter()
            .map(|id| id.id)
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert!(matches!(tab.layout, Layout::Split { ratio: 0.5, .. }));
    assert_eq!(tab.active.id, 2);

    let mut named = f.execution(1, 1, 2);
    named.execution.tabs[0].custom_name = true;
    named.execution.tabs[0].name = "Shared name".into();
    let view = f.hub.project_execution(named).unwrap();
    assert_eq!(
        view.session
            .tabs
            .iter()
            .find(|t| t.id.daemon == daemon)
            .unwrap()
            .name,
        "Shared name"
    );
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
                first: Box::new(Layout::Pane(PaneRef {
                    daemon: f.ids[1],
                    id: 2,
                })),
                second: Box::new(Layout::Pane(PaneRef {
                    daemon: f.ids[1],
                    id: 3,
                })),
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
        .find(|tab| tab.id.daemon == f.ids[1])
        .unwrap();
    assert_eq!(
        (remote.id),
        TabRef {
            daemon: f.ids[1],
            id: 1
        }
    );
    assert_eq!(remote.workspace, Some(workspace));
    assert_eq!(remote.name, "saved title");
    assert_eq!(
        (remote.active),
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
            .find(|tab| tab.id.daemon == f.ids[1])
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
                .project(BackendReply::Execution(
                    execution::ExecutionReply::Snapshot(Box::new(f.execution(origin, 1, 1))),
                ))
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
                layout: Layout::Pane(PaneRef { daemon, id: 5 }),
                active: PaneRef { daemon, id: 5 },
                name: "New tab in another window".into(),
            })
            .unwrap();
        // The store wakes this window before its stream delivers the new tab.
        f.hub.poll();
        let delayed = f.hub.take_state().unwrap();
        assert!(delayed.session.tabs.iter().all(|t| t.id != reference));
        let desktop = f.hub.desktop(&delayed.session);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        assert_eq!(assignment(&f.hub.store), Some(Some(workspace)));

        // Even a stale reply with higher topology IDs cannot confirm closure.
        let mut stale = f.execution(host, 0, 0);
        stale
            .execution
            .tabs
            .push(gmux_protocol::model::ExecutionTab {
                id: 100,
                created_at: 100,
                name: "Stale".into(),
                custom_name: false,
                layout: crate::model::PaneTree::Pane(101),
                directories: HashMap::new(),
            });
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
            .find(|t| t.id == reference)
            .unwrap();
        assert_eq!(tab.workspace, Some(workspace));
        // A tab awaiting its widgets is still live in the daemon snapshot.
        let mut presentation = caught_up.session;
        presentation.tabs.retain(|t| t.id != reference);
        let desktop = f.hub.desktop(&presentation);
        f.hub
            .store
            .save_desktop(desktop, f.hub.desktop_revision())
            .unwrap();
        assert_eq!(assignment(&f.hub.store), Some(Some(workspace)));

        let closed = f.execution(host, 1, 3);
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
    for (tab, created_at) in local.execution.tabs.iter_mut().zip([100, 300, 500]) {
        tab.created_at = created_at;
    }
    let reply = f.hub.project_execution(local).unwrap();
    let desktop = f.hub.desktop(&reply.session);
    f.hub
        .store
        .save_desktop(desktop, f.hub.desktop_revision())
        .unwrap();

    let mut remote = f.execution(1, 2, 1);
    for (tab, created_at) in remote.execution.tabs.iter_mut().zip([200, 400]) {
        tab.created_at = created_at;
    }
    f.hub.project_execution(remote).unwrap();
    let mut other = f.execution(2, 1, 1);
    other.execution.tabs[0].created_at = 350;
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
    let references = |_hub: &Connections, reply: &CommandReply| {
        reply
            .session
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(references(&f.hub, &reply), expected);

    // Consecutive discoveries on different hosts must precede saved tabs even
    // before the receiving window has saved either discovery.
    let mut remote = f.execution(1, 3, 2);
    for (tab, created_at) in remote.execution.tabs.iter_mut().zip([200, 400, 600]) {
        tab.created_at = created_at;
    }
    let reconnect = remote.execution.clone();
    let reply = f.hub.project_execution(remote).unwrap();
    assert_eq!(references(&f.hub, &reply)[0], reference(1, 7));
    let mut other = f.execution(2, 2, 2);
    for (tab, created_at) in other.execution.tabs.iter_mut().zip([350, 700]) {
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
    f.hub.origins.get_mut(&f.ids[1]).unwrap().execution = Default::default();
    let mut remote = f.execution(1, 3, 3);
    remote.execution = reconnect;
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
    let refs: HashSet<_> = reply.session.tabs.iter().map(|tab| tab.id).collect();
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
            .all(|tab| tab.id.daemon != f.ids[2])
    );
    assert_eq!(f.hub.origins[&f.ids[1]].revision, 2);
    assert_eq!(f.hub.origins[&f.ids[1]].execution.tabs.len(), 2);
    assert!(f.hub.external_tab(Some("host-2"), 1).is_err());
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

#[test]
fn closed_creation_results_keep_their_identity_without_live_topology() {
    let mut f = Fixture::new(2);
    for host in 0..2 {
        let daemon = f.ids[host];
        let mut raw = f.execution(host, 0, 1);
        raw.result = execution::CommandOutput::New { tab: 1, pane: 2 }.into();
        let projected = f.hub.project_execution(raw).unwrap();
        assert!(projected.session.tabs.is_empty());
        let CommandOutput::New { tab, pane, .. } = projected.result.output else {
            panic!("Expected creation")
        };
        assert_eq!(tab, TabRef { daemon, id: 1 });
        assert_eq!(pane, PaneRef { daemon, id: 2 });
        f.hub.origins.remove(&daemon);
        assert_eq!(
            projected.result.into_external_value(None),
            json!({"tab":1,"pane":2,"workspace":null,"daemon":daemon})
        );
    }
}

#[test]
fn socket_selectors_qualify_by_kind_and_connection_before_routing() {
    let mut f = Fixture::new(2);
    for host in 0..2 {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    let local = f.ids[0];
    let remote = f.ids[1];
    assert!(f.hub.external_pane(None, 1).is_err());
    assert!(f.hub.external_tab(None, 2).is_err());
    let local_pane = PaneRef {
        daemon: local,
        id: 2,
    };
    let remote_pane = PaneRef {
        daemon: remote,
        id: 2,
    };
    let raw = || Command::Tab(execution::TabCommand::Close { pane: Some(2) });
    let (command, destination) = f
        .hub
        .incoming_command(raw(), Some("host-1"), Some(local_pane), None)
        .unwrap();
    assert_eq!(destination, Some(remote));
    assert!(
        matches!(command, Command::Tab(TabCommand::Close { pane: Some(pane) }) if pane == remote_pane)
    );
    let Route {
        request: RoutedRequest::Execution { target, request },
        ..
    } = f.hub.route_command(command, destination).unwrap()
    else {
        panic!("Expected execution")
    };
    assert_eq!(target.daemon(), remote);
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"method":"_command","command":{"method":"close","pane":2}})
    );
    let implicit = Command::Tab(execution::TabCommand::Close { pane: None });
    assert!(
        f.hub
            .incoming_command(implicit, Some("host-1"), Some(local_pane), None)
            .is_err()
    );
    assert!(
        f.hub
            .route_request(InternalRequest::Busy {
                panes: vec![local_pane, remote_pane]
            })
            .is_err()
    );
}

#[test]
fn snapshot_order_and_window_history_do_not_change_qualified_identity() {
    let mut f = Fixture::new(2);
    let mut other = Fixture::new(1);
    other.hub.origins = f
        .ids
        .iter()
        .map(|&daemon| {
            (
                daemon,
                Origin::new(
                    f.hub.origins[&daemon].machine.clone(),
                    other.hub.wake.clone(),
                ),
            )
        })
        .collect();
    other.hub.primary = Some(f.ids[0]);
    other.hub.local = Some(f.ids[0]);
    for host in [0, 1] {
        f.hub.project_execution(f.execution(host, 1, 1)).unwrap();
    }
    for host in [1, 0] {
        other
            .hub
            .project_execution(f.execution(host, 1, 1))
            .unwrap();
    }
    let left = f.hub.snapshot().session;
    let right = other.hub.snapshot().session;
    for tab in left.tabs {
        let matching = right.tabs.iter().find(|other| other.id == tab.id).unwrap();
        assert_eq!(tab.layout, matching.layout);
        assert_eq!(tab.active, matching.active);
        assert_eq!(tab.directories, matching.directories);
    }
}

#[test]
#[ignore = "snapshot projection measurement; run alone with --nocapture"]
fn measure_snapshot_projection() {
    let before = unsafe { libc::mallinfo2() }.uordblks;
    let mut f = Fixture::new(2);
    for host in 0..2 {
        f.hub.project_execution(f.execution(host, 256, 1)).unwrap();
    }
    let retained = unsafe { libc::mallinfo2() }.uordblks.saturating_sub(before);
    for _ in 0..100 {
        std::hint::black_box(f.hub.snapshot());
    }
    let start = Instant::now();
    for _ in 0..5000 {
        std::hint::black_box(f.hub.snapshot());
    }
    eprintln!(
        "snapshot_projection tabs=512 panes=1024 iterations=5000 elapsed_us={} retained_heap_bytes={retained}",
        start.elapsed().as_micros()
    );
}
