# Background terminal sessions

Closing the window detaches the GTK frontend. `gmux-daemon` keeps the same PTYs,
shells, commands, terminal modes, scrollback, and pane IDs running. Opening Gmux
again reconnects to those sessions. A frontend crash also leaves them running.
Closing a pane still terminates that pane, and an exited shell removes its pane.

Use **Quit Gmux** in the main menu, Ctrl+Shift+Q, or `gmux quit` to stop the daemon
and its terminals. The GUI confirms Quit when a foreground job is running. The
CLI command is an explicit shutdown and does not show a dialog. A layout save failure produces a warning but still closes the terminals and daemon. SIGTERM/SIGINT
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
bounds each pane's pending input to 8 MiB. SIGCHLD wakes it through `signalfd` to
reap exited shells. An idle detached daemon blocks in `poll`; it has no periodic
16 ms wakeup and no reader or writer thread per pane. A render hold schedules
expiry checks until synchronized output completes. Attached frontends receive
fallback process metadata once a second and viewport batches at most once every
16 ms. Shell directory messages refresh the live process directory during the
same PTY update. Positive poll waits round up to avoid spinning before deadlines.
An `eventfd` wakes a GLib file-descriptor source when stream packets or GUI socket
requests arrive. There is no periodic frontend update poll. The source drains
bounded batches and rearms itself if work remains, visiting only panes with
received updates. Writer progress also wakes it to retry commands held by queue
backpressure. Focus changes update the daemon immediately; presentation saves
coalesce behind a two-second timer scheduled only after a change. Tab switches and
layout changes update viewport subscriptions. Resize indicators use their own
dismissal timers. Directory metadata arrives through the stream;
splitting a pane and checking for busy jobs still request fresh process state.
Activity in one pane does not rebuild idle panes' process metadata between the
once-a-second refreshes. Runtime control requests and settings writes use one
worker outside GTK's main thread. Adjacent presentation saves coalesce; commands
preserve queue order. Close and Quit check all affected panes in one asynchronous
busy-process request. Startup connects before presenting the window.

Git HEAD watches supply branch updates, and ancestor directory watches discover
repository creation, removal, and nested repositories. Two workers publish each
directory's result independently, share lookups between tabs, and coalesce queued
refreshes. A HEAD event needs one lookup for all tabs using that HEAD. Older
completions cannot replace a newer result for the same HEAD. Git commands have a
two-second timeout and bounded output; timed-out process groups are killed
and reaped. Working watches cause no idle Git launches. A 30-second fallback
retries failed watches and lookups.

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
These changes use protocol version 6, which includes immediate focus updates.
Restart an existing daemon before using the rebuilt binaries.

Clipboard and text pastes use messages of at most 16 KiB of UTF-8 text. The
daemon assembles a complete paste of up to 4 MiB before asking Ghostty to encode
it, preserving one bracketed-paste boundary and preventing incomplete transfers
from reaching a shell. The frontend bounds queued paste text to 8 MiB, and the
daemon bounds pending PTY input to 8 MiB. PTY writes yield after 512 KiB per poll.
Oversized or malformed frontend commands report errors without disconnecting.

`src/remote.rs` contains frontend session handles with cached cells and frame
metadata. Input, key and mouse encoding requests, paste, resize, theme changes,
and scrolling go to the daemon. Ghostty encodes them against the actual live
terminal modes. The daemon also owns history search and tracked selection
endpoints. The frontend owns fonts, glyph shaping, rendering, clipboard, Find
controls, accessible viewport text, and input methods. It uses the [GSK renderer](renderer.md).

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
Packets with no changed cells, frame metadata, notifications, search or selection
state, or scroll acknowledgement are suppressed. Key handlers draw when changed
terminal state arrives. The frontend applies packets in order before rendering.
The daemon still scans
and retains a viewport copy for each visible pane, but updates that copy only
for changed rows. It encodes packets directly into the socket output buffer and
reuses its polling buffers. It does not copy scrollback or send pixels through
the socket.

Bounded queues provide backpressure. A frontend that exceeds the daemon's 32 MiB
pending-output limit detaches while its terminal sessions continue. Reopening
resynchronizes complete viewports. If the backend disappears, the frontend
exits and the next launch starts a new daemon. Typed layout and workspace commands
share one daemon implementation for the GUI and detached CLI. Each command
returns authoritative layout, catalog, and process metadata for reconciliation.
The frontend persists presentation state and divider proportions without replacing
daemon topology. Completed live changes return their resulting state even if
saving the layout fails, with a separate persistence warning.

Missing settings, workspace, and session files use defaults. Invalid or unreadable
files produce a recovery error and remain unchanged. Startup failures show the
file path and recovery instructions in a window. Saves validate the existing
file before replacing it, so a file damaged after startup remains available for
repair. Move it aside to use defaults, or repair it before restarting or saving.

Find scans all retained rows in batches of 64 and yields between batches. It
joins soft-wrapped rows and preserves hard line breaks. A bounded unmatched
suffix carries queries across rows and scan batches, including wide characters.
Output
and resize invalidate results for a fresh scan. Navigation sends the viewport to
an offscreen match. Selection endpoints use Ghostty tracked references and survive
scrolling and terminal updates. Dragging a selection near or beyond the top or
bottom edge scrolls history while the pointer stays there. Moving farther outside
increases the speed. Releasing the button or returning inside stops scrolling.
Starting a selection cancels GTK's existing momentum and overscroll animation
so it cannot pull the viewport back toward an edge. Normal kinetic scrolling
resumes when the selection gesture ends or is cancelled.
During a drag, the frontend draws the range from the current pointer and viewport;
delayed daemon replies cannot restore an older highlight. Selection commands
coalesce once per GTK frame. Release sends the final endpoint before subsequent
input or Copy.
Ghostty formats copied text across history,
including wrapped lines. Copy responses travel separately from viewport packets
and are limited to 4 MiB.

## Validation

```sh
python3 scripts/check.py
python3 scripts/check.py --native
python3 scripts/control-smoke-test.py
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

The scheduling test checks native tab and split-pane focus before persistence,
prompt directory messages, zero Git launches while idle, branch updates during
a stuck lookup, and timeout recovery. Branch checks also cover repository
discovery through `.git` creation without a polling delay.

The follow-up review also passed the command palette accessibility test, including
retained search focus, disabled command handling, selection and no-result
announcements, and the search-to-list relation. Terminal Find, fonts, resizing,
workspace behavior, and the pane lifecycle audit passed separately.
