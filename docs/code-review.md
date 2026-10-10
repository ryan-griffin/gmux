# GNOME UI design

These notes explain the maintained widget and interaction choices. See
[code organization](code-organization.md) for module responsibilities,
[background sessions](terminal-sessions.md) for backend behavior, and
[rendering](renderer.md) for terminal drawing. Earlier review results are in
[the archive](archive/code-review-history.md).

## Layout and native widgets

The window uses an `AdwOverlaySplitView` with a searchable sidebar and terminal
content. Sidebar rows show the tab name, workspace, directory, Git branch, and
unread state. Nested splits use `GtkPaned` so each tab can retain its own pane
layout and divider proportions.

Dialogs use libadwaita's adaptive dialog, alert, preferences, shortcuts, and
about widgets. Directory and font selection use GTK choosers. Pane shadows come
from Adwaita's `.card` style; terminal colors, active-pane outlines, and inactive
pane dimming are application customizations.

Startup failures use `AdwAlertDialog` with a heading, plain-text error details,
and one neutral Close response. Close receives initial focus and is both the
default response for Enter and the close response for Escape and window-manager
dismissal. Default activation does not imply suggested-action styling.
Repeated activation, directory opening, or New Window requests during failed
startup raise the existing failure alert without retrying initialization.
The alert uses the saved appearance even when the daemon check fails before
window setup.

The startup alert is a necessary exception to the HIG's parent-window rule:
initialization can fail before a main window exists. Libadwaita's supported
parentless presentation creates its native window, which Gmux registers with
`GtkApplication` to stay alive until dismissal. It then reports the original
startup error to stderr and returns a failure exit status.

The previous fixed 560 × 340 window used `AdwStatusPage`, whose internal scrolled
window could not fit its large icon, spacing, text, and child button. The alert
uses libadwaita's content sizing and response layout. Ordinary errors fit without
scrolling; long descriptions can scroll while Close stays outside the message's
scrolled area. Error details remain plain text, including paths containing
markup characters.

Native validation of the previous layout found that GTK initially focused
Close automatically, but the window had no default widget or Escape binding.
The alert assigns focus and dismissal explicitly while retaining native styling.

The [native startup-alert screenshot](images/startup-error.png) shows the neutral
Close response with its keyboard focus outline.

The Find field combines a `GtkText`, match-count label, and clear button under
Adwaita's entry styling. This places the count inside the field while retaining
its focus ring and error state.

## Menus and focus

The main menu and both New Tab dropdowns share tab creation choices with live
workspace updates. New Window belongs to the main menu's creation group; the
specialized New Tab dropdowns contain only tab actions. Its label has no ellipsis
because it opens a window immediately, and its W mnemonic is distinct from the
other primary-menu access keys. Fullscreen uses a stateful action whose checkmark
follows the window.
The desktop launcher advertises the standard `new-window` action with the label
New Window and runs `gmux --new-window`. GNOME uses it for the app icon's context
menu and middle-click, keeping window creation on the same CLI path.
The main menu stays above the sidebar, including in the narrow sidebar overlay.
The separate Terminal Actions menu includes Rename Tab and
Close Tab for the active tab, and split, Find, zoom, and Close Pane for its active
pane. Its button is hidden when there is no active terminal.

Clear Screen sits beside Find in Terminal Actions and the terminal context menu,
and is available through the command palette. The
[GNOME HIG](https://developer.gnome.org/hig/patterns/controls/menus.html) assigns
content-specific actions to secondary menus and app-wide actions to the main
menu. Clearing targets one pane, so it stays out of the main menu, New Tab
dropdowns, and tab context menu. Each terminal menu has a distinct mnemonic for
the action. It has no default keyboard shortcut and is disabled in an empty window.

The daemon erases the primary screen and history through the existing typed
frontend command path, then updates all clients viewing that pane. It releases
their scroll anchors and selections and restarts history searches with their
existing queries. It avoids allocating a view or taking a snapshot to dispatch
the clear. VT erasure waits for a parser ground boundary and ends synchronized
output so the result can display immediately. Following Ghostty's Clear Screen
behavior, clearing is rejected on the alternate screen to preserve full-screen
applications' display and cursor state.

Remove Tab Name sits beside Rename Tab in both tab menus and is hidden for
automatic names. It restores automatic title updates immediately and targets
the clicked tab in the context menu.

Menus use accessible names, keyboard navigation, and distinct mnemonics. Dynamic
workspace labels escape literal underscores before choosing mnemonics. Tab
context-menu actions target the clicked tab, including workspace moves and
creation. Terminal context-menu actions target the clicked pane.

F10 reveals the sidebar and opens its main menu. The Menu key and Shift+F10 open
a terminal context menu; Shift+right-click overrides terminal mouse tracking.
Sidebar arrow navigation keeps focus in the list. Clicking a tab or pressing
Enter activates it and focuses its terminal.

Quit Gmux remains in the main menu as an explicit exception to the GNOME HIG
because closing the window leaves terminal processes running. Close Pane,
Close Tab, and Quit ask for confirmation when affected panes have busy processes.
[Session lifecycle](terminal-sessions.md) describes detach and shutdown behavior.

## Command palette and accessibility

Command rows use `AdwActionRow` and `AdwShortcutLabel`. Shortcut labels read the
registered application accelerators. Disabled commands remain visible and
keyboard navigation skips them. `AdwStatusPage` handles an empty search result.
Close Window shows the Close Pane shortcut only when the empty state is shown;
the label follows the empty-window action's availability while the palette is open.

Search ranks command labels before related terms and small spelling mistakes,
and combines words in any order. The search field retains keyboard focus while
an explicit `GtkViewport` keeps the selected row visible. Escape closes the
palette. Callbacks keep weak references to the dialog so closed dialogs can be
released.

The search field exposes keyboard instructions, its relationship to the command
list, and the active descendant. Accessibility announcements report selection
and empty results. A focus controller follows the field's internal text widget.
The [terminal renderer](renderer.md#implementation) documents the canvas's
read-only accessible text, caret, selection, and geometry.

## Preferences and shortcuts

Custom Font uses `AdwExpanderRow`'s built-in enable switch. Enabling the custom
font and expanding the row are separate properties; collapsing it preserves the
font choice. Pane Title Bars uses `AdwSwitchRow`, and Add Workspace uses
`AdwButtonRow`. Settings changes update existing panes and apply to new ones.

Terminal key handlers match GTK's registered accelerators for presses and
releases. Scrollback shortcuts also consume their releases. This keeps
application shortcuts out of programs that request extended keyboard reporting
while allowing unbound keys to reach the terminal.

Keyboard Shortcuts lists F10 for the main menu. The Close Pane shortcut item
explains that Ctrl+Shift+W closes the window when the empty state is shown.
Action labels use the same wording and capitalization in the command palette
and shortcut dialog. Copy Selection clarifies the terminal's Copy action, and
the checked Fullscreen menu item names the setting rather than its toggle action.
New Window appears in the Application shortcut section and uses Ctrl+Shift+N,
following the terminal's existing Shift-modified creation shortcuts. Its menu,
palette, and help labels use the registered action and accelerator. Native checks
exercise the menu item, W mnemonic, terminal shortcut, and palette activation,
including an empty window and the collapsed sidebar overlay.

GTK 4.22 and libadwaita 1.9 feature levels are declared in
[`Cargo.toml`](../Cargo.toml). The binaries deny deprecated Rust API use except
for the diagnostic below. Ghostty's terminal C API is pinned by
[`scripts/bootstrap-ghostty.py`](../scripts/bootstrap-ghostty.py).

## Limits

The screenshot diagnostic retains a block-scoped exception for deprecated GTK
CSS background and frame calls inside `capture_window` in
[`app/backend.rs`](../src/app/backend.rs). Allocation uses the supported
`compute_bounds()` and `baseline()` APIs.
`GtkWidgetPaintable` can return a cached render node when the compositor pauses
an occluded window. The diagnostic builds a fresh snapshot to capture current
content and window decoration. Normal UI controls do not use that exception.
The [screenshot investigation](screenshot-diagnostics.md) records tested public
alternatives and the remaining GTK API requirement. The isolated `screenshot`
suite checks fresh pixels and decoration while a fully covered window's frame
clock is paused, including fractional monitor scaling.

AT-SPI tests verify command announcements and search relationships; Orca speech
output has not been tested. Run the [native regression suite](testing.md) for
menu, focus, preferences, and accessibility checks.

## References

- [GNOME HIG: UI styling](https://developer.gnome.org/hig/guidelines/ui-styling.html)
- [GNOME HIG: menus](https://developer.gnome.org/hig/patterns/controls/menus.html)
- [GNOME HIG: keyboard interaction](https://developer.gnome.org/hig/guidelines/keyboard.html)
- [GNOME HIG: dialogs](https://developer.gnome.org/hig/patterns/feedback/dialogs.html)
- [AdwAlertDialog response appearance](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/method.AlertDialog.set_response_appearance.html)
- [AdwAlertDialog default response](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/method.AlertDialog.set_default_response.html)
- [AdwDialog parentless presentation](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/method.Dialog.present.html)
- [GtkApplication window lifetime](https://docs.gtk.org/gtk4/method.Application.add_window.html)
- [GNOME HIG: text fields](https://developer.gnome.org/hig/patterns/controls/text-fields.html)
- [GNOME HIG: writing style](https://developer.gnome.org/hig/guidelines/writing-style.html)
- [Libadwaita style classes](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/style-classes.html)
- [AdwShortcutLabel](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ShortcutLabel.html)
- [AdwExpanderRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ExpanderRow.html)
- [AdwButtonRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ButtonRow.html)
