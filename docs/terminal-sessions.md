# Background terminal sessions

Closing the window detaches the GTK frontend. `gmux-daemon` keeps the same PTYs,
shells, commands, terminal modes, scrollback, and pane IDs running. Opening Gmux
again reconnects to those sessions. A frontend crash also leaves them running.
Closing a pane still terminates that pane, and an exited shell removes its pane.

Use **Quit Gmux** in the main menu, Ctrl+Shift+Q, or `gmux quit` to stop the daemon
and its terminals. The GUI confirms Quit when a foreground job is running. The
CLI command is an explicit shutdown and does not show a dialog. SIGTERM/SIGINT
sent to the frontend detach it; sent to the daemon they stop its sessions.

A stopped daemon cannot resume arbitrary commands. At this stage, daemon restart
restores the saved layout and working directories with fresh shells when Restore
tabs is enabled. Output and scrollback survive window closure and frontend
crashes, but not daemon shutdown or a reboot. Disk terminal checkpoints are not
implemented. Interrupted commands are never replayed automatically.

The daemon starts automatically with the frontend. `scripts/run.sh` builds both
binaries; `scripts/install.sh` installs both. To start just the backend for shell
automation, run `target/debug/gmux-daemon`. `gmux list`, `new`, `split`, `send`,
`read`, `close`, workspace changes, and focus selection work with the window
closed. Screenshot, menus, search controls, and other GUI actions require an
open window. Desktop alerts arrive through the frontend. With it closed, the
daemon queues the latest 64 alerts per pane for delivery on reconnect. CLI and
terminal OSC alerts use the same bounded queue. Delivering an alert does not
clear its unread indicator. The daemon owns unread state across frontend
reconnects; selecting a pane acknowledges the alerts the frontend has seen.
Sequence numbers prevent a delayed acknowledgement from clearing a newer alert.
Unread state and queued alerts are kept in memory, not across daemon restarts.

## Ownership and scheduling

`src/daemon.rs` owns the live layout and `TerminalSession` objects.
`src/session.rs` owns PTYs, Ghostty engines, protocol replies, process titles,
working directories, and scrollback. Neither module depends on GTK, Pango, or
Cairo. The daemon binary links libghostty-vt and GLib without a graphics stack.

The daemon uses nonblocking PTY descriptors in one `poll` loop. It reads at most
512 KiB per ready pane before servicing other panes, handles partial writes, and
bounds each pane's pending input to 1 MiB. SIGCHLD wakes it through `signalfd` to
reap exited shells. An idle detached daemon blocks in `poll`; it has no periodic
16 ms wakeup and no reader or writer thread per pane. A render hold schedules
expiry checks until synchronized output completes. Attached frontends receive
process metadata once a second and viewport batches at most once every 16 ms.
An `eventfd` wakes a GLib file-descriptor source when stream packets or GUI socket
requests arrive. There is no periodic frontend update poll. The source drains
bounded batches and rearms itself if work remains, visiting only panes with
received updates. Writer progress also wakes it to retry commands held by queue
backpressure. Idle frontends still have a two-second settings and Git refresh.
Tab switches and layout changes update viewport subscriptions. Resize indicators
use their own dismissal timers. Directory metadata arrives through the stream;
splitting a pane and checking for busy jobs still request fresh process state.
Activity in one pane does not rebuild idle panes' process metadata between the
once-a-second refreshes.

Sessions use this same nonblocking I/O path in production and headless unit
tests. Explicitly closing a session or dropping its owner terminates the process.

## Frontend transport

The daemon owns `$GMUX_SOCKET`, normally `$XDG_RUNTIME_DIR/gmux.sock`. Its
`.stream` companion carries one persistent frontend connection; `.ui` routes
GUI socket methods to the attached frontend. A `.lock` file serializes daemon
startup, and sockets have owner-only permissions. The frontend and daemon use
one protocol version and reject mismatched binaries. Build and install both
binaries from the same revision. Gmux does not maintain older protocol versions
or migrate older saved formats during pre-release development.

`src/remote.rs` contains frontend session handles with cached cells and frame
metadata. Input, key and mouse encoding requests, paste, resize, theme changes,
and scrolling go to the daemon. Ghostty encodes them against the actual live
terminal modes. The frontend owns fonts, glyph shaping, rendering, selection,
search, clipboard, and input methods. It uses the [GSK renderer](renderer.md).

Only panes in the displayed tab subscribe to viewport updates. Hidden and
unattached panes continue parsing output and answering terminal requests without
automatic viewport extraction. An explicit `gmux read` still extracts text.
Each packet carries JSON metadata and binary terminal cells for changed rows.
Frame metadata includes the total scrollable rows, viewport offset, and visible
row count. Native GTK scrollbars use these values without copying the history.
Thumb movement sends an absolute row and request serial. The frontend keeps the
requested position until the daemon acknowledges it, and retries the latest
position if its command queue is full.
Initial attachment, a new subscription, and changed dimensions send a full
viewport. Subsequent packets compare rows against the last transmitted cells.
The frontend applies packets in order before rendering. The daemon still scans
and retains a viewport copy for each visible pane, but updates that copy only
for changed rows. It encodes packets directly into the socket output buffer and
reuses its polling buffers. It does not copy scrollback or send pixels through
the socket.

Bounded queues provide backpressure. A frontend that exceeds the daemon's 32 MiB
pending-output limit detaches while its terminal sessions continue. Reopening
resynchronizes complete viewports. If the backend disappears, the frontend
exits and the next launch starts a new daemon. Pane creation records a provisional
layout before responding, so a frontend crash during creation cannot orphan a
running pane. Normal UI changes then save the requested layout and names.

## Validation

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
python3 scripts/daemon-smoke-test.py
GTK_A11Y=none dbus-run-session -- python3 scripts/daemon-smoke-test.py --native
GTK_A11Y=none dbus-run-session -- python3 scripts/smoke-test.py --native
GTK_A11Y=none dbus-run-session -- python3 scripts/notifications-smoke-test.py
```

The daemon test covers actual PTYs without a display, background output, idle CPU
usage, one-row packets, hidden-pane output without automatic snapshots,
subscription resync, retained scrollback, terminal replies, shutdown, and layout
restore. Native checks cover titlebar close handling, commands completing while
detached, identical shell PIDs after reopen, frontend crash recovery, and Quit
closing both processes. On the development machine the idle daemon consumed zero
CPU ticks over a 1.1 second interval. This is an idle check, not a throughput or
memory benchmark. [Whole-application measurements](performance.md) cover CPU,
resident memory, and native input-to-render latency with three busy panes.

The notification test covers unread state after delivery and reconnect, GUI
acknowledgements, stale acknowledgements racing with new alerts, and the latest
64 messages from both CLI and OSC producers. It also verifies replies to 48
concurrent GUI requests when the event source processes 16 requests per turn.
Unit tests cover coalesced wakes and acknowledgement retries under backpressure.

The follow-up review also passed the command palette accessibility test, including
retained search focus, disabled command handling, selection and no-result
announcements, and the search-to-list relation. Terminal Find, fonts, resizing,
workspace behavior, and the pane lifecycle audit passed separately.
