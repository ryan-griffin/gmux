# Terminal rendering

The GTK frontend draws terminal cells with GSK render nodes. Ghostty parses PTY
output in the daemon; fonts and render nodes stay in each frontend.

## Implementation

`crates/gtk/src/renderer.rs` retains row nodes and shapes text through Pango. Compatible
glyphs share text nodes across font fallback changes when their ink does not
overlap. Cell graphics flush pending text batches to preserve paint order.
Backgrounds, underlines, and overlines precede text; strikethrough follows text.
Decorations and cursors use cell geometry. Single, double, curly, dotted, and dashed
underlines preserve independent RGB or palette colors. Unset underline colors
follow displayed text, including selection and block cursor colors. Faint and
invisible rendition applies to decorations as well as glyphs. Row background scans
collect decoration presence so rows skip absent decoration passes. Opaque dotted
and dashed runs share a GSK repeat tile when their device-pixel phase repeats
exactly. Short runs, faint colors, negative origins, and other scales use snapped
rectangles to preserve coverage and blending.

Adjacent printable ASCII cells with matching text styles and foreground colors
shape together, enabling the selected font's default ligatures and contextual
substitutions. Whitespace, style and foreground color changes, selection boundaries,
block cursors, and row ends separate these runs. Background colors paint separately
and do not split shaping. Each shaped cluster starts at its terminal column, so
ligatures preserve cursor and selection geometry. Unicode cells retain independent
shaping for combining characters, fallback fonts, and wide glyphs.

Output, selection, search highlights, and block cursors invalidate affected rows.
Cursor blinking uses GTK's blink cycle, enable preference, and inactivity timeout.
Input restores the visible phase and extends the desired deadline while retaining
a pending source. An early wake rearms without hiding the cursor. GTK timing
changes restart the local delay; only effective default enablement changes send
a daemon style update. Only mapped, focused panes in the active window
schedule a blink source, and program cursor visibility and blink modes stop it.
Block cursor transitions rebuild only the cursor row; bar and underline cursors
reuse all row nodes. OSC 22 pointer shapes apply to the terminal canvas through
GTK's native W3C cursor names. OSC 8 hover uses the
same pointer owner, so leaving a link restores the current OSC 22 request even
when the frame's pointer code is unchanged. Sparse hyperlink maps sit beside
the fixed cells. GTK captures the map and geometry after painting, and link
hover with Ctrl changes only affected row backgrounds and decorations, retaining
glyph nodes and shaped text. The active captured span uses Ghostty's single/double
underline rule with the program's underline color, opacity and concealment.
Link hit testing and styling add no socket requests and copy no URI strings.

Font, Pango context, cell metrics, theme, and pixel alignment changes invalidate
the relevant caches. Scrolling reuses rows after comparing overlapping content.
The shape cache holds at most 4096 entries per pane, with a 1 MiB budget for keys,
glyph arrays, and entry payloads. Larger shapes render without entering the cache.
Shared Pango fonts and spare hash-table capacity are outside that payload budget.

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
The renderer suite also drives allocated cursor test widgets through native
Wayland input, checking pointer initialization, focus, unmapping, window
activation, activity deadlines, timing preferences, default notifications, input
burst source reuse, and blink timeout teardown. Independent rectangle scenes
check dotted/dashed phase, clipping, colors and faint opacity at fractional scales
on Cairo and the selected GPU backend.
An included synthetic OpenType font checks multi-cell ligatures, contextual
substitutions, and boundaries without relying on installed programming fonts.
The selected GTK backend still handles the glyph and geometry checks.
Ligature checks compare each operator against independent Pango drawing on that
backend as well as exact pixels from the cache reference.
Vulkan texture rendering can produce transient pixel differences between
identical scenes, so it is unsuitable as the default cache reference.

To compare caches on the selected backend explicitly:

```sh
GSK_RENDERER=vulkan target/debug/examples/renderer-check \
  --cache-renderer selected --output artifacts/renderer-gpu-cache
```

A mismatch saves actual and expected PNGs, serialized nodes, and a JSON report
with changed-pixel counts and bounds. Outputs go under `artifacts/`.
Saved nodes use the scene's surface position for pixel snapping. When inspecting
pixels with `gtk4-rendernode-tool render`, use `--dont-move` and `--bounds` with
the original viewport. Autosizing to glyph ink bounds can add a fractional
translation after snapping.
The native input suite checks keyboard input, pointer selection, accessible text,
scrolling, and the Wayland clipboard. See [testing](testing.md) for prerequisites.

## Benchmarks

```sh
cargo build --release --locked --example renderer-benchmark
GTK_A11Y=none GSK_RENDERER=vulkan dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-benchmark
```

Use `--font 'JetBrains Mono 12'` to measure an installed ligature font. The code
fixture includes common programming operators alongside the other workloads.
The unique-text fixture changes identifiers on every update to measure shaping
when the text is absent from the cache. The decoration fixture draws all five
underline styles with independent colors and overlines. The cursor-blink workload
alternates block cursor visibility without changing terminal cells.

The benchmark records scene preparation separately from rendering and texture
readback, plus row rebuilds, shape-cache entries, and cache payload bytes.
Readback forces completion; these timings include texture allocation and CPU
readback, and exclude PTY parsing, transport, and window composition. They do not
measure application FPS.
See [performance](performance.md) for process and input-latency measurements.
