use super::*;
use gmux_protocol::transport::{Header, RECORD_PREFIX_BYTES, Record};
use tokio::io::AsyncWriteExt;

fn record(sequence: u64, text: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    Packet {
        header: Header {
            scroll_serial: sequence,
            notifications: vec!["x".repeat(text)],
            ..Default::default()
        },
        cells: vec![],
    }
    .encode_into(&mut bytes)
    .unwrap();
    bytes
}

#[test]
fn partial_bulk_records_cannot_block_small_streams_and_teardown_releases_credit() {
    let bulk = Arc::new(tokio::sync::Semaphore::new(incoming::MAX_BYTES));
    let small = Arc::new(tokio::sync::Semaphore::new(128 * 1024));
    let mut readers = Vec::new();
    let mut receivers = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..4 {
        let (socket, peer) = tokio::io::duplex(1024);
        let (sender, receiver) =
            incoming::channel_with_pools(incoming::MAX_BYTES, 4, bulk.clone(), small.clone());
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        readers.push(runtime::spawn(async move {
            read_packets(socket, &sender, &wake, Duration::from_secs(2)).await
        }));
        receivers.push(receiver);
        peers.push(peer);
    }
    // A hostile, but bounded, declaration consumes all bulk credit while waiting
    // for its JSON. Another bulk reader waits for credit. Neither can enter the
    // small pool's FIFO queue.
    let mut prefix = record(0, 0)[..RECORD_PREFIX_BYTES].to_vec();
    prefix[12..16].copy_from_slice(&(incoming::MAX_BYTES as u32).to_le_bytes());
    runtime::block_on(async {
        peers[0].write_all(&prefix).await.unwrap();
        peers[1].write_all(&prefix).await.unwrap();
        // Also keep a small record incomplete on another connection.
        peers[2]
            .write_all(&record(0, 0)[..RECORD_PREFIX_BYTES])
            .await
            .unwrap();
    });
    let start = Instant::now();
    while bulk.available_permits() != 0 {
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
    runtime::block_on(async {
        for sequence in 1..=3 {
            peers[3].write_all(&record(sequence, 0)).await.unwrap();
        }
    });
    let start = Instant::now();
    for sequence in 1..=3 {
        loop {
            if let Ok(packet) = receivers[3].try_recv() {
                assert_eq!(packet.unwrap().header.scroll_serial, sequence);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "small update blocked behind another stream"
            );
            std::thread::yield_now();
        }
    }
    for reader in &readers {
        reader.abort();
    }
    for reader in readers {
        let _ = runtime::block_on(reader);
    }
    drop(receivers);
    assert_eq!(bulk.available_permits(), incoming::MAX_BYTES);
    assert_eq!(small.available_permits(), 128 * 1024);
}

#[test]
fn expired_partial_record_returns_its_decode_credit() {
    let shared = Arc::new(tokio::sync::Semaphore::new(incoming::MAX_BYTES));
    let (sender, receiver) = incoming::channel_with_budget(incoming::MAX_BYTES, 4, shared.clone());
    let (socket, mut peer) = tokio::io::duplex(1024);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let bytes = record(1, 0);
    let reader = runtime::spawn(async move {
        read_packets(socket, &sender, &wake, Duration::from_millis(30)).await
    });
    runtime::block_on(async {
        peer.write_all(&bytes[..RECORD_PREFIX_BYTES]).await.unwrap();
    });
    assert!(
        runtime::block_on(reader)
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    assert_eq!(shared.available_permits(), incoming::MAX_BYTES);
    drop(receiver);
}

#[test]
fn canceled_blocking_decode_keeps_credit_until_worker_and_result_are_dropped() {
    let shared = Arc::new(tokio::sync::Semaphore::new(incoming::MAX_BYTES));
    let (sender, receiver) = incoming::channel_with_budget(incoming::MAX_BYTES, 4, shared.clone());
    let bytes = record(1, 128 * 1024);
    let framing = Record::decode(bytes[..RECORD_PREFIX_BYTES].try_into().unwrap()).unwrap();
    let reservation = runtime::block_on(sender.reserve_decode(framing.decode_bytes())).unwrap();
    let (started, ready) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let (ended, done) = std::sync::mpsc::channel();
    let task = runtime::spawn(async move {
        runtime::blocking(move || {
            let header = framing.parse_header(&bytes[RECORD_PREFIX_BYTES..]).unwrap();
            started.send(()).unwrap();
            finished.recv().unwrap();
            // Returned values still hold credit if the async caller was aborted.
            ended.send(()).unwrap();
            (header, reservation)
        })
        .await
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    task.abort();
    let _ = runtime::block_on(task);
    assert_eq!(
        shared.available_permits(),
        incoming::MAX_BYTES - framing.decode_bytes()
    );
    // Executor work proceeds even while that worker is deliberately stalled.
    runtime::block_on(runtime::spawn(async {})).unwrap();
    finish.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(2)).unwrap();
    let start = Instant::now();
    while shared.available_permits() != incoming::MAX_BYTES {
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
    drop(receiver);
}

#[test]
fn a_ready_large_cell_decode_yields_before_delaying_other_io() {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    let packet = Packet {
        header: Header {
            frame: Some(gmux_protocol::Frame {
                cols: 512,
                rows: 256,
                ..Default::default()
            }),
            rows: (0..256).collect(),
            ..Default::default()
        },
        cells: vec![Cell::default(); 512 * 256],
    };
    let mut bytes = Vec::new();
    packet.encode_into(&mut bytes).unwrap();
    let mut large = bytes.as_slice();
    let mut decoding = pin!(Packet::read_message_async(&mut large));
    let mut context = Context::from_waker(Waker::noop());
    // Even an always-ready reader exposing the entire record must yield.
    assert!(decoding.as_mut().poll(&mut context).is_pending());
    let small = record(42, 0);
    let mut small = small.as_slice();
    let mut other = pin!(Packet::read_message_async(&mut small));
    match other.as_mut().poll(&mut context) {
        Poll::Ready(Ok(Some(packet))) => assert_eq!(packet.header.scroll_serial, 42),
        _ => panic!("small record could not progress while the large decode yielded"),
    }
    let packet = runtime::block_on(decoding).unwrap().unwrap();
    assert_eq!(packet.cells.len(), 512 * 256);
}

#[test]
fn fragmented_large_clipboard_and_following_update_keep_order_and_release_credit() {
    let shared = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
    let (sender, receiver) = incoming::channel_with_budget(incoming::MAX_BYTES, 4, shared.clone());
    let text = format!("{}終🙂", "x".repeat(128 * 1024));
    let mut bytes = Vec::new();
    Packet {
        header: Header {
            copy: Some(gmux_protocol::clipboard::Write {
                destination: gmux_protocol::clipboard::Destination::Clipboard,
                content: Some(text.clone().into()),
            }),
            ..Default::default()
        },
        cells: vec![],
    }
    .encode_into(&mut bytes)
    .unwrap();
    bytes.extend_from_slice(&record(42, 0));
    let (socket, mut peer) = tokio::io::duplex(127);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let reader = runtime::spawn(async move {
        read_packets(socket, &sender, &wake, Duration::from_secs(2)).await
    });
    let writer = runtime::spawn(async move {
        peer.write_all(&bytes).await.unwrap();
    });
    runtime::block_on(writer).unwrap();
    assert!(runtime::block_on(reader).unwrap().is_err()); // Peer closed after both records.
    let (clipboard, held) = receiver.try_recv_reserved().unwrap();
    let clipboard = clipboard.unwrap().header.copy.unwrap().content.unwrap();
    assert_eq!(clipboard.0[0].data, text.as_bytes());
    assert_eq!(
        receiver.try_recv().unwrap().unwrap().header.scroll_serial,
        42
    );
    assert!(shared.available_permits() < 1024 * 1024);
    drop(held);
    assert_eq!(shared.available_permits(), 1024 * 1024);
}
