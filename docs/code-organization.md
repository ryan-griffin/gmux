# Code organization

Gmux has a GTK frontend and a headless daemon. The daemon owns terminal processes
and shared layouts. Each window owns its presentation and terminal views.
The Cargo workspace separates shared protocol, client behavior, and execution:

- `gmux-protocol`, in `crates/protocol`, defines shared terminal values, execution
  requests and snapshots, network handshakes, and codecs. It uses ordinary Rust
  serialization dependencies and has no platform I/O or native build dependencies.
- `gmux-client`, in `crates/client`, owns outgoing connections, retries, terminal
  caches, and bounded queues. Its `unix` module contains client platform adapters.
- `gmux-platform`, in `crates/platform`, provides shared platform services.
  `paths` formats paths against an explicit home directory, and `tcp` applies
  deadlines through standard-library sockets. `unix` contains the current Linux
  integration: control sockets, fd wakeups, cancellation, file locking, atomic
  writes, machine detection, identity generation, and local daemon lifecycle.
- `gmux-daemon`, at the repository root, owns PTYs, Ghostty and its native bridge,
  server listeners, execution, and daemon persistence. It exposes the daemon entry
  points and has no runtime dependency on the client library.
- `gmux`, in `crates/cli`, contains the `serve` command and management CLI.
- `gmux-gtk`, in `crates/gtk`, builds the GTK client and its resources.

Linux is currently the supported platform. The protocol crate is independent of
that choice; client integration still uses Linux implementations for socket
setup, fd notifications, configuration watches, and process watching. Local Unix
socket transport and local daemon management are separate responsibilities.
Additional client platforms require implementations of these adapters.

The daemon has no GUI dependencies. Build and install scripts compile it
independently in both modes so GTK dependency features cannot change the shipped
artifact. `scripts/check.py` enforces the package dependency boundaries.

## Frontend

`crates/gtk/src/main.rs` handles CLI entry and application startup. `crates/gtk/src/app.rs` owns the
application and window registry. Socket launches and GApplication activation share
the same connection selection and queued directory handling. Its submodules handle:

- `window.rs`: window construction and daemon updates.
- `tabs.rs`, `split.rs`, `workspaces.rs`: tabs, pane layouts, workspace controls,
  and sidebar search.
- `actions.rs`, `dialogs.rs`, `preferences.rs`: actions, dialogs, command palette,
  and settings.
- `backend.rs`: asynchronous requests, state reconciliation, presentation saves,
  and GUI socket commands.
- `management.rs`: management lists and temporary editor dialogs.
- `connections_dialog.rs`: local connection name and remote access editor, remote connection list and forms,
  authentication file selection, reconnect, and removal dialogs.
- `connection_menu.rs`: shared connection choices, stable daemon targets, and live availability.
- `machine.rs`: machine icons, labels, and connection captions.
- `branches.rs`: Git subscriptions and labels for metadata collected by the owning daemon.

`crates/gtk/examples/` contains the renderer and accessible-text benchmarks.
`crates/gtk/src/settings.rs` owns GTK preferences. The client library contains no frontend
appearance settings or GNOME icon names.

`crates/gtk/src/terminal.rs` builds pane widgets. `terminal/input.rs` handles keyboard,
pointer, clipboard, and scroll input. `crates/client/src/remote.rs` queues terminal commands
and applies viewport packets from the daemon.

See [UI behavior](ui.md) for interaction details.

## Terminal engine and rendering

`src/session.rs` owns PTYs and child processes. `src/engine.rs` wraps Ghostty's
terminal engine. `src/ghostty_bridge.h` defines the C/Rust boundary, and
`src/ghostty_bridge.c` keeps Ghostty implementation types private. `build.rs`
generates Rust bindings with bindgen and checks struct layouts at compile time.
Edit the shared header when changing the boundary.

`src/ghostty_shell.h` keeps tracked primary prompts and command-output selection
inside the C boundary. The semantic callback reports bounded command metadata
to `engine.rs`. `src/shell.rs` configures startup using the packaged Bash and
Zsh resources in `data/shell-integration`. Session command state is shared;
prompt navigation, captured context targets, and selection belong to each view.

`crates/gtk/src/renderer.rs` builds GSK scenes and caches rows and glyphs. `crates/gtk/src/render.rs`
handles Pango shaping and cell graphics. `crates/gtk/src/terminal_canvas.rs` implements the
scrollable widget and accessible text interface; `crates/gtk/src/accessible_terminal.rs`
maps cells to Unicode text and geometry.

See [terminal rendering](renderer.md) for cache behavior and diagnostics.

## Daemon and persistence

`src/daemon.rs` owns live sessions and execution state. Its submodules handle:

- `event_loop.rs`: PTYs, sockets, signals, and disk-worker completions.
- `frontend.rs`: stream input, paste assembly, subscriptions, and viewport output.
- `commands.rs`: layout and control commands, including ordered saves.

`crates/protocol/src/model.rs` and `model/state.rs` define daemon identities, qualified references,
execution state, and pane topology. `crates/platform/src/unix/storage.rs`
loads JSON and writes files atomically. `src/persistence.rs` creates the daemon identity and runs the disk worker.
The detached launcher only starts a process and waits for its control handshake;
the daemon validates and restores its own saved execution.
`src/scrollback.rs` handles history checkpoints.
`crates/platform/src/unix/machine.rs` detects and caches the hostname and hardware type.
`unix/identity.rs` generates daemon identities from the operating system.
`unix/paths.rs` discovers the local home directory and expands local home paths;
formatting with a supplied home directory lives in the portable `paths` module.
`branch/service.rs` shares requested Git metadata across clients, and
`branch/watches.rs` tracks repository changes on the owning machine.

`crates/protocol/src/control.rs` defines execution requests and replies, including
tab commands and pane requests shared with GTK. GTK adds workspace and window
operations. `crates/client/src/ipc.rs` handles execution endpoints and request
workers. `crates/platform/src/unix/ipc.rs` supplies local control sockets and cancellation.
`crates/protocol/src/transport.rs` and `transport/requests.rs` encode viewport
packets and stream commands. `crates/protocol/src/network.rs` defines handshakes
and bounded JSON framing. `crates/client/src/network.rs` authenticates outgoing
TCP connections. Both sides use `gmux-platform::tcp` for deadline-aware I/O.
`src/daemon/network.rs` accepts incoming TCP connections and
forwards execution requests and streams. `src/daemon/remote_access.rs` owns
listener configuration and lifecycle. There is no outgoing connection registry
or proxy in the daemon.

See [background sessions](terminal-sessions.md) for lifecycle and protocol limits.

## Client library

`gmux-client`, in `crates/client`, is shared by the built-in CLI and GTK app.
It has no GTK, PTY, libghostty, or native bridge build dependencies. Shared
protocol types come directly from `gmux-protocol`, without client re-exports.
Viewport values are ordinary Rust types; the daemon's generated ABI assertions
check their layout against the C bridge.

`connections.rs` owns saved destinations, identity pins, and client configuration
commands. `connection.rs` owns one daemon's asynchronous setup, retries, status,
and typed state updates. `remote.rs` maintains cached viewports and bounded
input/output queues. Incoming queue admission tests live with the client; codecs
and value tests live with the protocol.

`unix/connect.rs` implements cancellable TCP setup using the current Linux socket
APIs. `unix/watch.rs` watches saved connections using inotify. Direct TCP endpoints
accept in-memory credentials; saved connections load credentials from token files.
`gmux-platform::unix::local` provides explicit daemon startup and completed shutdown,
separate from connecting. GTK and `gmux serve --background` share its detached
launcher and readiness check. Daemon checkpoint formats and restoration stay in
`gmux-daemon`; GTK organization and saved connections stay with their owners.

## GTK organization

`crates/gtk/src/client/` owns workspaces, assignments, split proportions, filters,
and window defaults. Its store shares immutable snapshots between GTK windows
and saves organization atomically. Its connection adapter combines execution
snapshots with GTK organization, maps qualified daemon IDs to window handles,
and chooses tab order. It uses the client library for connections and execution.
Workspace operations never cross the execution protocol.

Request workers return daemon execution replies or GTK organization results.
Worker channels carry typed replies; execution snapshots are deserialized before
reaching GTK. Other control responses retain their JSON values.
The window applies its current organization when combining replies. Remote
connection edits use the selected daemon's queue and carry its pinned identity;
removal uses the configuration queue so it can cancel a stalled edit.
Closing a window cancels execution requests and leaves its pending organization
saves with the application. Finished workers are reclaimed without blocking GTK;
application exit waits for remaining saves.

Clients prune handles after reconciling views and finishing outstanding commands.
A burst of stream snapshots produces one combined GTK view per poll. Cached
execution survives disconnection until a new snapshot confirms what changed.
