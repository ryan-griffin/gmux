# Code organization

Gmux has a GTK frontend and a headless daemon. The daemon owns terminal processes
and shared layouts. Each window owns its presentation and terminal views.

## Frontend

`src/main.rs` handles CLI entry and application startup. `src/app.rs` owns the
application and window registry. Its submodules handle:

- `window.rs`: window construction and daemon updates.
- `tabs.rs`, `split.rs`, `workspaces.rs`: tabs, pane layouts, workspace controls,
  and sidebar search.
- `actions.rs`, `dialogs.rs`, `preferences.rs`: actions, dialogs, command palette,
  and settings.
- `backend.rs`: asynchronous requests, state reconciliation, presentation saves,
  and GUI socket commands.
- `branches.rs`: repository watches and branch updates.

`src/terminal.rs` builds pane widgets. `terminal/input.rs` handles keyboard,
pointer, clipboard, and scroll input. `src/remote.rs` queues terminal commands
and applies viewport packets from the daemon.

See [UI behavior](ui.md) for interaction details.

## Terminal engine and rendering

`src/session.rs` owns PTYs and child processes. `src/engine.rs` wraps Ghostty's
terminal engine. `src/ghostty_bridge.h` defines the C/Rust boundary, and
`src/ghostty_bridge.c` keeps Ghostty implementation types private. `build.rs`
generates Rust bindings with bindgen and checks struct layouts at compile time.
Edit the shared header when changing the boundary.

`src/renderer.rs` builds GSK scenes and caches rows and glyphs. `src/render.rs`
handles Pango shaping and cell graphics. `src/terminal_canvas.rs` implements the
scrollable widget and accessible text interface; `src/accessible_terminal.rs`
maps cells to Unicode text and geometry.

See [terminal rendering](renderer.md) for cache behavior and diagnostics.

## Daemon and persistence

`src/daemon.rs` owns live sessions and shared state. Its submodules handle:

- `event_loop.rs`: PTYs, sockets, signals, and disk-worker completions.
- `frontend.rs`: stream input, paste assembly, subscriptions, and viewport output.
- `commands.rs`: layout and control commands, including ordered saves.

`src/model.rs` defines settings, layouts, and workspaces. `src/storage.rs`
loads JSON and writes files atomically. `src/persistence.rs` runs the disk worker.
`src/scrollback.rs` handles history checkpoints.

`src/control.rs` defines control requests and replies. `src/ipc.rs` handles
control sockets and asynchronous requests. `src/transport.rs` encodes viewport
packets, and `transport/requests.rs` defines stream commands.

See [background sessions](terminal-sessions.md) for lifecycle and protocol limits.
