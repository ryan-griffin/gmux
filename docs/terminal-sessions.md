# Background terminal sessions

Closing the window detaches the GTK frontend. `gmux-daemon` keeps the same PTYs,
shells, commands, terminal modes, scrollback, and pane IDs running. Opening Gmux
again reconnects to those sessions. A frontend crash also leaves them running.
Closing a pane still terminates that pane, and an exited shell removes its pane.

Each window has a separate frontend connection. Tab creation, closure, names,
split topology, workspace catalog and membership, unread state, and PTY output
are shared. The daemon broadcasts versioned layout snapshots after mutations
and process exits, encoding each broadcast once for all connections.
A window ignores snapshots older than the latest revision it has applied,
including delayed command replies. Commands that create tabs or
panes reveal their results in the requesting window.

Active tabs and panes, workspace filters, sidebar controls, split ratios, zoom,
terminal search and selection, scroll position, fullscreen, and window dimensions
belong to each window. Each connection has its own viewport subscriptions,
search worker state, tracked scroll anchor and selection, paste assembly, and
clipboard replies. One terminal engine and PTY serve every view of a pane.
Ghostty render dirtiness has one consumer, so views share its normal render cache
and compare output against their own last sent cells. Synchronized output holds
a separate frozen viewport for each existing view.

The most recently focused client viewing a pane supplies its PTY dimensions.
Other clients remember their requested dimensions and apply them when focused.
Repeated requests with unchanged grid, cell, and pixel dimensions skip terminal
and PTY resizing.
Input from every client reaches the same PTY. Terminal applications still have
one grid size; a differently sized window renders that shared grid in its own
allocation. A detached window releases only its connection and views.
Presentation saves retain defaults for the next window and never replace shared
tab topology or another live window's choices. A new window starts at the bottom
of terminal history with fresh search and selection state.

Use **Quit Gmux** in the main menu, Ctrl+Shift+Q, or `gmux quit` to stop the daemon
and its terminals. The GUI confirms Quit when a foreground job is running. The
CLI command is an explicit shutdown and does not show a dialog. A layout or scrollback save
failure produces a warning but still closes the terminals and daemon. SIGTERM
and SIGINT sent to the frontend detach it; sent to the daemon they stop its
sessions.

A stopped daemon cannot resume arbitrary commands. Daemon restart
restores the saved tabs, layout, and working directories with fresh shells.
Window closure and frontend crashes keep the live scrollback.
Quit and orderly daemon shutdown checkpoint history for later restoration,
including after a reboot. Interrupted commands are never replayed automatically.

The daemon starts automatically with the frontend. `scripts/run.sh` builds both
binaries; `scripts/install.sh` installs both. To start just the backend for shell
automation, run `target/debug/gmux-daemon`. `gmux list`, `new`, `split`, `send`,
`read`, `close`, workspace changes, and focus selection work with the window
closed. Screenshot, menus, search controls, and other GUI actions require an
open window. Desktop alerts arrive through the frontend. With it closed, the
daemon queues up to 64 recent alerts per pane within a 512 KiB text budget for
delivery on reconnect. An individual alert larger than that budget is truncated
at a UTF-8 boundary. CLI and terminal OSC alerts use the same bounded queue.
Delivering an alert does not clear its unread indicator. The daemon owns unread
state across frontend reconnects; selecting a pane acknowledges the alerts the
frontend has seen.
Sequence numbers prevent a delayed acknowledgement from clearing a newer alert.
Unread state and queued alerts are kept in memory, not across daemon restarts.
Alerts from a newly created pane can arrive before its command reply creates the
frontend widget. The frontend delivers these with the title "Terminal" and keeps
the pane ID as their focus target. Clicking before the widget exists does not
change focus.

## Ownership and scheduling

The headless daemon owns live terminal state and PTYs. GTK owns the visible
window, clipboard, fonts, rendering, and accessible text. See
[code organization](code-organization.md) for their module boundaries.

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

Daemon saves run on one disk worker. Commands wait for their save result while
the owner thread continues processing PTYs, viewport updates, and read-only
requests. Later layout and workspace mutations wait in order. Adjacent automatic
layout snapshots coalesce; command saves do not. Workspace membership changes
take effect after the catalog save succeeds, then advance to a session save.
Each pending command tracks its current save stage and replies after the save
sequence finishes or fails. Preparing a session snapshot is separate from
submitting it to the worker.

## Scrollback checkpoints

The daemon captures Ghostty terminal snapshots on explicit Quit and orderly
SIGTERM/SIGINT shutdown. There is no periodic checkpoint. A daemon crash, SIGKILL,
or power loss can lose output since the last orderly shutdown. Existing layout
saves retain references to the last checkpoint for surviving panes. Live window
detach and reattach continue using the same PTYs, processes, and terminal state.

Snapshots serialize terminal state and history through
[`ghostty_snapshot_encode_buf`](https://github.com/ghostty-org/ghostty/blob/8f0dd3709050b1026f6324368805033197d8b4a5/include/ghostty/vt/snapshot.h).
They are distinct from `gmux_engine_snapshot`, which extracts viewport cells for
the frontend. Ghostty's binary format version 1 has no stability guarantee yet.
Gmux wraps each payload with its own format version, the exact Ghostty source
revision from the build pin, its pane ID, and its payload length. A different
source revision is incompatible even when Ghostty's format number matches.
`GHOSTTY_PREFIX` builds must use a library matching that pin. No snapshot format
migration is attempted.

Startup decodes and validates the whole payload, including Ghostty's record CRCs
and FINISH marker. It extracts primary-screen text and cell rendition with soft
wraps joined, then feeds them into a fresh terminal at the saved dimensions before spawning
the new shell. The shell starts on the next line with default input modes and
parser state, using the current theme and cursor preference. Resizing reflows
the restored text. History search and copying work across wrapped Unicode lines.
Historical foreground/background colors and text attributes survive. Indexed
colors retain their indices and use the current theme's ANSI palette or the
standard extended palette. Default colors follow the current theme; explicit
RGB colors keep their values across theme changes. Saved OSC palette and default
color overrides are discarded. Gmux does not install the saved palette into the
new shell. The existing snapshot format and source pin remain unchanged, so
compatible checkpoints saved by earlier builds also restore with color.

The pinned VT formatter runs with every terminal/screen extra disabled. Before
replay, Gmux validates the complete output as UTF-8 text, CRLF line breaks, and
only the formatter's SGR rendition operations. It rejects other controls and
incomplete sequences before changing the fresh engine. Replay ends with an SGR
reset before moving to the new shell's row, so historical rendition cannot
bleed into its prompt. Hyperlinks, graphics, alternate-screen contents, titles,
old process modes, and parser continuations are discarded.

The pinned formatter defers unwritten blank cells until later text, which can
give gaps after colored text the preceding rendition. Gmux formats disjoint
selection ranges at those gaps so they start with default rendition. Ghostty
still formats Unicode clusters, soft wraps and hard line breaks. Invalid page
coordinates fail restoration before replay.

Trailing unstyled whitespace and trailing blank rows are trimmed. Interior
blank lines survive. On rows with text, styled spaces and background-only cells
keep their rendition; background-only cells replay as spaces. Blank rows without
text carry no saved rendition. Copying still trims trailing spaces. The new
process can clear its screen or history during its own startup.

The checkpoint budget is 8 MiB of serialized state per pane, 64 MiB across the
layout, and 128 panes, visited in layout order. Gmux skips panes that exceed
these limits and reports a persistence warning. It tracks at most 64 KiB of
unfinished parser input. A pane with an unavailable continuation also gets
skipped. Snapshot decoding uses a 64 MiB allocation budget per pane; formatted
UTF-8/SGR replay, including its clean ending, has an 8 MiB limit. Alternating
styles can exceed that limit even when the binary snapshot fits. Restoration
validates the file header and remaining layout budget before reading its payload,
then decodes once and formats disjoint history ranges. These bounds can omit a
large history instead of keeping only its newest lines.
Capture copies bytes in memory on the PTY owner thread
at shutdown; file reads at startup finish before processing PTYs. The ordered
disk worker handles writes, syncing, and cleanup while the owner keeps servicing
PTYs. Output arriving after the Quit capture is outside that checkpoint.

The daemon owns `scrollback/` under the Gmux configuration directory. It creates
the directory with mode 0700 and snapshot files with mode 0600. Saved layouts
reference opaque filenames containing a random generation token and pane ID,
so reused IDs cannot inherit an unrelated checkpoint. The worker syncs snapshot
files and their directory before atomically publishing `session.json`. It holds
the session writer lock throughout publication and cleanup. The layout publication
commits the new generation. An interrupted publication leaves
the previous layout and its history usable. The old and new generations can
temporarily occupy twice the 64 MiB payload budget, plus file headers.

After a successful layout save, the worker deletes checkpoints for removed panes
and old generations, then syncs the directory. Startup and the next checkpoint
also reclaim orphaned files and interrupted temporary writes. Startup cleanup
uses the committed disk layout, so a failed shell spawn or disabling Restore
tabs cannot discard saved history before a replacement layout is committed.
Closing a pane, closing its tab, or an exited shell removes its checkpoint reference. Snapshot
failures do not prevent saving an otherwise valid layout or quitting. Quit waits
for the save result, returns any warning, and freezes layout mutations while
shutdown is in progress.

Missing, incompatible, corrupt, truncated, oversized, or unreadable checkpoints
log a warning and start the affected pane with empty history. They do not block
restoration of other panes. Gmux validates filenames, rejects symlinks and
nonregular snapshot files, and ignores files absent from the saved layout.
Removing `scrollback/` clears the disk history without changing the saved tabs.
Settings, workspace, and layout recovery still follows the protected-state
rules below.

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
`.stream` companion accepts independent persistent frontend connections; `.ui`
routes GUI socket methods through the application's window registry. A `.lock` file serializes daemon
startup, and sockets have owner-only permissions. The frontend and daemon use
one protocol version and reject mismatched binaries. Build and install both
binaries from the same revision. Gmux does not maintain older protocol versions
or migrate older saved formats during pre-release development.
The protocol version is defined in [`src/transport.rs`](../src/transport.rs).
When an upgrade changes it, quit the existing daemon before launching the
matching rebuilt binaries.

`gmux rpc '{"method":"windows"}'` lists window IDs. GUI requests can include a
`window` ID, for example `{"method":"list","window":2}`. Requests without it
target the active window, falling back to the newest remaining window.
`{"method":"new-window"}` creates a window. Unknown window IDs return an error.
Reading a visible pane returns that window's viewport, including its scroll
position. Hidden panes and headless reads use the daemon's live bottom viewport.

`ipc::Endpoint` keeps a daemon's control and stream paths together.
`SessionManager::connect_at` attaches to an existing endpoint without launching
a daemon or loading its layout and workspace files locally. Windows obtain
those objects from daemon snapshots and resolve implicit command targets locally
before sending requests. This supports future forwarded or remote transports
without coupling live window choices to the daemon transport. Saved presentation
fields supply startup defaults. The current launcher still connects to the local daemon; remote connection configuration is not implemented.

Clipboard and text pastes use messages of at most 16 KiB of UTF-8 text. The
daemon assembles a complete paste of up to 4 MiB before asking Ghostty to encode
it, preserving one bracketed-paste boundary and preventing incomplete transfers
from reaching a shell. The frontend bounds queued paste text to 8 MiB, and the
daemon bounds pending PTY input to 8 MiB. PTY writes yield after 512 KiB per poll.
Oversized or malformed frontend commands report errors without disconnecting.
Stream commands and internal control requests use shared Rust types for their
JSON fields. The daemon decodes each request before dispatch and
rejects invalid types and out-of-range integers. A malformed command cancels an
incomplete paste; a malformed attach disconnects the frontend.

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
Frontend sizing, daemon resizing, and packet decoding share limits of 1,000
columns, 1,000 rows, and 131,072 total cells. A large frontend allocation reduces
the row count to fit the total cell budget.
The daemon scans and retains a viewport copy for each visible pane, but updates
it only for changed rows. It encodes packets directly into the socket output buffer and
reuses its polling buffers. It does not copy scrollback or send pixels through
the socket.

Bounded queues provide backpressure. A frontend that exceeds the daemon's 32 MiB
pending-output limit detaches while its terminal sessions continue. Reopening
resynchronizes complete viewports. The frontend incoming queue retains at most
64 packets within a 32 MiB budget for decoded allocations. The reader waits for
space without discarding packets, preserving notifications and acknowledgements.
This budget excludes the packet being decoded and the installed pane viewports.
If the backend disappears, the frontend exits and the next launch starts a new
daemon. Typed layout and workspace commands
share one daemon implementation for the GUI and detached CLI. Each command
returns authoritative layout, catalog, and process metadata for reconciliation.
The frontend persists presentation state and divider proportions without replacing
daemon topology. Completed live changes return their resulting state even if
saving the layout fails, with a separate persistence warning.

The daemon does not reuse allocated tab, pane, or workspace IDs while it runs.
The frontend records terminal exits while command replies are pending and removes
those panes from older returned layouts, including panes that exit before their
widgets exist. Delayed replies also preserve later tab, pane, and workspace
selections when those targets still exist.

Missing settings, workspace, and session files use defaults. Invalid or unreadable
files produce a recovery error and remain unchanged. Startup failures show the
file path and recovery instructions in a window. Saves validate the existing
file before replacing it, so a file damaged after startup remains available for
repair. Move it aside to use defaults, or repair it before restarting or saving.
`src/storage.rs` owns these disk operations. The UI and daemon load workspace
catalogs through the same validation, retaining the first valid definition for
each ID without changing the file during a read.
Each state file has a separate writer lock and an exclusively created temporary
file in the same directory. Saves flush and sync the temporary file, rename it
over the destination, then sync the directory. Failed writes remove their
temporary files. Published files have owner-only read and write permissions.

Find scans all retained rows in batches of 64 on the viewport update schedule
and yields between batches. It joins soft-wrapped rows and preserves hard line
breaks. A bounded unmatched
suffix carries queries across rows and scan batches, including wide characters.
Output and resize invalidate results for a fresh scan. Navigation sends the
viewport to an offscreen match. Selection endpoints use Ghostty tracked references and survive
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
Ghostty formats copied text across history, including wrapped lines. Copy
responses travel separately from viewport packets
and are limited to 4 MiB.

## Validation

Use the [testing guide](testing.md) to run all checks or select the daemon,
control, scrollback, recovery, scheduling, and notification tests in the isolated fixture.

The daemon test covers actual PTYs without a display, background output, idle CPU
usage, one-row packets, hidden-pane output without automatic snapshots,
subscription resync, retained scrollback, terminal replies, shutdown, and layout
restore. Native checks cover titlebar close handling, commands completing while
detached, identical shell PIDs after reopen, frontend crash recovery, and Quit
closing both processes. On the development machine the idle daemon consumed zero
CPU ticks over a 1.1 second interval. This is an idle check, not a throughput or
memory benchmark. [Whole-application measurements](performance.md) cover CPU,
resident memory, and native input-to-render latency with three busy panes.

The scrollback test covers Quit/SIGTERM persistence, fresh shell PIDs and input,
exact indexed/RGB colors and text attributes, resets without prompt bleed,
Unicode and wrapped history, file permissions, PTY progress during a blocked
checkpoint commit, incompatible versions and source revisions, ownership,
corruption, truncation, trailing bytes, deleted-pane cleanup, and preserving
committed history when startup cannot restore the saved panes. Unit tests also
cover fresh-terminal mode and parser state, reflow and copying, bounded envelopes,
orphan cleanup, snapshot symlink rejection, and concurrent checkpoint publication.

Validation on 2026-10-08 used main `e7b6d45` and bootstrapped Ghostty revision
`8f0dd3709050b1026f6324368805033197d8b4a5` in this worktree. The scrollback
smoke test checks that saved envelopes declare that pin.
`python3 scripts/check.py` passed formatting, 120 Rust tests, Clippy with warnings
denied, builds, and the bootstrap, daemon, scrollback, control, and icon suites.
Selected native Wayland checks passed for daemon reconnect, state recovery,
window closure, terminal scrolling, terminal input, and fresh mapped and occluded
screenshots at 100% and 125% monitor scale. In that original validation, native
Find timed out at the sidebar directory assertion at line 118, both with the
scrollback changes and an untouched build of baseline `686a272`. Its later
history checks were then unverified. Known baseline native Resize and Find
failures were excluded from that run. History unit tests and headless checkpoint
regressions passed. The full native suite was not run.

A follow-up on main `5d20a97` fixed an unnecessary Wayland unmaximize request
in the resize diagnostic that returned Find's window to its narrow layout and
hid the sidebar. The full native Find suite then passed in dark and light
appearance. It now also verifies checkpoint-restored Unicode and wrapped
history after Quit and daemon restart, offscreen match navigation, and search
after grid reflow. The selected sidebar row still follows the focused pane's
directory. See the [Find diagnosis](screenshot-diagnostics.md#native-find-follow-up)
for the original failure and the follow-up evidence.

Color restoration validation on 2026-10-08 started from main `164d9bc` on
x86_64 Arch Linux with Rust 1.99.0 and Python 3.14.7. The pinned Ghostty source
archive matched its SHA256, Zig 0.16.0 built it in this worktree, and a live
daemon's `/proc/<pid>/maps` confirmed linkage to that local library. The standard
headless runner passed formatting, 126 Rust tests, Clippy with warnings denied,
binary/example builds, and all five headless suites, including colored Quit and
SIGTERM restart regressions.

Selected native `find` and `terminal-input` suites passed with `GSK_RENDERER=cairo`
in the isolated Wayland fixture. Find checked restored color pixels and native
clipboard copying after reflow in dark and light appearance. The environment
used GTK 4.24.1, libadwaita 1.10.0, Mutter 51.0, PipeWire 1.6.9, WirePlumber
0.5.18, and GStreamer 1.28.7. This color validation did not run the full native
suite or the OpenGL/Vulkan renderer checks.

The notification test covers unread state after delivery and reconnect, GUI
acknowledgements, stale acknowledgements racing with new alerts, and the latest
64 messages from both CLI and OSC producers. It also verifies replies to 48
concurrent GUI requests when the event source processes 16 requests per turn.
Unit tests cover coalesced wakes and acknowledgement retries under backpressure.
They also cover incoming byte limits, concurrent state writers, failed-write
cleanup, and ordered background saves. The control smoke test blocks state-file
locks to verify continued terminal I/O and delayed command replies.

The scheduling test checks native tab and split-pane focus before persistence,
prompt directory messages, zero Git launches while idle, branch updates during
a stuck lookup, and timeout recovery. Branch checks also cover repository
discovery through `.git` creation without a polling delay.
