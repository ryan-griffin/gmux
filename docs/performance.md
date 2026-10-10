# Application performance

The application benchmark runs Gmux on native Wayland in an isolated headless
Mutter compositor with XWayland disabled. It measures frontend and daemon CPU,
resident memory, and input-to-composited-frame latency with visible, hidden, and
detached workloads. Results describe the dated binaries identified below.
Rerun the benchmark after later code changes.

## Method

Build matching frontend and daemon binaries, then run:

```sh
cargo build --release --locked --bins
python3 scripts/performance-benchmark.py \
  --binary target/release/gmux --output artifacts/performance-run.json
```

The [testing guide](testing.md#prerequisites) lists the native prerequisites.
The selected binary must have its matching `gmux-daemon` alongside it. The script
starts its own D-Bus session, Wayland compositor, PipeWire server, and WirePlumber
session manager. Settings, runtime sockets, caches, shells, and accessibility
services are private. Test input stays in that compositor. Audio, Bluetooth, and
camera monitoring are disabled. The script stops its processes afterward.

The virtual monitor is 1280 × 960 at 60 Hz and 100% scale. The script verifies
`GdkWaylandDisplay` and window focus before sending input. GTK retains normal
frame pacing; the script clears inherited `GDK_DEBUG` settings. `--renderer gl`
and `--renderer cairo` select other GTK backends; the default is Vulkan.
`--seconds` and `--samples` change the resource and latency sample sizes.

Four panes start in one tab. One runs a raw terminal input probe; three each
write 20 colored Unicode lines per batch, up to 60 batches per second. The
hidden-workload case adds an input probe in a second tab while the three output
panes keep working. Detaching removes the frontend while retaining all
processes. Turning off output leaves the daemon idle.

Each resource sample lasts five seconds, with resident memory sampled ten times
per second. CPU includes all threads in the frontend or daemon, expressed as a
percentage of one CPU core. Shells, workload processes, the compositor, capture
services, and the benchmark observer are excluded. These processes still share
the machine and can affect timing. Main-thread context switches indicate idle
activity; they are not a direct count of wakeups. Linux CPU tick resolution is
about 0.2 percentage points over a five-second sample. RSS excludes separately
accounted GPU memory.

Latency runs send a native keysym press and release through Mutter's remote
desktop test session, then observe a changed terminal cell in a composited
PipeWire frame. GStreamer receives RGBA frames, with at most one queued frame.
Forty samples follow five warmup keys in each attached scenario. No application
RPC or screenshot runs on the timed path. This includes input delivery, GTK input
handling, daemon transport, the PTY round trip, parsing, rendering, compositor
presentation, capture, and CPU frame readback. It excludes physical display
scanout. Capture adds observer overhead, so these numbers are not a measurement
of physical key-to-photon latency.

The fixture uses Mutter's private RemoteDesktop and ScreenCast D-Bus interfaces.
They may change between Mutter releases; tested versions are recorded in the
reports. Other compositors, graphics drivers, monitor scales, and physical
displays need their own measurements.

Each application report records the frontend and daemon SHA-256 hashes. For
comparisons, keep each matching binary pair together and use separate output
paths. Record the source revision and any local source changes with the reports;
a binary hash identifies a build but does not recover its source.

`artifacts/` is ignored by Git. The report and profile paths below refer to
local files from recorded runs and are absent from a fresh checkout. Preserve
those files separately when sharing measurements.

## Accessible terminal text comparison

The isolated accessibility benchmark measures text construction without GTK,
transport, rendering, AT-SPI queries or notification delivery. Run it with:

```sh
cargo build --locked --release --example accessible-terminal-benchmark
taskset -c 2 target/release/examples/accessible-terminal-benchmark \
  > artifacts/accessibility-benchmark.json
```

Set `GHOSTTY_PREFIX` to the pinned library installation when building. Both the
benchmark and the linked Gmux library use Cargo's release profile with thin LTO.
This avoids measuring optimized benchmark code linked to a debug library.

The ASCII fixture fills every cell with `X`. The Unicode fixture repeats CJK,
combining characters, an emoji ZWJ sequence, wide-cell continuations and spaces.
Inputs and outputs pass through `black_box`. Each case has 20 warmup/calibration
iterations and 21 timed batches, with the batch size calibrated for about 10 ms
of work. Reported times are per-operation batch means, not individual latency
percentiles. `fresh` constructs and drops a new `Text`; `reused` rebuilds and
swaps two retained buffers. `color_row` compares one recolored row using the
same `Cell::text_eq` predicate as the frontend, then updates caret and selection.
It excludes the row copy common to both versions and the rest of packet handling.

On 2026-10-10, three alternating before/after runs used an Intel Core i7-13700H,
Linux 7.2.9 and Rust 1.99.0, pinned to logical CPU 2. The baseline compiled the
unchanged production `Text::from_frame` at `492b344` with the fresh-only version
of this benchmark. The table reports the median of the three batch medians,
in microseconds.

| Grid | Fixture | Baseline fresh rebuild | Reused rebuild | Color-only row |
| --- | --- | ---: | ---: | ---: |
| 80 × 24 | ASCII | 33.02 | 31.55 | 0.346 |
| 80 × 24 | Unicode | 35.59 | 33.41 | 0.345 |
| 240 × 80 | ASCII | 319.57 | 314.01 | 1.007 |
| 240 × 80 | Unicode | 366.81 | 328.20 | 1.007 |
| 512 × 256 | ASCII | 2358.29 | 2129.08 | 2.129 |
| 512 × 256 | Unicode | 2537.73 | 2255.10 | 2.128 |

The dominant saving is skipping text reconstruction for color and style changes.
Buffer reuse provides a smaller improvement and removes repeated allocation
when the new contents fit the retained capacities. Text changes still scan the
full viewport, including a single changed row. These results support the small
invalidation/reuse change; they do not establish an application CPU or latency
improvement or justify the complexity of updating global Unicode mappings by row.

The canvas retains two sets of buffers at their largest capacities until it is
destroyed. The old implementation also held old and new text during each
rebuild, but freed the old allocations afterward. The retained memory is the
tradeoff for reuse. Removal notifications still read the old text before the
canvas swaps buffers; insertion notifications read the new text afterward.

Local reports are `artifacts/accessibility-benchmark/before-{1,2,3}.json`,
`after-{1,2,3}.json` and `summary.json`. The preserved baseline executable's
SHA-256 is `6ff8e3278411e03e0f5fb7c0df9a93fee2acf7e8e92f8ce6c41d653f48af3806`;
the updated executable's is
`0a0c9e0da0baaf73c014908029d313db198199aa9512537674725f68fcd8bbe9`.

## Fallback-font batching comparison

This is the latest recorded application comparison, measured on 2026-10-04
before and after fallback-font batching and the accompanying I/O and storage
cleanup. It used the same Intel Core i7-13700H and Iris Xe/Mesa machine,
Linux 7.2.4, GTK 4.22.5, Mutter 50.5, Vulkan, and Monospace 12.

A 1 ms gprofng CPU profile of the application workload recorded
1.86 seconds in `gsk_text_node_new` out of 4.90 sampled CPU seconds. The renderer's
glyph-cache lookup used 0.034 seconds. The profile includes startup, idle, and
busy periods; it identifies expensive calls but is not a latency benchmark.
The [renderer design](renderer.md#implementation) describes the batching rules.

The renderer benchmark includes repeated ASCII/CJK/emoji output and updates
that change every row. For this 120 × 40 fallback-font fixture, full repainting
created 1,841 text nodes per frame before batching and 160 afterward. Median
scene preparation fell from 6.34 to 1.20 ms. These are local Vulkan measurements
with 120 frames after five warmup frames. GPU rendering and readback timings vary
between runs; node counts provide the direct check of reduced work.

Application comparisons use matching release frontend and daemon binaries,
five-second resource samples, and 40 latency samples. The first comparison
measured busy frontend CPU at 21.7% before and 9.4% afterward, with throughput
of 561 and 563 KiB/s. Busy input latency was 35.1 and 35.7 ms at the median,
and 65.6 ms at p95 in both runs. This establishes lower CPU use in this workload,
without an input-latency improvement. Idle and hidden frontends remained near
zero CPU. A second comparison measured 17.7% before and 9.6% afterward at
563 KiB/s, so busy frontend CPU fell by 46% to 57% across the two comparisons.
Busy median latency was 34.0 and 33.1 ms in the second comparison, with p95
at 48.1 and 51.2 ms. Machine load and graphics-driver state differed between
dates, so earlier measurements do not provide a direct comparison.

The application reports identify these builds by SHA-256 prefix:

| Report | Frontend | Daemon |
| --- | --- | --- |
| `improvements-before.json` and `improvements-before-2.json` | `eaa51b46de87` | `712ab1d5c853` |
| `improvements-after.json` | `b07602dad7c0` | `20d977ad147f` |
| `improvements-after-2.json` | `e099fafaddbb` | `51bf4ffb23d3` |

These report files are under `artifacts/`. The second post-change run used
rebuilt binaries; the hashes above distinguish them.
Renderer reports are under `artifacts/renderer-improvements-before/` and
`artifacts/renderer-improvements-after/`. The CPU profile and call breakdown are
`artifacts/frontend-profile-before.er` and `artifacts/frontend-profile-before.txt`.
The renderer validation covers Vulkan, OpenGL, and Cairo, including overlapping
fallback glyphs, italic and faint text, retained scrolling, and fractional scales.

## Historical native Wayland baseline

Measured on 2026-10-03 with an Intel Core i7-13700H, Intel Iris Xe/Mesa,
Linux 7.2.4, GTK 4.22.5, Mutter 50.5, Vulkan, and Monospace 12. The input pane
had 41 × 12 cells at 100% scale. This is one complete run with five-second
resource samples and 40 latency samples per attached scenario. Busy output
was 551–555 KiB/s.

| Scenario | Frontend CPU % | Daemon CPU % | Frontend RSS MiB | Daemon RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| Four idle visible panes | 0.0 | 0.0 | 108.8 | 7.6 |
| Three busy visible panes | 57.2 | 10.3 | 133.0 | 9.3 |
| Three busy hidden panes | 0.4 | 7.5 | 132.9 | 9.9 |
| Detached, three busy panes | — | 5.4 | — | 9.4 |
| Detached, idle | — | 0.0 | — | 9.9 |

RSS values are sampled medians. Both idle processes consumed zero CPU ticks;
this means their CPU use was below the sample's tick resolution. The idle
frontend had about 0.6 main-thread context switches per second.

Latency includes the capture and readback costs described above.

| Scenario | Median ms | p95 ms |
| --- | ---: | ---: |
| Four idle visible panes | 60.5 | 64.1 |
| Three busy visible panes | 64.5 | 82.0 |
| Three busy hidden panes | 47.0 | 50.6 |

The raw report is `artifacts/performance-wayland.json`, including individual
samples, geometry, monitor parameters, and binary hashes. The frontend SHA-256
prefix is `1cababa9de37`; the daemon prefix is `93b011ac1faf`. Timing varies
between runs. This run establishes a native Wayland measurement for that
dated build; it does not establish a scheduling improvement over an earlier build.

## Historical XWayland comparison

These earlier measurements used XWayland input synthesis and `XGetImage` window
readback with `GDK_DEBUG=no-vsync`. They excluded normal display pacing,
compositor presentation, and physical scanout. Their values cannot be compared
directly with the current native Wayland capture method.

The comparison isolates frontend scheduling changes. Both builds already used
the GSK terminal renderer and headless daemon; it does not measure the former
Cairo terminal renderer or the official Ghostty application. Idle main-thread
context switches fell from roughly 64 per second to 1.3, while resident memory
and busy rendering CPU stayed similar.

Measured on 2026-10-02 with an Intel Core i7-13700H, Intel Iris Xe/Mesa,
Linux 7.2.4, GTK 4.22.5, Vulkan, Monospace 12, and 200% device scale. The input
pane had 41 × 12 cells. Two complete comparisons ran serially, reversing build
order for the second pair. The resource table averages the two CPU measurements
and the two sampled RSS medians. Values are **polling build → event-driven build**.

| Scenario | Frontend CPU % | Daemon CPU % | Frontend RSS MiB | Daemon RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| Four idle visible panes | 0.6 → 0.2 | 0.0 → 0.0 | 120.0 → 119.7 | 8.0 → 8.0 |
| Three busy visible panes | 65.4 → 64.0 | 9.4 → 9.5 | 142.3 → 141.9 | 9.9 → 9.8 |
| Three busy hidden panes | 0.7 → 0.0 | 7.6 → 7.7 | 142.9 → 142.7 | 10.0 → 9.8 |
| Detached, three busy panes | — | 5.4 → 5.1 | — | 9.8 → 9.6 |
| Detached, idle | — | 0.0 → 0.0 | — | 10.1 → 9.3 |

Idle frontend main-thread context switches were 64.0–64.4/s before and
1.2–1.4/s after. With the busy panes hidden, they were 62.8–63.6/s before and
1.2/s after. The visible busy workload still requires frequent updates and
rendering, so its CPU cost changed little. Detached busy CPU differences are
small enough that these runs do not establish a throughput improvement.

Latency values below are ranges across the two runs, in milliseconds. Each
range summarizes the per-run median or p95, rather than pooling the samples.

| Scenario | Polling median | Event-driven median | Polling p95 | Event-driven p95 |
| --- | ---: | ---: | ---: | ---: |
| Four idle visible panes | 44.4–45.1 | 33.8–34.0 | 47.9–49.9 | 37.1–37.2 |
| Three busy visible panes | 51.9–58.6 | 39.8–45.4 | 64.0–78.6 | 52.3–70.8 |
| Three busy hidden panes | 31.5–32.1 | 28.3–29.1 | 45.7–48.7 | 32.4–33.1 |

Typical input latency improved in both comparisons. Busy visible p95 values
overlap and vary substantially, so a consistent tail-latency improvement is not
established. The daemon still batches viewport delivery at up to 16 ms intervals;
GTK rendering and window presentation also contribute to the remaining delay.

Raw reports are `artifacts/performance-baseline.json`,
`artifacts/performance-baseline-2.json`, `artifacts/performance-current.json`,
and `artifacts/performance-current-2.json`. They include individual latency
samples, workload throughput, RSS peaks, exact geometry, and binary hashes.
Frontend SHA-256 prefixes were `5d3ece131cf1` before and `7228aeff9c98` after;
daemon prefixes were `3158e48b9819` before and `93b011ac1faf` after.

The [renderer benchmark](renderer.md#benchmarks) separately measures scene
preparation and completed GPU rendering with texture readback. Other graphics
drivers, displays, and longer-running workloads need their own
measurements.
