# Terminal renderer

GSK is Gmux's only terminal renderer. The former Cairo terminal paint path,
DrawingArea integration, renderer switch, and direct pangocairo dependency have
been removed. Run normally with `./scripts/run.sh`.

GTK chooses the rendering backend. `GSK_RENDERER=gl` requests OpenGL,
`GSK_RENDERER=vulkan` requests Vulkan, and `GSK_RENDERER=cairo` requests GTK's
software backend. All three consume the same terminal render nodes. The software
backend remains usable without maintaining a second terminal implementation.
See [GTK's Cairo renderer](https://docs.gtk.org/gsk4/class.CairoRenderer.html).

Inspect the terminal renderer and actual GTK backend with
`gmux rpc '{"method":"renderer-info"}'`. The response also includes viewport
size, cell metrics, device scale, surface position, keyboard focus, window
activation, and the GDK display backend.

## Implementation

The custom terminal widget emits direct GSK text and color nodes. Pango shapes
terminal clusters and chooses fallback fonts. Compatible glyphs share text nodes
with explicit cell positions, so fractional font advances cannot accumulate
spacing errors between cells. Each row retains its background and foreground
nodes. Changes to output, selection, search highlights, and block cursors rebuild
the affected rows. Font descriptions, Pango context changes, cell metrics,
colors, and device pixel alignment invalidate the relevant caches. Font
descriptions and metrics stay cached until settings, zoom, or the Pango context
changes. Search highlight buffers remain cached between viewport, query, and
selected-match changes. An empty search needs no highlight buffer.

The glyph cache holds at most 4096 clusters per pane and evicts individual
entries as new clusters arrive. It retains fonts and glyph geometry, then drops
the temporary Pango layouts. Row nodes retain the resources they need even after
a cluster leaves the shaping cache. Adjacent backgrounds and decorations share
nodes. All backgrounds precede text so wide and tall glyphs remain visible.

Block elements and common box borders use cell geometry generated once per cell.
Shared boundaries, decorations, and cursor edges snap to surface device pixels,
accounting for fractional scales and pane positions. Block cursors cover both
cells of wide characters. The surface position includes GTK's window-shadow translation,
verified with native pointer input. Faint text uses node opacity to dim color
emoji. Text-node foreground alpha alone does not affect every color glyph. See the
[GSK text-node constructor](https://docs.gtk.org/gsk4/ctor.TextNode.new.html).

Sources are `src/renderer.rs`, `src/render.rs`, and `src/terminal_canvas.rs`.
GTK manages GPU glyph and texture caches. Cached rows retain an additional
viewport cell copy, about 333 KiB for 120 × 40 cells, plus render nodes and glyphs.
Whole-process resident memory is measured in the [application benchmark](performance.md).
GPU memory has not been measured.

The [daemon](terminal-sessions.md) sends changed rows and frame metadata, with a
complete viewport on attach or a new subscription. Fonts, selection, search,
and render nodes remain in the frontend. Hidden and detached panes keep parsing
output without automatic viewport extraction or terminal painting.

## Validation

```sh
cargo build --locked --bins --examples
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
GTK_A11Y=none GSK_RENDERER=vulkan dbus-run-session -- python3 scripts/renderer-smoke-test.py
GTK_A11Y=none GSK_RENDERER=gl dbus-run-session -- python3 scripts/renderer-smoke-test.py
GTK_A11Y=none GSK_RENDERER=cairo dbus-run-session -- python3 scripts/renderer-smoke-test.py
GTK_A11Y=none dbus-run-session -- python3 scripts/terminal-renderer-smoke-test.py
python3 scripts/terminal-input-smoke-test.py
```

`renderer-check` compares batched text with GTK's independent Pango layout
drawing for four font descriptions, light and dark colors, and normal and 150%
scales. Fixtures include CJK, combining marks, emoji, Hebrew, Arabic, a font
fallback symbol, bold, italic, faint, underline, and strikethrough. Checks also
cover concealed cells, decorations on spaces and wide spacers, selection and
search colors, wide cursors, background batching, and block seams at 100%, 125%,
150%, and 200% scale with fractional pane positions.

Retained output must match a freshly built scene after changes to cursor, focus,
selection, search, font, context, theme, scale, and viewport size. Unicode churn
must keep the glyph cache within its limit. The smoke script checks node types
and row rebuild counts for unchanged, single-row, and scrolling workloads.
Terminal scenes contain text nodes and no Cairo image nodes even when GTK uses
its software backend.

The native input test starts an isolated headless Mutter Wayland compositor
with `--no-x11`, a 1280 × 960 virtual monitor, and private D-Bus and PipeWire
sessions. It verifies the application uses `GdkWaylandDisplay`, sends native
keyboard and pointer input, and checks terminal accessibility, selection,
Ctrl+Shift+C, and the Wayland clipboard. Pointer coordinates come from colored
markers in composited frames, including the compositor's window placement.
No test input reaches the user's desktop.

This test and the application benchmark need Mutter with `--headless`,
`--no-x11`, and `--virtual-monitor` support, PipeWire, WirePlumber 0.5+, and
`dbus-run-session`. Python needs PyGObject and introspection data for GDK 4,
AT-SPI 2, GStreamer 1.0, and GstVideo 1.0. GStreamer needs `pipewiresrc` and
`videoconvert`. The accessibility registry must be installed at
`/usr/lib/at-spi2-registryd`. The fixture uses Mutter's private RemoteDesktop and
ScreenCast D-Bus interfaces, which may change between releases. Validation used
Mutter 50.5, PipeWire 1.6.9, WirePlumber 0.5.18, and GStreamer 1.28.7.
Fixture and session logs are under `artifacts/`.

The interactive smoke test runs Vim and top through input, edit/save, scrolling,
splits, zoom, resize, and detach/reattach while both applications keep running.
It verifies primary-screen restoration and 3000 lines of colored output. Other
native regressions cover system fonts, Find, resize overlays, notifications,
and daemon reconnect. Tests currently cover this Intel Iris Xe/Mesa machine;
other GPU vendors have not been tested here.

## Benchmarks

The [application benchmark](performance.md) measures frontend and daemon CPU,
resident memory, and native Wayland input-to-composited-frame latency with
several busy panes.
The example below isolates scene preparation and completed GPU rendering.

```sh
cargo build --release --locked --example renderer-benchmark
GTK_A11Y=none GSK_RENDERER=vulkan dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-release-vulkan
GTK_A11Y=none GSK_RENDERER=gl dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-release-gl
```

The example records scene preparation separately from completed rendering and
texture download. Readback forces GPU completion, so these numbers include
texture allocation and CPU readback. They exclude PTY parsing, daemon transport,
window composition, and presentation, and do not measure application FPS. Idle
terminal widgets do not repaint continuously. Five warmup frames precede each
measured workload. Reports include median and p95 durations, row rebuilds,
new text nodes, and shaping-cache entries, plus PNGs and serialized render nodes.

ASCII, mixed Unicode/styles, and colored full-screen application fixtures use
unchanged, one-row, and scrolling updates. GTK's
[rendernode benchmarking documentation](https://docs.gtk.org/gtk4/gtk4-rendernode-tool.html)
explains the distinction between submission and completed GPU rendering.

Current release results use the same 120 × 40 grid and Monospace 12 font,
with 60 measured frames after warmup. Native test applications were closed
during these runs. Median times below are milliseconds; total includes
scene preparation, completed rendering, and texture readback. Reports are
in `artifacts/renderer-release-vulkan/` and `artifacts/renderer-release-gl/`.

| Fixture and update | Vulkan preparation | Vulkan total | OpenGL preparation | OpenGL total |
| --- | ---: | ---: | ---: | ---: |
| ASCII unchanged | 0.08 | 4.22 | 0.08 | 3.85 |
| ASCII one row | 0.12 | 4.16 | 0.11 | 3.95 |
| ASCII scrolling | 0.65 | 4.88 | 0.64 | 4.60 |
| Mixed unchanged | 0.29 | 5.85 | 0.26 | 5.81 |
| Mixed one row | 0.21 | 4.71 | 0.42 | 6.05 |
| Mixed scrolling | 1.26 | 5.69 | 2.64 | 8.28 |
| Colored application unchanged | 0.15 | 4.71 | 0.16 | 4.44 |
| Colored application one row | 0.21 | 4.89 | 0.24 | 4.46 |
| Colored application scrolling | 1.29 | 6.06 | 1.34 | 5.56 |

Timings vary between runs. The earlier prototype comparison below records
the original decision to use GSK; the current benchmark can compare GTK
backends on other machines.

Before removing the Cairo path, the prototype comparison recorded the following
median milliseconds for preparation plus rendering and readback. These historical
results were measured on 2026-10-02 with GTK 4.22.5, Mesa 26.2.3, Intel Iris Xe
Raptor Lake-P, a release build, Monospace 12, and a 120 × 40 viewport over 60 frames.
The old renderer is no longer included in the benchmark.

| Fixture and update | Vulkan Cairo | Vulkan GSK | OpenGL Cairo | OpenGL GSK |
| --- | ---: | ---: | ---: | ---: |
| ASCII unchanged | 18.13 | 6.73 | 18.61 | 7.74 |
| ASCII one row | 20.59 | 7.03 | 17.70 | 8.05 |
| ASCII scrolling | 17.72 | 8.29 | 16.03 | 7.21 |
| Mixed unchanged | 19.34 | 10.76 | 19.29 | 8.74 |
| Mixed one row | 19.34 | 11.22 | 19.11 | 7.07 |
| Mixed scrolling | 25.43 | 11.79 | 18.33 | 13.88 |
