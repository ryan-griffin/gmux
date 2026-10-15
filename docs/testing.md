# Testing

Run commands from the repository root. The [README](../README.md#build-and-run)
lists build prerequisites. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a build of the same source revision.

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py --headless
python3 scripts/check.py
python3 scripts/check.py --native
```

The runner checks formatting, Rust tests, Clippy, builds, and eight headless smoke
suites. `--native` adds eight desktop suites in a private Wayland session.
Rust tests cover terminal parsing, search, layout changes, transport limits,
state writes, and scheduling. Integration suites exercise real PTYs and GTK.

`--headless` selects `gmux-core`, `gmux-client`, and `gmux`, checks that GTK
and libadwaita are absent from the daemon dependencies and linkage, and runs the eight
headless suites. The daemon suite exercises execution commands without a display,
including tab creation, terminal input, naming, and Stop. It verifies that
daemon commands bypass an existing frontend socket. The headless build does not need
GTK, libadwaita, GLib, or `glib-compile-resources`.

Check execution and the client library without a toolkit:

```sh
cargo test --locked -p gmux-core -p gmux-client --lib
cargo clippy --locked -p gmux-core -p gmux-client --all-targets -- -D warnings
```

The serve suite checks background startup readiness, detached standard I/O,
concurrent launch races, duplicate rejection, saved incoming settings, relative
token paths, live correction, and startup failure reporting. It records startup
time, idle CPU, and resident memory under `artifacts/`.

The ownership suite runs two daemons to check qualified references, identity
across restarts, rejection of client organization requests, invalid client files
being ignored, and ID allocation after deletions. GTK organization tests cover
atomic workspace edits, delayed saves, unavailable hosts, independent profiles,
and disk backpressure without blocking snapshots.

The remote suite checks authentication, pinned identity, execution isolation,
direct CLI and independent viewport clients without a local daemon, atomic catalog
edits, offline renaming, hosting persistence, and daemon independence from corrupt
client settings. The native remote-window suite also runs a GTK binary without a
sibling daemon executable, exercises workspace organization across remotes, removes
all connections, and verifies that quitting leaves remote execution running.

The Git suite exercises optional branch metadata over local and remote streams,
shared watches, unborn and detached branches, linked worktrees, directory changes,
repository creation and removal, unavailable Git, subprocess timeouts and
cancellation, and cleanup after the last subscriber disconnects.

The remote-window suite exercises the GTK sidebar with three hosts and colliding
IDs, workspace filters, explicit connection destinations, command routing, saved organization,
multiple windows, renaming without losing workspace assignments or selections,
unavailable hosts, reconnection, and local Quit. Explicit remote commands require
pane or tab targets and cannot fall back to the selected local tab.

The remotes-ui suite operates native GTK menus and dialogs through AT-SPI. It
checks authentication choices, failed additions and edits, prefilled editors,
Save and Cancel, editor dialogs over their managers, direct workspace creation from
menus and the command palette, workspace list updates, live local hosting,
token file selection, live catalog updates, reconnect, and
removal preserving remote execution. The plus menu lists local and remote
connections under a New Tab via Connection heading. Local and remote creation
inherit the workspace filter. Tests
check workspace-only filtering and the absence of other creation actions.
Local name edits and hostname resets update the connection menu.

The multi-window suite checks that repeated tab metadata updates keep retained
menu memory bounded and that closing windows releases client workers and
subscriptions. Resource samples are written under `artifacts/`.

## Select checks

```sh
python3 scripts/check.py --list
python3 scripts/check.py --only daemon scrollback
python3 scripts/check.py --native --skip-build --only renderer multi-window
```

`--only` selects suites by filename without `-smoke-test.py`; use `smoke` for
`smoke-test.py`. Rust checks still run unless `--skip-build` is set.
That option uses executable paths recorded by the previous runner build in
`artifacts/cargo-build.jsonl`. Rerun without it after source or build-config changes.
Direct smoke scripts build their executables through Cargo unless given an
explicit binary path. Cargo handles target directories and build configuration.
Headless builds record their executable paths in
`artifacts/cargo-build-headless.jsonl`. Use the same `--headless` setting with
`--skip-build`, and rebuild after changing source or build configuration.

Headless suites cover daemon lifecycle, shared clients, input backpressure and
UTF-8 paste, and durable history. Desktop suites cover application workflows,
multiple windows, native input and clipboard, notifications, and rendering.

## Native prerequisites

The native suites need PyGObject and these installed components:

- `dbus-run-session`, Mutter with headless Wayland support, PipeWire, and
  WirePlumber 0.5+.
- GStreamer with `pipewiresrc` and `videoconvert`, and introspection data for
  GStreamer, GstVideo, GDK 4, and AT-SPI 2.
- The AT-SPI accessibility bus and registry service files.
- `gtk4-rendernode-tool` for rendering checks.

The `local-execution` native suite checks daemon-free startup, offline workspaces
and hosting settings, explicit local startup, disconnecting without stopping jobs,
stopping and restarting local execution alongside remotes, and the saved preference.
Existing execution-focused fixtures explicitly enable local execution.

The fixture starts a 1280 × 960 headless Mutter monitor with XWayland disabled.
D-Bus, settings, sockets, shells, and capture services are private. Input stays
inside this session. It uses Mutter's private RemoteDesktop and ScreenCast
interfaces, so a Mutter update may require fixture changes.

Use `GSK_RENDERER=vulkan`, `gl`, or `cairo` to select a rendering backend.

## CI and failures

GitHub Actions checks the standalone daemon and the full workspace build in the Fedora container pinned in
`.github/workflows/check.yml`, using the development Rust toolchain from
`rust-toolchain.toml`. It does not run native desktop suites.
`ci/install-deps.sh` and `ci/check.sh` provide the same steps for a container run.

Logs, screenshots, and renderer reports go under the Git-ignored `artifacts/`
directory. The runner stops at the first failure. Its default deadlines are
1200 seconds per Cargo command and 300 seconds per smoke suite; change them with
`--build-timeout` and `--smoke-timeout`.
