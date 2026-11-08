//! Manual release benchmark. Rust allocation sizes include collection buckets,
//! retained capacities and overflow storage, but exclude native Ghostty storage.
//! Run alone with --ignored --nocapture --test-threads=1.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static RETAINED: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            RETAINED.fetch_add(layout.size(), Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        RETAINED.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(pointer, layout, size) };
        if !new.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            RETAINED.fetch_add(size, Relaxed);
            RETAINED.fetch_sub(layout.size(), Relaxed);
        }
        new
    }
}

#[test]
#[ignore = "manual release benchmark"]
fn frontend_pane_workload() {
    let visible: u64 = std::env::var("GMUX_BENCH_VISIBLE").map_or(4, |n| n.parse().unwrap());
    let hidden = visible;
    let panes: u64 = std::env::var("GMUX_BENCH_PANES").map_or(32, |n| n.parse().unwrap());
    let scheduled = std::env::var_os("GMUX_BENCH_SCHEDULED").is_some();
    let mut daemon = Daemon::empty_for_test();
    for id in 1..=panes {
        let mut session = TerminalSession::spawn(
            id,
            id,
            PathBuf::from("/tmp"),
            Default::default(),
            &gmux_protocol::theme::Theme::adwaita(true),
            None,
            Default::default(),
        )
        .unwrap();
        session.feed(b"\x1b[2J\x1b[Hbenchmark row\r\nsecond row\r\n");
        daemon.sessions.insert(id, session);
    }
    let mut readers = Vec::new();
    let before = RETAINED.load(Relaxed);
    for id in 1..=8 {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        readers.push(std::thread::spawn(move || {
            let mut bytes = [0; 65536];
            while peer.read(&mut bytes).is_ok_and(|n| n != 0) {}
        }));
        let mut frontend = Frontend::new(socket, &daemon.output_budget);
        frontend.attached = true;
        // Visit eight panes, then retain four hidden views and 24 metadata-only panes.
        for panes in [(1..=visible + hidden).collect(), (1..=visible).collect()] {
            daemon
                .input(
                    id,
                    &mut frontend,
                    StreamRequest::pane(1, StreamCommand::Subscribe { panes }),
                )
                .unwrap();
            daemon
                .update_frontend(&mut frontend, true, &mut HashMap::new())
                .unwrap();
        }
        daemon.frontends.insert(id, frontend);
    }
    let output_bytes: usize = daemon
        .frontends
        .values()
        .map(Frontend::retained_output)
        .sum();
    let retained = (RETAINED.load(Relaxed) - before).saturating_sub(output_bytes);
    let mut notifications = HashMap::new();
    let mut current = daemon.frontends.remove(&1).unwrap();
    let allocations_before = ALLOCATIONS.load(Relaxed);
    let started = thread_cpu();
    for i in 0..1000 {
        // Alternate one row without terminal input or PTY scheduling in the timed region.
        let session = daemon.sessions.get_mut(&1).unwrap();
        session.feed(if i % 2 == 0 {
            b"\x1b[HAAAA"
        } else {
            b"\x1b[HBBBB"
        });
        if visible != 0 {
            daemon
                .input(
                    1,
                    &mut current,
                    StreamRequest::pane(
                        1,
                        StreamCommand::Scroll {
                            delta: 0,
                            bottom: false,
                        },
                    ),
                )
                .unwrap();
        }
        if scheduled {
            daemon.frontends.insert(1, current);
            for frontend in daemon.frontends.values_mut() {
                frontend.next_frame = Instant::now();
            }
            daemon.changed.insert(1);
            let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), usize::MAX);
            daemon.updates_budgeted(false, &mut budget);
            current = daemon.frontends.remove(&1).unwrap();
        } else {
            daemon
                .update_frontend(&mut current, visible == 0, &mut notifications)
                .unwrap();
        }
    }
    let update_us = (thread_cpu() - started) / 1000.;
    let allocations = ALLOCATIONS.load(Relaxed) - allocations_before;
    daemon.frontends.insert(1, current);
    // Keep PTYs/native terminals out of the pane-owner teardown measurement.
    let sessions = std::mem::take(&mut daemon.sessions);
    let started = Instant::now();
    for id in 1..=panes {
        daemon.close(id);
    }
    let teardown_us = started.elapsed().as_micros();
    let output_after_close: usize = daemon
        .frontends
        .values()
        .map(Frontend::retained_output)
        .sum();
    let after_close = RETAINED
        .load(Relaxed)
        .saturating_sub(before + output_after_close);
    drop(daemon);
    drop(sessions);
    for reader in readers {
        reader.join().unwrap();
    }
    println!(
        "{}",
        json!({"panes":panes,"scheduled":scheduled,"allocations_per_update":allocations as f64 / 1000.,"frontends":8,"visible_per_frontend":visible,"hidden_views_per_frontend":hidden,"retained_rust_bytes_without_output":retained,"after_close_rust_bytes_without_output":after_close,"update_cpu_us":update_us,"teardown_us":teardown_us})
    );
}

fn thread_cpu() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as f64 * 1e6 + time.tv_nsec as f64 / 1e3
}
