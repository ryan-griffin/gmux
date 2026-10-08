# Diagnostic screenshots

`capture_window` must capture current terminal content and the window's CSS
decoration when Gmux is mapped but fully occluded. The compositor can stop its
Wayland frame callbacks in that state. Waiting for another frame or raising the
window would change the diagnostic's behavior.

The screenshot still completes pending layout, snapshots current children,
draws the root CSS background and frame, and renders the resulting GSK node to
a PNG. Images use logical pixels. The node's bounds include the CSS shadow, so
the PNG can extend beyond the window body. Adwaita changes that shadow when
window focus changes.

## Public API investigation

Investigated on 2026-10-08 with GTK 4.24.1, libadwaita 1.10.0, and gtk4-rs
0.11.5. The project still requires GTK 4.22 and libadwaita 1.9. No new toolkit
feature level is needed for this change.

GTK's [migration guidance](https://docs.gtk.org/gtk4/migrating-4to5.html)
recommends subwidgets for CSS drawing. It recommends `compute_bounds()` when
border-area dimensions matter, and renames `allocated_baseline()` to
`baseline()`. Gmux now uses those supported allocation APIs. Its top-level
Adwaita window has no CSS margins. Probes found equal allocated, content, and
border-bound dimensions for that window; the new capture checks preserve its
logical size. The deprecated exception now covers only CSS snapshot construction.

The tested public APIs do not provide an equivalent fresh decorated root
snapshot. The following experiments and GTK's implementation explain the gap.

### WidgetPaintable

Replacing the root CSS and child snapshot block with a paintable was tested in
the actual Gmux diagnostic, after the existing explicit allocation:

```rust
let paintable = gtk::WidgetPaintable::new(Some(&self.window));
paintable.invalidate_contents();
paintable.snapshot(
    &snapshot,
    self.window.width() as f64,
    self.window.height() as f64,
);
```

Mapped captures eventually showed the updated background, but the 560 by 400
image omitted the outer window shadow. After a fullscreen cover stopped frame
callbacks, new terminal pixels did not reach the capture. Keeping a paintable
across calls also returned old content. Adding `queue_draw()` immediately
before creating each paintable produced no render node even for mapped capture.

In [GTK 4.24.1's WidgetPaintable implementation](https://github.com/GNOME/gtk/blob/4.24.1/gtk/gtkwidgetpaintable.c),
ordinary snapshot calls use `current_image`. A new paintable derives that image
from the widget's cached render node and border bounds. `invalidate_contents()`
emits an invalidation signal; it does not force the window to snapshot.
[GTK's queue_draw implementation](https://github.com/GNOME/gtk/blob/4.24.1/gtk/gtkwidget.c)
clears the cached render node and schedules a future paint phase. An occluded
window may never reach that phase before the diagnostic needs to return.
`get_current_image()` shares this limitation.

### Snapshot children without root CSS

The second experiment kept the explicit allocation and `snapshot_child()` loop
but removed `style_context()`, `render_background()`, and `render_frame()`.
It captured the new background while the frame clock remained paused, but the
header background became transparent. The mapped PNG was 560 by 392 rather
than the working decorated capture's 610 by 450.

Freshness checks were also run with the decoration assertions temporarily
disabled so that the missing header could not hide the content result. These
experimental changes are not part of the retained implementation.

[snapshot_child()](https://docs.gtk.org/gtk4/method.Widget.snapshot_child.html)
snapshots descendants and ignores native children. It cannot snapshot the
top-level window as its own child. The public
[snapshot vfunc](https://docs.gtk.org/gtk4/vfunc.Widget.snapshot.html) describes
widget content. GTK's internal render-node construction draws the root CSS
background, border, clipping, and effects around that vfunc. Calling the vfunc
directly would therefore omit the root CSS work too. The fresh whole-widget
`gtk_widget_snapshot()` function is private and absent from the public widget
header. Depending on it or GTK's private CSS helpers would not be a supported
replacement.

## Retained behavior and follow-up

Keep the CSS snapshot exception until GTK exposes a public way to rebuild a
mapped native widget's full render node without waiting for a compositor frame.
Raising the GTK minimum to the tested 4.24.1 release would not solve this gap.
Reconstructing Adwaita decoration from guessed colors and shadow sizes would
lose theme and focus-state behavior.

The smallest next step is an upstream GTK API request with a reproducer that
maps a CSS-decorated window, covers it, queues a content change after frame
callbacks stop, and asks `WidgetPaintable` for the new image. The required API
must retain root CSS and expose the full shadow bounds. The `screenshot` suite
is the acceptance test for a future replacement. No upstream issue was filed
as part of this work.

The existing diagnostic's scope also stays the same. It requires a mapped
window and captures that window's widget tree. Native popover or separate
window surfaces are not added to the PNG. It does not capture the desktop.

## Validation

Bootstrap the worktree's pinned Ghostty revision and run:

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py --native
GSK_RENDERER=gl python3 scripts/check.py --native --skip-build --only screenshot
GSK_RENDERER=cairo python3 scripts/check.py --native --skip-build --only screenshot
GMUX_REVIEW_APPEARANCE=light python3 scripts/check.py --native --skip-build --only screenshot
```

The screenshot fixture uses Mutter's isolated 1280 by 960 virtual monitor,
private D-Bus and PipeWire services, and a solid fullscreen GTK cover. It
checks compositor pixels to prove occlusion and checks `renderer-info` to
prove the window stays mapped, its frame counter stops, and capture does not
resume it or raise the window. It checks new terminal colors before capture
and after reveal, opaque header and body pixels, translucent shadow pixels,
and a 560 by 400 logical window body at monitor scales 1 and 1.25. GTK reports
integer widget scale factors 1 and 2 for those surface scales.

The local native environment used Mutter 51.0, PipeWire 1.6.9, WirePlumber
0.5.18, and GStreamer 1.28.7. Native validation here uses GTK 4.24.1 and
libadwaita 1.10.0; the minimum toolkit versions were not separately installed.
Logs and PNGs live in ignored `artifacts/`, with investigation reports under
`artifacts/screenshot-investigation/`.

Historical screenshot-modernization results:

- The worktree bootstrap completed for Ghostty revision
  `4da7523faba68ccb4042ea20585817098a51c015`. Formatting, all 109 Rust tests,
  Clippy with warnings denied, builds, and the four headless smoke suites passed.
- The new screenshot suite passed with Vulkan, OpenGL, and Cairo in dark
  appearance, and Vulkan in light appearance. Each run checked both monitor
  scales with mapped, fully occluded, and revealed windows.
- The full native command stopped in `resize` at its one-pixel canvas-size
  assertion. A separate run with the original capture implementation failed
  at the same assertion. `find` later failed its focused sidebar-directory
  accessibility assertion, also reproduced with the original backend.
- The unchanged `renderer-check` example intermittently failed pixel equality
  for scrolled or edited cached rows on Vulkan. Its original-backend comparison
  passed, but a final rerun failed. That example does not call `capture_window`.
  Its failure log is preserved as `renderer-final-failure.log` in the
  investigation directory.
- All other native suites passed across selected runs. `terminal-context-menu`
  initially failed a pointer-input assertion, then passed with both the
  original backend and the final implementation on rerun.

These historical results did not establish a passing full native suite. At the
time of the screenshot investigation, Find stopped before its light-appearance
case, although its earlier dark screenshots succeeded. Those failures were
outside the screenshot change.

## Native Find follow-up

The Find timeout at the focused sidebar-directory assertion also reproduced
on untouched baseline `686a272`. A follow-up on main `5d20a97` traced the
failure through pane metadata, focus, the AT-SPI tree, and Wayland messages.
The first pane stayed active and both panes reported the expected directories.
Accessibility traversal cleared its caches, but the sidebar had collapsed and
hidden, so its rows were absent from the tree.

The `resize` diagnostic unconditionally called `unmaximize()` on an already
normal window. After GTK requested the wide 1120 by 740 geometry, Mutter's
reply to that unnecessary request configured the old 540 by 480 geometry.
The window returned to its narrow layout. Find's terminal-width wait could
mistake the space gained by hiding the sidebar for a completed wide resize.
The Wayland trace recorded the wide geometry followed by the old-size configure;
waiting for native window dimensions alone reproduced the failed wide resize.

The diagnostic now calls `unmaximize()` only for a maximized window. The
unchanged Find suite passed after that fix. Its regression checks now wait for
native window dimensions and the expanded sidebar before switching tabs. The
directory assertion checks the visible, selected `Find Test` row and still
requires its label to follow each focused pane's directory. No timeout was
extended and no assertion was skipped.

Validation on 2026-10-08 bootstrapped Ghostty revision
`8f0dd3709050b1026f6324368805033197d8b4a5` in the follow-up worktree, with the
verified source archive SHA256
`f4869fc667ea0a1c93705e363ceca06d394b67ad1d3dde756a7fa5c15691f9cb`.
The full native Find suite passed in dark and light appearance, including
retained-history Unicode matches, offscreen navigation, wrapping, live output,
tab and pane switching, clearing and reopening. Added cases also passed for
Unicode and wrapped history after a checkpoint-saving Quit and daemon restart,
then search after grid reflow. Native sidebar-search, branch, and terminal-input
checks passed, including scrollback selection and clipboard copying.
`python3 scripts/check.py` passed formatting, all 120 Rust tests, Clippy with
warnings denied, binary and example builds, and all five headless suites. The
engine tests include copying checkpoint-restored history across reflow.
The adjacent native `resize` suite also passed, including its pixel-only case.
These native checks used the repository's isolated Mutter Wayland fixture with
GTK 4.24.1, libadwaita 1.10.0, and Mutter 51.0.
Logs include `find-bootstrap.log`, `find-diagnose.log`,
`find-diagnose-protocol-app.log`, `find-native-validation.log`, and
`find-headless-validation.log` under ignored `artifacts/`; the adjacent Resize
result is in `find-adjacent-resize.log`.

The Find follow-up does not establish a passing full native suite or resolve
the renderer cache issue recorded above. Its passing adjacent Resize check
records the behavior of the guard. The separate Resize investigation below
confirms the cause and adds exact pixel-size and monitor-scale coverage.

## Resize follow-up

The pixel-only timeout also reproduced on baseline `686a272`. It reproduced
on `4740ff1` with Ghostty `8f0dd3709050b1026f6324368805033197d8b4a5` too.
It predates the screenshot and Ghostty modernizations.

A Wayland trace on 2026-10-08 identified an app resize RPC bug. Every request
called `unmaximize()`, even on an unmaximized window. The
[XDG shell protocol](https://github.com/wayland-mirror/wayland-protocols/blob/main/stable/xdg-shell/xdg-shell.xml#L942)
requires a configure reply even in that state. Mutter replied with the old
1040 by 680 geometry after GTK had applied 1041 by 680. GTK then restored the
old allocation. The canvas and independently probed PTY both remained 783 by
580 pixels at 27 rows and 87 columns. The grid-overlay cases could observe a
temporary resize before this reply arrived.

The [Find follow-up](https://github.com/ryan-griffin/gmux/pull/8) adds the
resize RPC guard so it calls `unmaximize()` only when GTK reports a maximized
window. With that guard, 1041 by 680 stays allocated, the canvas becomes
784 by 580, and `TIOCGWINSZ` reports the same pixel size at the unchanged grid.
The terminal renderer and PTY resize implementation already handled this
allocation correctly; neither needed changes. This fixes the app's diagnostic
resize request, rather than changing interactive compositor-driven resizing.

There was also an independent test assumption. The adaptive sidebar can consume
one window pixel. For example, after the fix, 1040 to 1039 moves the canvas
origin left by one pixel while its width remains 783. The resize suite now
waits for the requested actual GTK allocation, then allows a second adjacent
window pixel if the first did not change the canvas. It still requires exactly
one canvas pixel, unchanged rows and columns, matching PTY pixel geometry,
and no overlay. It checks width and height in both directions at monitor scales
1 and 1.25, and observes each result throughout the overlay dismissal interval
to catch delayed geometry rollback. Fractional-scale fixtures start with an
empty saved session sized to fit the logical monitor, avoiding Mutter's
automatic maximization of the default window.
Grid and geometry probes clear and home the terminal before running so shell
output stays above the overlay sample at fractional scale.

Follow-up validation used a fresh worktree-local bootstrap of Ghostty
`8f0dd3709050b1026f6324368805033197d8b4a5`, with no `GHOSTTY_PREFIX` override:

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native --skip-build --only resize font terminal-renderer screenshot
```

After rebasing onto `aa6644266cbd201690dc70a7ec82be3752bae3ec`, the merged
Find guard supplies the app fix. This follow-up adds only the actual
`renderer-info.window_size` diagnostic to app code. The Find checks and their
historical diagnosis remain in place. The standard headless runner and native
`resize screenshot` checks were rerun on the rebased tree; the font and
terminal-renderer results above came from the earlier validation with the same
guard and unchanged rendering code.

Formatting, all 120 Rust tests, Clippy with warnings denied, binaries/examples,
and all five headless suites passed. The complete resize suite passed at both
monitor scales. Font, terminal-renderer, and screenshot checks also passed in
the isolated native Mutter fixture. These selected results resolve the
historical Resize failure; they do not establish a passing full native suite.
The Find follow-up covers its historical timeout; the renderer-check failure
recorded above remains separate. Toolkit versions remain GTK 4.24.1 and
libadwaita 1.10.0; minimum versions were not separately tested.
A negative-control build with the old unconditional `unmaximize()` failed at
the requested 1000 by 680 GTK allocation. The revised tests reject the old
behavior; the guard was restored after this check.

The concise allocation and PTY probe results are in
`artifacts/resize-investigation.log`. Before/after Wayland traces are preserved
in `resize-before-fix-wayland.log` and `resize-after-fix-wayland.log` under
`artifacts/`. Final runner logs are `resize-headless-check.log` and
`resize-native-check.log`, with a JSON PTY report for each pixel-only case.
The earlier adjacent font and terminal-renderer results are preserved in
`resize-prebase-native-check.log`.
The negative-control failure is in `resize-negative-check.log`.
