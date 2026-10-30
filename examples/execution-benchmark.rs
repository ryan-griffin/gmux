//! Measure snapshot construction, packet storage, and control reply decoding without PTYs or GTK.
// Compile the daemon's private record directly to measure its production projection.
#[path = "../src/persistence/execution.rs"]
mod execution;
use execution::SavedExecution;
use gmux_protocol::{
    control::{CommandOutput, CommandReply, ExecutionRequest},
    model::{DaemonId, ExecutionTab, MachineKind, PaneTree},
    transport::{Header, Packet},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashMap,
    hint::black_box,
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

fn tree(first: u64) -> PaneTree {
    PaneTree::Split {
        vertical: true,
        first: Box::new(PaneTree::Pane(first)),
        second: Box::new(PaneTree::Split {
            vertical: false,
            first: Box::new(PaneTree::Pane(first + 1)),
            second: Box::new(PaneTree::Split {
                vertical: true,
                first: Box::new(PaneTree::Pane(first + 2)),
                second: Box::new(PaneTree::Pane(first + 3)),
            }),
        }),
    }
}

fn measure_decode(bytes: &[u8], typed: bool) -> anyhow::Result<serde_json::Value> {
    let iterations = 2_000;
    let mut timings = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    for _ in 0..iterations {
        let began = Instant::now();
        if typed {
            drop(black_box(
                ExecutionRequest::State.decode_reply(black_box(bytes))?,
            ));
        } else {
            let value: serde_json::Value = serde_json::from_slice(black_box(bytes))?;
            gmux_protocol::network::check_error(&value)?;
            drop(black_box(serde_json::from_value::<CommandReply>(value)?));
        }
        timings.push(began.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Relaxed) / iterations;
    let allocated_bytes = BYTES.load(Relaxed) / iterations;
    timings.sort_unstable();
    Ok(serde_json::json!({
        "allocations": allocations, "allocated_bytes": allocated_bytes,
        "median_ns": timings[iterations / 2], "p95_ns": timings[iterations * 95 / 100],
    }))
}

fn run(tabs: u64, checkpoints: bool) -> anyhow::Result<serde_json::Value> {
    let saved = SavedExecution {
        last_id: 10_000,
        tabs: (0..tabs)
            .map(|index| {
                let id = index * 5 + 1;
                ExecutionTab {
                    id,
                    created_at: index + 1,
                    name: format!("Terminal {index}"),
                    custom_name: false,
                    layout: tree(id + 1),
                    directories: (id + 1..id + 5)
                        .map(|pane| (pane, "/srv/project/src".into()))
                        .collect(),
                }
            })
            .collect(),
        scrollback: if checkpoints {
            (0..tabs)
                .flat_map(|index| index * 5 + 2..index * 5 + 6)
                .take(128)
                .map(|pane| {
                    (
                        pane,
                        format!("{pane}-0123456789abcdef0123456789abcdef.snapshot"),
                    )
                })
                .collect()
        } else {
            HashMap::new()
        },
    };
    let iterations = 10_000;
    drop(black_box(saved.snapshot()));
    let mut timings = Vec::with_capacity(iterations);
    ALLOCATIONS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    for _ in 0..iterations {
        let began = Instant::now();
        drop(black_box(black_box(&saved).snapshot()));
        timings.push(began.elapsed().as_nanos());
    }
    let allocations = ALLOCATIONS.load(Relaxed) / iterations;
    let allocated_bytes = BYTES.load(Relaxed) / iterations;
    timings.sort_unstable();
    let packet = Packet {
        header: Header {
            state: Some(CommandReply {
                daemon: DaemonId::from_bytes([1; 16]),
                home_directory: "/home/test".into(),
                machine_kind: MachineKind::Server,
                hostname: "benchmark".into(),
                revision: 1,
                result: CommandOutput::Done { ok: true }.into(),
                execution: saved.snapshot(),
                metadata: vec![],
            }),
            ..Header::default()
        },
        cells: vec![],
    };
    let mut wire = Vec::new();
    packet.encode_into(&mut wire)?;
    let decoded = Packet::read(&mut wire.as_slice())?;
    let reply_bytes = serde_json::to_vec(packet.header.state.as_ref().unwrap())?;
    let json_decode = measure_decode(&reply_bytes, false)?;
    let typed_decode = measure_decode(&reply_bytes, true)?;
    Ok(serde_json::json!({
        "tabs": tabs, "checkpoint_references": saved.scrollback.len(),
        "reply_decoding": {"json_value":json_decode,"typed":typed_decode},
        "allocations_per_snapshot": allocations, "allocated_bytes_per_snapshot": allocated_bytes,
        "median_ns": timings[iterations / 2], "p95_ns": timings[iterations * 95 / 100],
        "wire_bytes": wire.len(), "retained_source_bytes": packet.memory_size(),
        "retained_decoded_bytes": decoded.memory_size(),
    }))
}

fn main() -> anyhow::Result<()> {
    let results = [0, 1, 8, 32]
        .into_iter()
        .flat_map(|tabs| [false, true].map(move |checkpoints| (tabs, checkpoints)))
        .map(|(tabs, checkpoints)| run(tabs, checkpoints))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for pair in results.as_chunks::<2>().0 {
        for field in [
            "allocations_per_snapshot",
            "allocated_bytes_per_snapshot",
            "wire_bytes",
            "retained_source_bytes",
            "retained_decoded_bytes",
        ] {
            assert_eq!(
                pair[0][field], pair[1][field],
                "Checkpoint metadata affected {field}"
            );
        }
    }
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
