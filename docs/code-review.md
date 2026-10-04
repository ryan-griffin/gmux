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

The Find field combines a `GtkText`, match-count label, and clear button under
Adwaita's entry styling. This places the count inside the field while retaining
its focus ring and error state.

## Menus and focus

The main menu and both New Tab dropdowns share a menu model with live workspace
choices. Show Tabs and Fullscreen use stateful actions whose checkmarks follow
the widgets. The separate Terminal Actions menu targets the active pane and
includes split, Find, zoom, Close Pane, and Close Tab actions.

Menus use accessible names, keyboard navigation, and distinct mnemonics. Dynamic
workspace labels escape literal underscores before choosing mnemonics. Tab
context-menu actions target the clicked tab, including workspace moves and
creation. Terminal context-menu actions target the clicked pane.

The main menu is GTK's primary menu for F10. The Menu key and Shift+F10 open a
terminal context menu; Shift+right-click overrides terminal mouse tracking.
Sidebar arrow navigation keeps focus in the list. Clicking a tab or pressing
Enter activates it and focuses its terminal.

Quit Gmux remains in the main menu because closing the window leaves terminal
processes running. Close Pane, Close Tab, and Quit ask for confirmation when
affected panes have busy processes. [Session lifecycle](terminal-sessions.md) describes detach and
shutdown behavior.

## Command palette and accessibility

Command rows use `AdwActionRow` and `AdwShortcutLabel`. Shortcut labels read the
registered application accelerators. Disabled commands remain visible and
keyboard navigation skips them. `AdwStatusPage` handles an empty search result.

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

GTK 4.22 and libadwaita 1.9 feature levels are declared in
[`Cargo.toml`](../Cargo.toml). The binaries deny deprecated Rust API use except
for the diagnostic below. Ghostty's terminal C API is pinned by
[`scripts/bootstrap-ghostty.py`](../scripts/bootstrap-ghostty.py).

## Limits

The screenshot diagnostic uses deprecated GTK CSS background and frame calls
inside `capture_window` in [`app/backend.rs`](../src/app/backend.rs).
`GtkWidgetPaintable` can return a cached render node when the compositor pauses
an occluded window. The diagnostic builds a fresh snapshot to capture current
content and window decoration. Normal UI controls do not use that exception.

AT-SPI tests verify command announcements and search relationships; Orca speech
output has not been tested. Run the [native regression suite](testing.md) for
menu, focus, preferences, and accessibility checks.

## References

- [GNOME HIG: UI styling](https://developer.gnome.org/hig/guidelines/ui-styling.html)
- [GNOME HIG: menus](https://developer.gnome.org/hig/patterns/controls/menus.html)
- [GNOME HIG: dialogs](https://developer.gnome.org/hig/patterns/feedback/dialogs.html)
- [GNOME HIG: text fields](https://developer.gnome.org/hig/patterns/controls/text-fields.html)
- [GNOME HIG: writing style](https://developer.gnome.org/hig/guidelines/writing-style.html)
- [Libadwaita style classes](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/style-classes.html)
- [AdwShortcutLabel](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ShortcutLabel.html)
- [AdwExpanderRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ExpanderRow.html)
- [AdwButtonRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ButtonRow.html)
