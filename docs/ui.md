# UI behavior

The window uses an `AdwOverlaySplitView` with a tab sidebar and terminal
content. Local and remote tabs share the list. A connection identifies one
terminal service, with a display name that defaults to its hostname. Each tab row
shows its title on the top left and its connection on the top right, its directory
on the second line, and its Git branch and workspace on the bottom left and right.
The footer keeps its height when optional metadata is absent. An unread dot
appears to the left of the title and occupies no space when hidden. An icon beside
the connection name identifies the computer's hardware type; disconnected
connections use an offline icon and warning color. The tooltip retains the full
connection name and includes its status when connecting or disconnected.
Nested `GtkPaned` widgets hold split panes.
Tooltips for truncated tab titles, branches, and workspace names show their full
text. Icon tooltips explain their meaning, and hovering unused row space shows
no tooltip.
Directory labels and workspace starting-directory fields shorten each connection's
own home directory to `~`. Remote home paths come from the owning daemon. Terminal
tooltips and pane paths remain absolute; workspaces also accept `~` and `~/…`.

Local and remote tabs show the Git branch of their active pane. The owning daemon
collects this optional metadata while clients subscribe. Repository watches and
bounded Git workers are shared by those clients and released when unused. Branch
updates refresh the sidebar without redrawing the terminal. A detached HEAD,
non-repository directory, unavailable Git, or disconnected connection hides the label.

The sidebar filter menu lists All Tabs and workspaces. The header shows the
selected filter. New Tab uses the window's default connection; its dropdown chooses the local or a
saved remote connection. Both inherit the workspace filter. Each workspace
has optional starting directories keyed by connection identity. Tabs use the
destination's configured directory, or its home directory when no override is
set. A missing configured directory reports an error without creating a tab.
The selected tab does not affect new tab creation. Removing a workspace returns
windows filtering it to All Tabs. Its tabs remain open without an assignment.
Workspace membership belongs to the receiving desktop. It orders tabs newest first
using creation times from their owning daemons, including existing tabs when adding
a connection. Notifications and selection do not change the order. A disconnected
tab keeps its last viewport. Remote connections retry automatically and show
Reconnecting while recovering. Use Retry Now in Manage Connections to retry
immediately while waiting. Authentication, version, and identity errors pause
automatic recovery. Edit the connection or retry manually after correcting
the problem.

## Menus and focus

The main menu contains window and tab creation, workspace and connection
management, fullscreen, preferences, shortcuts, and Quit. The New Tab dropdowns
list the enabled local connection and saved remote connections directly under a
New Tab via Connection heading. Choosing one creates a tab there, keeping the selected
workspace. Disconnected connections remain visible with their icons, but their
menu items are disabled until reconnection. Availability follows live connection
events, including while a menu is open. Connection names update when renamed.
The main menu keeps its New Tab action and a New Tab via Connection submenu.
F10 reveals the sidebar and opens the main menu.

The empty-state New Tab split button uses custom pill proportions, retaining
Adwaita's colors and interaction states.

Terminal Actions targets the active pane or tab. A terminal's context menu
targets the clicked pane. Use Shift+right-click to open it while an application
captures mouse input; the Menu key and Shift+F10 also open it. Tab context menus
target the clicked tab.

Arrow navigation keeps focus in the sidebar. Clicking a tab or pressing Enter
activates it and focuses its terminal. Application shortcuts consume key presses
and releases before terminal encoding.

Clear Screen erases the primary screen and scrollback for all views of a pane,
clears selections, and restarts each view's Find query. It waits for a complete
terminal sequence and rejects the alternate screen used by full-screen apps. Use
Automatic Tab Name restores the active pane's automatic title.

Closing a window detaches it. Close Pane, Close Tab, and Quit ask for
confirmation when their affected panes have busy processes.

## Dialogs and accessibility

Manage Connections uses the same dialog layout as Manage Workspaces, with an Add
Connection row, the local connection labeled This Computer, then saved remote
connections. Local and remote rows show their connection status beside Retry Now.
Connected uses the theme's success color, recovery after a failure uses warning,
and disconnection with an error uses error. Connecting and Disabled stay neutral.
The local row shows Disabled when local execution is off. Status remains readable
as text in every theme. Retry stays visible but disabled
while a connection is connected or connecting. The local button appears only
when local execution is enabled and uses the shared local recovery flow to skip
the retry delay. The local entry remains available when its daemon is stopped or
local execution is disabled. The Local Connection dialog includes its name,
a Local Daemon toggle with the subtitle Run terminals on this computer,
and a separate Remote Access group. The toggle defaults to
off and stays temporary until Save. Cancel discards the draft. The daemon state
changes after the preference is saved to disk. If saving fails, the editor keeps
the draft and displays the error. Saving it on starts or attaches to the daemon,
remembers the choice for future launches, and restarts
the daemon if it stops. Failed recovery backs off from one second to thirty
seconds. The delay resets after a connection stays healthy for ten seconds.
Saving it off asks for confirmation, stops local
terminals, and cancels recovery while keeping the app and remote connections open.
Canceling the confirmation keeps the draft open and leaves the saved settings and
daemon unchanged. Use Hostname restores the default name when saved. Allow Remote
Connections enables a listener on the chosen IP and port. Authentication can be
None or Token; Token uses matching token contents on both computers. These
connections need a private network or an encrypted tunnel. Save applies and
persists the settings. Disabling remote access disconnects incoming clients and
leaves terminals running. Remote access settings can be saved while the daemon
is stopped. They do not enable local execution.

The empty page shows No Connections, Connecting, Reconnecting, or Not Connected.
It offers Manage Connections when no daemon is connected. Workspace organization
remains available without any daemon.

Management rows open editors without navigation arrows; Add rows have a plus icon
on the left. Editors are independent dialogs. Opening one from a manager leaves
the list underneath; Add Workspace from a menu, command palette, or tab action
opens the editor directly without opening the manager. Editors have Cancel and a
suggested Save or Add action in the header, which stays visible while longer forms
scroll. Saving or removing closes the editor and updates any open manager.
Cancel and Escape close only the editor and discard unsaved changes.
The manager's Close button dismisses the list. Removal and saving Local Daemon off
open confirmations whose affirmative buttons use the destructive style.
Workspace editors have a directory row for each configured
connection. Use Home Directory resets a row to `~`. Only the local directory row
has a folder picker; remote rows accept paths on the remote computer. Remote
paths must be absolute or start with `~/`. The destination daemon expands `~`
and checks that the directory exists when creating a tab. Local typed relative
paths resolve from the frontend's working directory and are checked when saved.
Move to New Workspace fills the row for the tab's connection with its current
pane directory; other connections start at home. Token-file rows also have
picker buttons. Token-file paths expand `~` and resolve relative paths on the
frontend before being sent to the daemon. Pickers use the standard file chooser,
and removals use confirmation alerts. The remote connection form accepts a name,
address, and
optional token file. A blank name on a new connection uses the remote hostname,
with a suffix if the name is taken. It checks additions and changed connection
settings asynchronously and displays errors in the form. The lists update when
another window or the CLI changes workspace definitions, connection names,
incoming connection settings, or saved remote connections. Renames update tab
captions and filters in all windows.
Connection editors enable Save when the form has a valid change. Reverting all
values to their saved settings disables Save again.

Dialogs use libadwaita widgets. Startup errors use a parentless `AdwAlertDialog`
registered with the application, since initialization can fail before a window
exists. Close has initial focus and handles Enter, Escape, and window dismissal.
Repeated activation raises the existing error dialog.

The command palette ranks labels, related terms, and small spelling mistakes.
The search field keeps keyboard focus while the selected row scrolls into view.
Disabled commands remain visible but keyboard navigation skips them. Accessible
relations and announcements expose the current selection and empty results.

Find combines a text field, match count, and clear button. Preferences apply
font, cursor, color scheme, and pane-title changes to existing panes. Each window keeps
its own sidebar visibility and zoom.

The terminal exposes read-only viewport text, caret, selection, and cell
geometry through `GtkAccessibleText`. See [terminal rendering](renderer.md).

## Screenshot diagnostic

The `screenshot` socket command builds a fresh window snapshot because an
occluded window can have a paused frame clock and a stale `GtkWidgetPaintable`.
`capture_window` in `crates/gtk/src/app/backend.rs` uses a scoped deprecated GTK CSS call
for the window background and frame. This is separate from terminal rendering.
