use super::*;
use crate::remote::tests::{layout, request, wire};
use crate::remote::*;
use gmux_protocol::cells::{decoration, underline};
use gmux_protocol::transport::PaneHeader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[test]
fn cache_pressure_preserves_displayed_prediction_until_real_invalidation() {
    let (mut manager, sender, session) = manager();
    let frame = Frame {
        cols: 2,
        rows: 2,
        scrollback: gmux_protocol::Scrollback {
            total: 20,
            offset: 0,
            len: 2,
        },
        ..Frame::default()
    };
    {
        let mut session = session.borrow_mut();
        session.viewport.frame = frame;
        session.viewport.cache.configure(7, frame);
        session.viewport.cache.rebase(100);
        let mut cell = Cell::default();
        cell.set_text("p").unwrap();
        session.viewport.cache.insert(0, &vec![cell; 20], &[0; 10]);
        session.viewport.scroll_serial.set(2);
        session.viewport.scroll_target = Some(2);
        assert!(session.viewport.predict(2));
        session.viewport.cache.evict();
    }
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: session.borrow().viewport.meta.clone(),
                frame: Some(frame),
                generation: 7,
                cache_origin: 100,
                cache: Some(Default::default()),
                ..PaneHeader::default()
            },
            cells: Vec::new(),
        })))
        .unwrap();
    manager.poll();
    assert!(session.borrow().viewport.prediction.is_some());
    assert_eq!(session.borrow().snapshot().cells[0].text(), "p");
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: session.borrow().viewport.meta.clone(),
                generation: 8,
                cache_origin: 100,
                ..PaneHeader::default()
            },
            cells: Vec::new(),
        })))
        .unwrap();
    manager.poll();
    assert!(session.borrow().viewport.prediction.is_none());
}

#[test]
fn manager_shutdown_releases_optional_storage_even_when_views_survive() {
    let (mut manager, _, session) = manager();
    let frame = Frame {
        cols: 2,
        rows: 2,
        scrollback: gmux_protocol::Scrollback {
            total: 20,
            offset: 0,
            len: 2,
        },
        ..Frame::default()
    };
    {
        let mut session = session.borrow_mut();
        session.viewport.frame = frame;
        session.viewport.cache.configure(1, frame);
        session
            .viewport
            .cache
            .insert(0, &vec![Cell::default(); 20], &[0; 10]);
        assert!(session.viewport.predict(2));
    }
    manager.clear();
    assert_eq!(session.borrow().viewport.cache.retained_bytes(), 0);
    assert!(session.borrow().viewport.prediction.is_none());
}

fn viewport() -> Viewport {
    let mut view = Viewport::new(
        Metadata {
            id: 1,
            directory: "/tmp".into(),
            title: "shell".into(),
            size: (2, 2),
            ..Metadata::default()
        },
        &Theme::adwaita(true),
        SCROLL_CADENCE,
        false,
    );
    view.frame = Frame {
        cols: 2,
        rows: 2,
        ..Frame::default()
    };
    view.cells = vec![Cell::default(); 4];
    view.row_flags = vec![0; 2];
    view
}
fn apply_rows(view: &mut Viewport, rows: Vec<u16>, cells: Vec<Cell>) -> ViewChanges {
    view.apply(
        PaneUpdate {
            header: PaneHeader {
                meta: view.meta.clone(),
                frame: Some(view.frame),
                rows,
                ..PaneHeader::default()
            },
            cells,
        },
        false,
        Instant::now(),
    )
}

fn manager() -> (SessionManager, incoming::Sender, SessionHandle) {
    let (socket, _) = UnixStream::pair().unwrap();
    let (commands, _) = tokio::sync::mpsc::channel(1);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let session = Rc::new(RefCell::new(RemoteSession {
        connection: connection.clone(),
        git: None,
        wake: wake.clone(),
        viewport: viewport(),
    }));
    (
        SessionManager {
            endpoint: ipc::Endpoint::local(),
            daemon: gmux_platform::unix::identity::daemon_id().unwrap(),
            connection,
            receiver,
            focused: ValueCell::new(false),
            sessions: HashMap::from([(1, session.clone())]),
            metadata: HashMap::new(),
            metadata_revision: 0,
            git: HashMap::new(),
            git_panes: HashSet::new(),
            git_subscription_pending: false,
            visible: HashSet::new(),
            subscription_pending: false,
            frame_interval: transport::DEFAULT_FRAME_INTERVAL,
            frame_interval_sent: true,
            clipboard_writes: false,
            pending_reads: HashMap::new(),
            wake,
            disconnected: false,
            disconnect_error: None,
            state: None,
        },
        sender,
        session,
    )
}

#[test]
fn clipboard_generations_coalesce_across_panes_and_retry_after_queue_pressure() {
    use gmux_protocol::clipboard::Destination::{Clipboard, Primary};
    let (mut manager, _, first) = manager();
    manager.metadata.insert(
        2,
        Metadata {
            id: 2,
            ..first.borrow().viewport.meta.clone()
        },
    );
    let second = manager.attach(2, &Theme::adwaita(true)).unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    first.borrow_mut().connection = connection.clone();
    second.borrow_mut().connection = connection;
    for index in 0..128 {
        let session = if index % 2 == 0 { &first } else { &second };
        session.borrow().clipboard_changed(Clipboard, 1).unwrap();
    }
    assert!(second.borrow().clipboard_changed(Clipboard, 2).is_err());
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"clipboard-changed","destination":"clipboard","epoch":1})
    );
    second.borrow().clipboard_changed(Clipboard, 2).unwrap();
    assert!(first.borrow().clipboard_changed(Primary, 1).is_err());
    assert_eq!(wire(receiver.try_recv().unwrap())["epoch"], 2);
    first.borrow().clipboard_changed(Primary, 1).unwrap();
    second.borrow().clipboard_changed(Primary, 1).unwrap();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"clipboard-changed","destination":"primary","epoch":1})
    );
    assert!(receiver.try_recv().is_err());

    // A replacement connection must advertise its own initial generation.
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let (socket, _peer) = UnixStream::pair().unwrap();
    second.borrow_mut().connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    second.borrow().clipboard_changed(Clipboard, 2).unwrap();
    assert_eq!(wire(receiver.try_recv().unwrap())["epoch"], 2);
}

#[test]
fn clipboard_capability_changes_coalesce_and_failed_advertisements_can_retry() {
    let (mut manager, _, _) = manager();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let (socket, _peer) = UnixStream::pair().unwrap();
    manager.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager.set_clipboard_writes(false).unwrap();
    assert!(receiver.try_recv().is_err());
    for _ in 0..10 {
        manager.set_clipboard_writes(true).unwrap();
    }
    assert!(manager.set_clipboard_writes(false).is_err());
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"clipboard-writes","enabled":true})
    );
    manager.set_clipboard_writes(false).unwrap();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"clipboard-writes","enabled":false})
    );
    manager.set_clipboard_writes(false).unwrap();
    assert!(receiver.try_recv().is_err());
}

#[test]
fn clipboard_events_keep_wire_order_without_mutating_selection_or_replay() {
    use gmux_protocol::clipboard::{Destination, Write};
    use std::os::fd::AsRawFd;
    let (mut manager, sender, handle) = manager();
    handle.borrow_mut().viewport.selection = Some(Selection::linear(0, 1));
    for (index, (destination, text)) in [
        (Destination::Clipboard, Some("first 界🙂")),
        (Destination::Primary, Some("")),
        (Destination::Clipboard, None),
        (Destination::Clipboard, Some("last")),
    ]
    .into_iter()
    .enumerate()
    {
        let content = text.map(|text| {
            let mut offer: gmux_protocol::clipboard::Offer = gmux_protocol::clipboard::Content {
                text: text.into(),
                html: (text == "last").then(|| "<div>last</div>".into()),
            }
            .into();
            if text == "last" {
                offer.0.push(gmux_protocol::clipboard::Representation {
                    mime: "image/png".into(),
                    data: include_bytes!("../../../../../scripts/fixtures/clipboard.png").to_vec(),
                });
            }
            offer
        });
        let packet = Packet::Clipboard(Write {
            destination,
            content,
        });
        // Include actual encoding, decoding, and retained-byte admission.
        let mut bytes = vec![];
        packet.encode_into(&mut bytes).unwrap();
        sender
            .send(Ok(Packet::read(&mut &bytes[..]).unwrap()))
            .unwrap();
        if index == 0 {
            sender
                .send(Ok(Packet::ClipboardRead(transport::ClipboardRead {
                    pane: 2,
                    event: gmux_protocol::clipboard::ReadEvent::Cancel { id: 9 },
                })))
                .unwrap();
        }
    }
    let mut writes = vec![];
    let mut events_seen = vec![];
    // No packet may be consumed when another connection used the budget.
    let mut exhausted = gmux_platform::work::Budget::new(Duration::from_secs(60), 0);
    assert!(
        manager
            .poll_with_budget(&mut exhausted, |_| panic!("budget exhausted"))
            .is_empty()
    );
    for count in 1..=5 {
        manager.wake.drain();
        let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), 1);
        assert!(
            manager
                .poll_with_budget(&mut budget, |event| {
                    match event {
                        gmux_protocol::clipboard::Event::Write(write) => {
                            writes.push(write);
                            events_seen.push("write");
                        }
                        gmux_protocol::clipboard::Event::Read { pane, event } => {
                            assert_eq!(pane, 2);
                            assert!(matches!(
                                event,
                                gmux_protocol::clipboard::ReadEvent::Cancel { id: 9 }
                            ));
                            events_seen.push("read");
                        }
                    }
                })
                .is_empty()
        );
        assert_eq!(events_seen.len(), count);
        let mut ready = libc::pollfd {
            fd: manager.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut ready, 1, 0) },
            i32::from(count != 5)
        );
    }
    assert_eq!(events_seen, ["write", "read", "write", "write", "write"]);
    assert_eq!(
        writes
            .iter()
            .map(|w| (
                w.destination,
                w.content.as_ref().and_then(|c| c.plain_text())
            ))
            .collect::<Vec<_>>(),
        [
            (Destination::Clipboard, Some("first 界🙂")),
            (Destination::Primary, Some("")),
            (Destination::Clipboard, None),
            (Destination::Clipboard, Some("last")),
        ]
    );
    assert_eq!(
        writes[3].content.as_ref().unwrap().get("text/html"),
        Some(b"<div>last</div>".as_slice())
    );
    assert_eq!(
        writes[3].content.as_ref().unwrap().get("image/png"),
        Some(include_bytes!("../../../../../scripts/fixtures/clipboard.png").as_slice())
    );
    assert_eq!(
        handle.borrow().viewport.selection,
        Some(Selection::linear(0, 1))
    );
    manager.poll_with_clipboard(|_| panic!("replayed clipboard write"));
}

#[test]
fn link_only_frames_and_late_scroll_replies_keep_displayed_targets_atomic() {
    use gmux_protocol::hyperlinks::Hyperlinks;
    let (mut first, sender, session) = manager();
    let (mut second, other_sender, other) = manager();
    let send = |sender: &incoming::Sender, handle: &SessionHandle, serial, uri: &str| {
        let mut frame = handle.borrow().viewport.frame;
        frame.scrollback.offset = serial;
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: handle.borrow().viewport.meta.clone(),
                    frame: Some(frame),
                    scroll_serial: serial,
                    hyperlinks: Some(Hyperlinks {
                        targets: vec![uri.into()],
                        spans: vec![[0, 2, 0]],
                    }),
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
    };
    send(&sender, &session, 2, "test:newer-scroll");
    send(&other_sender, &other, 7, "test:other-client");
    first.poll();
    second.poll();
    // No hover request can race: even an earlier scroll reply's targets
    // describe exactly the viewport installed from that packet.
    send(&sender, &session, 1, "test:late-scroll");
    assert!(first.poll()[&1].changed);
    let screen = session.borrow();
    assert_eq!(screen.snapshot().frame.scrollback.offset, 1);
    assert_eq!(
        screen.snapshot().hyperlinks.target_at(1),
        Some("test:late-scroll")
    );
    assert_eq!(screen.snapshot().revision, 0);
    assert_eq!(screen.snapshot().text_revision, 0);
    assert_eq!(
        other.borrow().snapshot().hyperlinks.target_at(1),
        Some("test:other-client")
    );
    drop(screen);
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: session.borrow().viewport.meta.clone(),
                frame: Some(session.borrow().viewport.frame),
                hyperlinks: Some(Hyperlinks::default()),
                ..PaneHeader::default()
            },
            cells: vec![],
        })))
        .unwrap();
    first.poll();
    assert_eq!(session.borrow().snapshot().hyperlinks.memory_size(), 0);
    send(&sender, &session, 3, "test:before-resize");
    first.poll();
    let link_revision = session.borrow().snapshot().hyperlink_revision;
    let mut resized = session.borrow().viewport.frame;
    resized.cols = 3;
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: session.borrow().viewport.meta.clone(),
                frame: Some(resized),
                ..PaneHeader::default()
            },
            cells: vec![],
        })))
        .unwrap();
    assert!(first.poll()[&1].changed);
    {
        let screen = session.borrow();
        let snapshot = screen.snapshot();
        assert_eq!(snapshot.frame.cols, 3);
        assert_eq!(snapshot.hyperlinks.memory_size(), 0);
        assert_ne!(snapshot.hyperlink_revision, link_revision);
        assert_eq!(snapshot.text_revision, 1);
        assert_eq!(snapshot.revision, 0);
    }
    let (_, _, fresh) = manager();
    assert_eq!(fresh.borrow().snapshot().hyperlinks.target_at(0), None);
}

#[test]
fn cached_predictions_hide_links_until_the_canonical_view_is_displayed() {
    use gmux_protocol::hyperlinks::Hyperlinks;
    let (mut manager, sender, handle) = manager();
    manager.visible.insert(1);
    let (commands, mut _receiver) = tokio::sync::mpsc::channel(32);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    handle.borrow_mut().connection = connection;
    let frame = Frame {
        cols: 2,
        rows: 2,
        scrollback: gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        },
        ..Frame::default()
    };
    let header = |offset, serial| PaneHeader {
        frame: Some(Frame {
            scrollback: gmux_protocol::Scrollback {
                offset,
                ..frame.scrollback
            },
            ..frame
        }),
        scroll_serial: serial,
        generation: 1,
        ..PaneHeader::default()
    };
    let row = |value| {
        let mut cell = Cell::default();
        cell.set_text_bytes(&[value]).unwrap();
        vec![cell.clone(), cell]
    };
    let links = |uri: &str| {
        Some(Hyperlinks {
            targets: vec![uri.into()],
            spans: vec![[0, 4, 0]],
        })
    };
    let cache = || {
        Some(transport::CachedRows {
            start: 0,
            row_flags: vec![0; 10],
        })
    };
    let send = |mut header: PaneHeader, cells| {
        header.meta = handle.borrow().viewport.meta.clone();
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header,
                cells,
            })))
            .unwrap();
    };
    let initial = || [row(b'e'), row(b'f'), (b'a'..=b'j').flat_map(row).collect()].concat();
    send(
        PaneHeader {
            rows: vec![0, 1],
            cache: cache(),
            hyperlinks: links("test:initial"),
            ..header(4, 0)
        },
        initial(),
    );
    manager.poll();
    let authoritative_revision = handle.borrow().snapshot().hyperlink_revision;
    handle.borrow_mut().scroll_to(5);
    let predicted_revision = handle.borrow().snapshot().hyperlink_revision;
    assert_ne!(predicted_revision, authoritative_revision);
    assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);

    // Older canonical deltas and map-only replies cannot lend targets to
    // the retained prediction, including shifted rows with new targets.
    send(
        PaneHeader {
            row_shift: -1,
            rows: vec![0],
            hyperlinks: links("test:shifted"),
            ..header(3, 0)
        },
        row(b'd'),
    );
    manager.poll();
    let cells = handle.borrow().snapshot().cells.to_vec();
    send(
        PaneHeader {
            hyperlinks: links("test:map-only"),
            ..header(3, 0)
        },
        vec![],
    );
    manager.poll();
    {
        let session = handle.borrow();
        let screen = session.snapshot();
        assert_eq!(screen.frame.scrollback.offset, 5);
        assert!(screen.cells == cells);
        assert_eq!(screen.hyperlinks.memory_size(), 0);
        assert_eq!(session.viewport.frame.scrollback.offset, 3);
        assert_eq!(session.viewport.cells[0].text(), "d");
        assert_eq!(
            session.viewport.hyperlinks.target_at(0),
            Some("test:map-only")
        );
    }
    send(
        PaneHeader {
            rows: vec![0, 1],
            hyperlinks: links("test:matched"),
            ..header(5, 1)
        },
        [row(b'f'), row(b'g')].concat(),
    );
    manager.poll();
    assert!(handle.borrow().viewport.prediction.is_none());
    assert_eq!(
        handle.borrow().snapshot().hyperlinks.target_at(0),
        Some("test:matched")
    );
    assert_ne!(
        handle.borrow().snapshot().hyperlink_revision,
        predicted_revision
    );

    // Completion without a new map still changes displayed map identity.
    handle.borrow_mut().scroll_to(4);
    send(
        PaneHeader {
            rows: vec![0, 1],
            hyperlinks: links("test:four"),
            ..header(4, 1)
        },
        [row(b'e'), row(b'f')].concat(),
    );
    manager.poll();
    assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
    let before_ack = handle.borrow().snapshot().hyperlink_revision;
    send(header(4, 2), vec![]);
    manager.poll();
    let canonical_revision = handle.borrow().snapshot().hyperlink_revision;
    assert_ne!(canonical_revision, before_ack);
    assert_eq!(
        handle.borrow().snapshot().hyperlinks.target_at(0),
        Some("test:four")
    );

    // Metadata-only generation and truncation invalidation restore the
    // canonical frame/map together; neither needs a map-bearing reply.
    handle.borrow_mut().scroll_to(3);
    assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
    send(
        PaneHeader {
            generation: 2,
            ..PaneHeader::default()
        },
        vec![],
    );
    manager.poll();
    assert_eq!(
        handle.borrow().snapshot().hyperlink_revision,
        canonical_revision
    );
    assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 4);
    assert_eq!(
        handle.borrow().snapshot().hyperlinks.target_at(0),
        Some("test:four")
    );
    for resize in [false, true] {
        send(
            PaneHeader {
                generation: 2,
                cache: cache(),
                ..header(4, 3)
            },
            (b'a'..=b'j').flat_map(row).collect(),
        );
        manager.poll();
        handle.borrow_mut().scroll_to(3);
        assert!(handle.borrow().viewport.prediction.is_some());
        let predicted_revision = handle.borrow().snapshot().hyperlink_revision;
        assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
        if resize {
            handle.borrow_mut().resize(3, 2, 0, 0, 0, 0).unwrap();
        } else {
            send(
                PaneHeader {
                    generation: 2,
                    cache_valid_before: Some(4),
                    ..PaneHeader::default()
                },
                vec![],
            );
            manager.poll();
        }
        let session = handle.borrow();
        let screen = session.snapshot();
        assert!(session.viewport.prediction.is_none());
        assert_ne!(screen.hyperlink_revision, predicted_revision);
        assert_eq!(screen.frame.scrollback.offset, 4);
        assert_eq!(screen.hyperlinks.target_at(0), Some("test:four"));
    }
    send(
        PaneHeader {
            generation: 3,
            frame: Some(Frame { cols: 3, ..frame }),
            rows: vec![0, 1],
            ..header(4, 4)
        },
        vec![Cell::default(); 6],
    );
    manager.poll();
    assert_eq!(handle.borrow().snapshot().frame.cols, 3);
    assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
}

#[test]
fn streamed_oversized_packet_disconnects_after_applying_earlier_packets() {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    let (socket, mut daemon_socket) = UnixStream::pair().unwrap();
    daemon_socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut manager = SessionManager::from_attached_stream(
        ipc::Endpoint::local(),
        gmux_platform::unix::ipc::Wake::new().unwrap(),
        gmux_platform::unix::identity::daemon_id().unwrap(),
        socket.into(),
    )
    .unwrap();
    let meta = Metadata {
        id: 1,
        title: "original".into(),
        ..Metadata::default()
    };
    manager.install_metadata(std::slice::from_ref(&meta), 1);
    let session = manager.attach(1, &Theme::adwaita(true)).unwrap();
    let mut packet = Packet::Pane(gmux_protocol::transport::PaneUpdate {
        header: PaneHeader {
            meta,
            notifications: vec!["earlier notification".into()],
            ..PaneHeader::default()
        },
        cells: vec![],
    });
    packet.as_pane_mut().unwrap().header.meta.title = "before oversized packet".into();
    let mut bytes = Vec::new();
    packet.encode_into(&mut bytes).unwrap();
    // An untrusted declaration cannot allocate beyond the local ceiling.
    // Only send the fixed framing, so rejection cannot depend on payload I/O.
    let mut oversized = bytes[..transport::RECORD_PREFIX_BYTES].to_vec();
    oversized[12..16].copy_from_slice(&((incoming::MAX_BYTES + 1) as u32).to_le_bytes());
    bytes.extend_from_slice(&oversized);
    packet.as_pane_mut().unwrap().header.notifications.clear();
    packet.as_pane_mut().unwrap().header.meta.title = "after oversized packet".into();
    packet.encode_into(&mut bytes).unwrap();
    drop(packet);
    daemon_socket.write_all(&bytes).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut notifications = Vec::new();
    while !manager.disconnected() {
        for update in manager.poll().into_values() {
            notifications.extend(update.notifications);
        }
        assert!(
            Instant::now() < deadline,
            "reader did not report oversized packet"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        manager
            .disconnect_error()
            .unwrap()
            .to_string()
            .contains("decode allowance")
    );
    assert_eq!(
        session.borrow().viewport.meta.title,
        "before oversized packet"
    );
    assert_eq!(notifications, ["earlier notification"]);
}

#[test]
fn older_stream_state_cannot_replace_metadata_from_a_completed_command() {
    let (mut manager, sender, handle) = manager();
    let mut current = handle.borrow().viewport.meta.clone();
    current.title = "newer state".into();
    let mut added = current.clone();
    added.id = 2;
    manager.install_metadata(&[current.clone(), added], 2);
    current.title = "older stream state".into();
    manager.install_metadata(&[current.clone()], 1);
    let state = serde_json::from_value(serde_json::json!({
            "daemon":manager.daemon,"home_directory":"/tmp","machine_kind":"unknown","hostname":"host", "revision":1,
            "result":{"ok":true},"execution":{"tabs":[]},"metadata":[current]
        })).unwrap();
    sender.send(Ok(Packet::Execution(state))).unwrap();
    manager.poll();
    assert!(
        manager.take_state().is_none(),
        "An older stream snapshot was published"
    );
    assert_eq!(handle.borrow().viewport.meta.title, "newer state");
    assert!(
        manager.attach(2, &Theme::adwaita(true)).is_ok(),
        "A new pane's metadata was lost"
    );
}

#[test]
fn git_metadata_is_connection_scoped_ignores_old_directories_and_does_not_repaint() {
    let (mut local, local_sender, local_session) = manager();
    let (mut remote, remote_sender, remote_session) = manager();
    let send = |sender: &incoming::Sender, directory: &str, branch: &str| {
        sender
            .send(Ok(Packet::Git(transport::GitMetadata {
                id: 1,
                directory: directory.into(),
                branch: branch.into(),
            })))
            .unwrap();
    };
    for manager in [&mut local, &mut remote] {
        manager.git_panes.insert(1);
    }
    local_session.borrow_mut().viewport.selection = Some(Selection::linear(1, 3));
    send(&local_sender, "/tmp", "local-main");
    send(&remote_sender, "/tmp", "remote-main");
    let updates = local.poll();
    assert!(updates[&1].git_changed);
    assert!(!updates[&1].changed && !updates[&1].directory_changed);
    assert_eq!(
        local_session.borrow().viewport.selection,
        Some(Selection::linear(1, 3))
    );
    remote.poll();
    assert_eq!(local_session.borrow().branch(), "local-main");
    assert_eq!(remote_session.borrow().branch(), "remote-main");
    assert_eq!(local_session.borrow().viewport.revision, 0);
    assert_eq!(local_session.borrow().viewport.text_revision, 0);

    // A delayed completion from the prior CWD cannot label the current pane.
    remote_session.borrow_mut().viewport.meta.directory = "/tmp/other".into();
    assert_eq!(remote_session.borrow().branch(), "");
    send(&remote_sender, "/tmp", "old-directory");
    remote.poll();
    assert_eq!(remote_session.borrow().branch(), "");
    send(&remote_sender, "/tmp/other", "current-directory");
    remote.poll();
    assert_eq!(remote_session.borrow().branch(), "current-directory");
    let _ = remote.set_git_panes(HashSet::new());
    send(&remote_sender, "/tmp/other", "late-unsubscribed");
    assert!(remote.poll().is_empty());
    assert_eq!(remote_session.borrow().branch(), "");
    assert_eq!(local_session.borrow().branch(), "local-main");
    local.clear();
    assert_eq!(local_session.borrow().branch(), "");
}

#[test]
fn git_subscriptions_retry_after_command_backpressure() {
    let (mut manager, _, _) = manager();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let (socket, _peer) = UnixStream::pair().unwrap();
    manager.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager
        .connection
        .send(StreamRequest::connection(StreamCommand::Focused {
            active: true,
        }))
        .unwrap();
    assert!(manager.set_git_panes(HashSet::from([1])).is_err());
    assert!(manager.git_subscription_pending);
    let _ = receiver.try_recv().unwrap();
    manager.poll();
    assert!(!manager.git_subscription_pending);
    let mut bytes = Vec::new();
    receiver
        .try_recv()
        .unwrap()
        .outgoing
        .write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"method":"git-subscribe","panes":[1]})
    );
}

#[test]
fn progress_and_program_status_changes_preserve_viewport_and_ordered_metadata() {
    let (mut manager, sender, session) = manager();
    // Metadata must also leave a populated history cache and its active
    // prediction alone, rather than only preserving an empty live viewport.
    manager.visible.insert(1);
    let (commands, mut _outgoing) = tokio::sync::mpsc::channel(8);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    {
        let mut session = session.borrow_mut();
        session.connection = connection;
        session.viewport.frame.scrollback = gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        };
        let frame = session.viewport.frame;
        session.viewport.cache.configure(1, frame);
        session.viewport.cache.insert(
            0,
            &std::array::from_fn::<_, 20, _>(|_| Cell::default()),
            &[0; 10],
        );
        session.scroll_to(3);
    }
    let before = session.borrow();
    let revision = before.snapshot().revision;
    let text_revision = before.snapshot().text_revision;
    let frame = before.snapshot().frame;
    let cache_bytes = before.viewport.cache.retained_bytes();
    drop(before);
    let mut meta = session.borrow().viewport.meta.clone();
    for state in [
        gmux_protocol::progress::ProgressState::Determinate,
        gmux_protocol::progress::ProgressState::Error,
        gmux_protocol::progress::ProgressState::Paused,
        gmux_protocol::progress::ProgressState::Indeterminate,
        gmux_protocol::progress::ProgressState::Inactive,
    ] {
        meta.progress.apply(state, Some(42));
        meta.program_status = Some(
            serde_json::from_value(serde_json::json!({
                "count": 1, "record": {"id":"worker", "state":"blocked", "kind":"question",
                    "progress":42, "app":"cargo", "title":"Tests", "message": format!("{state:?}")}
            }))
            .unwrap(),
        );
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: meta.clone(),
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
        let updates = manager.poll();
        assert!(updates[&1].progress_changed);
        assert!(updates[&1].program_status_changed);
        assert!(!updates[&1].changed);
        let borrowed = session.borrow();
        assert_eq!(borrowed.progress(), meta.progress);
        assert_eq!(borrowed.snapshot().revision, revision);
        assert_eq!(borrowed.snapshot().text_revision, text_revision);
        assert!(borrowed.snapshot().frame == frame);
        assert!(borrowed.viewport.prediction.is_some());
        assert_eq!(borrowed.viewport.cache.retained_bytes(), cache_bytes);
        assert!(borrowed.snapshot().cells == std::array::from_fn::<_, 4, _>(|_| Cell::default()));
        assert!(!borrowed.unread());
        assert!(!borrowed.busy());
        drop(borrowed);
        let mut stale = meta.clone();
        stale.progress = Default::default();
        stale.program_status = None;
        manager.install_metadata(&[stale], 1);
        assert_eq!(session.borrow().progress(), meta.progress);
        assert_eq!(
            session.borrow().program_status(),
            meta.program_status.as_ref()
        );
    }
}

#[test]
fn idle_and_metadata_updates_do_not_repaint_or_change_cells() {
    let (mut manager, sender, session) = manager();
    assert!(manager.poll().is_empty());
    let mut meta = session.borrow().viewport.meta.clone();
    meta.directory = PathBuf::from("/tmp/project");
    meta.title = "worker".into();
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta,
                notifications: vec!["ready".into()],
                ..PaneHeader::default()
            },
            cells: vec![],
        })))
        .unwrap();
    let updates = manager.poll();
    assert_eq!(updates.len(), 1);
    let update = &updates[&1];
    assert!(!update.changed);
    assert!(update.directory_changed);
    assert_eq!(update.notifications, ["ready"]);
    let session = session.borrow();
    assert_eq!(session.title(), "worker");
    assert_eq!(session.directory(), Path::new("/tmp/project"));
    assert_eq!(session.snapshot().revision, 0);
    assert!(session.snapshot().cells == std::array::from_fn::<_, 4, _>(|_| Cell::default()));
    assert!(manager.poll().is_empty());
}

#[test]
fn state_from_another_daemon_stops_recovery_without_replacing_the_local_view() {
    let (mut manager, sender, _session) = manager();
    let packet = |daemon| {
        Packet::Execution(gmux_protocol::control::CommandReply {
            home_directory: "/home/test".into(),
            machine_kind: gmux_protocol::model::MachineKind::Unknown,
            hostname: "test-host".into(),
            daemon,
            revision: 1,
            result: gmux_protocol::control::CommandOutput::Done { ok: true }.into(),
            execution: ExecutionSnapshot::default(),
            metadata: vec![],
        })
    };
    sender.send(Ok(packet(manager.daemon))).unwrap();
    assert!(manager.poll().is_empty());
    assert_eq!(manager.take_state().unwrap().daemon, manager.daemon);
    sender
        .send(Ok(packet(
            gmux_platform::unix::identity::daemon_id().unwrap(),
        )))
        .unwrap();
    let mut connection = crate::connection::Connection::from_session(
        manager,
        gmux_platform::unix::ipc::Wake::new().unwrap(),
    );
    connection.set_auto_reconnect(true);
    let update = connection.poll();
    assert!(matches!(
        connection.status(),
        crate::connection::Status::Disconnected { error: Some(_) }
    ));
    assert!(connection.next_retry().is_none());
    assert!(update.state.is_none());
    assert!(
        update.panes[&1]
            .error
            .as_ref()
            .unwrap()
            .contains("different daemon")
    );
}

#[test]
fn exits_arrive_for_unattached_panes_and_survive_update_coalescing() {
    let (mut manager, sender, session) = manager();
    for (id, exited) in [(2, false), (2, true), (1, true), (1, false)] {
        let mut meta = session.borrow().viewport.meta.clone();
        meta.id = id;
        meta.exited = exited;
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
    }
    let updates = manager.poll();
    assert_eq!(updates.len(), 2);
    assert!(updates[&1].exited);
    assert!(updates[&2].exited);
    assert!(!manager.sessions.contains_key(&2));
    assert!(!manager.metadata.contains_key(&2));
    assert!(manager.poll().is_empty());
}

#[test]
fn unattached_notifications_arrive_in_order_without_empty_metadata_updates() {
    let (mut manager, sender, session) = manager();
    for (notifications, exited) in [
        (vec![], false),
        (vec!["first"], false),
        (vec!["second"], true),
    ] {
        let mut meta = session.borrow().viewport.meta.clone();
        meta.id = 2;
        meta.exited = exited;
        let empty = notifications.is_empty();
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta,
                    notifications: notifications.into_iter().map(str::to_owned).collect(),
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
        if empty {
            assert!(manager.poll().is_empty());
        }
    }
    let updates = manager.poll();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[&2].notifications, ["first", "second"]);
    assert!(updates[&2].exited);
    assert!(!manager.sessions.contains_key(&2));
    assert!(!manager.metadata.contains_key(&2));
    assert!(manager.poll().is_empty());
}

#[test]
fn queued_rows_coalesce_without_losing_cells_or_cursor_changes() {
    let (mut manager, sender, session) = manager();
    let mut frame = session.borrow().viewport.frame;
    for (row, text) in [(0, b'a'), (1, b'b')] {
        let mut cell = Cell::default();
        cell.set_text_bytes(&[text]).unwrap();
        frame.cursor_y = row;
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: session.borrow().viewport.meta.clone(),
                    frame: Some(frame),
                    rows: vec![row],
                    ..PaneHeader::default()
                },
                cells: vec![cell.clone(), cell],
            })))
            .unwrap();
    }
    let updates = manager.poll();
    assert_eq!(updates.len(), 1);
    assert!(updates[&1].changed);
    assert!(!updates[&1].directory_changed);
    let session = session.borrow();
    let screen = session.snapshot();
    assert_eq!(screen.revision, 2);
    assert_eq!(screen.text_revision, 2);
    assert_eq!(screen.frame.cursor_y, 1);
    assert_eq!(
        screen.cells.iter().map(Cell::text).collect::<Vec<_>>(),
        ["a", "a", "b", "b"]
    );
}

fn send_frame(
    sender: &incoming::Sender,
    session: &SessionHandle,
    frame: Frame,
    rows: Vec<u16>,
    cells: Vec<Cell>,
    selection: Option<Selection>,
) {
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: session.borrow().viewport.meta.clone(),
                frame: Some(frame),
                rows,
                selection,
                ..PaneHeader::default()
            },
            cells,
        })))
        .unwrap();
}

#[test]
fn deferred_row_shifts_apply_in_order_before_disconnect() {
    let (mut manager, sender, session) = manager();
    let cells = |text: &str| {
        text.chars()
            .map(|c| {
                let mut cell = Cell::default();
                cell.set_text(&c.to_string()).unwrap();
                cell
            })
            .collect()
    };
    let frame = session.borrow().viewport.frame;
    send_frame(&sender, &session, frame, vec![0, 1], cells("aabb"), None);
    for text in ["cc", "dd"] {
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: session.borrow().viewport.meta.clone(),
                    frame: Some(frame),
                    row_shift: 1,
                    rows: vec![1],
                    ..PaneHeader::default()
                },
                cells: cells(text),
            })))
            .unwrap();
    }
    drop(sender);
    for expected in ["aabb", "bbcc", "ccdd"] {
        let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), 1);
        manager.poll_with_budget(&mut budget, |_| panic!("unexpected side effect"));
        assert_eq!(
            session
                .borrow()
                .viewport
                .cells
                .iter()
                .map(Cell::text)
                .collect::<String>(),
            expected
        );
        assert!(!manager.disconnected());
    }
    // Channel closure must request a turn even with no packet left to wake it.
    manager.poll();
    assert!(manager.disconnected());
}

#[test]
fn color_and_style_rows_preserve_text_revision_and_still_repaint() {
    let mut view = viewport();
    let canonical_storage = view.cells.as_ptr();
    let mut cells = view.cells.clone();
    cells[0].set_text("界").unwrap();
    cells[1].flags = 64;
    cells[2]
        .set_text(&format!("e{}", "\u{301}".repeat(64)))
        .unwrap();
    assert!(
        apply_rows(&mut view, vec![0, 1], cells.clone())
            .presentation
            .changed
    );
    assert_eq!(view.snapshot().text_revision, 1);
    for cell in &mut cells {
        cell.fg = [1, 2, 3];
        cell.bg = [4, 5, 6];
        cell.flags |= 63;
        cell.decoration = underline::DASHED | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
        cell.underline_color = [8, 9, 10];
    }
    assert!(
        apply_rows(&mut view, vec![0, 1], cells.clone())
            .presentation
            .changed
    );
    let screen = view.snapshot();
    assert_eq!(screen.revision, 2);
    assert_eq!(screen.text_revision, 1);
    assert!(screen.cells == cells);
    assert_eq!(screen.cells.as_ptr(), canonical_storage);
}

#[test]
fn single_row_text_and_wide_layout_updates_invalidate_accessible_text() {
    let mut view = viewport();
    let mut row = vec![Cell::default(); 2];
    row[0].set_text("e\u{301}").unwrap();
    row[1].set_text("X").unwrap();
    assert!(
        apply_rows(&mut view, vec![1], row.clone())
            .presentation
            .changed
    );
    assert_eq!(view.snapshot().text_revision, 1);
    assert!(view.snapshot().cells[..2] == std::array::from_fn::<_, 2, _>(|_| Cell::default()));
    // The same concatenated text can occupy different cells. Its caret and
    // geometry mappings must still be rebuilt.
    row[0].set_text("").unwrap();
    row[0].set_text("e").unwrap();
    row[1].set_text("\u{301}X").unwrap();
    assert!(
        apply_rows(&mut view, vec![1], row.clone())
            .presentation
            .changed
    );
    assert_eq!(view.snapshot().text_revision, 2);
    row[1].flags = 64;
    assert!(apply_rows(&mut view, vec![1], row).presentation.changed);
    assert_eq!(view.snapshot().text_revision, 3);
}

#[test]
fn selection_replies_wait_for_the_latest_range_or_clear() {
    use gmux_protocol::transport::SelectionMode;
    let (mut manager, sender, session) = manager();
    let (commands, mut _outgoing) = tokio::sync::mpsc::channel(16);
    let (socket, _peer) = UnixStream::pair().unwrap();
    session.borrow_mut().connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    let send = |serial, selection| {
        sender
            .send(Ok(transport::Packet::Pane(
                gmux_protocol::transport::PaneUpdate {
                    header: PaneHeader {
                        meta: session.borrow().viewport.meta.clone(),
                        selection,
                        selection_serial: serial,
                        ..PaneHeader::default()
                    },
                    cells: Vec::new(),
                },
            )))
            .unwrap();
    };
    session
        .borrow()
        .select((0, 0), (1, 0), SelectionMode::Word)
        .unwrap();
    send(0, Some(Selection::linear(2, 3)));
    manager.poll();
    assert!(session.borrow().selection_pending());
    assert_eq!(session.borrow().viewport.selection(), None);
    send(1, Some(Selection::linear(0, 1)));
    assert!(manager.poll()[&1].changed);
    assert!(!session.borrow().selection_pending());
    assert_eq!(
        session.borrow().viewport.selection(),
        Some(Selection::linear(0, 1))
    );
    session.borrow().clear_selection().unwrap();
    send(1, Some(Selection::linear(2, 3)));
    manager.poll();
    assert!(session.borrow().selection_pending());
    assert_eq!(
        session.borrow().viewport.selection(),
        Some(Selection::linear(0, 1))
    );
    send(2, None);
    assert!(manager.poll()[&1].changed);
    assert!(!session.borrow().selection_pending());
    assert_eq!(session.borrow().viewport.selection(), None);
}

#[test]
fn geometry_and_positions_survive_frames_without_row_deltas() {
    let (mut manager, sender, session) = manager();
    let frame = Frame {
        cursor_x: 1,
        cursor_y: 1,
        ..session.borrow().viewport.frame
    };
    send_frame(
        &sender,
        &session,
        frame,
        vec![],
        vec![],
        Some(Selection::linear(1, 3)),
    );
    assert!(manager.poll()[&1].changed);
    assert_eq!(session.borrow().snapshot().text_revision, 0);
    assert_eq!(session.borrow().snapshot().frame.cursor_y, 1);
    assert_eq!(
        session.borrow().viewport.selection(),
        Some(Selection::linear(1, 3))
    );
    let frame = Frame {
        cols: 4,
        rows: 1,
        ..frame
    };
    send_frame(&sender, &session, frame, vec![], vec![], None);
    assert!(manager.poll()[&1].changed);
    let session = session.borrow();
    let screen = session.snapshot();
    assert_eq!(screen.revision, 0);
    assert_eq!(screen.text_revision, 1);
    assert_eq!((screen.frame.cols, screen.frame.rows), (4, 1));
    assert_eq!(screen.cells.len(), 4);
}
#[test]
fn output_and_pruning_preserve_predictions_until_their_rows_change() {
    let (mut manager, sender, handle) = manager();
    manager.visible.insert(1);
    let (commands, mut _outgoing) = tokio::sync::mpsc::channel(32);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    handle.borrow_mut().connection = connection;
    let frame = Frame {
        cols: 2,
        rows: 2,
        scrollback: gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        },
        ..Frame::default()
    };
    let row = |value| {
        let mut cell = Cell::default();
        cell.set_text_bytes(&[value]).unwrap();
        [cell.clone(), cell]
    };
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                frame: Some(frame),
                generation: 1,
                rows: vec![0, 1],
                cache: Some(transport::CachedRows {
                    start: 0,
                    row_flags: vec![0; 10],
                }),
                ..PaneHeader::default()
            },
            cells: row(b'e')
                .into_iter()
                .chain(row(b'f'))
                .chain((b'a'..=b'j').flat_map(row))
                .collect(),
        })))
        .unwrap();
    manager.poll();
    handle.borrow_mut().scroll_to(3);
    let send = |origin, valid, frame| {
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: handle.borrow().viewport.meta.clone(),
                    generation: 1,
                    cache_origin: origin,
                    cache_valid_before: Some(valid),
                    frame,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap()
    };
    send(0, 8, None);
    manager.poll();
    assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
    send(
        2,
        6,
        Some(Frame {
            scrollback: gmux_protocol::Scrollback {
                total: 18,
                offset: 2,
                len: 2,
            },
            ..frame
        }),
    );
    manager.poll();
    assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 1);
    assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
    // Copy and selection metadata omit cache state and must not reset its origin.
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                ..PaneHeader::default()
            },
            cells: vec![],
        })))
        .unwrap();
    manager.poll();
    assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
    send(2, 2, None);
    assert!(manager.poll()[&1].changed);
    assert!(handle.borrow().viewport.prediction.is_none());
    handle.borrow_mut().viewport.clear_cache();
    handle.borrow_mut().scroll_to(10);
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                generation: 2,
                cache_origin: 100,
                frame: Some(frame),
                ..PaneHeader::default()
            },
            cells: vec![],
        })))
        .unwrap();
    manager.poll();
    assert_eq!(
        handle.borrow().viewport.scroll_target,
        Some(10),
        "A new generation's origin cannot rebase a target from an unknown cache"
    );
}

#[test]
fn cached_edge_requests_coalesce_during_refill_but_misses_keep_pipelining() {
    let (mut manager, _sender, handle) = manager();
    let (commands, mut outgoing) = tokio::sync::mpsc::channel(8);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let mut session = handle.borrow_mut();
    session.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    session.viewport.frame.scrollback = gmux_protocol::Scrollback {
        total: 20,
        offset: 4,
        len: 2,
    };
    let frame = session.viewport.frame;
    session.viewport.cache.configure(1, frame);
    session.viewport.cache.insert(
        4,
        &std::array::from_fn::<_, 6, _>(|_| Cell::default()),
        &[0; 3],
    );
    session.scroll_to(4);
    assert_eq!(wire(outgoing.try_recv().unwrap())["row"], 4);
    session.scroll_to(5);
    assert_eq!(session.snapshot().frame.scrollback.offset, 5);
    assert!(outgoing.try_recv().is_err());
    assert!(session.viewport.scroll_defer_until.get().is_some());
    session.scroll_to(10);
    assert!(session.viewport.scroll_defer_until.get().is_none());
    session
        .viewport
        .scroll_sent_at
        .set(Instant::now() - session.viewport.scroll_cadence);
    drop(session);
    manager.poll();
    assert_eq!(wire(outgoing.try_recv().unwrap())["row"], 10);
    assert!(handle.borrow().scroll_pending());
}

#[test]
fn returning_to_the_live_bottom_does_not_debounce_cursor_synchronization() {
    let (_manager, _sender, handle) = manager();
    let (commands, mut outgoing) = tokio::sync::mpsc::channel(8);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let mut session = handle.borrow_mut();
    session.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    session.viewport.frame.scrollback = gmux_protocol::Scrollback {
        total: 20,
        offset: 4,
        len: 2,
    };
    session.viewport.frame.cursor_visible = 0;
    let frame = session.viewport.frame;
    session.viewport.cache.configure(1, frame);
    session.viewport.cache.insert(
        10,
        &std::array::from_fn::<_, 20, _>(|_| Cell::default()),
        &[0; 10],
    );
    session.scroll_to(16);
    assert_eq!(session.snapshot().frame.scrollback.offset, 16);
    assert!(session.viewport.scroll_defer_until.get().is_some());
    assert!(outgoing.try_recv().is_err());
    session.scroll_to(18);
    assert_eq!(session.snapshot().frame.scrollback.offset, 18);
    assert!(
        session.viewport.scroll_defer_until.get().is_none(),
        "The live cursor must not wait for cache settle"
    );
    assert_eq!(
        wire(outgoing.try_recv().unwrap()),
        json!({"method":"scroll-to", "pane":1, "row":18, "serial":2})
    );
}

#[test]
fn display_interval_changes_update_scroll_pacing_and_retry_after_backpressure() {
    let (mut manager, _sender, handle) = manager();
    let (commands, mut outgoing) = tokio::sync::mpsc::channel(1);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    handle.borrow_mut().connection = connection;
    manager.set_focused(true).unwrap();
    manager.set_frame_interval(Duration::from_micros(4167));
    assert_eq!(
        handle.borrow().viewport.scroll_cadence,
        Duration::from_micros(4167)
    );
    assert_eq!(wire(outgoing.try_recv().unwrap())["method"], "focused");
    manager.poll();
    assert_eq!(
        wire(outgoing.try_recv().unwrap()),
        json!({"method":"frame-interval", "micros":4167})
    );
    assert!(manager.frame_interval_sent);
}

#[test]
fn wire_cache_retention_never_replaces_authoritative_viewport_deltas() {
    // The same wire-valid supplemental window exceeds the ring's row
    // capacity at this width. A taller viewport disables retention entirely.
    for rows in [20, 60] {
        let (mut manager, sender, handle) = manager();
        manager.visible.insert(1);
        let cols = 1000;
        let cached = transport::cache_row_limit(cols);
        let frame = Frame {
            cols: cols as u16,
            rows: rows as u16,
            scrollback: gmux_protocol::Scrollback {
                total: 1000,
                offset: 100,
                len: rows as u64,
            },
            ..Frame::default()
        };
        let cell = |text| {
            let mut cell = Cell::default();
            cell.set_text(text).unwrap();
            cell
        };
        let send = |header, cells| {
            let mut bytes = Vec::new();
            Packet::Pane(gmux_protocol::transport::PaneUpdate { header, cells })
                .encode_into(&mut bytes)
                .unwrap();
            sender
                .send(Ok(Packet::read(&mut bytes.as_slice()).unwrap()))
                .unwrap();
        };
        let mut cells = vec![cell("viewport"); rows * cols];
        cells.extend(vec![cell("supplemental"); cached * cols]);
        send(
            PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                generation: 1,
                frame: Some(frame),
                rows: (0..rows as u16).collect(),
                row_flags: (0..rows).map(|row| (row % 2) as u8).collect(),
                cache: Some(transport::CachedRows {
                    start: 80,
                    row_flags: vec![0; cached],
                }),
                ..PaneHeader::default()
            },
            cells,
        );
        manager.poll();
        {
            let session = handle.borrow();
            assert!(!manager.disconnected);
            assert!(
                session
                    .snapshot()
                    .cells
                    .iter()
                    .all(|c| c.text() == "viewport")
            );
            assert_eq!(session.viewport.cache.is_empty(), rows == 60);
        }
        // A following delta depends on the authoritative grid, regardless
        // of which overlapping supplemental rows the client retained.
        send(
            PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                generation: 1,
                frame: Some(Frame {
                    scrollback: gmux_protocol::Scrollback {
                        offset: 101,
                        ..frame.scrollback
                    },
                    ..frame
                }),
                row_shift: 1,
                rows: vec![rows as u16 - 1],
                row_flags: vec![1],
                ..PaneHeader::default()
            },
            vec![cell("exposed"); cols],
        );
        manager.poll();
        let session = handle.borrow();
        let snapshot = session.snapshot();
        assert_eq!(snapshot.frame.scrollback.offset, 101);
        assert!(
            snapshot.cells[..(rows - 1) * cols]
                .iter()
                .all(|c| c.text() == "viewport")
        );
        assert!(
            snapshot.cells[(rows - 1) * cols..]
                .iter()
                .all(|c| c.text() == "exposed")
        );
        assert_eq!(snapshot.row_flags[0], 1);
        assert_eq!(snapshot.row_flags[rows - 1], 1);
        assert_eq!(session.viewport.cache.is_empty(), rows == 60);
    }
}

#[test]
fn cached_scrolling_is_immediate_and_reconciles_ordered_deltas_and_invalidations() {
    let (mut manager, sender, handle) = manager();
    manager.visible.insert(1);
    let (commands, mut receiver) = tokio::sync::mpsc::channel(32);
    let (socket, _peer) = UnixStream::pair().unwrap();
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    handle.borrow_mut().connection = connection;
    let row = |value: u8| {
        let mut cell = Cell::default();
        cell.set_text_bytes(&[value]).unwrap();
        cell.decoration = underline::CURLY;
        vec![cell.clone(), cell]
    };
    let frame = Frame {
        cols: 2,
        rows: 2,
        scrollback: gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        },
        cursor_visible: 1,
        cursor_y: 1,
        ..Frame::default()
    };
    let send = |offset, serial, generation, shift, rows, cells, cache| {
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: handle.borrow().viewport.meta.clone(),
                    frame: Some(Frame {
                        scrollback: gmux_protocol::Scrollback {
                            offset,
                            ..frame.scrollback
                        },
                        ..frame
                    }),
                    scroll_serial: serial,
                    generation,
                    row_shift: shift,
                    rows,
                    cache,
                    ..PaneHeader::default()
                },
                cells,
            })))
            .unwrap();
    };
    let mut initial = [row(b'e'), row(b'f')].concat();
    initial.extend((b'a'..=b'j').flat_map(row));
    send(
        4,
        0,
        1,
        0,
        vec![0, 1],
        initial,
        Some(transport::CachedRows {
            start: 0,
            row_flags: vec![0; 10],
        }),
    );
    manager.poll();
    for target in [3, 6, 2, 5] {
        handle.borrow_mut().scroll_to(target);
        let session = handle.borrow();
        let screen = session.snapshot();
        assert_eq!(screen.frame.scrollback.offset, target);
        assert_eq!(
            &screen.cells[0].text().as_bytes()[0..1],
            &[b'a' + target as u8]
        );
        assert_eq!(screen.cells[0].decoration, underline::CURLY);
        assert!(session.scroll_pending());
    }
    assert!(
        receiver.try_recv().is_err(),
        "Cached reversals should not stream intermediate targets"
    );
    assert!(manager.next_scroll().is_some());
    handle
        .borrow()
        .viewport
        .scroll_defer_until
        .set(Some(Instant::now() - Duration::from_millis(1)));
    manager.poll();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":5,"serial":4})
    );
    // This old viewport shifts the canonical grid to [d,e]. It must
    // not shift the locally predicted [f,g] or lose the delta dependency.
    send(3, 1, 1, -1, vec![0], row(b'd'), None);
    manager.poll();
    assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 5);
    assert_eq!(handle.borrow().snapshot().cells[0].text(), "f");
    send(
        5,
        4,
        1,
        0,
        vec![0, 1],
        [row(b'f'), row(b'g')].concat(),
        None,
    );
    manager.poll();
    assert!(!handle.borrow().scroll_pending());
    assert!(handle.borrow().viewport.prediction.is_none());
    assert_eq!(handle.borrow().text(), "ff\ngg");
    handle.borrow_mut().scroll_to(4);
    assert_eq!(handle.borrow().snapshot().cells[0].text(), "e");
    handle
        .borrow()
        .copy(gmux_protocol::clipboard::Formats::Text)
        .unwrap();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":4,"serial":5})
    );
    assert_eq!(wire(receiver.try_recv().unwrap())["method"], "copy");
    sender
        .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                meta: handle.borrow().viewport.meta.clone(),
                generation: 2,
                ..PaneHeader::default()
            },
            cells: Vec::new(),
        })))
        .unwrap();
    assert!(manager.poll()[&1].changed);
    assert!(
        handle.borrow().viewport.prediction.is_none(),
        "Metadata-only invalidation must discard predicted rows"
    );
    // New output may trim/reflow history even if total and size are
    // unchanged. A new generation must discard every old row.
    send(
        4,
        5,
        2,
        0,
        vec![0, 1],
        [row(b'x'), row(b'y')].concat(),
        None,
    );
    manager.poll();
    handle.borrow_mut().scroll_to(2);
    assert!(handle.borrow().viewport.prediction.is_none());
    assert_eq!(handle.borrow().text(), "xx\nyy");
    manager.set_visible(HashSet::new()).unwrap();
    assert!(handle.borrow().viewport.cache.is_empty());
}

#[test]
fn key_actions_survive_the_frontend_command_queue() {
    let (_, _, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
    session.borrow_mut().connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    for (action, expected) in [
        (KeyAction::Press, "press"),
        (KeyAction::Repeat, "repeat"),
        (KeyAction::Release, "release"),
    ] {
        session
            .borrow()
            .key(
                "exclam",
                KeyLayout {
                    physical: 2,
                    unshifted: 49,
                    consumed_mods: 1,
                },
                33,
                action,
                "!",
            )
            .unwrap();
        let request = wire(receiver.try_recv().unwrap());
        assert_eq!(
            request,
            json!({"method":"key","pane":1,"name":"exclam","physical":2,"unshifted":49,"consumed_mods":1,"mods":33,"action":expected,"text":"!"})
        );
        assert!(
            matches!(StreamRequest::decode(&serde_json::to_vec(&request).unwrap()).command,
                StreamCommand::Key { action: actual, .. } if actual == action)
        );
    }
}

#[test]
fn rapid_scroll_events_pipeline_before_viewport_replies_arrive() {
    let (mut manager, sender, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(128);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;
    // The full cadence is longer than any test runtime, so the release in
    // the dedicated cadence test is the only deadline path.
    session.borrow_mut().viewport.scroll_cadence = Duration::from_secs(600);

    session.borrow_mut().scroll_to(10);
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":10,"serial":1})
    );
    let reply = |serial, frame| {
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: session.borrow().viewport.meta.clone(),
                    frame,
                    scroll_serial: serial,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
    };
    // A responsive viewport acknowledges each scroll before the next wheel
    // event arrives: scrolling streams at input rate instead of pacing at
    // one update per round-trip.
    for (serial, index) in (1..).zip(0..120) {
        let row = if index % 2 == 0 { 80 } else { 20 };
        reply(serial, Some(session.borrow().viewport.frame));
        manager.poll();
        session.borrow_mut().scroll_to(row);
        session
            .borrow_mut()
            .set_theme(&Theme::adwaita(true))
            .unwrap();
        session
            .borrow()
            .set_cursor_style(CursorStyle::Block, false)
            .unwrap();
        session
            .borrow()
            .key("a", layout(b'a' as u32), 0, KeyAction::Release, "")
            .unwrap();
        manager.poll();
        let commands: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(wire)
            .collect();
        assert_eq!(commands.len(), 4);
        assert_eq!(
            commands[0],
            json!({"method":"scroll-to","pane":1,"row":row,"serial":serial + 1})
        );
        assert_eq!(commands[1]["method"], "theme");
        assert_eq!(commands[2]["method"], "style");
        assert_eq!(commands[3]["method"], "key");
    }
    assert!(session.borrow().scroll_pending());
    assert!(manager.next_scroll().is_none());
    assert!(receiver.try_recv().is_err());

    // Replays and older serials never acknowledge the latest target, and
    // acknowledgements must not re-send settled scrolls.
    let mut cell = Cell::default();
    cell.set_text("x").unwrap();
    send_frame(
        &sender,
        &session,
        session.borrow().viewport.frame,
        vec![0],
        vec![cell.clone(), cell],
        None,
    );
    manager.poll();
    assert!(session.borrow().scroll_pending());
    assert_eq!(session.borrow().snapshot().cells[0].text(), "x");
    assert!(manager.next_scroll().is_none());

    let mut frame = session.borrow().viewport.frame;
    frame.scrollback.offset = 20;
    reply(121, Some(frame));
    assert!(manager.poll()[&1].changed);
    assert!(!session.borrow().scroll_pending());
    assert_eq!(session.borrow().snapshot().frame.scrollback.offset, 20);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn a_coalesced_scroll_target_releases_at_the_cadence_without_responses() {
    let (mut manager, _sender, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;

    session.borrow_mut().scroll_to(10);
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":10,"serial":1})
    );
    // While the scroll is outstanding, further wheel events only coalesce
    // into the latest target...
    manager.wake.drain();
    session.borrow_mut().scroll_to(20);
    session.borrow_mut().scroll_to(30);
    {
        use std::os::fd::AsRawFd;
        let mut event = libc::pollfd {
            fd: manager.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut event, 1, 0) },
            1,
            "A deferred target must wake the interface to schedule its deadline"
        );
    }
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        manager.next_scroll(),
        Some(session.borrow().viewport.scroll_sent_at.get() + SCROLL_CADENCE)
    );
    // ...and release after the cadence expires with no response in sight,
    // so a slow viewport can never freeze scrolling on repeated targets.
    session
        .borrow_mut()
        .viewport
        .scroll_sent_at
        .set(Instant::now() - SCROLL_CADENCE);
    manager.poll();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":30,"serial":3})
    );
    assert!(session.borrow().scroll_pending());
    assert!(manager.next_scroll().is_none());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn queue_backpressure_suspends_expired_scroll_deadlines_until_progress() {
    let (mut manager, _sender, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        socket: socket.into(),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
    });
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;
    session.borrow_mut().scroll_to(10);
    session.borrow_mut().scroll_to(20);
    session
        .borrow()
        .viewport
        .scroll_sent_at
        .set(Instant::now() - SCROLL_CADENCE);
    manager.poll();
    assert!(session.borrow().scroll_pending());
    assert!(
        manager.next_scroll().is_none(),
        "A full queue must not arm an expired timer"
    );
    assert_eq!(wire(receiver.try_recv().unwrap())["row"], 10);
    // The writer wakes the manager after releasing a queue slot.
    manager.wake.notify();
    manager.poll();
    assert_eq!(wire(receiver.try_recv().unwrap())["row"], 20);
    assert!(manager.next_scroll().is_none());
}

#[test]
fn commands_follow_the_latest_scroll_target_under_backpressure() {
    let (mut manager, _sender, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(2);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;
    let mut meta = session.borrow().viewport.meta.clone();
    meta.id = 2;
    manager.metadata.insert(2, meta);
    let other = manager.attach(2, &Theme::adwaita(true)).unwrap();

    // Fill the command queue and coalesce wheel events into the latest
    // target. Other panes and queued commands keep their order.
    other.borrow_mut().scroll_to(30);
    session
        .borrow()
        .key("a", layout(b'a' as u32), 0, KeyAction::Press, "a")
        .unwrap();
    session.borrow_mut().scroll_to(10);
    session.borrow_mut().scroll_to(20);
    assert!(session.borrow().scroll_pending());
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":2,"row":30,"serial":1})
    );
    assert_eq!(wire(receiver.try_recv().unwrap())["method"], "key");
    // The next command releases the coalesced target ahead of itself so
    // viewport-dependent input never overtakes older scrolls.
    session
        .borrow()
        .key("b", layout(b'b' as u32), 0, KeyAction::Press, "b")
        .unwrap();
    let commands: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
        .take(2)
        .map(wire)
        .collect();
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands[0],
        json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
    );
    assert_eq!(commands[1]["method"], "key");
    assert!(receiver.try_recv().is_err());
}

#[test]
fn scroll_requests_coalesce_under_backpressure_and_ignore_stale_acknowledgements() {
    let (mut manager, sender, session) = manager();
    let (socket, _) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);

    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    connection
        .send(request(json!({"method":"input","bytes":[]})))
        .unwrap();
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;
    session.borrow_mut().scroll_to(10);
    session.borrow_mut().scroll_to(20);
    assert!(session.borrow().scroll_pending());
    assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
    manager.poll();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
    );
    for serial in [1, 2] {
        let mut frame = session.borrow().viewport.frame;
        frame.scrollback = gmux_protocol::Scrollback {
            total: 100,
            offset: serial * 10,
            len: 2,
        };
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: session.borrow().viewport.meta.clone(),
                    frame: Some(frame),
                    scroll_serial: serial,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().scroll_pending(), serial < 2);
        assert_eq!(session.borrow().snapshot().revision, 0);
        assert_eq!(
            session.borrow().snapshot().frame.scrollback.offset,
            serial * 10
        );
    }
    manager.poll();
    assert!(receiver.try_recv().is_err());
}

#[test]
fn viewport_subscription_retries_after_command_backpressure() {
    let (mut manager, _sender, _session) = manager();
    let (socket, _) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);

    manager.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager
        .connection
        .send(request(json!({"method":"input","bytes":[]})))
        .unwrap();
    assert!(manager.set_visible(HashSet::from([1])).is_err());
    assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
    assert!(manager.poll().is_empty());
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"subscribe","panes":[1]})
    );
    assert!(manager.poll().is_empty());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn acknowledging_an_alert_ignores_old_packets_and_keeps_new_alerts() {
    let (mut manager, sender, session) = manager();
    session.borrow_mut().viewport.meta.unread_sequence = 7;
    assert!(session.borrow().unread());
    manager.mark_read(1);
    assert!(!session.borrow().unread());
    for (sequence, expected) in [(7, false), (0, false), (8, true)] {
        let mut meta = session.borrow().viewport.meta.clone();
        meta.unread_sequence = sequence;
        sender
            .send(Ok(Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
        assert_eq!(manager.poll()[&1].unread, Some(expected));
        assert_eq!(session.borrow().unread(), expected);
    }
}

#[test]
fn read_acknowledgements_retry_after_command_backpressure() {
    let (mut manager, _sender, session) = manager();
    let (socket, _) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);

    manager.connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager
        .connection
        .send(request(json!({"method":"input","bytes":[]})))
        .unwrap();
    session.borrow_mut().viewport.meta.unread_sequence = 7;
    manager.mark_read(1);
    assert!(!session.borrow().unread());
    assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
    manager.poll();
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"mark-read","pane":1,"through":7})
    );
    manager.poll();
    assert!(receiver.try_recv().is_err());
}

#[test]
fn scroll_effects_commit_only_after_admission_and_retry_at_the_display_cadence() {
    let mut view = viewport();
    let now = Instant::now();
    let first = view.scroll_to(10, now).scroll.unwrap();
    view.scroll_enqueued(first, now, EnqueueOutcome::Rejected);
    assert_eq!(view.scroll_sent.get(), 0);
    assert!(view.scroll_release().is_none());
    // More local input coalesces even while admission is blocked.
    let latest = view.scroll_to(20, now).scroll.unwrap();
    assert_eq!(latest, ScrollCommand { row: 20, serial: 2 });
    view.scroll_enqueued(latest, now, EnqueueOutcome::Accepted);
    assert_eq!(view.scroll_sent.get(), 2);
    assert!(view.scroll_to(30, now).scroll.is_none());
    let release = now + SCROLL_CADENCE;
    assert_eq!(view.scroll_release(), Some(release));
    assert!(
        view.retry_scroll(release - Duration::from_nanos(1))
            .is_none()
    );
    let retry = view.retry_scroll(release).unwrap();
    assert_eq!(retry, ScrollCommand { row: 30, serial: 3 });
    view.scroll_enqueued(retry, release, EnqueueOutcome::Rejected);
    assert!(view.scroll_release().is_none());
    assert_eq!(view.forced_scroll(), Some(retry));
    let retry = view.retry_scroll(release).unwrap();
    view.scroll_enqueued(retry, release, EnqueueOutcome::Accepted);
    // Metadata-only packets cannot acknowledge scrolling. A late frame cannot
    // settle the newer target, even after it has entered the outgoing queue.
    for (frame, serial, pending) in [
        (None, 3, true),
        (Some(view.frame), 2, true),
        (Some(view.frame), 3, false),
    ] {
        let changes = view.apply(
            PaneUpdate {
                header: PaneHeader {
                    meta: view.meta.clone(),
                    frame,
                    scroll_serial: serial,
                    ..PaneHeader::default()
                },
                cells: vec![],
            },
            false,
            release,
        );
        assert!(changes.scroll.is_none());
        assert_eq!(view.scroll_pending(), pending);
    }
}

#[test]
fn a_forced_scroll_can_succeed_before_selection_admission_fails() {
    use gmux_protocol::transport::SelectionMode;
    let (mut manager, sender, session) = manager();
    let (socket, _peer) = UnixStream::pair().unwrap();
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    let connection = Rc::new(Connection {
        clipboard_epochs: Default::default(),
        commands,
        task: None,
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        socket: socket.into(),
    });
    manager.connection = connection.clone();
    session.borrow_mut().connection = connection;
    session.borrow_mut().viewport.scroll_cadence = Duration::from_secs(600);
    session.borrow_mut().scroll_to(10);
    assert_eq!(wire(receiver.try_recv().unwrap())["serial"], 1);
    session.borrow_mut().scroll_to(20);
    // Forcing the deferred target fills the final queue slot. The selection
    // command fails and must neither consume its serial nor hide old selection.
    assert!(
        session
            .borrow()
            .select((0, 0), (1, 0), SelectionMode::Word)
            .is_err()
    );
    assert_eq!(
        wire(receiver.try_recv().unwrap()),
        json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
    );
    assert_eq!(session.borrow().viewport.selection_serial.get(), 0);
    assert!(!session.borrow().selection_pending());
    assert!(session.borrow().viewport.forced_scroll().is_none());
    session
        .borrow()
        .select((0, 0), (1, 0), SelectionMode::Word)
        .unwrap();
    assert_eq!(wire(receiver.try_recv().unwrap())["serial"], 1);
    assert!(session.borrow().clear_selection().is_ok());
    assert_eq!(wire(receiver.try_recv().unwrap())["serial"], 2);
    for (serial, selection) in [(1, Some(Selection::linear(0, 1))), (2, None)] {
        sender
            .send(Ok(Packet::Pane(PaneUpdate {
                header: PaneHeader {
                    meta: session.borrow().viewport.meta.clone(),
                    selection_serial: serial,
                    selection,
                    ..PaneHeader::default()
                },
                cells: vec![],
            })))
            .unwrap();
        manager.poll();
        assert_eq!(session.borrow().selection_pending(), serial == 1);
        assert_eq!(session.borrow().selection(), None);
    }
    // Closed queues follow the same failed-admission rule and keep their error
    // available to the caller. They must not consume a selection serial either.
    drop(receiver);
    assert!(session.borrow().clear_selection().is_err());
    assert_eq!(session.borrow().viewport.selection_serial.get(), 2);
}
