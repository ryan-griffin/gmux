# UI behavior

The window uses an `AdwOverlaySplitView` with a tab sidebar and terminal content.
Rows show the tab name, workspace, directory, Git branch, and unread state.
Nested `GtkPaned` widgets hold split panes.

## Menus and focus

The main menu contains window and tab creation, workspace management,
fullscreen, preferences, shortcuts, and Quit. The New Tab dropdowns contain
only tab creation actions. F10 reveals the sidebar and opens the main menu.

Terminal Actions targets the active pane or tab. A terminal's context menu
targets the clicked pane. Use Shift+right-click to open it while an application
captures mouse input; the Menu key and Shift+F10 also open it.
Tab context menus target the clicked tab.

Arrow navigation keeps focus in the sidebar. Clicking a tab or pressing Enter
activates it and focuses its terminal. Application shortcuts consume key presses
and releases before terminal encoding.

Clear Screen erases the primary screen and scrollback for all views of a pane,
clears selections, and restarts each view's Find query. It waits for a complete
terminal sequence and rejects the alternate screen used by full-screen apps.
Remove Tab Name restores the active pane's automatic title.

Closing a window detaches it. Close Pane, Close Tab, and Quit ask for confirmation
when their affected panes have busy processes.

## Dialogs and accessibility

Dialogs use libadwaita widgets. Startup errors use a parentless `AdwAlertDialog`
registered with the application, since initialization can fail before a window
exists. Close has initial focus and handles Enter, Escape, and window dismissal.
Repeated activation raises the existing error dialog.

The command palette ranks labels, related terms, and small spelling mistakes.
The search field keeps keyboard focus while the selected row scrolls into view.
Disabled commands remain visible but keyboard navigation skips them. Accessible
relations and announcements expose the current selection and empty results.

Find combines a text field, match count, and clear button. Preferences apply
font, cursor, theme, and pane-title changes to existing panes. Each window keeps
its own sidebar visibility and zoom.

The terminal exposes read-only viewport text, caret, selection, and cell geometry
through `GtkAccessibleText`. See [terminal rendering](renderer.md).

## Screenshot diagnostic

The `screenshot` socket command builds a fresh window snapshot because an
occluded window can have a paused frame clock and a stale `GtkWidgetPaintable`.
`capture_window` in `src/app/backend.rs` uses a scoped deprecated GTK CSS call
for the window background and frame. This is separate from terminal rendering.
