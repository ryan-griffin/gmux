//! Measure binary clipboard transport without a desktop or terminal parser.
use gmux_client::{
    clipboard::{Destination, Offer, Representation, Write as ClipboardWrite},
    transport::{Header, Packet},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    time::Instant,
};

struct Counted;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Counted = Counted;
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size(), Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size(), Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size, Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

fn run(name: &str, contents: Vec<Representation>) -> anyhow::Result<serde_json::Value> {
    let payload: usize = contents.iter().map(|r| r.data.len()).sum();
    let packet = Packet {
        header: Header {
            copy: Some(ClipboardWrite {
                destination: Destination::Clipboard,
                content: Some(Offer(contents)),
            }),
            ..Header::default()
        },
        cells: vec![],
    };
    let mut wire = Vec::with_capacity(payload + 16384);
    let mut timings = Vec::with_capacity(100);
    // Warm up serde and the reusable transport buffer before measuring.
    packet.encode_into(&mut wire)?;
    drop(Packet::read(&mut wire.as_slice())?);
    ALLOCATIONS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    for _ in 0..100 {
        wire.clear();
        let began = Instant::now();
        packet.encode_into(&mut wire)?;
        let decoded = Packet::read(&mut wire.as_slice())?;
        assert_eq!(decoded.header.copy, packet.header.copy);
        timings.push(began.elapsed().as_secs_f64() * 1000.0);
    }
    let allocations = ALLOCATIONS.load(Relaxed);
    let allocated_bytes = BYTES.load(Relaxed);
    timings.sort_by(f64::total_cmp);
    let rejection = if payload >= 1024 * 1024 {
        let mut queue = vec![0; 32];
        queue.reserve(1024);
        let mut times = Vec::with_capacity(100);
        ALLOCATIONS.store(0, Relaxed);
        BYTES.store(0, Relaxed);
        for _ in 0..100 {
            let began = Instant::now();
            assert!(packet.encode_into_limit(&mut queue, 4096).is_err());
            times.push(began.elapsed().as_nanos());
            assert_eq!(queue.len(), 32);
        }
        let allocations = ALLOCATIONS.load(Relaxed);
        let bytes = BYTES.load(Relaxed);
        times.sort_unstable();
        Some(serde_json::json!({
            "median_ns":times[50], "max_ns":times[99],
            "allocations_per_request":allocations / 100,
            "allocated_bytes_per_request":bytes / 100,
        }))
    } else {
        None
    };
    Ok(serde_json::json!({
        "case":name, "payload_bytes":payload, "wire_bytes":wire.len(), "full_queue_rejection":rejection,
        "median_ms":timings[50], "max_ms":timings[99],
        "allocations_per_request":allocations / 100,
        "allocated_bytes_per_request":allocated_bytes / 100,
        "retained_source_bytes":packet.memory_size(), "wire_capacity":wire.capacity(),
    }))
}

fn main() -> anyhow::Result<()> {
    let mut results = vec![];
    for size in [0, 1024, 1024 * 1024, 4 * 1024 * 1024] {
        for mime in ["image/png", "text/plain"] {
            results.push(run(
                &format!("{mime}/{size}"),
                vec![Representation {
                    mime: mime.into(),
                    data: vec![if mime == "image/png" { 0 } else { 1 }; size],
                }],
            )?);
        }
    }
    results.push(run(
        "alternatives/4MiB",
        vec![
            Representation {
                mime: "image/png".into(),
                data: vec![0; 2 * 1024 * 1024],
            },
            Representation {
                mime: "text/html".into(),
                data: vec![0; 1024 * 1024],
            },
            Representation {
                mime: "text/plain".into(),
                data: vec![b'x'; 1024 * 1024],
            },
        ],
    )?);
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
