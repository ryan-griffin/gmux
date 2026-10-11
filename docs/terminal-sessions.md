# Background terminal sessions

`gmux-daemon` owns PTYs, shells, terminal state, tabs, and workspaces. Closing a
window or losing the frontend leaves those sessions running. Reopening Gmux
reconnects to the same processes. Closing a pane terminates its process; an
exited shell removes its pane.

Quit Gmux, Ctrl+Shift+Q, or `gmux quit` stops every terminal and the daemon.
SIGTERM and SIGINT detach a frontend or shut down the daemon, depending on which
process receives the signal.

## Multiple windows

Windows share tab order, names, split topology, workspace membership, unread
alerts, and PTY output. The daemon broadcasts layout revisions after mutations
and process exits. Windows ignore older revisions, including delayed replies.

Each window keeps its active tab and pane, workspace filter, sidebar search and
visibility, split proportions, zoom, Find query, selection, scroll position,
and window dimensions. One terminal engine serves all views of a pane. Input
from any view reaches the same PTY.

The most recently focused client viewing a pane controls its PTY size. Other
views remember their requested sizes and apply them when they become the owner.
Presentation saves provide defaults for new windows without replacing shared
layout topology or other live windows' choices.

## Ownership and scheduling

The daemon reads nonblocking PTYs in one `poll` loop, with a 512 KiB read budget
per ready pane and an 8 MiB pending-input limit per pane. An idle detached daemon
blocks in `poll`. Signals, sockets, and disk-worker completions wake it.
Attached views receive viewport batches at most every 16 ms and process metadata
once a second. Hidden panes keep parsing output without extracting viewports.

GTK owns rendering, fonts, the clipboard, and accessible text. An `eventfd` wakes
the frontend when packets or requests arrive. Both sides process bounded batches
to keep input and other panes responsive.

A disk worker writes daemon state outside the PTY owner thread. Later layout
mutations wait for earlier command saves; read-only requests and PTYs continue.
Adjacent automatic layout saves coalesce. Workspace membership changes take
effect after the catalog save succeeds, followed by a session save.

## Persistence and recovery

State lives under `$XDG_CONFIG_HOME/gmux/`, normally `~/.config/gmux/`:

- `settings.json`: preferences, with omitted settings using defaults.
- `workspaces.json`: workspace definitions.
- `session.json`: tabs, directories, layout, and presentation defaults.
- `scrollback/`: daemon-owned history checkpoints.
- `daemon.log`: startup and runtime errors.

Missing state files use defaults. Invalid or unreadable JSON produces an error
and remains untouched. Writers use a lock, a private temporary file, syncing,
and atomic rename so readers see a complete state file.

This is a pre-alpha project. Gmux reads the current saved format and does not
migrate old layouts or settings. Move an outdated JSON file aside to start with
defaults. Build and install the frontend and daemon from the same revision.

## Scrollback checkpoints

Explicit Quit and orderly daemon shutdown checkpoint primary-screen text history.
A restart restores tabs and directories with fresh shells. Commands, jobs,
terminal modes, hyperlinks, and alternate-screen contents do not resume.
Unicode, wrapped lines, colors, and text attributes survive. Indexed and default
colors follow the current theme; explicit RGB colors retain their values.

Each checkpoint contains a Ghostty snapshot with Gmux's format version, exact
Ghostty source revision, pane ID, and payload length. `GHOSTTY_PREFIX` must point
to a build matching the pin in `scripts/bootstrap-ghostty.py`.
Missing, mismatched, or damaged checkpoints log a warning and leave that pane
with empty history. Other panes still restore.

Limits are 8 MiB per pane, 64 MiB per layout, and 128 panes. Snapshot decoding has
a 64 MiB allocation budget per pane; formatted history replay has an 8 MiB limit.
A pane that exceeds a limit may lose its saved history. A crash or power loss can
lose output since the last orderly shutdown.

The worker syncs checkpoint files before publishing the layout that references
them. A failed publication leaves the previous generation usable. Cleanup
removes unreferenced checkpoints after a successful save. Removing `scrollback/`
clears saved history without changing tabs.

## Frontend transport

The control socket is `$GMUX_SOCKET`, normally `$XDG_RUNTIME_DIR/gmux.sock`.
Its `.stream` companion carries persistent frontend connections, and `.ui`
routes GUI requests. A lock serializes daemon startup; sockets are owner-only.
Protocol version checks reject mismatched binaries.

Viewport packets contain JSON metadata and fixed-width cell rows. A new
subscription receives a complete viewport; later packets carry changed rows.
Each client owns its subscriptions, search, selection, paste assembly, and copy
replies. Stream packets are limited to 16 MiB; commands to 1 MiB. Pastes use
UTF-8 chunks with a 4 MiB total limit.

CLI operations such as `list`, `new`, `split`, `send`, `read`, workspace changes,
and `quit` also work with the window closed. GUI actions and screenshots need
an open window. See `gmux --help` for commands.

## Notifications

The daemon tracks unread alerts with sequence numbers so a delayed acknowledgement
cannot clear a newer alert. The frontend sends desktop notifications and
suppresses them for the focused pane while its window is active.

While detached, the daemon queues up to 64 alerts per pane within a 512 KiB text
budget. Queued alerts arrive on reconnect. Unread state and queued messages stay
in memory and do not survive daemon restarts.
