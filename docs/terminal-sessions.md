# Background terminal sessions

`gmux` owns local PTYs, shells, terminal state, and tab/pane topology. Closing a
window or losing the frontend leaves those sessions running. Reopening Gmux
reconnects to the same processes. Closing a pane terminates its process; an
exited shell removes its pane.

Quit Gmux and Ctrl+Shift+Q stop the local daemon when the app is connected to it,
including terminals used by other clients. Remote terminals keep running. A
remote-only app quits without stopping an unused local daemon. `gmux stop` stops
the addressed daemon and all of its terminals, whether or not GTK is open.
SIGTERM and SIGINT detach a frontend or shut down the daemon, depending on which
process receives the signal.

## Multiple windows

Windows share tab order, names, split topology, workspace membership, unread
alerts, and PTY output. The daemon broadcasts execution revisions after mutations and process exits.
The GTK store shares workspace assignments across windows.
Windows ignore older execution revisions, including delayed replies.

Local and remote tabs share each window's sidebar, newest first by creation time.
Adding a connection merges its existing tabs by their original creation times.
The owning daemon records each tab's creation time and preserves it across restarts;
the client chooses the combined order. Notifications, output, and tab selection
do not change that order. Machines should have synchronized clocks for chronology
across connections. Workspaces belong to the GTK app.

GTK identifies each tab and pane by its daemon identity and daemon-local ID.
The GTK socket's `list` response includes numeric `id` fields and qualified
`reference` fields; layout leaves contain qualified pane references. Commands
accept numeric selectors with the connection that owns them.

Each window keeps its active tab and pane, workspace filter, sidebar search and
visibility, split proportions, zoom, Find query, selection, scroll position,
and window dimensions. One terminal engine serves all views of a pane. Input
from any view reaches the same PTY.

The most recently focused client viewing a pane controls its PTY size. Other
views remember their requested sizes and apply them when they become the owner.
Presentation saves provide defaults for new windows without replacing shared
layout topology or other live windows' choices.

## Ownership and scheduling

The daemon owns PTYs and terminal engines in one nonblocking `ppoll` loop.
Each turn gives frontend commands, PTY work, and viewport publication separate
2 ms budgets. The stages also cap work at eight command connections, 32 PTYs,
and 64 pane views respectively. Ordered control requests have their own shared
2 ms, 32-item budget, including mutations waiting for a save. A command connection
decodes up to eight requests per visit. PTY reads yield after 500 microseconds or 512 KiB, checking
time between 16 KiB reads. Pending input remains limited to 8 MiB per pane.
Ready queues retain deferred work ahead of newly discovered readiness.
An idle detached daemon blocks in `ppoll`. Signals, sockets, disk-worker
completions, and the nearest active progress expiry deadline wake it. Each connection paces viewport batches
independently. GTK reports its display's refresh interval; other clients default
to 16 ms. Submillisecond deadlines retain their precision, and metadata is
refreshed once a second. Dirty viewport extraction, visible presentation changes,
incremental history-search batches, and changed progress reports restart the
connection's frame interval. Other metadata packets and passes without that
work do not postpone the next visible response. Hidden
panes keep parsing output without extracting viewports. A frontend pass can
continue across turns; only visited panes consume their dirty flags. Its frame
interval starts with the pass, and continuations do not wait for another frame.
Frontends rotate between turns, including background clients. Socket flushes
write at most 256 KiB per visit. When no publication is due, flushing has a
separate 1 ms, 16-connection budget.

The daemon shares [frontend resource budgets](resource-budgets.md#daemon-frontend-storage)
across local and remote streams. Output pressure disconnects slow consumers;
optional snapshot pressure releases delta caches and sends a full viewport on
the next update. Engine views retain their scroll, selection, search, and
synchronized-output state.

Output preserves cached scrollback rows and invalidates the previously mutable
screen rows. Pruning advances the history origin without discarding surviving
rows. Clears, reflow, screen switches, and palette or theme changes reset the
cache. Immutable history can be prefetched during output; live screen prefetch
waits for 100 ms of quiet.

GTK owns rendering, fonts, the clipboard, and accessible text. An `eventfd` wakes
the frontend when packets or requests arrive. All windows queue work through
one idle-priority dispatcher with a shared 2 ms, 64-item budget, allowing input
and frame-clock sources between batches. Connections rotate within a window;
each applies at most 16 FIFO packets per visit. Clipboard effects run at their
position in that stream, and viewport deltas apply in order. Windows coalesce
wakeups while queued and return to the queue's tail when more work arrives.

These are cooperative limits, not hard deadlines. A packet, native snapshot,
command handler, or GTK presentation update completes before yielding. Readiness
scans, dirty-state fan-out, control mutations, and persistence bookkeeping also
run on the owner thread. Pending userspace work requests another turn explicitly;
blocked sockets wait for readiness. Read batches that yield before observing
WouldBlock request another turn even after their process exits. Drained queues
remove their continuation source, without an idle polling timer.

A disk worker writes daemon state outside the PTY owner thread. Explicit layout,
settings, and Stop mutations wait for earlier command saves; read-only requests,
terminal input, and PTY processing continue. Execution changes advance the live
revision before their save completes. Streams and reads can observe that state
while the originating command waits for a durable reply. Save failures return a
warning alongside the successful command result; live processes and topology
remain changed. Delivering a reply does not advance the revision.

Process exits remove panes and publish one revision per nonempty batch through
the same execution effects. Their automatic saves follow queued writes, and
adjacent automatic execution saves coalesce. A delayed command reply contains
the current live topology and revision, including any intervening exits. Clients
reject snapshots older than their last accepted revision.

Scrollback settings save before the owner publishes and applies the policy.
A failed save cannot prune history. During shutdown, exiting processes remain
in restart topology and do not advance its revision. Stop waits for its checkpoint
write and reports failures; a failed checkpoint gets one final attempt before
exit. Signal shutdown waits for an outstanding explicit save, then checkpoints.
The worker drains queued writes before the daemon releases its execution lock.

A separate client request worker persists workspace definitions and assignments
together in one atomic write. Slow client disk writes leave the UI free to read
the last committed state.

## Persistence and recovery

State lives under `$XDG_CONFIG_HOME/gmux/`, normally `~/.config/gmux/`:

- `clients/gtk/settings.json`: GTK preferences, including `local_daemon_enabled`, which
  defaults to false. It controls the Local Daemon toggle, starts or attaches on
  launch, and enables automatic recovery if the daemon stops while GTK is open.
- `daemon-id.json`: persistent daemon identity.
- `daemon-settings.json`: daemon-owned per-terminal scrollback retention policy.
- `execution.json`: local tabs and their creation times, pane topology,
  directories, checkpoint references, and the tab/pane ID allocation counter.
- `clients/gtk/state.json`: frontend-owned workspace definitions, starting
  directories keyed by daemon identity, workspace ID allocation, qualified tab
  presentation and assignments, selection, split proportions, and window defaults.
  Layout leaves and pane selections contain both the daemon identity and pane ID.
- `connections.json`: client-owned local alias, remote names, addresses,
  authentication file paths, and pinned identities.
- `remote-access.json`: daemon-owned TCP listener settings.
- `scrollback/`: daemon-owned history checkpoints.
- `daemon.log`: startup and runtime errors.

Missing state files use defaults. Invalid or unreadable JSON produces an error
and remains untouched. Writers use a lock, a private temporary file, syncing,
and atomic rename so readers see a complete state file.

The daemon creates its identity and validates saved execution during startup.
Detached startup failures return a nonzero status and write details to `daemon.log`.
Execution restores only from `execution.json`. Desktop records cannot create
shells or alter pane topology. Presentation and workspace changes write desktop
state without rewriting execution or history references. The daemon never loads
or writes client profiles. Invalid client preferences or organization do not
prevent a headless daemon from running. GTK sends creation assignments after the
execution command reply and preserves any execution save warning. A client
presentation save cannot make unsaved execution durable. Missing presentation
uses default split proportions and leaves a tab unassigned.

Daemon identity survives restarts and address changes. Tab and pane IDs are scoped
to that identity and never reused after deletion or restart. Workspace IDs belong
to the desktop catalog. Desktop saves preserve records belonging to other daemons;
those records cannot start local processes.

Build and install the frontend and daemon from the same revision.

## Scrollback checkpoints

[Runtime retention limits](scrollback.md) apply before restored text enters a
fresh terminal. They do not change the checkpoint safety budgets below.

Explicit Quit and orderly daemon shutdown checkpoint primary-screen text history.
A restart restores tabs and directories with fresh shells. Commands, jobs,
terminal modes, progress, hyperlinks, and alternate-screen contents do not resume.
Unicode, wrapped lines, colors, and text attributes survive. Indexed and default
colors follow the current theme; explicit RGB colors retain their values.

Each checkpoint contains a Ghostty snapshot with Gmux's format version, exact
Ghostty source revision, pane ID, and payload length. `GHOSTTY_PREFIX` must point
to a build matching the pin in `scripts/bootstrap-ghostty.py`.
Missing, mismatched, or damaged checkpoints log a warning and leave that pane
with empty history. Other panes still restore. Updating the Ghostty pin, including
the update to 1.4.0, invalidates history saved by the previous revision. Tabs,
directories, and saved scrollback limits still restore with fresh shells. To keep
text before an upgrade, export it with `gmux read` while the old daemon is running.

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
Its `.stream` companion carries persistent execution client connections. The
frontend owns a separate `.ui` socket for organization, navigation, and GUI
requests. The GTK client CLI routes its commands there when the frontend is running.
The daemon CLI always addresses the daemon socket directly.
Execution commands remain available after a frontend crash. A lock serializes
daemon startup; sockets are owner-only.
Protocol version checks reject mismatched binaries. Stream attachment also checks
the daemon identity returned by the control handshake, and the frontend rejects
state from a different daemon.

Viewport packets contain JSON metadata and fixed-width cell rows. A new
subscription receives a complete viewport; later packets carry changed rows.
Each client owns its subscriptions, search, selection, paste assembly, and copy
replies. Stream packets are limited to 64 MiB; commands to 1 MiB. Pastes use
UTF-8 chunks with a 4 MiB total limit.

CLI execution operations such as `list`, `new`, `split`, `send`, `read`, and
`stop` work with the window closed. Specify a pane when multiple panes exist.
Workspace changes, navigation, GUI actions, and screenshots require a running
client and use its socket. The standalone daemon excludes workspace and GUI commands.
The `gmux-client` library provides execution connections and viewport handling.
Each frontend defines its own organization. See `gmux --help`
for terminal commands and `gmux-gtk --help` for GTK client commands.

`gmux-gtk quit`, `screenshot`, workspace commands, and workspace focus require a
running GTK app and do not accept `--connection`. Tab and pane focus can select
a connection. `focus` accepts one target at a time. CLI close and quit commands
act immediately; GUI actions ask for confirmation when affected panes are busy.

`list` and `status` include `daemon`, the persistent identity. Each listed
tab and pane has a `reference` containing that identity and its numeric `id`.
Each tab's `created_at` is measured in microseconds since the Unix epoch.
Layout command replies also include `daemon` alongside their numeric result IDs.
CLI tab and pane arguments remain numeric and refer to the addressed daemon.
With a window open, `list` includes its integrated local and remote tabs, each
with a connection name and status. `active_reference` identifies its
selected tab across connections. Explicit numeric IDs on the local socket address
local execution; omitted tab and pane targets use the window's active tab or pane.
New tabs use the window's default connection, regardless of the active tab. Choose a
connection from the New Tab dropdown or with the CLI's `--connection` option. The
workspace filter assigns new tabs on any connection to that workspace. Omitted
directories use the workspace setting for the destination connection, or that
connection's home directory.
For `gmux-gtk new --directory`, local relative paths resolve from the CLI's
working directory. Remote paths remain on the destination, including when the
window selects the remote by default; they must be absolute or start with `~/`.
An implicit `gmux-gtk notify` uses the calling shell's pane or the active pane,
including an active remote pane. Without a GUI, terminal commands use the local
daemon or an explicit `--connection` and do not apply workspace organization.
`focus` accepts a connection's name as `connection`, and `filter` accepts
either `workspace` or `value` of `all`.

An optional TCP listener exposes local execution without desktop workspaces or
presentation. The client library opens remote control requests and viewport streams
directly. See [remote daemons](remote-daemons.md) for authentication and setup.

## Notifications

The daemon tracks unread alerts with sequence numbers so a delayed acknowledgement
cannot clear a newer alert. The frontend sends desktop notifications and
suppresses them for the focused pane while its window is active.

The sidebar uses a corner dot on its activity icon for unread alerts, or a dot
alone when there is no activity. Reading an alert removes the dot and leaves
reported program state intact.

While detached, the daemon queues up to 64 alerts per pane within a 512 KiB text
budget. Queued alerts arrive on reconnect. Unread state and queued messages stay
in memory and do not survive daemon restarts.

## Progress reports

The daemon retains each execution's latest OSC 9;4 state across frontend detach
and reconnect. Identical reports refresh a 15-second deadline without another
progress broadcast. Reset, exit, removal, and expiry clear it. Progress is
independent of busy checks and unread alerts. See [terminal progress](progress.md)
for script usage, status fields, and the tab selection rule.

Program status reports are also execution-owned. The daemon retains a bounded
record set across screen switches and client reconnects, sends a representative
through live metadata, and exposes complete details on request. Done/error
records keep an exited pane available until acknowledged. Fresh shells do not
inherit reports. See [program status](program-status.md) for UI and lifecycle rules.
