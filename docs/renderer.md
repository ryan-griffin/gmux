# Terminal renderer

Gmux's terminal widget emits GSK render nodes with Pango glyph shaping.

GTK chooses the rendering backend. `GSK_RENDERER=gl` requests OpenGL,
`GSK_RENDERER=vulkan` requests Vulkan, and `GSK_RENDERER=cairo` requests GTK's
software backend. All three consume the same terminal render nodes.
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
nodes. Up to eight pending text batches can combine glyphs with the same font
and color across fallback-font changes. Ink extents prevent combining batches
when that would reorder overlapping glyphs. Cell graphics flush the batches
before drawing. The renderer reuses the batch and glyph buffers across rows and
snapshots, and releases them when the font or Pango context changes.
Changes to output, selection, search highlights, and block cursors rebuild
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

See [code organization](code-organization.md) for the rendering modules.
`TerminalCanvas` implements `GtkScrollable` inside a `GtkScrolledWindow`. Its
vertical adjustment measures Ghostty's scrollback in logical pixels using the
current cell height. GTK handles wheel and touchpad scrolling, including momentum,
and supplies the libadwaita scrollbar styling, overlay indicators, and drag controls.
The canvas converts pixel positions to terminal rows and preserves fractional
motion across daemon acknowledgements. Font zoom changes the pixel range without
changing touchpad sensitivity. Mouse-tracking applications receive wheel buttons;
touchpad deltas accumulate a cell height before sending each button.
Each pane has its own adjustment, and panes without scrollback hide their bars.
Scrolling preserves selection. Updated terminal content and overlays draw when
their daemon state arrives. Keys that leave the viewport unchanged schedule no
terminal redraw. The native input test covers flick
momentum and rapid reversals as well as wheel input and small touchpad movements.
GTK manages GPU glyph and texture caches. Cached rows retain an additional
viewport cell copy, about 333 KiB for 120 × 40 cells, plus render nodes and glyphs.
When rows shift, the renderer verifies their overlapping content and moves the
existing row caches. It rebuilds incoming rows and any rows whose selection,
search, cursor, or device-pixel alignment changed. Fractional scale checks compare
these reused rows against a freshly rendered viewport.
Whole-process resident memory is measured in the [application benchmark](performance.md).
GPU memory has not been measured. Terminal image protocols are not implemented.

The [daemon](terminal-sessions.md) sends changed rows and frame metadata, with a
complete viewport on attach or a new subscription. The daemon also sends history
Find highlights and tracked selection ranges. Fonts and render nodes remain in
the frontend. Hidden and detached panes keep parsing
output without automatic viewport extraction or terminal painting.

The canvas implements `GtkAccessibleText` alongside `GtkScrollable`. It exposes
Unicode viewport text, character offsets, caret, selection, text boundaries, and
cell extents. The frontend tracks a text revision while applying row deltas,
comparing glyph bytes and wide-cell continuation flags. Color and style changes
still repaint but reuse the accessible text and its mappings. Actual text or
layout changes rebuild the full viewport with a spare set of text buffers;
the old contents remain readable until the removal notification completes.
After shrinking, buffers with capacity above four times the current live text
storage release their excess capacity, and the canvas discards an oversized
spare. The [performance notes](performance.md#buffer-retention-after-resizing)
record the retained memory and reuse timings.
Cached and rebuilt selections share the same cell-end mapping,
including wide glyphs and row boundaries. Boundary queries reuse a lazily created
GTK text buffer until the text changes. Changes to colors alone do not emit text
replacement notifications. The interface is read-only; keyboard input still
uses terminal encoding and the input method.
Wide glyphs expose their full two-cell bounds, combining characters share their
glyph's bounds, and points outside the terminal grid have no character offset.

## Validation

Use the [testing guide](testing.md) for full or selected native checks, GTK
backend selection, appearance variants, prerequisites, and log locations.

`renderer-check` compares batched text with GTK's independent Pango layout
drawing for four font descriptions, light and dark colors, and normal and 150%
scales. Fixtures include CJK, combining marks, emoji, Hebrew, Arabic, a font
fallback symbol, bold, italic, faint, underline, and strikethrough. Checks also
cover concealed cells, decorations on spaces and wide spacers, selection and
search colors, overlapping fallback glyphs with italics and opacity, wide cursors,
background batching, and block seams at 100%, 125%,
150%, and 200% scale with fractional pane positions.

Retained output must match a freshly built scene after changes to cursor, focus,
selection, search, font, context, theme, scale, and viewport size. Each cache case
requires exact pixels from a separate Cairo renderer. This includes initial
scenes, scrolling at all tested scales and pane positions, edge edits, and a
single-row output change. Unicode churn must keep
the glyph cache within its limit. The smoke script checks node types
and row rebuild counts for unchanged, single-row, and scrolling workloads.
Terminal scenes contain text nodes and no Cairo image nodes even when GTK uses
its software backend.

The native input test starts an isolated headless Mutter Wayland compositor
with `--no-x11`, a 1280 × 960 virtual monitor, and private D-Bus and PipeWire
sessions. It verifies the application uses `GdkWaylandDisplay`, sends native
keyboard and pointer input, and checks accessible text, caret and selection,
row-end selection bounds and repeated text boundary queries across output changes,
selection across scrollback, stationary drag autoscroll in both directions,
scroll cancellation, fast edge drags during GTK overscroll without viewport
bouncing, rapid reversals with delayed viewport replies, Ctrl+Shift+C,
and the Wayland clipboard. Pointer coordinates come from colored
markers in composited frames, including the compositor's window placement.
No test input reaches the user's desktop.

The terminal context menu test uses the same isolated compositor. It checks
accessible menu names, Copy and Paste availability, selection preservation,
clipboard contents, Find focus, keyboard navigation and activation, mnemonics,
pane-specific split and close actions, busy-process confirmation, and the
Shift+right-click override in mouse-tracking applications. Pointer motion and
wheel input must stay in the open menu.

The main menu test checks accessible item names, the hidden Terminal Actions menu
in an empty window, tab creation with the sidebar hidden, workspace destinations
and starting directories, workspace menu updates and mnemonics, directory selection
cancellation, zoom, keyboard mnemonics, standard dialogs, and busy-process Quit
confirmation. It checks the separate Terminal Actions menu, active-pane splits,
Find focus, active-tab renaming with the sidebar hidden, pane closure, and
whole-tab closure with busy-process confirmations.
The Fullscreen checkmark must also follow changes made through F11. The sidebar
shortcut must toggle visibility, and F10 must reveal the sidebar and open its
main menu. It saves main-menu, workspace, and terminal-menu screenshots under
`artifacts/`. It also checks workspace management, radio states and mnemonics in
the workspace filter, both New Tab dropdowns, and keyboard and pointer invocation of tab context
menus. Workspace commands must target the clicked tab, preserve its panes, and
leave unrelated tabs alone.

The window-close test checks Ctrl+Shift+W on an empty startup window, after the
last pane closes, and in an empty workspace with a hidden running terminal.
It also checks the Close Window command with a running terminal. Window closure
must detach the app while preserving the daemon and any running terminals.

The interactive smoke test runs Vim and top through input, edit/save, scrolling,
splits, zoom, resize, and detach/reattach while both applications keep running.
It verifies primary-screen restoration and 3000 lines of colored output. Other
native regressions cover system fonts, Find, resize overlays, notifications,
and daemon reconnect. Tests currently cover this Intel Iris Xe/Mesa machine;
other GPU vendors have not been tested here.

## Cache comparison investigation

On 2026-10-08, the unchanged checker failed once in ten focused Vulkan runs
against main `5d20a97`, using the worktree's bootstrapped Ghostty revision
`8f0dd3709050b1026f6324368805033197d8b4a5`. Its archive matched the bootstrap's
SHA-256. `GHOSTTY_PREFIX` was unset; builds used `.deps/ghostty` and the installed
libclang 23.1.1. The native fixture used Mutter 51.0, GTK 4.24.1, libadwaita
1.10.0, Mesa 26.2.4, and Intel Iris Xe graphics.

The historical edited-row failure changed three channels of one pixel at
`(69, 78)` in a 400 by 192 image. Bounded diagnostics reproduced one- and
two-pixel outliers in other cache cases, including an initial scene where both
application renderers were fresh. In every captured mismatch, the cached and
fresh nodes serialized identically. Repeated downloads of each failed texture
returned unchanged bytes. Re-rendering the same nodes removed the outliers.
The fixture supplies a native display, but these checks render directly to
textures without compositor capture or frame callbacks. GTK's
[GPU texture renderer](https://github.com/GNOME/gtk/blob/4.24.1/gsk/gpu/gskgpurenderer.c)
waits for the submitted frame before returning the texture.

The evidence identifies unstable Vulkan rendering as the cache comparison's
unsuitable pixel reference. It does not establish whether GTK or Mesa causes
the transient output. Disabling GPU merging, all GPU optimizations, dmabuf,
worker threads, or dual-source blending did not eliminate failures. A separate renderer
realized for offscreen drawing also failed. Simple solid-color, Pango-layout,
and saved-node probes each passed 1000 repeated renders; none reproduces the
checker's complete sequence of font, context, and glyph-cache changes.

The checker now uses exact Cairo pixels for cache invariants. Cairo consumes
the actual retained and independently rebuilt GSK scenes. It adds no pixel
tolerance or retry. The selected GTK backend still runs all sixteen independent Pango-layout cases, the overlapping fallback
paint-order comparison, and the geometry, selection, decoration, and cursor
pixel checks. `checks.json` and smoke output report both the selected backend
and the cache oracle. No application cache or drawing behavior changed.

To run the original GPU cache comparison explicitly:

```sh
GSK_RENDERER=vulkan target/debug/examples/renderer-check \
  --cache-renderer selected --output artifacts/renderer-gpu-cache
```

A mismatch remains a failure. Node serialization runs on failure, so font
serialization does not add work to every passing comparison. It writes the
actual and expected PNGs and node files under `--output`, plus a JSON report
with image dimensions, node equality, changed-pixel count, inclusive bounds, maximum channel difference, and at most
eight pixel samples. The panic reports only that summary and the report path.
The default oracle also writes these diagnostics if a cache regression occurs.

Focused validation used main `aa6644266cbd201690dc70a7ec82be3752bae3ec`.
The final rebase onto `d8daabcc12b4cb9405d96186a4652edb8ade6fce` preserved
the Find and Resize changes. The checker binary's SHA-256 remained identical,
so the focused repeat evidence applies to that final binary too.

- Thirty fresh-process Vulkan runs passed with the default Cairo cache oracle.
  Each checked all sixteen independent GPU layout cases and the selected GPU's
  geometry and paint order, plus exact cache pixels. These runs shared one
  isolated fixture and did not retry failed checks.
- The explicit Vulkan GPU cache comparison failed three of ten runs, each
  with one changed pixel and equal node serialization. This mode exposes the
  remaining graphics-stack instability.
- Five OpenGL runs and three Cairo runs passed with `--cache-renderer selected`,
  so both also checked exact cached/fresh pixels on their selected backend.
- After the final rebase, native `renderer` and `terminal-renderer` passed on
  Vulkan, including row
  reuse and bounded cache counts, Vim editing, live `top`, scrolling, resize,
  reattachment, and 3000 colored output lines.
- After the final rebase, `python3 scripts/check.py` passed formatting, all 120 Rust tests, Clippy with
  warnings denied, binary/example builds, and the five headless suites.
- A temporary one-pixel red overlay caused an exact cache comparison failure.
  Its report identified one changed pixel at `(0, 0)` and saved both images and
  node files; the panic log stayed below 2 KiB. The injection was removed before
  the final build.

Investigation logs and probes live in ignored `artifacts/renderer-investigation/`.
Repeat-run results above describe the checker and this graphics stack; they do
not prove Vulkan output is deterministic or validate other GPU vendors.

The follow-up [GTK/Pango reproducer and upstream-report draft](vulkan-texture-report.md)
records a failure outside Gmux's renderer, bounded mismatch artifacts, backend
comparisons and the limits of GTK/Mesa attribution.

## Benchmarks

The [application benchmark](performance.md) measures frontend and daemon CPU,
resident memory, and native Wayland input-to-composited-frame latency with
several busy panes.
The example below isolates scene preparation and completed GPU rendering. It
requires a running graphical session; the automated native checks use the
isolated compositor described in the [testing guide](testing.md).

```sh
cargo build --release --locked --example renderer-benchmark
GTK_A11Y=none GSK_RENDERER=vulkan dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-benchmark-vulkan
GTK_A11Y=none GSK_RENDERER=gl dbus-run-session -- \
  target/release/examples/renderer-benchmark --frames 60 --cols 120 --rows 40 \
  --output artifacts/renderer-benchmark-gl
```

The example records scene preparation separately from completed rendering and
texture download. Readback forces GPU completion, so these numbers include
texture allocation and CPU readback. They exclude PTY parsing, daemon transport,
window composition, and presentation, and do not measure application FPS. Idle
terminal widgets do not repaint continuously. Five warmup frames precede each
measured workload. Reports include median and p95 durations, row rebuilds,
new text nodes, and shaping-cache entries, plus PNGs and serialized render nodes.

ASCII, mixed Unicode/styles, colored full-screen applications, and repeated
ASCII/CJK/emoji fallback fixtures use unchanged, one-row, scrolling, and
all-rows-changed updates. GTK's
[rendernode benchmarking documentation](https://docs.gtk.org/gtk4/gtk4-rendernode-tool.html)
explains the distinction between submission and completed GPU rendering.

## Recorded renderer comparisons

These tables describe earlier implementation stages. The newest fallback-font
batching results are in [application performance](performance.md#fallback-font-batching-comparison).
All report directories named here are local files under the Git-ignored
`artifacts/` directory. They are absent from a fresh checkout.

The standalone renderer reports record the GTK backend, font, geometry, and
sample count but do not record a source revision or binary hash. Keep that
information alongside reports when comparing builds. Dates were not recorded
for the reference and row-cache comparisons below.

### Row-cache reuse comparison

Row-cache reuse was compared with the preceding build in the isolated Wayland
fixture using Vulkan, Monospace 12, a 120 × 40 grid, and 60 measured frames after
five warmups. Two comparisons reversed the build order. The table averages the
two per-run medians for CPU scene preparation, in milliseconds.

| Scrolling fixture | Before reuse | After reuse |
| --- | ---: | ---: |
| ASCII | 1.37 | 0.32 |
| Mixed Unicode and styles | 5.44 | 1.00 |
| Colored application | 5.11 | 1.02 |

Each scrolling frame rebuilt one row instead of 40. Preparation time fell by
about 76% to 82%; these measurements describe scene preparation rather than
application FPS or total CPU use. Raw reports are in
`artifacts/renderer-scroll-before-{1,2}/` and `artifacts/renderer-scroll-after-{1,2}/`.

### Reference before row-cache reuse

Reference release results before row-cache reuse use the same 120 × 40 grid and
Monospace 12 font, with 60 measured frames after warmup. Native test applications
were closed during these runs. Median times below are milliseconds; total includes
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

Timings vary between runs. The benchmark above can compare GTK backends on
other machines.

### Historical Cairo prototype comparison

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
