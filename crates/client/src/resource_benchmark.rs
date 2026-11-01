//! Repeatable retained-storage pressure probe. Run alone in release mode.
use crate::{incoming, runtime, scroll_cache::RowCache};
use gmux_protocol::{Cell, Frame, Scrollback};
use std::time::{Duration, Instant};

fn rss() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
#[ignore = "resource measurement; run alone with --release --ignored --nocapture"]
fn resource_pressure() {
    let before = rss();
    let mut caches = Vec::new();
    let frame = Frame {
        cols: 1000,
        rows: 20,
        scrollback: Scrollback {
            total: 2000,
            offset: 100,
            len: 20,
        },
        ..Frame::default()
    };
    let rows = vec![Cell::default(); 1000 * 100];
    for _ in 0..64 {
        let mut cache = RowCache::default();
        cache.configure(1, frame);
        cache.insert(0, &rows, &[0; 100]);
        caches.push(cache);
    }
    let cache_bytes: usize = caches.iter().map(RowCache::retained_bytes).sum();
    let mut queues = Vec::new();
    let mut workers = Vec::new();
    for stream in 0..8 {
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        queues.push(receiver);
        workers.push(runtime::spawn(async move {
            for sequence in 0..48 {
                sender
                    .send_async(Err(format!(
                        "{stream}:{sequence}:{}",
                        "x".repeat(1024 * 1024)
                    )))
                    .await
                    .unwrap();
            }
        }));
    }
    std::thread::sleep(Duration::from_secs(2));
    let queue_bytes: usize = queues.iter().map(incoming::Receiver::retained_bytes).sum();
    let peak_rss = rss();
    let start = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    runtime::spawn(async move {
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let control_us = start.elapsed().as_micros();
    let start = Instant::now();
    let mut cells = Vec::new();
    let mut flags = Vec::new();
    let mut hits = 0;
    for _ in 0..1000 {
        hits += usize::from(
            caches
                .last()
                .unwrap()
                .copy_view(80, 20, &mut cells, &mut flags),
        );
    }
    let scroll_us = start.elapsed().as_micros() / 1000;
    let mut received = [0; 8];
    let start = Instant::now();
    while received.iter().sum::<usize>() != 8 * 48 {
        for (stream, queue) in queues.iter().enumerate() {
            if let Ok(Err(text)) = queue.try_recv() {
                assert!(text.starts_with(&format!("{stream}:{}:", received[stream])));
                received[stream] += 1;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    for worker in workers {
        runtime::block_on(worker).unwrap();
    }
    drop(queues);
    drop(caches);
    println!(
        "{}",
        serde_json::json!({"streams":8,"views":64,"cache_bytes":cache_bytes,
        "queue_bytes":queue_bytes,"rss_before_kib":before,"rss_pressure_kib":peak_rss,
        "rss_after_kib":rss(),"control_us":control_us,"scroll_copy_us":scroll_us,"scroll_hits":hits,
        "drain_ms":start.elapsed().as_millis()})
    );
}
