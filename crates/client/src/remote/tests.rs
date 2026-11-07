use super::*;
use std::{io::BufReader, os::unix::net::UnixStream};

#[test]
fn clipboard_results_stream_as_one_reserved_operation_with_raw_chunks() {
    use gmux_protocol::clipboard::{ReadDescriptor, ReadResult, ReadStatus, Representation};
    use std::io::{BufRead, Read};
    let data = (0..=255).cycle().take(40_000).collect::<Vec<u8>>();
    let outgoing = Outgoing::ClipboardReply {
        pane: 7,
        result: ReadResult {
            id: 42,
            status: ReadStatus::Success,
            epoch: 2,
            available: vec![],
            contents: vec![ReadDescriptor {
                mime: "application/octet-stream".into(),
                len: data.len(),
            }],
        },
        contents: vec![Representation {
            mime: "application/octet-stream".into(),
            data: data.clone(),
        }],
    };
    assert!(outgoing.bytes() >= data.len());
    let mut encoded = Vec::new();
    outgoing
        .write(&mut encoded, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
        .unwrap();
    let mut reader = std::io::Cursor::new(encoded);
    let mut received = Vec::new();
    let mut methods = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let header: serde_json::Value = serde_json::from_str(&line).unwrap();
        methods.push(header["method"].as_str().unwrap().to_owned());
        if header["method"] == "clipboard-read-data" {
            let len = header["size"].as_u64().unwrap() as usize;
            assert!(len <= gmux_protocol::clipboard::READ_CHUNK);
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes).unwrap();
            received.extend(bytes);
        }
    }
    assert_eq!(methods.first().unwrap(), "clipboard-read-result");
    assert_eq!(methods.last().unwrap(), "clipboard-read-end");
    assert_eq!(received, data);
}

pub(super) fn layout(unshifted: u32) -> gmux_protocol::transport::KeyLayout {
    gmux_protocol::transport::KeyLayout {
        unshifted,
        ..Default::default()
    }
}

#[test]
fn heartbeat_acknowledgements_bypass_saturated_decode_admission() {
    let shared = Arc::new(tokio::sync::Semaphore::new(64 * 1024));
    let held = shared.clone().try_acquire_many_owned(64 * 1024).unwrap();
    let (sender, receiver) = incoming::channel_with_budget(64 * 1024, 8, shared.clone());
    // One prefix fits; the writer cannot finish three acknowledgements
    // unless the reader keeps consuming them with every byte reserved.
    let (socket, mut peer) = tokio::io::duplex(4);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let task = runtime::spawn(async move {
        read_packets(socket, &sender, &wake, Duration::from_secs(2)).await
    });
    runtime::block_on(async {
        tokio::time::timeout(Duration::from_secs(1), peer.write_all(&[0; 12]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shared.available_permits(), 0);
        // After releasing admission, a real packet follows the
        // acknowledgements without losing its replayed prefix or header.
        drop(held);
        let mut bytes = Vec::new();
        Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: gmux_protocol::transport::PaneHeader {
                notifications: vec!["after heartbeats".into()],
                ..Default::default()
            },
            cells: Vec::new(),
        })
        .encode_into(&mut bytes)
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), peer.write_all(&bytes))
            .await
            .unwrap()
            .unwrap();
    });
    drop(peer);
    assert!(runtime::block_on(task).unwrap().is_err());
    let packet = receiver.try_recv().unwrap().unwrap();
    assert_eq!(
        packet.as_pane().unwrap().header.notifications,
        ["after heartbeats"]
    );
    assert_eq!(shared.available_permits(), 64 * 1024);
}

#[test]
fn heartbeat_replies_keep_idle_streams_alive_without_waking_the_ui() {
    use std::{
        io::{BufRead, Write},
        os::fd::AsRawFd,
    };
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let mut manager = SessionManager::with_heartbeat(
        ipc::Endpoint::local(),
        wake.clone(),
        gmux_platform::unix::identity::daemon_id().unwrap(),
        socket.into(),
        Duration::from_millis(10),
        Duration::from_secs(1),
    )
    .unwrap();
    let cached = manager.connection.clone();
    let (ready, done) = mpsc::channel();
    let (release, hold) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        // Run longer than the read deadline. Other tests decode maximum
        // packets on the same process executor and share admission, so a
        // sub-second scheduling delay must not decide this keepalive test.
        for _ in 0..120 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.as_bytes(), PING);
            peer.write_all(transport::HEARTBEAT_REPLY).unwrap();
        }
        ready.send(()).unwrap();
        let _ = hold.recv();
    });
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(manager.poll().is_empty());
    assert!(!manager.disconnected());
    let mut event = libc::pollfd {
        fd: wake.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut event, 1, 0) }, 0);
    drop(manager);
    let deadline = Instant::now() + Duration::from_secs(2);
    // The old viewport can retain its connection without retaining a worker.
    loop {
        if cached.commands.is_closed() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Closing the manager retained its heartbeat worker"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    release.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn an_open_socket_without_daemon_replies_times_out() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let mut manager = SessionManager::with_heartbeat(
        ipc::Endpoint::local(),
        gmux_platform::unix::ipc::Wake::new().unwrap(),
        gmux_platform::unix::identity::daemon_id().unwrap(),
        socket.into(),
        Duration::from_millis(20),
        Duration::from_millis(200),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !manager.disconnected() {
        manager.poll();
        assert!(
            Instant::now() < deadline,
            "An unresponsive daemon stayed connected"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        manager
            .disconnect_error()
            .unwrap()
            .to_string()
            .contains("not responding")
    );
}

#[test]
fn heartbeat_probes_can_be_interleaved_with_paste_chunks() {
    let text = "a界".repeat(transport::PASTE_CHUNK);
    let mut bytes = Vec::new();
    let mut heartbeat = Heartbeat::new(Duration::ZERO);
    Outgoing::prepare(request(json!({"method":"paste","pane":7,"text":text})))
        .unwrap()
        .write(&mut bytes, &mut heartbeat)
        .unwrap();
    let commands: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert!(commands.iter().any(|command| command["method"] == "ping"));
    let paste: Vec<_> = commands
        .iter()
        .filter(|command| command["method"] != "ping")
        .collect();
    assert_eq!(paste.first().unwrap()["method"], "paste-begin");
    assert_eq!(paste.last().unwrap()["method"], "paste-end");
    let received: String = paste
        .iter()
        .filter_map(|command| command["text"].as_str())
        .collect();
    assert_eq!(received, text);
}

pub(super) fn request(value: Value) -> StreamRequest {
    serde_json::from_value(value).unwrap()
}

pub(super) fn wire(queued: Queued) -> Value {
    let Queued {
        outgoing,
        reservation: _reservation,
    } = queued;
    let mut bytes = Vec::new();
    outgoing
        .write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn outgoing_capacity_is_shared_and_failed_or_canceled_work_releases_it() {
    let make = || {
        let (socket, peer) = UnixStream::pair().unwrap();
        let (commands, queue) = tokio::sync::mpsc::channel(16);
        (
            Connection {
                commands,
                task: None,
                pending_bytes: Arc::new(AtomicUsize::new(0)),
                socket: socket.into(),
                clipboard_epochs: Default::default(),
            },
            queue,
            peer,
        )
    };
    let (a, mut aq, _ap) = make();
    let (b, bq, _bp) = make();
    let item = || Outgoing::Command(Vec::with_capacity(1024));
    let bytes = item().retained_bytes();
    let budget = Arc::new(tokio::sync::Semaphore::new(bytes * 2));
    a.enqueue_with_budget(item(), budget.clone()).unwrap();
    b.enqueue_with_budget(item(), budget.clone()).unwrap();
    assert!(a.enqueue_with_budget(item(), budget.clone()).is_err());
    assert_eq!(budget.available_permits(), 0);
    let in_flight = aq.try_recv().unwrap();
    assert_eq!(budget.available_permits(), 0);
    drop(in_flight);
    a.enqueue_with_budget(item(), budget.clone()).unwrap();
    drop((aq, bq));
    assert_eq!(budget.available_permits(), bytes * 2);
    assert!(a.enqueue_with_budget(item(), budget.clone()).is_err());
    assert_eq!(budget.available_permits(), bytes * 2);
    assert!(!Arc::ptr_eq(
        &outgoing_budget(true),
        &outgoing_budget(false)
    ));
}

#[test]
fn bulk_input_leaves_queue_slots_for_keys_and_control() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut queue) = tokio::sync::mpsc::channel(16);
    let connection = Connection {
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
        clipboard_epochs: Default::default(),
    };
    for _ in 0..14 {
        connection
            .enqueue(Outgoing::Command(vec![b'x'; 40 * 1024]))
            .unwrap();
    }
    assert!(
        connection
            .enqueue(Outgoing::Command(vec![b'x'; 40 * 1024]))
            .is_err()
    );
    connection
        .send(request(
            json!({"method":"key","pane":1,"name":"a","text":"a","action":"press"}),
        ))
        .unwrap();
    connection
        .send(StreamRequest::connection(StreamCommand::Focused {
            active: true,
        }))
        .unwrap();
    assert_eq!(std::iter::from_fn(|| queue.try_recv().ok()).count(), 16);
    assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn large_pastes_use_bounded_utf8_chunks_and_preserve_operation_options() {
    let text = "a界\n\0".repeat(transport::MAX_PASTE / 6);
    let mut bytes = Vec::new();
    Outgoing::prepare(request(json!({"method":"paste", "pane":7, "text":text, "clipboard":true, "enter":true, "bottom":true}))).unwrap().write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL)).unwrap();
    let commands: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            assert!(line.len() < 100_000);
            serde_json::from_slice(line).unwrap()
        })
        .collect();
    assert_eq!(commands[0]["method"], "paste-begin");
    assert_eq!(commands[0]["size"], text.len());
    assert_eq!(commands[0]["enter"], true);
    assert_eq!(commands.last().unwrap()["method"], "paste-end");
    let mut received = String::new();
    for command in &commands[1..commands.len() - 1] {
        assert_eq!(command["method"], "paste-data");
        assert_eq!(command["pane"], 7);
        let chunk = command["text"].as_str().unwrap();
        assert!(chunk.len() <= transport::PASTE_CHUNK);
        received.push_str(chunk);
    }
    assert_eq!(received, text);
}

#[test]
fn paste_queue_limits_count_bytes_and_failed_enqueues_release_the_reservation() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(2);
    let connection = Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    };
    let paste = || request(json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)}));
    connection.send(paste()).unwrap();
    connection.send(paste()).unwrap();
    assert!(connection.send(paste()).is_err());
    assert_eq!(
        connection.pending_bytes.load(Ordering::Relaxed),
        2 * transport::MAX_PASTE
    );
    assert!(
        connection
            .send(request(
                json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE + 1)})
            ))
            .is_err()
    );
    assert!(
        connection
            .send(request(json!({"method":"paste","text":""})))
            .is_err()
    );
    assert_eq!(
        connection.pending_bytes.load(Ordering::Relaxed),
        2 * transport::MAX_PASTE
    );
    assert!(receiver.try_recv().is_ok());
    assert_eq!(
        connection.pending_bytes.load(Ordering::Relaxed),
        transport::MAX_PASTE
    );
    drop(receiver);
    assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn aborting_a_blocked_write_releases_in_flight_and_queued_byte_reservations() {
    use tokio::io::AsyncReadExt;
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut queue) = tokio::sync::mpsc::channel(2);
    let connection = Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    };
    for _ in 0..2 {
        connection
            .send(request(
                json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)}),
            ))
            .unwrap();
    }
    let (writer, mut reader) = tokio::io::duplex(32);
    let task = runtime::spawn(async move {
        write_commands(
            writer,
            &mut queue,
            &gmux_platform::unix::ipc::Wake::new().unwrap(),
            HEARTBEAT_INTERVAL,
            RESPONSE_TIMEOUT,
        )
        .await
    });
    // Receiving a byte proves the writer has taken the first operation and
    // is blocked on this tiny socket, with another operation still queued.
    runtime::block_on(async {
        runtime::deadline(Instant::now() + Duration::from_secs(2), async {
            reader.read_exact(&mut [0]).await?;
            Ok(())
        })
        .await
        .unwrap();
    });
    assert_eq!(
        connection.pending_bytes.load(Ordering::Relaxed),
        2 * transport::MAX_PASTE
    );
    task.abort();
    assert!(runtime::block_on(task).unwrap_err().is_cancelled());
    assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn oversized_commands_fail_before_enqueue_and_leave_later_input_usable() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(2);
    let connection = Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    };
    for command in [
        json!({"method":"find","query":"\u{1}".repeat(200_000)}),
        json!({"method":"input","bytes":vec![255u8; 300_000]}),
    ] {
        assert!(
            connection
                .send(request(command))
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
        assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), 0);
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
    connection
        .send(request(json!({"method":"input","bytes":[97]})))
        .unwrap();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"input","bytes":[97]})
    );
}

#[test]
fn ordinary_commands_share_the_outgoing_byte_budget_with_pastes() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(16);
    let connection = Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    };
    connection
        .send(request(
            json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)}),
        ))
        .unwrap();
    let input = || request(json!({"method":"input","bytes":vec![255u8; 250_000]}));
    for _ in 0..4 {
        connection.send(input()).unwrap();
    }
    let before = connection.pending_bytes.load(Ordering::Relaxed);
    assert!(before <= 2 * transport::MAX_PASTE);
    assert!(connection.send(input()).is_err());
    assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), before);
    assert_eq!(std::iter::from_fn(|| receiver.try_recv().ok()).count(), 5);
}
