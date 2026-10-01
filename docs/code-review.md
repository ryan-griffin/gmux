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

The app already uses the appropriate adaptive dialogs, alert dialogs, preferences, shortcuts, about dialog, entry rows, directory/font choosers, header bars, and overlay split view. These do not need custom replacements. Native widget typography and style classes remain the default.

The Find field remains a small compound widget because it places the match count inside the entry. The sidebar rows preserve the requested title, workspace, directory, branch, and unread layout. Nested terminal splits still use `GtkPaned`; `AdwTabView` does not replace a nested split layout.

## Library integration and performance

The GTK, libadwaita, Pango/Cairo, GLib Unix, and PTY Rust bindings were checked against their current stable releases. GTK 4.22 and libadwaita 1.9 feature levels now expose current API deprecations during compilation. Deprecated Rust API use is denied, with one documented exception below. The build was checked with GTK 4.22.5 and libadwaita 1.9.4.

The Ghostty revision remains pinned because its terminal C API is still changing. The bridge now uses that revision's dirty-row iterator to update the persistent cell buffer. Search tracks the buffer revision so another reader cannot consume an update before Find sees it. Glyph-cache lookups no longer allocate a string for every displayed cell.

Ghostty's render-hold callback now handles synchronized output. The renderer captures the last complete frame at the start of a hold and waits for release before showing subsequent output. A one-second timeout prevents a program from leaving the display frozen.

Paste encoding now goes through Ghostty's paste API, which handles terminal modes, newline encoding, and control-byte filtering. Paste bytes remain separate from already queued device replies. Middle-click reads the desktop's primary selection.

Git branch refreshes use one persistent worker with at most one batch in flight. Tabs in the same directory share a lookup within each batch. Slow Git commands therefore cannot create overlapping batches or block GTK.

Terminal keyboard handlers now match the application's registered accelerators with `GdkKeyEvent.matches` for both presses and releases. The previous hardcoded checks swallowed unbound Ctrl+Shift key presses and leaked some shortcut releases into programs using extended keyboard reporting. Local scrollback shortcuts also reserve their releases. GDK handles keyboard groups and consumed modifiers when matching shortcuts.

Font measurement and cached glyph layouts now share the terminal widget's Pango context. Metrics are measured once per font or context change. The cache also checks the context identity and serial, so reparenting and GTK text-setting changes invalidate it. Ordinary redraws no longer create a measurement layout.

Find reuses its match and highlight buffers and the text-to-cell mapping storage across rows. ASCII matching avoids per-cell lowercase string allocations. The Find controls only update when the query presence, match count, or selected match changes. Keyboard, pointer, and title buffers use stack storage before allocating their final result.

Underline and strikethrough rendering now includes blank cells and wide-character continuation cells. Initial split-divider notifications no longer overwrite the saved proportion before GTK applies it.

## Remaining limits

The screenshot diagnostic still uses deprecated GTK calls for the window's CSS background and frame. `GtkWidgetPaintable` returns cached content when the compositor pauses an occluded window, which fails the existing fresh-screenshot regression test. The exception is confined to `capture_window`; normal application controls use current APIs.

The terminal still uses Pango/Cairo rather than Ghostty's GPU renderer. Terminal image protocols and the full GTK accessible text interface remain outside the current implementation. Search and selection operate on the visible viewport.

PTY output, process titles, and socket requests still use the existing 16 ms GTK update loop. Moving these to event-driven sources would be a larger change and needs separate idle-CPU profiling and lifecycle checks.

## Validation

`cargo fmt --check`, `cargo build --locked`, all 36 unit tests, and Clippy with warnings denied passed. New tests cover dirty-row updates, synchronized output and its timeout, paste encoding with queued device replies, bounded Git refreshes, and command search. Command search tests cover related terms, word order, typo edits, relevance ranking, and unrelated or oversized queries.

Native smoke tests passed for PTYs, notifications, session restore, workspace filters, sidebar search, resize indicators, foreground titles, terminal Find, GNOME font changes, and the existing code audit cases. AT-SPI checks also passed for the native font switch, workspace button, command availability, empty command searches, and narrow dialogs in light, dark, and high-contrast appearances.

The follow-up review reran the native regression suite and checked pane-title visibility through AT-SPI. Command sizing and complete selected-row visibility passed in light and dark appearances. Real GTK key events verified that Ctrl-minus changes the PTY size without sending a release packet to a program using Kitty keyboard reporting, while unbound Ctrl+Shift+U sends both its press and release. Shift+Page Up/Down produces no stray release packets. Native screenshots verified decorations on spaces and wide characters. Workspace restore now checks the saved split layout before and after drawing it.

Find's native test keeps its own frame clock running with `GDK_DEBUG=no-vsync`. This avoids waiting indefinitely for a reveal animation when the compositor pauses an occluded test window. The application retains normal frame pacing.

The Commands regression test captures real AT-SPI announcement events, checks the search-to-list relation, and verifies related terms, typo matching, ranking, disabled matches, equal row heights, and retained focus in light and dark mode. Real GTK keyboard events also verified command activation and Escape dismissal. Orca speech output has not been tested.

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
