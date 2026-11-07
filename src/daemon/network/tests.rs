//! TCP setup, handoff, admission, and drain regression checks.
use super::*;
use std::time::Duration;

fn dispatch() -> (Dispatch, mpsc::Receiver<Incoming>) {
    Dispatch::new(
        gmux_platform::unix::ipc::Wake::new().unwrap(),
        Default::default(),
    )
}
fn daemon() -> DaemonId {
    gmux_platform::unix::identity::daemon_id().unwrap()
}
fn connect(server: &Server, daemon: DaemonId, channel: Channel) -> TcpStream {
    gmux_client::network::connect(
        &server.address.to_string(),
        &Default::default(),
        channel,
        Some(daemon),
    )
    .unwrap()
    .0
}
#[test]
fn discarded_and_paused_listeners_release_inherited_descriptors() {
    let (dispatch, _incoming) = dispatch();
    for running in [false, true] {
        let prepared = Server::prepare("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let address = prepared.address;
        let inherited = prepared.listener.0.try_clone().unwrap();
        if running {
            prepared.start(dispatch.clone(), daemon()).pause();
        } else {
            drop(prepared);
        }
        TcpListener::bind(address).expect("Inherited listener retained port");
        drop(inherited);
    }
}
#[test]
fn setup_deadline_releases_trickling_unauthenticated_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    let connection = Arc::new(Connection {
        socket,
        cancelled: Arc::new(AtomicBool::new(false)),
        clients: Weak::new(),
        id: 0,
    });
    let server = std::thread::spawn(move || {
        serve(
            connection,
            daemon(),
            Some("secret"),
            Instant::now() + Duration::from_millis(100),
        )
        .err()
        .unwrap()
    });
    let writer = std::thread::spawn(move || {
        for _ in 0..100 {
            if peer.write_all(b" ").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    assert_eq!(
        server
            .join()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    writer.join().unwrap();
}
#[test]
fn handoff_preserves_pipelined_attach_binary_prefix_and_remote_attribution() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peer = TcpStream::connect(server.address).unwrap();
    // One write can be read ahead by either handshake or Attach BufReader.
    let prefix = b"{\"method\":\"clipboard-read-data\",\"pane\":1,\"id\":7,\"size\":6}\na\n\0{}x{\"method\":\"ping\"}\n";
    let mut bytes = serde_json::to_vec(&Handshake {
        version: transport::VERSION,
        channel: Channel::Stream,
        daemon: Some(id),
        token: None,
    })
    .unwrap();
    bytes.push(b'\n');
    bytes.extend(
        serde_json::to_vec(
            &json!({"method":"attach","version":transport::VERSION,"daemon":id,"remote":false}),
        )
        .unwrap(),
    );
    bytes.push(b'\n');
    bytes.extend(prefix);
    peer.write_all(&bytes).unwrap();
    let Incoming::Stream(stream) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("expected stream")
    };
    let mut owner = super::super::Daemon::empty_for_test();
    owner.id = id;
    owner.admit_remote(stream).unwrap();
    let frontend = owner.frontends.values_mut().next().unwrap();
    assert!(frontend.remote);
    let requests = frontend.requests().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        matches!(&requests[0].command, transport::StreamCommand::ClipboardReadData { bytes, .. } if bytes == b"a\n\0{}x")
    );
    assert!(matches!(
        requests[1].command,
        transport::StreamCommand::Ping
    ));
    server.set_token(Some("changed".into()));
    assert!(frontend.requests().is_err());
    drop(owner);
    assert!(server.clients.sockets.lock().unwrap().is_empty());
}
#[test]
fn control_pipeline_waits_for_completion_and_drains_after_half_close() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peer = connect(&server, id, Channel::Control);
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.write_all(b"{\"method\":\"state\"}\n{\"method\":\"state\"}\n")
        .unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    let mut owner = super::super::Daemon::empty_for_test();
    owner.id = id;
    let worker = crate::persistence::Worker::new(gmux_platform::unix::ipc::Wake::new().unwrap());
    for _ in 0..2 {
        let Incoming::Control(request) = incoming.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected control")
        };
        assert!(
            incoming.try_recv().is_err(),
            "A later command passed its pending predecessor"
        );
        assert!(super::super::PendingControl::start(request, &mut owner, &worker).is_none());
    }
    let mut output = String::new();
    peer.read_to_string(&mut output).unwrap();
    assert_eq!(output.lines().count(), 2);
    for line in output.lines() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(line).unwrap()["daemon"],
            json!(id)
        );
    }
}
#[test]
fn revocation_cancels_queued_handoffs_and_controls_without_retaining_slots() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peer = connect(&server, id, Channel::Control);
    peer.write_all(b"{\"method\":\"new\"}\n").unwrap();
    let Incoming::Control(request) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("expected control")
    };
    server.set_token(Some("new-credential".into()));
    let mut owner = super::super::Daemon::empty_for_test();
    let worker = crate::persistence::Worker::new(gmux_platform::unix::ipc::Wake::new().unwrap());
    assert!(super::super::PendingControl::start(request, &mut owner, &worker).is_none());
    assert!(owner.sessions.is_empty());
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    server.set_token(None);
    let mut peer = connect(&server, id, Channel::Stream);
    write_json(
        &mut peer,
        &json!({"method":"attach","version":transport::VERSION,"daemon":id}),
    )
    .unwrap();
    let Incoming::Stream(stream) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("stream")
    };
    let connection = Arc::downgrade(&stream.connection);
    server.set_token(Some("revoked-before-owner-handoff".into()));
    assert!(!owner.admit_remote(stream).unwrap());
    assert!(
        connection.upgrade().is_none(),
        "Cancelled handoff retained its descriptor and slot"
    );
}
#[test]
fn tcp_admission_and_shutdown_cover_idle_authenticated_controls() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peers = Vec::new();
    for _ in 0..32 {
        peers.push(connect(&server, id, Channel::Control));
    }
    let mut rejected = TcpStream::connect(server.address).unwrap();
    rejected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(rejected.read(&mut [0]).unwrap(), 0);
    assert_eq!(server.clients.sockets.lock().unwrap().len(), 32);
    drop(server);
    for mut peer in peers {
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    drop(incoming);
}
#[test]
fn partial_control_messages_and_nonreading_replies_have_absolute_deadlines() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    socket.set_nonblocking(true).unwrap();
    let connection = Arc::new(Connection {
        socket,
        cancelled: Arc::new(AtomicBool::new(false)),
        clients: Weak::new(),
        id: 0,
    });
    let (dispatch, _incoming) = dispatch();
    let mut control = Control::new(connection, b"{".to_vec());
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    control.service(&dispatch, &wake).unwrap();
    let deadline = control.deadline.unwrap();
    control.service(&dispatch, &wake).unwrap();
    assert_eq!(control.deadline, Some(deadline));
    control.deadline = Some(Instant::now());
    assert!(control.service(&dispatch, &wake).is_err());
    control.reply(json!("reply")).unwrap();
    control.deadline = Some(Instant::now());
    assert!(control.service(&dispatch, &wake).is_err());
}
#[test]
fn tcp_output_drains_exact_bytes_under_backpressure_after_half_close() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peer = connect(&server, id, Channel::Stream);
    write_json(
        &mut peer,
        &json!({"method":"attach","version":transport::VERSION,"daemon":id}),
    )
    .unwrap();
    let Incoming::Stream(stream) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("stream")
    };
    let send_buffer: libc::c_int = 1024;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                stream.connection.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&send_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&send_buffer) as libc::socklen_t,
            )
        },
        0
    );
    let mut owner = super::super::Daemon::empty_for_test();
    owner.id = id;
    owner.admit_remote(stream).unwrap();
    let frontend = owner.frontends.values_mut().next().unwrap();
    let payload: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    frontend.queue_encoded(&payload).unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    assert!(frontend.requests().unwrap().is_empty());
    assert!(frontend.drain_deadline.is_some());
    frontend.flush().unwrap();
    assert!(!frontend.output.is_empty());
    peer.set_nonblocking(true).unwrap();
    let mut received = Vec::new();
    let mut buffer = [0; 8192];
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < until, "output failed to drain");
        while let Ok(size) = peer.read(&mut buffer) {
            if size == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..size]);
        }
        if frontend.flush().is_err() {
            break;
        }
        std::thread::yield_now();
    }
    peer.set_nonblocking(false).unwrap();
    // Closing the owner releases the socket after its final successful write.
    owner.frontends.clear();
    peer.read_to_end(&mut received).unwrap();
    assert_eq!(received, payload);
}
#[test]
fn failed_handoff_and_aggregate_rejection_return_the_tcp_slot() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut owner = super::super::Daemon::empty_for_test();
    owner.id = id;
    let mut local_peers = Vec::new();
    for _ in 0..super::super::resources::MAX_FRONTENDS {
        let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        owner.admit_frontend(socket).unwrap();
        local_peers.push(peer);
    }
    let mut peer = connect(&server, id, Channel::Stream);
    write_json(
        &mut peer,
        &json!({"method":"attach","version":transport::VERSION,"daemon":id}),
    )
    .unwrap();
    let Incoming::Stream(stream) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("stream")
    };
    let connection = Arc::downgrade(&stream.connection);
    assert!(!owner.admit_remote(stream).unwrap());
    assert!(connection.upgrade().is_none());
    drop(incoming);
    let mut peer = connect(&server, id, Channel::Stream);
    write_json(
        &mut peer,
        &json!({"method":"attach","version":transport::VERSION,"daemon":id}),
    )
    .unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}
#[test]
fn direct_controls_share_local_admission_and_return_it_after_reply() {
    let id = daemon();
    let (dispatch, incoming) = dispatch();
    let admission = dispatch.admission.clone();
    let mut occupied: Vec<_> = (0..64).map(|_| admission.acquire().unwrap()).collect();
    let server = Server::prepare("127.0.0.1:0".parse().unwrap(), None)
        .unwrap()
        .start(dispatch, id);
    let mut peer = connect(&server, id, Channel::Control);
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.write_all(b"{\"method\":\"state\"}\n").unwrap();
    let mut reader = std::io::BufReader::new(peer.try_clone().unwrap());
    assert!(
        read_json(&mut reader, MAX_REPLY).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("admission")
    );
    assert!(incoming.try_recv().is_err());
    occupied.pop();
    peer.write_all(b"{\"method\":\"state\"}\n").unwrap();
    let Incoming::Control(request) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
        panic!("control")
    };
    assert!(admission.acquire().is_none());
    let mut owner = super::super::Daemon::empty_for_test();
    owner.id = id;
    let worker = crate::persistence::Worker::new(gmux_platform::unix::ipc::Wake::new().unwrap());
    super::super::PendingControl::start(request, &mut owner, &worker);
    assert_eq!(
        read_json(&mut reader, MAX_REPLY).unwrap()["daemon"],
        json!(id)
    );
    // A subsequent request can reuse the same returned shared slot.
    peer.write_all(b"{\"method\":\"state\"}\n").unwrap();
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(2)).unwrap(),
        Incoming::Control(_)
    ));
    drop(server);
    assert!(admission.acquire().is_some());
}
