//! Run alone in release mode, measuring the real socket reader and shared executor.
use super::*;
use gmux_protocol::transport::PaneHeader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn rss() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
fn cpu() -> u128 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec as u128 + usage.ru_stime.tv_sec as u128) * 1_000_000
        + usage.ru_utime.tv_usec as u128
        + usage.ru_stime.tv_usec as u128
}
fn packet(text: String) -> Vec<u8> {
    let mut bytes = Vec::new();
    Packet::Pane(gmux_protocol::transport::PaneUpdate {
        header: PaneHeader {
            notifications: vec![text],
            ..Default::default()
        },
        cells: vec![],
    })
    .encode_into(&mut bytes)
    .unwrap();
    bytes
}
#[test]
#[ignore = "measurement; run alone in release mode"]
fn packet_admission_pressure() {
    measure_packet_admission("json", || packet("x".repeat(8 * 1024 * 1024)));
}

#[test]
#[ignore = "measurement; run alone in release mode"]
fn clipboard_decode_pressure() {
    measure_packet_admission("clipboard", || {
        let mut bytes = Vec::new();
        Packet::Clipboard(gmux_protocol::clipboard::Write {
            destination: gmux_protocol::clipboard::Destination::Clipboard,
            content: Some("x".repeat(4 * 1024 * 1024).into()),
        })
        .encode_into(&mut bytes)
        .unwrap();
        bytes
    });
}

fn measure_packet_admission(kind: &str, make_large: impl FnOnce() -> Vec<u8>) {
    let before = rss();
    let cpu_before = cpu();
    let small = packet("interactive".into());
    let large = make_large();
    let record_bytes = large.len();
    let shared = Arc::new(tokio::sync::Semaphore::new(incoming::PROCESS_BYTES));
    let mut tasks = Vec::new();
    let mut peers = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..3 {
        let (socket, peer) = tokio::io::duplex(64 * 1024);
        let (sender, receiver) =
            incoming::channel_with_budget(incoming::MAX_BYTES, 64, shared.clone());
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        tasks.push(runtime::spawn(async move {
            read_packets(socket, &sender, &wake, Duration::from_secs(5)).await
        }));
        peers.push(peer);
        receivers.push(receiver);
    }
    runtime::block_on(async {
        // Send all framing but no JSON.
        let header_offset = transport::RECORD_PREFIX_BYTES;
        for peer in &mut peers[..2] {
            peer.write_all(&small[..header_offset]).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        peers[2].write_all(&small).await.unwrap();
    });
    let start = Instant::now();
    while receivers[2].try_recv().is_err() && start.elapsed() < Duration::from_millis(250) {
        std::thread::sleep(Duration::from_micros(100));
    }
    let partial_us = start.elapsed().as_micros();
    let partial_reserved = incoming::PROCESS_BYTES - shared.available_permits();
    for task in &tasks {
        task.abort();
    }
    for task in tasks.drain(..) {
        let _ = runtime::block_on(task);
    }
    drop(peers);
    drop(receivers);
    let (socket, mut peer) = tokio::io::duplex(64 * 1024);
    let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let reader = runtime::spawn(async move {
        read_packets(socket, &sender, &wake, Duration::from_secs(10)).await
    });
    let writer = runtime::spawn(async move {
        for _ in 0..32 {
            peer.write_all(&large).await.unwrap();
        }
    });
    let (interactive, peer) = tokio::io::duplex(64 * 1024);
    let (read, mut write) = tokio::io::split(interactive);
    let (mut peer_read, mut peer_write) = tokio::io::split(peer);
    let (sender, updates) = incoming::channel(incoming::MAX_BYTES, 64);
    let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
    let interactive_reader =
        runtime::spawn(
            async move { read_packets(read, &sender, &wake, Duration::from_secs(10)).await },
        );
    let (input, mut commands) = tokio::sync::mpsc::unbounded_channel::<u8>();
    let input_writer = runtime::spawn(async move {
        while let Some(byte) = commands.recv().await {
            write.write_all(&[byte]).await.unwrap();
        }
    });
    let start = Instant::now();
    let decode_cpu_before = cpu();
    let mut gaps = Vec::new();
    let mut inputs = Vec::new();
    let mut update_times = Vec::new();
    let mut peak_rss = rss();
    let mut seen = 0;
    while seen < 32 {
        let tick = Instant::now();
        runtime::block_on(runtime::spawn(async {})).unwrap();
        gaps.push(tick.elapsed().as_micros());
        let tick = Instant::now();
        input.send(1).unwrap();
        runtime::block_on(async {
            peer_read.read_exact(&mut [0; 1]).await.unwrap();
        });
        inputs.push(tick.elapsed().as_micros());
        let tick = Instant::now();
        runtime::block_on(async {
            peer_write.write_all(&small).await.unwrap();
        });
        while updates.try_recv().is_err() {
            assert!(tick.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_micros(50));
        }
        update_times.push(tick.elapsed().as_micros());
        if receiver.try_recv().is_ok() {
            seen += 1;
        }
        peak_rss = peak_rss.max(rss());
        assert!(start.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_micros(100));
    }
    let decode_us = start.elapsed().as_micros();
    let decode_cpu_us = cpu() - decode_cpu_before;
    runtime::block_on(writer).unwrap();
    let _ = runtime::block_on(reader);
    drop(input);
    runtime::block_on(input_writer).unwrap();
    interactive_reader.abort();
    let _ = runtime::block_on(interactive_reader);
    for values in [&mut gaps, &mut inputs, &mut update_times] {
        values.sort_unstable();
    }
    println!(
        "{}",
        serde_json::json!({"kind":kind,"record_bytes":record_bytes,"partial_update_us":partial_us,"partial_reserved_bytes":partial_reserved,
        "decode_wall_us":decode_us,"decode_cpu_us":decode_cpu_us,
        "executor_p99_us":gaps[gaps.len()*99/100],"executor_max_us":gaps.last(),
        "input_p99_us":inputs[inputs.len()*99/100],"input_max_us":inputs.last(),
        "update_p99_us":update_times[update_times.len()*99/100],"update_max_us":update_times.last(),
        "rss_before_kib":before,"rss_pressure_kib":peak_rss,"rss_after_kib":rss(),"samples":gaps.len(),"cpu_us":cpu()-cpu_before})
    );
}
