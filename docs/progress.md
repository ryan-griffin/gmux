# Terminal progress

Programs can report progress with OSC 9;4. GTK shows a thin native progress bar
directly below each pane's header, or at the top of the terminal when headers
are hidden. It overlays the terminal without moving its text or changing its
grid size. Errors use red and paused progress uses the warning color.
Indeterminate progress uses native activity mode only while the pane is mapped;
hidden panes retain no progress animation callbacks or timers.

The bar appears in panes. The sidebar also shows a static working, paused or
error icon, including for hidden tabs. An unread alert adds a small corner dot to
that icon, using one slot beside the tab title. Accessible progress values name
the state and percentage. CLI and socket status retain reports for hidden panes
and include a representative pane for each tab.

OSC 7501 uses the same bar and sidebar indicator. Retained OSC 7501 status takes
precedence within its pane; OSC 9;4 becomes visible again when those records clear.
The underlying reports and their lifetimes remain independent. See
[program status](program-status.md#shared-progress-and-sidebar-presentation) for
the combined sidebar selection rule.

For example, a shell script can report 42%, then remove its indication:

```sh
printf '\033]9;4;1;42\007'
printf '\033]9;4;0\007'
```

The states follow [Ghostty's OSC 9;4 semantics](https://ghostty.org/docs/vt/osc/conemu):

- `0` removes progress and resets its percentage to zero.
- `1` sets determinate progress. An omitted percentage starts at zero.
- `2` reports an error. An omitted percentage retains the preceding value.
- `3` reports indeterminate progress and preserves the preceding percentage for
  a later paused or error report. That retained number is not a current estimate.
- `4` reports paused progress. An omitted percentage retains the preceding value.

The pinned Ghostty parser accepts BEL or ST terminators, clamps parsed unsigned
percentages to 100, and ignores percentages for remove and indeterminate states.
Applications should send valid unsigned decimal percentages between 0 and 100.
Progress at 100% remains visible until removed, reset, exited, or expired.

The daemon retains the latest report for each execution. Local and remote
clients, hidden panes, and reconnecting windows see that same state. Every
non-remove report, including an identical report, refreshes a 15-second stale
report deadline. Programs should refresh reports at least once per second.
An unchanged keepalive does not broadcast another progress update. Expiry works
with no clients attached and clears the state once.

Each connection delivers changed progress at its own frame interval, retaining
the latest value while waiting. Progress packets alone do not extract viewport
rows or bypass the interval for other panes.

RIS, an explicit remove report, process exit, and pane removal clear progress.
A soft terminal reset does not clear it. Progress is not saved with scrollback
or desktop state; a new shell starts inactive. It is independent of busy-process
checks and unread terminal alerts.

The tab's protocol-specific `progress` metadata selects one reporting pane,
without averaging percentages. Error takes
priority over paused, then determinate, then indeterminate. Equal-priority
reports use the lowest daemon-local pane ID. Changing the active pane does not
change that rule. Each visible pane still shows its own progress, independent
of whether pane titles are enabled. The `list` response identifies the selected
source pane.

`gmux list`, `gmux-gtk list`, and their socket equivalents include each pane's
`progress` object, for example `{"state":"determinate","percentage":42}`.
An inactive pane has `{"state":"inactive","percentage":0}`. A tab's `progress`
is null when no pane reports progress, or
`{"pane":2,"progress":{"state":"determinate","percentage":42}}`.
The pane ID belongs to the tab's daemon, like its other numeric pane IDs.

Toolkit-free consumers can read `RemoteSession::progress()` and observe
`SessionUpdate::progress_changed`. The shared `progress` module provides named
`ProgressState`, validated `Progress`, and the same `representative` selection
rule used by CLI and socket status. Progress arrives through pane metadata;
progress alone does not change client viewport or text revisions.

The implementation uses the pinned
[callback API](https://github.com/ghostty-org/ghostty/blob/83fb95624b0439ee660c2679b43b38c8b63769cd/include/ghostty/vt/terminal.h),
[reset effect](https://github.com/ghostty-org/ghostty/blob/83fb95624b0439ee660c2679b43b38c8b63769cd/src/terminal/stream_terminal.zig),
and [15-second timeout](https://github.com/ghostty-org/ghostty/blob/83fb95624b0439ee660c2679b43b38c8b63769cd/src/apprt/gtk/class/surface.zig).
