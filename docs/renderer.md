# Terminal rendering

The GTK frontend draws terminal cells with GSK render nodes. Ghostty parses PTY
output in the daemon; fonts and render nodes stay in each frontend.

## Implementation

`src/renderer.rs` retains row nodes and shapes text through Pango. Compatible
glyphs share text nodes across font fallback changes when their ink does not
overlap. Cell graphics flush pending text batches to preserve paint order.
Backgrounds precede text; decorations and cursors use cell geometry.

Output, selection, search highlights, and block cursors invalidate affected rows.
Font, Pango context, cell metrics, theme, and pixel alignment changes invalidate
the relevant caches. Scrolling reuses rows after comparing overlapping content.
The glyph cache holds at most 4096 clusters per pane.

Borders, blocks, decorations, and cursor edges snap to device pixels, including
fractional scales and pane positions. Block cursors span both cells of a wide
glyph. Faint text uses node opacity so color emoji dim with other text.

`TerminalCanvas` implements `GtkScrollable` inside a `GtkScrolledWindow`.
Its vertical adjustment maps logical pixels to Ghostty history rows. GTK handles
scrollbars, wheel and touchpad scrolling, and momentum. Mouse-tracking programs
receive wheel buttons instead of history scrolling.

The canvas also implements `GtkAccessibleText`. It exposes Unicode viewport
text, character offsets, caret, selection, text boundaries, and cell extents.
Color-only updates reuse text mappings. Text or layout changes rebuild the
viewport text, with retained buffers capped after shrinking. Wide glyphs expose
two-cell bounds, and combining characters share their glyph's bounds.

## Validation

```sh
python3 scripts/check.py --native --only renderer terminal-input
GSK_RENDERER=gl python3 scripts/check.py --native --skip-build --only renderer
```

`renderer-check` compares text nodes against independent Pango layout drawing
and checks cell graphics, styles, selection, cursors, fractional alignment,
and retained rows. Cache comparisons use exact pixels from a Cairo renderer.
The selected GTK backend still handles the glyph and geometry checks.
Vulkan texture rendering can produce transient pixel differences between
identical scenes, so it is unsuitable as the default cache reference.

To compare caches on the selected backend explicitly:

```sh
GSK_RENDERER=vulkan target/debug/examples/renderer-check \
  --cache-renderer selected --output artifacts/renderer-gpu-cache
```

A mismatch saves actual and expected PNGs, serialized nodes, and a JSON report
with changed-pixel counts and bounds. Outputs go under `artifacts/`.
The native input suite checks keyboard input, pointer selection, accessible text,
scrolling, and the Wayland clipboard. See [testing](testing.md) for prerequisites.

## Benchmarks

```sh
cargo build --release --locked --example renderer-benchmark
GTK_A11Y=none GSK_RENDERER=vulkan dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-benchmark
```

The benchmark records scene preparation separately from rendering and texture
readback, plus row rebuilds and glyph-cache entries. Readback forces completion;
these timings include texture allocation and CPU readback, and exclude PTY
parsing, transport, and window composition. They do not measure application FPS.
See [performance](performance.md) for process and input-latency measurements.
