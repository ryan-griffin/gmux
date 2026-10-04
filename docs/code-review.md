# Code and GNOME UI review

Reviewed the Rust application, C bridge, stylesheet, build scripts, and native regression tests. The existing sidebar and pane layout remains intact.

## Widget changes

- Commands uses `AdwShortcutLabel`, replacing `GtkShortcutLabel`, which GTK deprecated in 4.18. Labels read the application's registered accelerators instead of maintaining a second shortcut table.
- Command rows use `AdwActionRow` with centered shortcut labels and the same native navigation-list styling as the sidebar. Disabled commands remain visible; keyboard navigation skips them. An `AdwStatusPage` handles an empty command search.
- The Commands search entry sits in the header and closes its dialog on Escape. An explicit `GtkViewport` keeps the complete selected row visible, including its native margins, while the search entry retains keyboard focus.
- Command labels use GNOME header capitalization and ellipses for actions that require input. Search ranks literal labels before related terms and small spelling mistakes, and accepts multiple words in any order.
- The Commands search entry exposes its keyboard instructions and its relationship to the command list. `GtkAccessible` active-descendant updates identify the selected row, and native announcements report selection and empty results. A focus controller tracks the entry's internal text widget so announcements follow keyboard navigation while focus stays in search.
- Custom Font uses `AdwExpanderRow`'s built-in enable switch. Enabling the setting and expanding the row are separate properties, so collapsing the row does not turn off the custom font.
- Add Workspace in the management dialog uses `AdwButtonRow`, with its native button role and styling.
- Command-dialog callbacks use weak references to the dialog, avoiding reference cycles that retained closed dialogs.
- Pane shadows remain owned by Adwaita's `.card` style. Removed redundant shadow overrides and the fixed size on terminal status text. The pane's thumbnail outline, terminal colors, and inactive dimming remain deliberate customizations.
- Pane Title Bars uses `AdwSwitchRow`. The saved setting updates existing panes and applies to newly created panes.
- The main menu uses header capitalization, unique mnemonics, and a final group containing Preferences, Keyboard Shortcuts, and About Gmux. Tab creation remains available when the sidebar is hidden. The main menu and both New Tab dropdowns share one menu model, including live workspace choices and a disabled workspace entry when none exist. Show Tabs and Fullscreen are checked items in their own section, and their checkmarks follow the actual widget state.
- A separate Terminal Actions menu in the content header contains split, Find, zoom, and closing commands for the active terminal. Close Pane and Close Tab form its last group and reuse the existing busy-process confirmations. Both menus have accessible labels and native keyboard navigation. The main menu is marked as the primary menu for GTK's F10 binding.
- Manage Workspaces is available in the main menu when the sidebar is hidden. Tab context menus group workspace membership commands and put Close Tab in a final section. Move to New Workspace describes the command's effect on the clicked tab. The workspace filter, tab menus, and New Tab dropdowns use the same accessible item labeling as the header and terminal menus. Dynamic workspace labels escape literal underscores and choose distinct mnemonics within each menu.
- Sidebar selection preserves keyboard focus in the tab list so arrow navigation and Shift+F10 remain available. Clicking a tab or pressing Enter activates it and focuses the terminal.

The [GNOME menu guidelines](https://developer.gnome.org/hig/patterns/controls/menus.html) discourage Quit in the main menu. Gmux deliberately retains Quit Gmux because closing its window only detaches the frontend and leaves terminal processes running. Quit stops the daemon and terminals, with confirmation when processes are busy.

The app already uses the appropriate adaptive dialogs, alert dialogs, preferences, shortcuts, about dialog, entry rows, directory/font choosers, header bars, and overlay split view. These do not need custom replacements. Native widget typography and style classes remain the default.

The Find field remains a small compound widget because it places the match count inside the entry. The sidebar rows preserve the requested title, workspace, directory, branch, and unread layout. Nested terminal splits still use `GtkPaned`; `AdwTabView` does not replace a nested split layout.

## Library integration and performance

The GTK, libadwaita, Pango, GLib Unix, and PTY Rust bindings were checked against their current stable releases. GTK 4.22 and libadwaita 1.9 feature levels now expose current API deprecations during compilation. Deprecated Rust API use is denied, with one documented exception below. The build was checked with GTK 4.22.5 and libadwaita 1.9.4.

The Ghostty revision remains pinned because its terminal C API is still changing. The bridge now uses that revision's dirty-row iterator to update the persistent cell buffer. The daemon scans retained history for Find in bounded row batches after terminal updates. Glyph-cache lookups no longer allocate a string for every displayed cell.

Ghostty's render-hold callback now handles synchronized output. The renderer captures the last complete frame at the start of a hold and waits for release before showing subsequent output. A one-second timeout prevents a program from leaving the display frozen.

Paste encoding now goes through Ghostty's paste API, which handles terminal modes, newline encoding, and control-byte filtering. Paste bytes remain separate from already queued device replies. Middle-click reads the desktop's primary selection.

Git branch refreshes use one persistent worker with at most one batch in flight. Tabs in the same directory share a lookup within each batch. Slow Git commands therefore cannot create overlapping batches or block GTK.

Terminal keyboard handlers now match the application's registered accelerators with `GdkKeyEvent.matches` for both presses and releases. The previous hardcoded checks swallowed unbound Ctrl+Shift key presses and leaked some shortcut releases into programs using extended keyboard reporting. Local scrollback shortcuts also reserve their releases. GDK handles keyboard groups and consumed modifiers when matching shortcuts.

Font measurement and glyph shaping share the terminal widget's Pango context. Metrics are measured once per font or context change. The cache also checks the context identity and serial, so reparenting and GTK text-setting changes invalidate it. Ordinary redraws no longer create a measurement layout.

Find reuses its match and highlight buffers and the text-to-cell mapping storage across rows. ASCII matching avoids per-cell lowercase string allocations. The Find controls only update when the query presence, match count, or selected match changes. Keyboard, pointer, and title buffers use stack storage before allocating their final result.

Underline and strikethrough rendering now includes blank cells and wide-character continuation cells. Initial split-divider notifications no longer overwrite the saved proportion before GTK applies it.

## Remaining limits

The screenshot diagnostic still uses deprecated GTK calls for the window's CSS background and frame. `GtkWidgetPaintable` returns cached content when the compositor pauses an occluded window, which fails the existing fresh-screenshot regression test. The exception is confined to `capture_window`; normal application controls use current APIs.

Terminal rendering now uses [GSK nodes with Pango shaping](renderer.md). Terminal image protocols remain outside the current implementation. Find now searches retained history, selection spans scrollback, and the canvas exposes read-only GTK accessible text, caret, and selection.

The [headless daemon](terminal-sessions.md) owns event-driven PTY I/O and process state. Stream packets and GUI requests now wake a GLib file-descriptor source; the frontend has no periodic update poll. A two-second settings and Git refresh remains.

## Validation

These results describe the earlier UI review. Current renderer and lifecycle checks are documented in [the renderer notes](renderer.md) and [the daemon notes](terminal-sessions.md).

`cargo fmt --check`, `cargo build --locked`, all 36 unit tests, and Clippy with warnings denied passed. New tests cover dirty-row updates, synchronized output and its timeout, paste encoding with queued device replies, bounded Git refreshes, and command search. Command search tests cover related terms, word order, typo edits, relevance ranking, and unrelated or oversized queries.

Native smoke tests passed for PTYs, notifications, session restore, workspace filters, sidebar search, resize indicators, foreground titles, terminal Find, GNOME font changes, and the existing code audit cases. AT-SPI checks also passed for the native font switch, workspace button, command availability, empty command searches, and narrow dialogs in light, dark, and high-contrast appearances.

The follow-up review reran the native regression suite and checked pane-title visibility through AT-SPI. Command sizing and complete selected-row visibility passed in light and dark appearances. Real GTK key events verified that Ctrl-minus changes the PTY size without sending a release packet to a program using Kitty keyboard reporting, while unbound Ctrl+Shift+U sends both its press and release. Shift+Page Up/Down produces no stray release packets. Native screenshots verified decorations on spaces and wide characters. Workspace restore now checks the saved split layout before and after drawing it.

Find and sidebar native tests keep their own frame clocks running with `GDK_DEBUG=no-vsync`. This avoids waiting indefinitely for layout or reveal transitions when the compositor pauses an occluded test window. The application retains normal frame pacing. The Commands test refreshes AT-SPI state before checking focus, avoiding a cached state captured before GTK transfers focus.

The Commands regression test captures real AT-SPI announcement events, checks the search-to-list relation, and verifies related terms, typo matching, ranking, disabled matches, equal row heights, and retained focus in light and dark mode. Real GTK keyboard events also verified command activation and Escape dismissal. Orca speech output has not been tested.

## Daemon and renderer cleanup

The frontend processes received pane updates through an event-driven source
instead of polling every 16 ms. Tab and layout changes update
viewport subscriptions, with retries after temporary command-queue backpressure.
Resize indicators have independent timers. Periodic directory checks use
streamed metadata; split and busy-job checks still request fresh process state.

The daemon reuses its polling collections, updates its retained viewport only
for changed rows, and encodes packets into the socket output buffer. Process
metadata is rebuilt for panes with activity and during the once-a-second refresh.
Partial writes compact the buffer after enough data has been consumed. Rendering
caches font descriptions and search highlights and generates terminal graphics
in one pass. Cached frontend snapshots are infallible, and terminal text extraction
uses one string instead of allocating strings for individual rows.

Validation passed 37 library tests, 11 application tests, the example tests,
Clippy with warnings denied, and release builds for both binaries and examples.
All 14 native checks passed, including all three GTK rendering backends,
background reconnects, resizing, font changes, Find, workspaces, notifications,
Vim/top, actual keyboard and clipboard input, and pane lifecycle checks. The
detached idle daemon again consumed zero CPU ticks during the smoke test.
These checks verify behavior; they do not measure overall FPS or GPU memory.

The subsequent reliability pass unified unread state in the daemon and bounded
both CLI and OSC alert queues to the latest 64 messages per pane. Reading a pane
acknowledges a specific alert sequence, so old packets cannot restore read
indicators and stale acknowledgements cannot discard new alerts. Reconnects keep
unread state even after queued desktop alerts have been delivered.

The historical [application benchmark comparison](performance.md#historical-xwayland-comparison)
measured the preceding polling build and the event-driven build using XWayland
input synthesis and window readback. It excluded physical display scanout and
normal compositor frame pacing.

The reliability pass passed 40 library tests, 11 application tests, the example
tests, Clippy with warnings denied, and release builds for both binaries.
Native checks passed for notification queues and unread acknowledgements,
sidebar restoration, Find, resizing, fonts, workspaces, titles, Vim/top,
keyboard and clipboard input, command accessibility, pane lifecycle, and
frontend reconnects. The benchmark completed two comparisons with reversed
build order; the detached idle daemon consumed zero CPU ticks in all four runs.

The input test and application benchmark now run in an isolated headless Mutter
Wayland compositor with XWayland disabled. They send native keyboard and pointer
input and observe composited PipeWire frames. The input test passed terminal
accessibility, keyboard input, pointer selection, Ctrl+Shift+C, and clipboard
contents. The benchmark completed all five workload scenarios with normal GTK
frame pacing. Its latency includes compositor presentation and capture overhead,
so it cannot be compared directly with the historical XWayland measurements.
All 40 library tests, 11 application tests, eight example tests, formatting,
Clippy with warnings denied, and debug and release binary builds passed.

## Command and history cleanup

The daemon now applies the same typed layout and workspace commands for the GUI
and detached CLI. The frontend adopts the returned state; its presentation saves
cannot replace daemon topology. Workspace names are trimmed before uniqueness
checks. Quit finishes after a layout save failure, and completed live commands
return their changed state with a persistence warning.

One frontend worker handles runtime requests and settings writes outside GTK's
main thread. It coalesces adjacent presentation saves while preserving command
order. Busy-process checks batch all affected panes. Unchanged viewport packets
are suppressed without dropping initial viewports or scroll acknowledgements.
Key presses and releases rely on changed terminal state to trigger drawing.

Find scans retained history in batches of 64 rows. Selection uses Ghostty tracked
references and its plain-text formatter, including wrapped lines and scrollback.
The terminal canvas exposes Unicode viewport text, caret, and selection through
`GtkAccessibleText`. Native regressions exercise offscreen Unicode matches and
selection across history. Shared test helpers and `scripts/check.py` provide one
entry point for headless and isolated Wayland checks.

The follow-up pass added bounded paste messages with complete-operation encoding,
Find across soft wraps, strict state-file reads and protected saves, a visible
startup recovery window, and corrected accessible glyph bounds and hit testing.
Daemon execution matches typed commands directly and returns typed results to
the frontend. Missing and explicit-null workspace assignments retain their
different meanings.

Rust checks passed all 73 tests, formatting, Clippy with warnings denied, and
debug builds. `python3 scripts/check.py --native --skip-build` passed all 21
smoke suites. Release builds of both binaries also passed. The control regression covers normalized
workspace duplicates, preservation of daemon topology, unchanged key releases,
scroll acknowledgements, live changes after save errors, and Quit after a save
error. Native checks verify accessible text, caret and selection, and copying a
selection spanning 120 scrollback lines. Engine tests also verify selection
through reflow and reversed endpoints. Additional regressions cover a complete
4 MiB bracketed paste through a real PTY, oversized-command recovery, damaged
state files before and after startup, Unicode queries across wraps and scan
batches, wide and combining glyph geometry, off-grid hit testing, and a large
Unicode clipboard paste through GTK.

The final cleanup pass moves coalesced save payloads and daemon command payloads
instead of copying them. Layout traversal collects pane IDs in one vector.
Closing a pane discards its pending input error, paste operation, and viewport
subscription. Find labels now describe terminal search rather than restricting
it to the visible viewport.

Accessible text uses one mapping for fresh and cached caret and selection
updates. A native regression caught a cached row-end selection that included an
extra newline. Boundary queries reuse GTK's text buffer, and color changes avoid
text replacement notifications. Tests cover repeated boundary queries after
new output, wide glyphs, combining characters, and row-end selections.

Validation passed all 74 Rust tests, all 21 smoke suites, formatting, Clippy with
warnings denied, and debug and release binary builds. Selection autoscroll and
the regressions for delayed highlights and GTK overscroll also passed.

## References

- [GNOME HIG: UI styling](https://developer.gnome.org/hig/guidelines/ui-styling.html)
- [GNOME HIG: dialogs](https://developer.gnome.org/hig/patterns/feedback/dialogs.html)
- [GNOME HIG: text fields](https://developer.gnome.org/hig/patterns/controls/text-fields.html)
- [GNOME HIG: writing style](https://developer.gnome.org/hig/guidelines/writing-style.html)
- [GNOME HIG: search](https://developer.gnome.org/hig/patterns/nav/search.html)
- [Libadwaita style classes](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/style-classes.html)
- [AdwShortcutLabel](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ShortcutLabel.html)
- [AdwExpanderRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ExpanderRow.html)
- [AdwButtonRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ButtonRow.html)
- [GdkKeyEvent.matches](https://docs.gtk.org/gdk4/method.KeyEvent.matches.html)
- [GtkAccessible.announce](https://docs.gtk.org/gtk4/method.Accessible.announce.html)
- [GTK accessible relations](https://docs.gtk.org/gtk4/enum.AccessibleRelation.html)
- [Pango context serial](https://docs.gtk.org/Pango/method.Context.get_serial.html)
- [Ghostty terminal API](https://github.com/ghostty-org/ghostty/blob/4da7523faba68ccb4042ea20585817098a51c015/include/ghostty/vt/terminal.h)
- [Ghostty render API](https://github.com/ghostty-org/ghostty/blob/4da7523faba68ccb4042ea20585817098a51c015/include/ghostty/vt/render.h)
- [Ghostty paste API](https://github.com/ghostty-org/ghostty/blob/4da7523faba68ccb4042ea20585817098a51c015/include/ghostty/vt/paste.h)
