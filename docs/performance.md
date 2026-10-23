# Performance measurements

Benchmarks write local reports under `artifacts/`, which Git ignores. Keep source
revisions and matching frontend/daemon binaries with reports when comparing
builds. Results depend on the compositor, driver, font, and machine.

## Rectangular selection

The renderer resolves rectangle columns and wide-glyph edges once per visible
row. It retains the existing row caches and creates no selection object per
cell. A frontend's optional selection occupies 24 bytes on x86-64, the same as
the previous optional endpoint tuple. The native engine and each native view
use eight more bytes for shape and screen ownership.

Measure changing selection boundaries on unchanged text with the release
renderer. The fixture runs in the private Wayland session and checks rectangle
pixels against independently recolored cells:

```sh
cargo build --locked --release -p gmux-gtk --example renderer-benchmark --example renderer-check
python3 scripts/renderer-smoke-test.py --selection \
  --binary target/release/examples/renderer-benchmark \
  --check-binary target/release/examples/renderer-check \
  --output artifacts/selection-rendering
```

Reports separate scene preparation from rendering and texture download. Rust
allocation counts cover preparation only, excluding GTK, Pango, GSK, and native
terminal allocations. Changing a rectangle can invalidate every visible row;
changing a linear endpoint usually invalidates one row. Wide glyphs can make
adjacent rectangle endpoints cover the same cells. The benchmark checks the
exact number of rebuilt rows in each frame, including those unchanged wide
glyph boundaries.

`accessible-terminal-benchmark` reports rectangle range updates and retained
range capacity independently of GTK. It reuses one character range per selected
physical row, including blank cells and complete wide/combining glyphs.

The native benchmark measures selection updates and copies without GTK or
sockets. It counts newly created tracked references after drag setup:

```sh
ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
mkdir -p artifacts
cc -O2 -std=c11 -DGMUX_VERSION='"benchmark"' -Isrc -I"$ghostty_prefix/include" \
  scripts/selection-benchmark.c src/ghostty_bridge.c \
  -L"$ghostty_prefix/lib" -Wl,-rpath,"$ghostty_prefix/lib" \
  -Wl,--wrap=ghostty_terminal_grid_ref_track -lghostty-vt \
  -o artifacts/selection-benchmark
artifacts/selection-benchmark 512 200000
artifacts/selection-benchmark 10000 200000
```

Mode 0 is linear and mode 3 is rectangular. Timings use process CPU time;
copying formats the final selection, whose output differs by shape. Rectangle
copy uses Ghostty's formatter per physical row, then restores trailing unwritten
cell padding. Selection updates reuse the two tracked corners and do not scan
history. Compile with `GMUX_LINEAR_BASELINE` and the older bridge's headers and
source to compare its linear path.

## Application

```sh
python3 scripts/cargo_build.py --release gmux gmux-gtk
python3 scripts/performance-benchmark.py \
  --binary target/release/gmux-gtk --output artifacts/performance.json
```

The benchmark starts the private Wayland fixture described in [testing](testing.md).
It measures frontend and daemon CPU, resident memory, and native input-to-frame
latency with visible, hidden, detached, and idle workloads. The selected frontend
must have its matching `gmux` alongside it.

CPU is expressed as a percentage of one core. RSS excludes GPU memory. Shells,
workload processes, the compositor, and the observer are outside the process
samples, though they can affect timing. Input latency ends when the changed cell
arrives in a composited PipeWire frame; it includes capture and readback overhead
and excludes physical display scanout.

Use `--renderer gl` or `--renderer cairo` to change the default Vulkan backend.
`--seconds` and `--samples` control sample sizes. Reports include binary hashes.

## Daemon input

```sh
python3 scripts/frontend-input-smoke-test.py --binary target/release/gmux \
  --measure --report artifacts/frontend-input.json
```

The headless probe measures socket input to the corresponding changed-cell
packet, including the child's PTY response. It compares idle input with input
immediately after visible or hidden pane metadata changes. It excludes GTK,
compositing, and display latency. The suite also checks viewport coalescing,
search pacing, request fairness, and large pastes. `--measure` reports timings
without applying input-latency thresholds.

## Semantic shell markers

`scripts/shell-benchmark.c` measures native marker processing and prompt jumps
with 24 inactive views. It also counts tracked references, allocator usage, and
render updates. Compile with the same pinned Ghostty library for comparisons:

```sh
ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
cc -O2 -std=c11 -DGMUX_VERSION='"benchmark"' -I"$ghostty_prefix/include" \
  scripts/shell-benchmark.c src/ghostty_bridge.c \
  -L"$ghostty_prefix/lib" -Wl,-rpath,"$ghostty_prefix/lib" \
  -Wl,--wrap=ghostty_terminal_new \
  -Wl,--wrap=ghostty_terminal_grid_ref_track \
  -Wl,--wrap=ghostty_tracked_grid_ref_free \
  -Wl,--wrap=ghostty_render_state_update -lghostty-vt \
  -o artifacts/shell-benchmark
artifacts/shell-benchmark 1 50000 100000 0
artifacts/shell-benchmark 1 50000 100000 1
```

Arguments select OSC 133 markers, command count, retained-line limit, and
allocation counting. A zero line limit uses the terminal's default history
budget. Timing uses process CPU and excludes GTK, Rust metadata serialization,
socket delivery, and shell execution. Count allocations separately from CPU
measurements. The allocator figures cover libghostty bookkeeping, excluding
its memory-mapped grid pages. `prompt_anchor_bytes` separately measures the
bridge's prompt index. It starts at 512 bytes on 64-bit systems, doubles
as needed up to 32 KiB, and is released when no prompt anchors remain.
The benchmark asserts that all tracked references and counted allocations
are released and no render updates occur for inactive views.

Each pane has at most 2,048 prompt references and two reusable output targets.
Each output target uses two tracked cells to detect discarded locations, including
partial-page erasures that clamp references. Monotonic prompt IDs reject targets
whose command was replaced in the active area. Each frontend allocates another pair
only when a context-menu output target is captured.
Navigation searches the prompt index and reads only the target's row metadata.
Output selection also validates the owning prompt's row metadata. New primary
prompts check the prior prompt's active row for erasure, without visiting history.
Compression activity changes when a requested target needs decompression;
ordinary marker bookkeeping and command metadata reads do not keep history awake.

## Idle scrollback compression

The daemon starts compressing a pane after its libghostty compression activity
token stays unchanged for five seconds. It runs at most one incremental call
across all panes per ten milliseconds, serving the oldest deadline first.
Input, PTY output, search, and frame work run before compression on the same
thread.
Completion removes the deadline. Reading compressed history restores it and
starts another idle delay. Attached, hidden, and detached panes use this policy.

`gmux status` includes `terminal_memory` entries for each pane. `memory` reports
page counts, reserved virtual bytes, resident page bytes, compressed pages and
bytes, image bytes, and platform support. These totals combine both screens.
Page figures exclude images, native bookkeeping, render caches, Rust buffers,
and frontend history caches. `prompt_anchors` separately counts retained primary
prompt references, and `prompt_anchor_bytes` reports their index's allocated bytes.
Statistics walk the page list on request without
decompressing it. They are not sampled in the background. `compression` reports
the number of incremental calls, cumulative wall time in `work_ns`, and the
longest call in `max_step_ns`. `scheduled` includes the idle delay and pending
continuations.

```sh
python3 scripts/cargo_build.py --release gmux
python3 scripts/compression-smoke-test.py --binary target/release/gmux \
  --baseline artifacts/gmux-before-compression \
  --report artifacts/compression.json
```

Save the baseline executable before building the comparison revision, and use
the same pinned library for both. Omit `--baseline` for a single-build check.
The Linux probe generates colored Unicode logs with hyperlinks in four panes,
first detached, then with one visible pane and the remaining panes hidden. It
records library page storage, process RSS and CPU, incremental work duration,
and socket-input-to-changed-cell latency during and after compression. The
latency includes the worker's PTY response and daemon frame pacing. It excludes
GTK and presentation. Baseline binaries without memory statistics still report
process samples. The comparison build uses the same observation duration and
input sampling window as the candidate.

The default run keeps production retention, so old generated lines may already
have been pruned. To measure larger retained histories and the combined work
limit with many panes:

```sh
python3 scripts/compression-smoke-test.py --binary target/release/gmux \
  --baseline artifacts/gmux-before-compression --history-mib 64 \
  --lines 20000 --panes 24 --samples 100 \
  --report artifacts/compression-large.json
```

`--history-mib` builds a Linux `LD_PRELOAD` interposer that sets the public
libghostty scrollback byte limit at terminal creation in both builds. The
temporary interposer applies to the test daemons and their children. Reports
identify this override and hash the binaries. Keep these results separate from
production-retention measurements.
Process CPU includes control sampling and input work; `work_ns` measures only
the elapsed time inside compression calls and may include preemption.

The probe observes the detached main thread through `/proc` without sending
requests for 5.5 seconds after completion and after removing all panes. Both
intervals must have zero voluntary context switches, catching continued timer
wakeups beyond the idle delay. Resident-byte reductions and RSS reductions are
separate observations. Compression support depends on the platform; macOS may
reclaim the discarded physical memory lazily, so RSS can exceed library
resident-byte statistics. This probe does not measure macOS reclamation.

## Rendering

`crates/gtk/examples/renderer-benchmark.rs` measures scene preparation, completed rendering,
and texture download for unchanged, single-row, scrolling, and full-screen
updates. It reports median and p95 durations, row rebuild counts, and cache sizes.
See [terminal rendering](renderer.md#benchmarks) for commands and timing limits.

## Remote scrolling

```sh
cargo run --locked --release -p gmux-client --example scroll-benchmark \
  > artifacts/scroll-latency.json
cargo run --locked --release -p gmux-client --example scroll-benchmark -- 0 \
  > artifacts/local-scroll-latency.json
python3 scripts/frontend-input-smoke-test.py --binary target/release/gmux \
  --frame-interval-us 6944 --report artifacts/frontend-input.json
GMUX_TEST_REFRESH_RATE=144 python3 scripts/check.py --native --only terminal-input
```

The client benchmark compares cached and uncached viewport updates with a
simulated reply delay, defaulting to 100 ms. Pass `0` for Unix socket timings
without an added delay. It measures the session snapshot, excluding GTK
and display presentation. The frontend-input suite measures wire bytes for 64
single-row scrolls and reports daemon RSS before and after the trace. Its full-grid
cell-byte count is a reference for sending one complete viewport per step. It
also checks that connections with different display intervals remain independent
and that output preserves unchanged history without repeatedly sending windows.

The native suite pauses viewport replies during touchpad reversals and verifies
that cached content follows the local scrollbar while live screen output continues.
It also checks GTK's display interval. Its report includes observed
input-to-snapshot timing, reply bytes, and retained history-cache bytes. This
timing includes test IPC and excludes compositor presentation.

## Accessible text

```sh
cargo build --release --locked -p gmux-gtk --example accessible-terminal-benchmark
mkdir -p artifacts
target/release/examples/accessible-terminal-benchmark \
  > artifacts/accessibility-benchmark.json
```

This benchmark isolates text construction for ASCII and Unicode grids. It
compares fresh allocation, retained-buffer rebuilds, and color-only row updates.
It excludes GTK, transport, rendering, and AT-SPI delivery. Timings are calibrated
batch means per operation, not individual input-latency percentiles.

## OSC 8 snapshots

The native probe measures unchanged snapshots and identical first-row rewrites
on an 80 × 24 grid with 20 rows of ordinary text, one repeated short target, or
one repeated 1,500-byte target. It reports process CPU per snapshot, bridge
allocation calls, URI lookups and retained link capacity before and after clear.
The counters exclude allocations inside the shared Ghostty library.

```sh
mkdir -p artifacts
cc -O2 -std=c11 -DGMUX_VERSION='"benchmark"' -DGMUX_BENCH_INCREMENTAL -I.deps/ghostty/include -Isrc \
  scripts/hyperlink-benchmark.c -L.deps/ghostty/lib \
  -Wl,-rpath,"$PWD/.deps/ghostty/lib" -Wl,--wrap=malloc \
  -Wl,--wrap=calloc -Wl,--wrap=realloc \
  -Wl,--wrap=ghostty_grid_ref_hyperlink_uri -lghostty-vt \
  -o artifacts/hyperlink-benchmark
artifacts/hyperlink-benchmark long-output 1000
```

Use matching compiler flags and Ghostty builds when comparing revisions. This
probe does not measure frontend input latency or physical display latency.
Use the renderer benchmark separately to check row rebuilds and drawing costs.
