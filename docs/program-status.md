# Program status

Programs can report their current state with [OSC 7501, revision 0.4](https://www.superlogical.com/rex/docs/build/program-status).
Gmux uses libghostty's parser and support-query response.

Each pane uses the same thin progress bar as OSC 9;4. Its tab shows one static
activity icon, including while the tab is hidden. Hover the sidebar icon for the
state, blocked reason, available percentage, app, title or record ID, message and
record count. Tooltips and accessible descriptions retain the complete text;
pane numbers are available in CLI/socket metadata. A tab's machine label identifies
its connection. Report text is plain text, with invisible formatting characters
removed for display.

Status reports do not produce desktop notifications or unread alerts. An unread
alert adds a small dot to the activity icon's corner, using the same sidebar slot.
With no reported activity, the dot appears alone. Reading the alert removes the
dot without acknowledging program results or changing the reported state.

Blocked records take priority over errors, finished work, working records and
idle records, in that order. Within a pane, equal-priority records use lexical ID
order. The protocol-specific tab summary breaks pane ties by the lowest pane ID.
A blocked child therefore stays visible even when its root is idle or working.
The percentage belongs to the selected record; Gmux does not average unrelated
work. Missing progress means indeterminate work.

## Shared progress and sidebar presentation

Within a pane, retained OSC 7501 status takes precedence over OSC 9;4 for both
the progress bar and sidebar selection. Clearing all status records reveals any
still-active OSC 9;4 report. Neither protocol changes the other's retained data
or lifetime. Idle and finished statuses hide the bar; they do not revive an older
OSC 9;4 percentage.

Working reports show their percentage in the bar, or use activity mode when no
estimate is available. Blocked reports show a paused warning bar. A blocked report
without progress or a failed report shows a static colored marker with no numeric
accessible value. It never implies zero or 100 percent. Only visible working bars
with unknown progress animate; hidden tabs have no animation callbacks.

The sidebar selects across both protocols after applying the per-pane precedence.
Blocked work takes priority over failures, paused progress, finished work, working
reports and idle reports. Determinate work precedes indeterminate work; other ties
use the lowest pane ID. The icon uses the same loading, error and pause symbols
for either protocol, with question and check symbols for blocked and finished
program status. OSC 9;4 at 100 percent still means working, not finished.

CLI and socket `progress` and `program_status` fields remain independent protocol
summaries. Toolkit-independent clients can use `gmux_client::activity` for the
same combined presentation and source selection.

## Send a report

This Bash/Zsh example reports work, then waits for approval:

```sh
{
  printf '\033]7501;state=working:app=example:progress=30\007'
  printf '\033]7501;state=blocked:kind=permission:app=example:msg=Q29udGludWU/\007'
  read -r answer
  printf '\033]7501;state=done:app=example:msg=RmluaXNoZWQ=\007'
}
```

The messages decode to `Continue?` and `Finished`. Use `question` for a requested
answer or `auth` for authentication. The states are `idle`, `working`, `done`,
`blocked` and `error`. Each report replaces its entire record. Repeat optional
fields that should remain, including `app`. Free text in `title` and `msg` uses
base64-encoded UTF-8 without control characters.

Omitting `id` addresses the root. IDs such as `build/test` address independent
child records. A child can exist without its parent and inherits an absent app
from the closest ancestor that supplies one. `state=clear:id=build` removes
`build` and its descendants, while leaving `builder` intact. Omitting `id` on a
clear removes all records.

## Lifetime and limits

The daemon owns up to 64 records per pane across both terminal screens. Inserting
a 65th record evicts the least recently reported record. Repeating an unchanged
report refreshes this eviction order without sending a metadata update. Retained
text is bounded by the protocol's 128-byte ID, 32-byte app, 192-byte title and
2048-byte message limits. There is no status timeout or heartbeat.

A new primary shell prompt or attached-process exit drops `working` and `blocked`
records. Idle records remain. Done/error records survive until replaced, cleared,
or acknowledged by a key press/repeat, nonempty input or paste from a frontend
viewing that pane. Key releases, focusing a pane, synchronizing unread alerts and
CLI `send` do not acknowledge results. Acknowledgement removes all done/error
records in that pane for every window. A pane whose process has exited remains
open while it has a result; acknowledging it or closing the pane removes it.

Full terminal reset clears all records. Soft reset, primary/alternate screen
switches, frontend disconnect and reconnect preserve them. Reports are not saved
in history checkpoints. Fresh shells after a daemon restart start without them.

OSC 9;4 retains its existing expiry. It neither replaces OSC 7501 records nor
changes their lifetime.

## Inspect reports

`gmux list` and `gmux-gtk list` include `program_status` on panes and tabs. Pane
summaries contain a selected `record` and `count`; tab summaries also contain the
source `pane`. Absent status is `null`. The selected record's `app` includes
ancestor inheritance.

```sh
gmux metadata --pane 2
gmux --connection server metadata --pane 2
```

Metadata replies include `meta.program_status` and `program_status_records`, an
ID-sorted array of the complete retained reports with their original, uninherited
`app` values. The array preserves decoded report text; consumers displaying it
outside the terminal should disarm invisible formatting too. The normal stream,
state snapshots and lists carry only the representative record, keeping their
size independent of the retained record count. Status-only packets contain no
viewport or cell rows and use each connection's existing frame pacing.
