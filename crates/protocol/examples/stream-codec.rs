//! Isolated codec probe. Run alone in release mode; buffers are reused after warmup.
use gmux_protocol::{
    Cell, Frame,
    transport::{Metadata, Packet, PaneHeader},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
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

// Linux's thread CPU clock excludes descheduling time. Other platforms still
// report elapsed timings and allocations. This probe adds no library dependency.
#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> Option<u64> {
    use std::os::raw::{c_int, c_long};
    #[repr(C)]
    struct Timespec {
        seconds: c_long,
        nanoseconds: c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: c_int, time: *mut Timespec) -> c_int;
    }
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // CLOCK_THREAD_CPUTIME_ID is 3 in the Linux ABI; Timespec matches libc.
    (unsafe { clock_gettime(3, &mut time) } == 0)
        .then_some(time.seconds as u64 * 1_000_000_000 + time.nanoseconds as u64)
}
#[cfg(not(target_os = "linux"))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

fn measure(name: &str, packet: Packet, iterations: usize) -> anyhow::Result<()> {
    if std::env::args().nth(1).is_some_and(|case| case != name) {
        return Ok(());
    }
    let mut wire = Vec::new();
    packet.encode_into(&mut wire)?;
    let retained = Packet::read(&mut wire.as_slice())?.memory_size();
    let mut stats = Vec::new();
    for encode in [true, false] {
        let mut times = Vec::with_capacity(iterations);
        let cpu_start = thread_cpu_ns();
        ALLOCATIONS.store(0, Relaxed);
        BYTES.store(0, Relaxed);
        for _ in 0..iterations {
            let began = Instant::now();
            if encode {
                wire.clear();
                black_box(&packet).encode_into(black_box(&mut wire))?;
            } else {
                drop(black_box(Packet::read(&mut black_box(wire.as_slice()))?));
            }
            times.push(began.elapsed().as_nanos());
        }
        let allocations = ALLOCATIONS.load(Relaxed) / iterations;
        let bytes = BYTES.load(Relaxed) / iterations;
        let cpu_ns = thread_cpu_ns()
            .zip(cpu_start)
            .map(|(end, start)| (end - start) / iterations as u64);
        times.sort_unstable();
        stats.push(serde_json::json!({"mean_thread_cpu_ns":cpu_ns,"operation":if encode {"encode"} else {"decode"},"median_ns":times[iterations/2],"p95_ns":times[iterations*95/100],"allocations":allocations,"allocated_bytes":bytes}));
    }
    println!(
        "{}",
        serde_json::json!({"case":name,"wire_bytes":wire.len(),"source_retained":packet.memory_size(),"decoded_retained":retained,"measurements":stats})
    );
    Ok(())
}
fn main() -> anyhow::Result<()> {
    let mut cell = Cell::default();
    cell.set_text("x").unwrap();
    for (name, cols, rows) in [
        ("metadata", 0, 0),
        ("pane-row", 80, 1),
        ("pane-full", 80, 24),
    ] {
        measure(
            name,
            Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    meta: Metadata {
                        id: 1,
                        title: "shell".into(),
                        directory: "/tmp".into(),
                        size: (80, 24),
                        ..Default::default()
                    },
                    frame: (cols != 0).then_some(Frame {
                        cols,
                        rows: 24,
                        ..Default::default()
                    }),
                    rows: (0..rows).collect(),
                    generation: 1,
                    ..Default::default()
                },
                cells: vec![cell.clone(); cols as usize * rows as usize],
            }),
            10000,
        )?;
    }
    for tabs in [0, 1, 8] {
        let mut state: gmux_protocol::control::CommandReply = serde_json::from_value(
            serde_json::json!({
                "daemon":"01010101010101010101010101010101", "home_directory":"/home/test", "machine_kind":"server", "hostname":"probe", "revision":1, "result":{"ok":true}, "execution":{"tabs":[]}, "metadata":[]
            }),
        )?;
        for tab in 0..tabs {
            let pane = 2 * tab + 2;
            state
                .execution
                .tabs
                .push(gmux_protocol::model::ExecutionTab {
                    id: pane - 1,
                    created_at: 1,
                    name: "shell".into(),
                    custom_name: false,
                    layout: gmux_protocol::model::PaneTree::Pane(pane),
                    directories: [(pane, "/tmp".into())].into(),
                });
            state.metadata.push(Metadata {
                id: pane,
                directory: "/tmp".into(),
                title: "shell".into(),
                size: (80, 24),
                ..Default::default()
            });
        }
        measure(
            &format!("execution/{tabs}"),
            Packet::Execution(state),
            10000,
        )?;
    }
    let read = gmux_protocol::clipboard::ReadEvent::Request {
        request: gmux_protocol::clipboard::ReadRequest {
            id: 3,
            destination: gmux_protocol::clipboard::Destination::Clipboard,
            mimes: vec!["text/plain".into()],
            list: false,
            name: "probe".into(),
            granted: false,
            epoch: None,
        },
    };
    let git = gmux_protocol::transport::GitMetadata {
        id: 1,
        directory: "/tmp".into(),
        branch: "main".into(),
    };
    measure(
        "clipboard-read",
        Packet::ClipboardRead(gmux_protocol::transport::ClipboardRead {
            pane: 1,
            event: read,
        }),
        10000,
    )?;
    measure("git", Packet::Git(git), 10000)?;
    measure(
        "clipboard-clear",
        Packet::Clipboard(gmux_protocol::clipboard::Write {
            destination: gmux_protocol::clipboard::Destination::Clipboard,
            content: None,
        }),
        10000,
    )?;
    for size in [0, 1024, 4 * 1024 * 1024] {
        measure(
            &format!("clipboard/{size}"),
            Packet::Clipboard(gmux_protocol::clipboard::Write {
                destination: gmux_protocol::clipboard::Destination::Clipboard,
                content: Some('x'.to_string().repeat(size).into()),
            }),
            if size > 1024 { 200 } else { 10000 },
        )?;
    }
    Ok(())
}
