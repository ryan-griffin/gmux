# Testing

Run commands from the repository root. [Build prerequisites](../README.md#run)
apply to all Rust checks. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a compatible installation.

## Prerequisites

The headless smoke tests need Python 3.12+, PyGObject, GdkPixbuf 2 introspection
data, and its SVG loader. They use real PTYs and private settings and sockets.
They require no display or running Gmux instance.

The native suite and application benchmark also need:

- `dbus-run-session`, Mutter with `--headless`, `--no-x11`, and
  `--virtual-monitor` support, PipeWire, and WirePlumber 0.5+.
- GStreamer with `pipewiresrc` and `videoconvert`, plus introspection data for
  GStreamer 1.0, GstVideo 1.0, GDK 4, GdkPixbuf 2, and AT-SPI 2.
- The accessibility registry at `/usr/lib/at-spi2-registryd` and GNOME desktop
  settings schemas for the font tests.
- `gtk4-rendernode-tool` for renderer checks, Git for branch checks, and Vim
  and `top` for interactive terminal checks.

The native fixture uses Mutter's private RemoteDesktop and ScreenCast D-Bus
interfaces. The recorded environment used Mutter 50.5, PipeWire 1.6.9,
WirePlumber 0.5.18, and GStreamer 1.28.7. These are tested versions, not minimum
requirements beyond those stated above.

## Run checks

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native
```

The runner checks formatting, runs Rust unit, integration, and documentation
tests, runs Clippy with warnings denied, and builds both binaries and the
examples. It then runs the daemon, control, and icon smoke tests. `--native` includes the desktop suite in
an isolated headless Wayland compositor with XWayland disabled.

The renderer examples include source modules whose unit tests already run in
the application target. The runner skips repeating those unit tests for the
examples; the `renderer` smoke suite runs the tools to check drawing and cache
behavior.

The fixture creates a 1280 × 960 virtual monitor and private D-Bus, PipeWire,
settings, runtime sockets, and caches. Keyboard, pointer, and clipboard input
stay in that compositor. The fixture stops its processes when the run ends.

## Select smoke tests

```sh
python3 scripts/check.py --list
python3 scripts/check.py --native --only find terminal-input
python3 scripts/check.py --native --skip-build --only renderer terminal-renderer
python3 scripts/check.py --skip-build --only control icon
```

`--only` selects smoke tests by filename without the `-smoke-test.py` suffix.
Use `smoke` for `smoke-test.py`. Rust checks still run unless `--skip-build` is
set; that option skips all formatting, Rust tests, Clippy, and build steps.
Use it after a successful build while checking an unchanged binary.
Include `--native` for desktop tests so the runner provides their isolated
display and services.

`--list` shows every suite and whether it needs the desktop fixture, without
building or starting services. Unknown names and desktop selections without
`--native` fail before setup. Repeated names run once.

| Smoke tests | What they cover |
| --- | --- |
| `daemon`, `control`, `state-recovery` | Session lifecycle, command ordering, persistence failures, transport validation, and startup recovery. |
| `workspace`, `sidebar-search`, `branch`, `title`, `scheduling` | Workspace state, sidebar behavior, process metadata, focus, and background work. |
| `find`, `commands`, `font`, `resize` | Search, palette accessibility, live settings, and sizing. |
| `main-menu`, `terminal-context-menu` | Menu actions, keyboard navigation, and busy-process confirmation. |
| `terminal-input`, `terminal-scrollbar`, `terminal-renderer`, `renderer` | Input, clipboard, selection, scrolling, interactive applications, and render-node correctness. |
| `notifications`, `notification-focus`, `notification-delivery` | Alert queues, unread acknowledgements, reconnects, focus suppression, desktop delivery, and click navigation. |
| `smoke`, `audit`, `icon` | Application workflows, pane lifecycle cases, and icon decoding. |

Detailed rendering checks are in [renderer validation](renderer.md#validation).
Backend cases are in [session validation](terminal-sessions.md#validation).
Test counts come from the runner output; the suite changes as regressions are
added.

The general `smoke` suite covers startup, PTY workflows, and session reconnects.
Notification edge cases belong to the notification suites. The `audit` suite
runs each regression in a separate application fixture. Named scenarios print
their status so failures identify the behavior being checked.

## Backends and appearance

Check GTK's Vulkan, OpenGL, and software backends through the same fixture:

```sh
for backend in vulkan gl cairo; do
  GSK_RENDERER="$backend" python3 scripts/check.py --native --skip-build --only renderer
done
```

The menu tests also accept light and high-contrast appearances:

```sh
GMUX_REVIEW_APPEARANCE=light python3 scripts/check.py --native --skip-build --only main-menu terminal-context-menu
ADW_DEBUG_HIGH_CONTRAST=1 python3 scripts/check.py --native --skip-build --only main-menu terminal-context-menu
```

## Failures and artifacts

The runner stops at the first failed command and gives each check a five-minute
timeout. Read the command's output and its files under `artifacts/`. Native
fixture logs include `check-session.log` and `check-wayland.log`; individual
checks write their own logs, screenshots, and render-node reports.

`artifacts/` is ignored by Git. A fresh checkout has none of the local reports
referenced in design or benchmark notes. Preserve reports separately when sharing
results. [Application performance](performance.md) and
[renderer benchmarks](renderer.md#benchmarks) describe how to generate them.

Shared socket calls, waits, accessibility traversal, and process fixtures live
in [`scripts/test_support.py`](../scripts/test_support.py). Native input and
capture helpers live in
[`scripts/wayland_test_support.py`](../scripts/wayland_test_support.py).

`AppFixture` tracks processes before waiting for startup, supplies private state
and a shell, and shuts down the daemon even if the frontend has already exited.
`AccessibilityFixture` waits for the registry's D-Bus name and stops it only if
it started the process. Readiness checks wait for observable state. Timed
observations remain where a check needs to verify dismissal deadlines or the
absence of delayed updates.
