# Performance measurements

Benchmarks write local reports under `artifacts/`, which Git ignores. Keep source
revisions and matching frontend/daemon binaries with reports when comparing
builds. Results depend on the compositor, driver, font, and machine.

## Grapheme storage and transport

```sh
cargo test --locked --release -p gmux-daemon grapheme_measurements -- --ignored --nocapture
cargo run --locked --release -p gmux-client --example scroll-benchmark -- 0
```

The grapheme probe rewrites a 120 × 40 grid 500 times for ASCII, `e` with one
combining acute accent, and `a` with 64 three-byte combining marks. It reports
PTY parsing plus snapshot time, packet encode/decode time, retained packet
storage including overflow charges, wire bytes, and exported glyph length.
Encode and decode also run separately. This excludes sockets, shaping, GTK,
and display presentation. Run without other builds or benchmarks competing
for CPU. On hybrid CPUs, pin comparison runs to the same core with `taskset`.
The long fixture is 193 UTF-8 bytes per occupied cell.

On x86-64, cells occupy 40 bytes before overflow text. The probe's ASCII and
short-combining packets each retain 193,032 bytes; wire sizes are 68,050 and
77,650 bytes. The long fixture retains a conservative 1,229,832 bytes and sends
989,650 bytes. Retention includes each cell's overflow allocation even if
other snapshots share it. The common case has no per-cell text allocation;
unchanged overflow text reuses its allocation across dirty-row rewrites.
Native extraction and wire decoding handle empty and single-byte ASCII values
directly; other values pass through the UTF-8, NUL and byte-limit checks.

Owned overflow values and validated variable-length decoding cost more CPU
than fixed byte-buffer copies. Measure both packet CPU time and Unix/TCP
scroll latency when changing this representation. The scroll benchmark's
cached path includes cloning the requested viewport, and its uncached path
includes socket delivery and packet decoding. The prediction cache includes
row allocations and overflow text within its 2 MiB budget. See
[native grapheme limits](native-library.md#grapheme-storage) and the
[wire format](daemon-api.md) for admission bounds.

## Blinking text

Measure retained text phases separately from output and cursor changes:

```sh
cargo build --release --locked -p gmux-gtk --example renderer-benchmark
GTK_A11Y=none GSK_RENDERER=cairo target/release/examples/renderer-benchmark \
  --blink --frames 60 --cols 120 --rows 40 --output artifacts/text-blink-renderer
python3 scripts/cargo_build.py --release gmux gmux-gtk
GSK_RENDERER=gl python3 scripts/text-blink-benchmark.py \
  --binary target/release/gmux-gtk --seconds 5 --output artifacts/text-blink-cpu.json
```

The renderer benchmark alternates visible and hidden phases over ASCII, Unicode,
fallback, ligatures and decorated cells, with no, one-cell, full-screen and
alternating-cell blink. Warmed phase changes assert zero rebuilt rows and zero
new glyph nodes. The shape cache remains unchanged. Rust allocation counters
measure scene preparation and exclude GTK/Pango/GSK allocations. Phase nodes
share glyphs and backgrounds instead of storing a second cell grid.

The process benchmark uses a private Wayland session, disables the terminal
cursor and sets a 1200 ms desktop cycle. It measures ordinary, sparse, dense and
alternating decorated text, a hidden blinking tab, removed blink and detached
idle. Samples contain no RPCs or writer output. Each interval checks that daemon
snapshot and sent-row counters stay unchanged. CPU is a percentage of one core;
main-thread context switches approximate wakeups and include compositor event
delivery. RSS samples include shared libraries, caches and the rest of the app.
The detached sample measures only the daemon. Native prerequisites are listed
in [testing](testing.md#native-prerequisites).

An i7-13700H run with GTK 4.24.1, Monospace 12 and Cairo measured these
120 × 40 preparation medians across 60 warmed frames:

| Blink density | Mixed Unicode | Colored decorations |
| --- | ---: | ---: |
| None | 0.280 ms | 0.412 ms |
| One cell | 0.280 ms | 0.402 ms |
| Every cell | 0.268 ms | 0.369 ms |
| Alternating cells | 0.342 ms | 0.952 ms |

Every warmed phase sample rebuilt zero rows, created zero glyph nodes and made
zero Rust allocations. Ordinary static preparation before/after blink support
was 0.071/0.082 ms for ASCII, 0.312/0.292 ms for mixed Unicode, and 0.423/0.414 ms
for decorated text. Ordinary full-row update preparation was 0.459/0.436,
2.480/2.500 and 3.430/3.479 ms respectively, with identical allocation counts,
shape-cache entries and glyph-node counts. These are individual run medians,
rather than regression thresholds. Completed rendering includes texture readback
and costs more than preparation; alternating decoration fragments increase that
cost even though their nodes remain retained.

The native 84 × 28 decorated viewport used the following frontend CPU across
five-second intervals on the same machine:

| Content | Cairo, % of one core | OpenGL, % of one core |
| --- | ---: | ---: |
| Ordinary | 0 observed ticks | 0 observed ticks |
| One blinking cell | 0.4% | 0.4% |
| Every cell blinking | 2.0% | 1.2% |
| Alternating blinking cells | 5.4% | 3.4% |
| Hidden blinking tab | 0 observed ticks | 0 observed ticks |
| Blink removed | 0 observed ticks | 0 observed ticks |

Active blinking produced about 6–21 main-thread context switches per second,
depending on density and backend. Hidden text measured 0 switches/s on Cairo
and 0.2/s on OpenGL; removing blink measured 0/s on both. Detached daemon idle
had zero CPU ticks and switches. All intervals produced zero daemon viewport
snapshots and sent rows. The clock resolution was 0.2% of one core per sample.
Frontend RSS across the four visible cases was 64.93–65.12 MiB on Cairo and
285.38–285.45 MiB on OpenGL. Backend/driver memory is included, so these totals
do not isolate retained-node storage.

## Keyboard input

Run `python3 scripts/keyboard-benchmark.py` to measure the native encoder bridge
and the production GTK layout lookup/device snapshot in a private Wayland
display. It builds the release GTK example and an optimized C probe, and writes
samples, source/binary hashes and summaries to `artifacts/keyboard-benchmark.json`.
The native desktop prerequisites in [testing](testing.md) apply. To compare an
older bridge, pass `--baseline-source /path/to/ghostty_bridge.c` with its matching
header alongside it. The C probe uses an indirect call to preserve the FFI
boundary, and the public Ghostty allocator API to count key-object allocations.

An x86-64 Linux run on an Intel Core i7-13700H with GTK 4.24.1 used seven samples
of one million operations. The baseline was `bb733cf`, using the same pinned
library and media-key patch. Median CPU nanoseconds per operation were:

| Path | Baseline | Current |
| --- | ---: | ---: |
| Native bridge, legacy | 241.5 | 62.2 |
| Native bridge, Kitty | 341.7 | 157.1 |
| GTK metadata, Wayland | unavailable | 564.6 |

Median wall times were 62.5/158.2 ns for the current legacy/Kitty bridge and
558.5 ns for Wayland metadata. The C workload cycles press, repeat and
release of Shift+A; both revisions emit identical bytes. Metadata timing covers
the layout lookup and lock-state snapshot, excluding event dispatch, JSON, IPC,
PTY handling and presentation. The frontend-input smoke test separately checks
socket-to-viewport responsiveness under output and metadata loads.

The measured loops made zero native key-object allocations and zero Rust
metadata allocations. GDK's C allocations are not counted by the Rust probe.
The compiled physical-key lookup occupied 780 bytes of read-only data. Consumed
shortcuts bypass layout translation. Lock notifications use a fixed pair of
handlers and maintain a shared snapshot between focus-entry queries.
Wayland translates base characters
directly, without allocating a keymap entry list.

## Application clipboard writes

Measure admission and binary transport independently of GTK:

```sh
cargo run --locked --release -p gmux-client --example clipboard-benchmark > artifacts/clipboard-transport-benchmark.json
cargo test --locked --release -p gmux-daemon clipboard_admission_measurements -- --ignored --nocapture
python3 scripts/check.py --native --only clipboard clipboard-ui
```

The client benchmark performs 100 encode/decode/byte-comparison iterations per
case with a reusable output buffer. Allocation counts cover Rust allocation and
reallocation calls during those iterations, excluding source construction and
GTK/native allocators. Binary cases use synthetic NUL bytes with the PNG MIME
type; native tests separately transfer a real PNG. Text cases include bytes
that JSON would escape. The whole payload travels as raw bytes.

On an x86-64 i7-13700H release build, a 4 MiB binary offer took a median 0.48 ms
and maximum 0.71 ms for encoding, decoding and comparison. Its wire record added
575 bytes. Each iteration allocated five times, totaling 4,195,056 bytes,
including one decoded payload allocation. A 4 MiB three-format offer allocated
nine times, totaling 4,195,242 bytes, and added 646 wire bytes. The source offer,
reusable transport buffer, and decoded offer each retain one payload copy.
GDK takes ownership of the decoded byte vectors without another payload copy;
actual plain text also needs a String provider.

Borrowed 4 MiB PNG admission took a median 0.12 ms and maximum 1.15 ms over 100
requests, including the first allocation. The event queue retained the payload
plus bounded MIME/vector metadata. A second large request returned busy without
copying its payload. Queue-overflow tests also verify that rejected encoding
does not allocate a payload-sized output buffer or lose earlier offers.

The same benchmarks measure full-queue rejection separately. Size admission runs
before UTF-8/NUL validation, so backpressure never scans a rejected payload. A
4 MiB plain-text request took a median 112 ns to return busy from engine admission
and 58 ns to reject in transport encoding. Transport rejection allocated only its
72-byte error object and left the queue buffer's length and capacity unchanged.
These timings exclude native protocol decoding, which precedes the callback.

The `clipboard` smoke suite records `artifacts/clipboard-stream-measurements.json`.
It sends 48 one-MiB rich writes at 5 ms intervals to a stopped reader, observes
the daemon detaching that recipient, and verifies a replacement receives no old
offers. A debug-build run completed in 1.01 s including fixture file I/O and the
intentional pauses. Another PTY processed input in 1.98 ms during the flood.
Daemon RSS was 54,144 KiB before, 111,464 KiB at its process high-water mark, and
63,772 KiB after. Idle sampling with writes enabled and disabled recorded zero
daemon CPU ticks over 1.1 s in each case. These short idle samples do not measure
sub-tick work. Clipboard delivery adds no polling timer.

The native parser bounds Kitty MIME and alias counts separately from decoded
data. Measure metadata retained before commit against the pinned library:

```sh
cc -O2 -std=c11 '-DGMUX_VERSION="test"' -I.deps/ghostty/include \
  scripts/clipboard-alias-benchmark.c -L.deps/ghostty/lib \
  -Wl,-rpath,"$PWD/.deps/ghostty/lib" -lghostty-vt \
  -o artifacts/clipboard-alias-benchmark
artifacts/clipboard-alias-benchmark 1000000
```

Repeated aliases update their targets in place. The probe verifies that the
embedding callback runs only at commit and reports RSS before and after the
transaction. See the [current clipboard limits](clipboard.md#application-writes).

## Program status

OSC 7501 callbacks update a session-owned set of at most 64 records directly.
There is no report queue. Unchanged reports update eviction order without string
allocation or metadata delivery. Changes to a record that leave the representative
unchanged also avoid allocating or publishing a summary. Live packets carry one
record. The complete set is serialized only for an explicit metadata request.
Status-only writes preserve viewport, search and compression activity tokens.

The full message remains in the sidebar tooltip and accessible description.
Persistent status rendering uses the sidebar icon and thin pane progress bar.
Sidebar icons are static. Working progress with no estimate uses the existing
mapped-only bar animation; blocked and failed markers never animate. Status and
progress updates refresh only affected tabs. Legacy progress changes masked by
retained OSC 7501 status skip sidebar updates. Sidebar aggregation borrows its
selected summary without cloning report strings. Protocol-specific CLI lists
clone only their selected record.

Measure native parsing and retained storage with the matching pinned library:

```sh
cargo test --locked --release -p gmux-daemon program_status_measurements -- --ignored --nocapture
cargo test --locked --release -p gmux-daemon maximum_text_and_json_escaping -- --nocapture
```

An x86-64 release run at the record cap took about 1.8 microseconds per unchanged
short report and 2.3 microseconds per changing report. The inline record owner is
152 bytes before allocation. Maximum valid text across 64 records plus the cached
summary retained 163,168 bytes, excluding allocator bookkeeping and the inline
owner. Worst-case JSON escaping produced 302,657 bytes for complete records and
4,750 bytes for the live summary. Transport queue accounting includes retained
summary strings.

Protocol-specific CLI aggregation borrows candidates and clones only the chosen
record. The combined sidebar selector allocates no strings or report copies. A
release microbenchmark with maximum-length messages measured about 0.5 microseconds
for 64 OSC 7501 panes, or 0.9 microseconds when alternating OSC 7501 and OSC 9;4.
At 256 panes those figures were 1.9 and 3.5 microseconds. The borrowed selection
occupies 24 inline bytes on x86-64. The sidebar icon and unread badge share a
fixed 16-pixel slot and retain three GTK widgets, with no additional report storage.
The ignored benchmark above measures protocol-specific and combined aggregation
with 1, 16, 64 and 256 panes.

The existing native shell benchmark with 300,000 commands and 24 hidden views
measured median write CPU of 270 ms without shell markers and 376 ms with them.
The main-branch bridge measured 283 ms and 370 ms respectively with the same
library and compiler flags. Both performed zero render updates. Interleave
revisions when repeating this comparison to reduce machine-load differences.

Measure visible, hidden and detached reporting alongside real input:

```sh
python3 scripts/cargo_build.py --release gmux gmux-gtk
python3 scripts/performance-benchmark.py --binary target/release/gmux-gtk \
  --workload program-status --seconds 2 --samples 15 \
  --output artifacts/program-status-performance.json
```

This workload retains 64 records in each of three reporting panes and sends
20 maximum-message reports per writer batch at up to 60 batches per second.
It starts with idle records and samples indeterminate working records separately.
Use `--workload program-status-mixed` to measure changing OSC 9;4 progress beneath
a retained OSC 7501 report, including the sidebar update suppression.
On the measured system, visible reporting used 10% of one frontend core;
composited input latency was 66 ms median and 86 ms at the 95th percentile,
including PipeWire capture. Hidden reporting used about 5% of a frontend core
and had 51 ms median and 67 ms 95th-percentile input latency. Three visible native
activity bars used about 7.5% of a frontend core with zero daemon CPU ticks.
Hidden indeterminate work, true attached idle and detached idle samples all
measured zero CPU ticks. Frontend resident memory stayed around 119 MiB across
these scenarios. These figures include the whole application and depend on the
machine and compositor. With the mixed-protocol workload, visible and hidden
reporting each used about 2% of a frontend core, with 48–49 ms median input
latency. Idle and hidden indeterminate samples still measured zero CPU ticks.

The headless program-status suite also records daemon CPU, retained bytes and
input round-trip time under rapid hidden reports. It asserts zero viewport
snapshots for both visible status-only changes and hidden reporting.

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

## Scrollback retention

```sh
python3 scripts/cargo_build.py --release gmux
python3 scripts/scrollback-limits-smoke-test.py --binary target/release/gmux \
  --measure --panes 4 --lines 20000 --samples 100 \
  --report artifacts/scrollback-limits-release.json
```

The Linux probe generates styled Unicode logs with hyperlinks and gives each
pane a 64 MiB byte allowance. It measures library page allocation and process
RSS before and after lowering all panes to 256 KiB and 200 physical lines.
The update duration includes ordered disk publication and native pruning.
It samples socket control and input-to-changed-cell latency before and after
pruning on one visible pane while the other panes remain detached. Input timing
includes the PTY worker and daemon frame pacing, excluding GTK and display
presentation. It repeats pruning after history compresses, then erases history
with byte zero and observes the idle main thread for 5.5 seconds without RPCs
after compression completes.

Page `virtual_bytes` describe reserved logical page allocation; `resident_bytes`
and `compressed_bytes` describe native page storage. Whole-process RSS also
includes allocator retention, engine bookkeeping, stacks, and caches. Neither
page statistic is an RSS cap. The report records binary and library hashes.
See [retention configuration](scrollback.md) for defaults and page granularity.

An x86-64 Linux release run on an Intel Core i7-13700H used 100 samples per
latency phase. The first workload
had four panes with 20,000 generated lines each; the second had twelve panes
with 12,000 lines each. Values below sum native memory across all panes.

| Workload | Page virtual MiB, before / after | Page resident MiB, before / after | Process RSS MiB, before / after | Save + prune |
| --- | ---: | ---: | ---: | ---: |
| 4 panes | 102.27 / 8.23 | 96.14 / 2.11 | 102.70 / 11.45 | 6.20 ms |
| 12 panes | 192.19 / 26.25 | 173.81 / 7.88 | 180.57 / 19.45 | 15.62 ms |

Input p95 was 15.97 / 15.93 ms for four panes and 15.66 / 15.75 ms for twelve.
Control p95 was 0.74 / 0.75 ms and 0.86 / 0.89 ms respectively. These are
single-process observations, not latency guarantees. After compression, pruning
took 2.86 ms and 3.55 ms, removing 140 and 252 compressed pages respectively.
Their compressed payloads occupied 12.07 MiB and 21.76 MiB before pruning.
Neither detached daemon woke its main thread during the final 5.5-second idle
observation. Byte zero erased remaining history while preserving active-page
allocations, so native page bytes did not fall further in that step.

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

`--history-mib` sets the candidate daemon's saved scrollback byte limit before
creating panes. A Linux `LD_PRELOAD` interposer sets the same public libghostty
limit at terminal creation for older baseline builds without retention controls.
The temporary interposer applies to test daemons and their children. Reports
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

## Clipboard copy

Plain copy streams one formatter pass into a bounded buffer. HTML is generated
only on request, with an independent 8 MiB writer budget and final-output limit.
It adds no work to viewport extraction and no copy cache to engines or views.
The desktop retains the current offer until it is replaced.

Measure the complete Rust engine path, including HTML normalization and JSON
size accounting, in fresh processes:

```sh
GMUX_COPY_CASE=styled GMUX_COPY_CELLS=100000 GMUX_COPY_FORMATS=text-html \
  cargo test --locked --release -p gmux-daemon copy_measurements -- --ignored --nocapture
```

Cases are `uniform`, alternating indexed colors (`styled`), or alternating RGB
colors with bold, italic and decorations (`heavy`). Formats are `text` and
`text-html`. Cell counts are multiples of 1,000, up to 131,000. The probe reports
50-copy mean wall time, actual representation sizes, RSS with the offer alive,
and peak RSS growth after resetting Linux's ingestion high-water mark. RSS
includes allocator retention and does not measure GTK, transport, or GPU memory.

HTML normalization copies matching ASCII runs in batches and writes RGB colors
without general-purpose formatting. It reserves room for inline wrappers and
caps every output-buffer growth request at the HTML limit. A run that needs
whitespace reconciliation falls back once, avoiding repeated scans of a long
prefix before trimmed spaces.

An x86-64 release measurement used three fresh processes per case, with 50
copies per process. The table reports median process means for 100,000 selected
characters, including formatting, normalization and JSON accounting:

| Content | Plain + HTML | HTML bytes | Peak RSS growth |
| --- | ---: | ---: | ---: |
| Uniform | 2.58 ms | 100,285 | 0.3 MiB |
| Alternating indexed colors | 39.72 ms | 5,400,285 | 14.7 MiB |
| Heavy styles | 7.12 ms | Omitted at the limit | 12.7 MiB |

At 2,000 characters, rich copies took 0.10 ms uniform, 0.99 ms with alternating
indexed colors, and 1.73 ms with heavy styles. Reported peak growth stayed below
1 MiB. RSS varies with allocator state and page accounting. These are engine
measurements; destination paste performance varies.

The native plain-copy comparison includes the former size-query pass and the
new streaming allocation. `copy-benchmark.c` temporarily permits 64 MiB of
retained terminal data so large selections do not prune themselves:

```sh
ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
cc -O2 -std=c11 -DGMUX_VERSION='"benchmark"' -I"$ghostty_prefix/include" \
  scripts/copy-benchmark.c -L"$ghostty_prefix/lib" \
  -Wl,-rpath,"$ghostty_prefix/lib" -lghostty-vt -o artifacts/copy-benchmark
artifacts/copy-benchmark 4000000 styled
```

To compare another bridge, define `GMUX_BRIDGE_SOURCE` as its quoted absolute C
source path and `GMUX_COPY_BASELINE` for the former buffer API. Five-process
median CPU time fell from 0.40 to 0.19 ms for 100,000 uniform characters and
from 15.97 to 8.05 ms for 4,000,000. Alternating indexed colors took 16.28 ms
before and 8.34 ms after at 4,000,000 characters. Streaming buffer growth raised
peak copy scratch from about 4 to 6 MiB at that size. All buffers are temporary;
plain-only requests allocate no HTML storage.
