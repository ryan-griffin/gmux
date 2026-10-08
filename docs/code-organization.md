# Code organization

The GTK application and headless daemon keep their state in parent modules.
Child modules implement methods on those owners, with `pub(super)` visibility
where other parts of the owner need them. This follows the existing workspace UI
pattern and keeps UI types out of the daemon.

## GTK application

`src/app.rs` contains `Ui`, `Tab`, startup and shutdown cleanup, search feedback,
and notification handling. Its child modules group the larger workflows.

| Module | Responsibility |
| --- | --- |
| `app/window.rs` | Construct the window, widgets, menus, and update callbacks. |
| `app/tabs.rs` | Create and restore tabs, manage pane layouts, and synchronize focus. |
| `app/actions.rs` | Register window actions and keyboard accelerators. |
| `app/dialogs.rs` | Close and Quit confirmation, folder selection, tab naming, and the command palette. |
| `app/preferences.rs` | Preferences, settings persistence, and terminal settings updates. |
| `app/backend.rs` | Queue backend requests, reconcile returned state, save presentation, and handle UI socket methods. |
| `app/branches.rs` | Repository watches, branch lookup scheduling, and branch labels. |
| `app/workspaces.rs` | Workspace dialogs, filters, menus, and sidebar search. |

`src/terminal.rs` builds the pane widgets and manages rendering, selection, and
session updates. `terminal/input.rs` installs keyboard, pointer, selection,
clipboard, focus, and scrolling callbacks through separate setup methods.
The callbacks keep weak references to the terminal.

## Terminal rendering

| Module | Responsibility |
| --- | --- |
| `src/renderer.rs` | Build GSK scenes and retain row, text-batch, and glyph caches. |
| `src/render.rs` | Shape glyphs with Pango and generate cell graphics, decorations, and cursors. |
| `src/terminal_canvas.rs` | Implement the scrollable terminal widget and its accessible text interface. |
| `src/accessible_terminal.rs` | Map viewport cells to accessible text, caret, selection, and geometry. |

The [renderer design](renderer.md) explains paint order, invalidation, and scroll
cache reuse.

## Headless daemon

`src/daemon.rs` owns sessions, the live layout, notifications, and state helpers.

| Module | Responsibility |
| --- | --- |
| `daemon/event_loop.rs` | Poll PTYs, sockets, signals, and disk-worker completions. |
| `daemon/frontend.rs` | Buffer stream traffic, dispatch terminal input, assemble pastes, and publish viewports. |
| `daemon/commands.rs` | Execute layout and control commands and track their persistence stages. |

The command module represents prepared saves and pending save stages explicitly.
[Session scheduling](terminal-sessions.md#ownership-and-scheduling) describes
their ordering, deferred workspace changes, and failure behavior.

## Shared data and protocols

`src/ghostty_bridge.h` is the source of truth for the C/Rust engine boundary.
It defines `GmuxCell`, `GmuxFrame`, `GmuxScrollback`, event and writer callbacks,
and all exported function signatures using C types. `src/ghostty_bridge.c` includes that
header and keeps upstream Ghostty types private, including the opaque engine.
The C compiler rejects conflicting definitions and exported functions without
header prototypes.

`build.rs` uses bindgen to generate the Rust declarations in `OUT_DIR` for
Cargo's target architecture, using the C compiler's flags and include paths.
The generated code checks each struct's size, alignment, and every member
offset at compile time against Clang's C layout.
These checks run during ordinary builds and tests on x86_64 and aarch64 Linux;
they do not run a host executable or assume the host's pointer width. Edit the
header for ABI changes, and keep ownership and callback handling in
`src/engine.rs`. Generated declarations are not tracked.

`Frame` and `Scrollback` retain their serde derives and field order. Viewport
packets still serialize JSON metadata and individual cell fields rather than
copying C struct memory or padding.

`src/model.rs` contains settings, layouts, sessions, and workspace definitions.
Its catalog validation removes invalid IDs, blank names, and duplicate IDs.
`src/storage.rs` reads state files and publishes durable atomic writes.
`storage::load_catalog()` applies the same validation for the UI and daemon.
`src/persistence.rs` runs the daemon's ordered disk worker.

`src/control.rs` defines layout commands, results, and internal control requests.
Decoded requests determine whether a command must wait for an earlier save.
`src/transport/requests.rs` defines typed frontend stream commands and paste
options; `src/transport.rs` encodes viewport packets and enforces wire limits.
The frontend queues typed commands in `src/remote.rs` and serializes them at the
socket boundary. Invalid stream fields retain enough routing information to
report an error to the affected pane.

Use the [testing guide](testing.md) for Rust and isolated Wayland checks after
changes across these modules.
