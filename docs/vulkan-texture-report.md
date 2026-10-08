# Upstream report draft: intermittent pixels from equivalent GSK text scenes

This is a draft for review. No upstream issue has been filed.

Suggested title: `GskVulkanRenderer intermittently returns pixel outliers when repeatedly rendering a fixed text scene on Intel Iris Xe`

Repeatedly rendering one GSK node object occasionally returns different pixels through
`gsk_renderer_render_texture()`. The scene comes from GTK snapshots and Pango
layouts. The same symptom also occurs with two independently built equivalent
scenes, whose captured nodes serialize identically. The `--keep-node` case builds its scene once, then only renders and downloads it.
This reproduces outside Gmux's terminal renderer and uses no Ghostty,
application row cache, compositor capture, or frame callbacks.

The probe compares exact downloaded pixels and stops that process at its first
mismatch. A fixed group of fresh processes measures the frequency. It never
retries a failed check to obtain a passing result. Diagnostic downloads and renders
run only after failure and cannot change its exit status.

## Steps

From a Gmux checkout with the native test prerequisites in
[the testing guide](testing.md#prerequisites):

```sh
python3 scripts/gsk-texture-repro.py --backend vulkan --runs 10 --rounds 1000 \
  --sequence fixed --keep-node --output artifacts/vulkan-texture-vulkan
python3 scripts/gsk-texture-repro.py --backend gl --runs 5 --rounds 1000 \
  --sequence fixed --keep-node --output artifacts/vulkan-texture-gl
python3 scripts/gsk-texture-repro.py --backend cairo --runs 3 --rounds 1000 \
  --sequence fixed --keep-node --output artifacts/vulkan-texture-cairo
```

The Python runner compiles the [standalone C source](../scripts/gsk-texture-repro.c)
with the installed GTK development libraries. It starts the repository's isolated
Mutter Wayland fixture with private D-Bus, PipeWire, settings and caches, XWayland
disabled, and a 1280 × 960 virtual monitor. It records the actual renderer and
requires `GdkWaylandDisplay`. Output directories must be new to preserve previous
failures. It does not build Gmux or change system packages.

The C source also runs directly in a Wayland session:

```sh
cc -Wall -Wextra -Werror -O0 -g scripts/gsk-texture-repro.c \
  -o /tmp/gsk-texture-repro $(pkg-config --cflags --libs gtk4) -lm
GSK_RENDERER=vulkan GTK_A11Y=none /tmp/gsk-texture-repro \
  --sequence fixed --keep-node --rounds 1000 --output /tmp/gsk-texture-mismatch
```

This direct command uses that session's fonts and environment. The measured
results below use the isolated fixture. Use its runner to reproduce those settings.

## Expected and actual behavior

Expected: equivalent scenes rendered consecutively on the same GTK renderer do not
acquire isolated transient pixels. The probe uses byte equality, not a pixel
tolerance. Passing samples do not establish deterministic output on Vulkan.

Actual: some fresh processes fail with one or two changed pixels even though both nodes
serialize identically. Three additional downloads of each captured texture return
the original bytes. Three additional render pairs of those same nodes agree.
The original comparison remains failed.

Downloaded bytes use native Cairo ARGB32, which is BGRA on this little-endian
machine. See [the GTK texture download contract](https://docs.gtk.org/gdk4/method.Texture.download.html).
The JSON samples use that order. Bounds are inclusive. The file names `actual`
and `expected` identify the first and second downloads on the selected backend.
This reproducer compares pixels within one backend. Each failed process retains
two PNGs, two transformed `.node` files, at most eight pixel samples, and one
mismatch JSON report. It retains no per-frame images on passing processes.

## Environment

Recorded 2026-10-08 on x86_64 Arch Linux:

- Intel Iris Xe Graphics RPL-P, PCI `8086:a7a0`, revision 04, kernel driver `i915`.
- Mesa `26.2.4-arch1.1`, Intel open-source Mesa Vulkan driver, Vulkan API `1.4.354`.
- Linux `7.2.9-arch1-1`, Mutter `51.0`, GTK `4.24.1`, libadwaita `1.10.0`.
  The standalone probe does not link libadwaita.
- Pango `1.58.2`, GLib `2.90.1`, Cairo `1.18.6`, FreeType `2.14.3`,
  HarfBuzz `14.6.0`, Fontconfig `2.18.3`, GCC `16.2.1`.
- Private fixture `Monospace` resolves to Noto Sans Mono. Emoji resolves to
  Apple Color Emoji. Saved nodes also name Adwaita Mono, Liberation Mono,
  Noto Sans Mono CJK KR and Arial fallbacks with bold/italic variants.
  Host desktop font preferences do not describe the private fixture's resolution.
- Native scale 1. The full probe explicitly renders at 1, 1.25, 1.5 and 2 and
  uses origins `0,0` and `0.25,0.5`. These are texture transforms, not monitor changes.

[Measurement summary](vulkan-texture-sample/measurements.json) preserves every
group's process exits and the checked-in probes' source and binary hashes.
The checked-in runner writes `environment.json` with exact inventory
commands, their outputs, source and binary SHA-256, compiler arguments, Git head,
and inherited graphics switches. `summary.json` records every process exit,
actual renderer, completed pairs and the complete failure count.

## Reduction and measurements

Investigation starts at main `555b8efe5faec1537a66b9ae396cf9407964fee8`, after
renderer checker PR #10 merged. The selected-Vulkan baseline copied the prior
validated checker binary with SHA-256
`f2e617f971e453f0fd56a7b854d5d600b0461e8102deb468ce31548ee73b0c26`.
It failed 1/10 fresh processes here at `cache-context`, with one changed pixel at
`201,63` and equal nodes. Its source is the checker merged in PR #10.

The C fixture visits a 40 × 8 grid and builds per-cluster layouts through `gtk_snapshot_append_layout()`.
It includes ASCII, combining accents, CJK, emoji, Hebrew, Arabic and a fallback
symbol, bold/italic rows and one opacity row. Its initial mode builds both scenes before the first texture render. The
held-node mode builds one scene for the entire process. It uses no terminal glyph extraction or text batching.

The initial sequence renders a font/theme prelude, compares 19 pairs while changing
font/context/scale/origin, builds and discards 16 scenes of 320 CJK layouts without
rendering them, then compares 56 pairs across scale/origin/text shifts. The initial
C probe failed 1/20 processes, one pixel at `215,136` in a 400 × 168 image, with equal
nodes. Removing the prelude also failed 1/20 processes. Each of the following
independent groups passed 20/20: omit the context change, omit CJK churn, and run
only the 56 scale/origin/text-shift pairs. These counts do not establish necessary
conditions or exclude a rarer failure in the passing variants.

The probe with the prelude omitted failed 3/50 Vulkan processes. All three
failures occurred at pairs 8, 10 or 11, before its font-size or context changes
and before CJK churn. Each had one changed pixel, equal nodes, stable downloads
and three agreeing diagnostic render pairs. OpenGL and Cairo each passed 20/20
processes using that sequence.

The constant-scene reduction uses `--sequence fixed --rounds 100`. Every pair
builds two new snapshots of the same fixture, font, scale 1 and origin `0,0`.
It removes all preceding render/font/context/glyph-churn/scroll changes. Vulkan
failed 4/20 fresh processes at pairs 9, 24, 55 and 59. All four failures had one
changed pixel, equal nodes, stable downloads and three agreeing diagnostic render
pairs. OpenGL and Cairo each passed 20/20 fresh processes and 2000 exact pairs.
Vulkan completed 1747 pairs because each failed process stopped at its first
mismatch. These groups shared one private fixture per backend and did not
retry failed comparisons.

Rendering the same node object twice per pair, while constructing one new scene
at the start of each pair, also failed 3/20 Vulkan processes at pairs 17, 23 and 37.
Both render calls can receive the same node object.
Building one full Pango layout per row instead of per-cluster layouts passed
20/20 Vulkan processes of 100 pairs. This variant changes the emitted text nodes
and allocation sequence. Its passing sample does not establish why the original
scene fails.

A control that constructs the scene only once per process and keeps the same
node for all 1000 comparison pairs failed 9/10 Vulkan processes. This removes
layout construction and scene changes between comparisons. Each capture again
had one changed pixel, equal nodes and stable downloads. `--keep-node` exposes
this case in the checked-in source and implies `--same-node`. It requires
`--sequence fixed`. This is the narrowest verified sequence. The scene still
contains many per-cluster text nodes; the whole-layout control passed.

The checked-in held-node mode failed 9/10 Vulkan processes, completing 4596
pairs before the nine first mismatches. Seven failures changed one pixel and
two changed two pixels. All had equal nodes and stable downloads. All three
diagnostic render pairs agreed in each capture and matched one of the original
downloads. The texture outlier could occur on either side of the comparison.
OpenGL passed 5/5 fresh processes and 5000 exact pairs. Cairo passed 3/3 fresh
processes and 3000 exact pairs in the same held-node mode. Frequencies use fixed
measurement groups; they do not predict other machines or establish deterministic
Vulkan output.

The checked-in same-node mode failed 6/20 Vulkan processes at pairs 33, 53, 56,
68, 75 and 93. Each stopped at its first mismatch. The six captures each contain
one changed pixel with equal nodes and stable downloads. All three diagnostic
render pairs agreed in every capture. The captures also record whether those
diagnostic pixels match each original download. Every capture had one original
that matched all diagnostic renders and one that differed at the outlier.
OpenGL and Cairo each passed 20/20 processes and 2000 exact pairs in this mode.
Vulkan completed 1778 pairs. The failure count is per fresh process, not per
independent frame sample.

A representative [mismatch report](vulkan-texture-sample/mismatch.json) records
BGRA `[255, 255, 255, 255]` versus `[75, 73, 73, 255]` at `265,138`, pair 411.
The [actual PNG](vulkan-texture-sample/actual.png),
[expected PNG](vulkan-texture-sample/expected.png),
[actual node](vulkan-texture-sample/actual.node) and
[expected node](vulkan-texture-sample/expected.node) are checked in for review.
The nodes include the texture transform and are byte-identical. Both render
calls in this capture received the same node object, which stayed alive for
all preceding comparisons too.
[Capture environment](vulkan-texture-sample/environment.json) records the exact
source/binary hashes and native inventory. Local full-run artifacts are under
`artifacts/vulkan-repro/`, which Git ignores. Captures alone have not been shown
to reproduce the failure when replayed independently.

## Earlier evidence and attribution limits

The earlier investigation recorded an unchanged checker failure in 1/10 Vulkan
processes and explicit selected-Vulkan cache failures in 3/10 processes after
PR #10. Captured pairs had one changed pixel and equal serialized nodes. The
historical pixel at `69,78` changed BGRA `[79,77,77,255]` versus `[255,255,255,255]`
in a 400 × 192 image. Initial scenes with both application renderers fresh also
failed. The previous solid-color, simple Pango and saved-node probes each passed
1000 renders. They did not measure the sequence here.

Previous trials disabling GPU merging, all GPU optimizations, dmabuf, worker
threads or dual-source blending still failed. A separate offscreen renderer also
failed. Those results are in the prior investigation, not repeated claims about
the C probe.

GTK 4.24.1's [GPU texture rendering implementation](https://github.com/GNOME/gtk/blob/4.24.1/gsk/gpu/gskgpurenderer.c)
calls `gsk_gpu_frame_sync()` and `gsk_gpu_frame_wait()` before returning the
texture. The probe therefore does not assume a delayed compositor frame will fix
readback. Stable downloads narrow the observation to the captured texture's
contents. They do not identify whether GTK's upload/render/readback code or Mesa's
Vulkan driver produced the outlier. Equal serialization also does not prove that
all internal font or GPU cache state was identical.

Gmux's checker fix uses exact Cairo pixels for application cache invariants and
retains independent selected-GPU layout, paint-order, geometry and pixel checks.
It does not fix this GTK/Mesa behavior. The original explicit selected-Vulkan
comparison remains available and every mismatch still fails. This investigation
changes no Gmux drawing code, checker comparison, tolerance or coverage.

Validation compiled the standalone source with `-Wall -Wextra -Werror`, checked
Python syntax and argument bounds, and verified the captured PNG/node metadata
and documentation links. A private one-pixel red-overlay control failed on Cairo
at `0,0`, with unequal nodes and disagreeing diagnostic render pairs. Its exit
remained 1. The injection was never added to the checked-in source. No Gmux
source, checker or CI file changed, so the Rust and application suites were not
rerun. The supplied main validation remains separate from these diagnostic runs.

No toolkit rebuild, driver substitution or system package change was made.
Other GPU vendors have not been tested. A renderer class alone does not establish
which graphics subsystem is responsible.
