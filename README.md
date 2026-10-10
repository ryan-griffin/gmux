# Gmux

A GNOME terminal inspired by [cmux](https://cmux.com/), built with Rust, GTK 4, libadwaita, and libghostty-vt.

- Flat, searchable tab sidebar with workspace filters and Git branch information.
- Split terminal panes, background sessions, layout restoration, and a command palette.
- Adwaita light and dark themes, GNOME's monospace font, and configurable cursor and pane title bars.
- Desktop notifications with unread indicators and a local CLI for shell automation.

## Run

Requires Linux, Rust 1.95+, a C compiler, libclang for FFI binding generation, pkg-config, GTK 4.22+, libadwaita 1.9+, GLib's `glib-compile-resources`, curl, and Python 3.12+.
With rustup, [`rust-toolchain.toml`](rust-toolchain.toml) selects Rust 1.99.0
with rustfmt and Clippy for development. The [testing guide](docs/testing.md#rust-requirements)
explains the minimum compiler and provides the Fedora 44 validation environment.

```sh
./scripts/run.sh
# Start a tab in a specific directory:
./scripts/run.sh --directory ~/Projects/my-app
```

The first run downloads Zig and builds the pinned Ghostty library under `.deps/`. Nothing is installed system-wide.

To install in `~/.local`, run:

```sh
./scripts/install.sh
```

Pass another installation prefix as the script's first argument.

## Use

Closing the window keeps panes and commands running in a headless daemon. Reopen Gmux to reconnect to the same terminals. Use **Quit Gmux**, Ctrl+Shift+Q, or `gmux quit` to stop them.

Open another window with **New Window**, Ctrl+Shift+N, or `gmux --new-window`.
After installation, GNOME also offers New Window in the app icon's context menu
and opens another window when you middle-click the icon.
Windows share tabs, split topology, tab names, workspace membership, unread alerts,
and live terminals. You can type into the same pane from either window.
Each window keeps its own active tab and pane, workspace filter, sidebar search
and visibility, split proportions, zoom, terminal search, selection, scroll
position, and window size. The most recently focused window viewing a pane
controls its PTY size. Closing a window detaches that window; Quit stops all
windows and terminals. Saved presentation supplies defaults for the next window.

Create tabs with the plus button. Manage Workspaces in the main menu lets you add, edit, or remove saved workspaces. The sidebar filter also offers workspace management. Sidebar visibility is remembered when you reopen Gmux. Right-click a tab to rename it, move it to an existing or new workspace, or remove its workspace membership; middle-click to close it. Deleting a workspace keeps its tabs open and unassigned. Exiting a shell closes its pane.

The main menu stays above the sidebar and groups New Window with tab creation, including tabs in a chosen directory or workspace. F10 reveals the sidebar and opens the menu. New Tab in Workspace lists saved workspaces and is disabled when none exist. The New Tab dropdowns contain only tab creation actions. Fullscreen is a checked item. Preferences, Keyboard Shortcuts, and About Gmux are grouped at the bottom.

The three-dot Terminal Actions menu in the content header contains Rename Tab, Split Right, Split Down, Find, zoom controls, Close Pane, and Close Tab. Rename Tab changes the active tab's name. To remove a custom name, choose Remove Tab Name beside Rename Tab in the Terminal Actions menu or the tab's context menu. The tab then follows the active pane's title again. Close Pane affects the active pane; Close Tab closes every pane in the active tab. Both close actions ask for confirmation when processes are running. This menu is available with the sidebar hidden.

Right-click a terminal for Copy, Paste, Find, Split Right, Split Down, and Close Pane. In apps that capture mouse input, use Shift+right-click to open the menu. The Menu key and Shift+F10 also open it.

| Action | Shortcut |
| --- | --- |
| New window | Ctrl+Shift+N |
| New tab | Ctrl+Shift+T |
| New tab in directory | Ctrl+Shift+O |
| Split right / down | Ctrl+Shift+D / Ctrl+Shift+E |
| Close pane, or window when the empty state is shown | Ctrl+Shift+W |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab |
| Copy / paste | Ctrl+Shift+C / Ctrl+Shift+V |
| Find in terminal history | Ctrl+Shift+F |
| Commands | Ctrl+Shift+P |
| Main menu | F10 |
| Preferences | Ctrl+, |

The app's Keyboard Shortcuts dialog lists the remaining shortcuts. Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`.

Explicit Quit and orderly daemon shutdown save bounded scrollback checkpoints there. With Restore tabs enabled, reopening restores the primary-screen text history and starts fresh shells in the saved directories. Commands and jobs do not resume. Unicode, wrapped lines, colors, and text attributes survive; hyperlinks and full-screen application contents do not. A Ghostty source upgrade or damaged checkpoint starts that pane with empty history. A crash or power loss can lose output since the last orderly shutdown. See [scrollback checkpoints](docs/terminal-sessions.md#scrollback-checkpoints) for limits and recovery.

## Shell automation

With the Gmux daemon running, including while its window is closed, use `gmux` inside a pane or `target/debug/gmux` from another terminal:

```sh
gmux list
gmux new --name server --directory ~/Projects/my-app
gmux workspace new --name my-app --directory ~/Projects/my-app
gmux split --down
gmux send 'cargo test' --enter
gmux notify 'Tests finished'
```

Pane commands default to the calling shell's pane, or the active pane when called outside Gmux. Run `gmux --help` for all commands and `gmux <command> --help` for options.

Alerts from the focused terminal pane stay read and do not send desktop notifications while the Gmux window is active. Other panes and tabs still notify, including split panes in the active tab. When another window has focus, every pane can notify.

## Development

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
# Include the desktop regression suite in an isolated Wayland compositor:
python3 scripts/check.py --native
```

The runner checks formatting, Rust tests, Clippy, builds, and smoke tests.
GitHub Actions runs the headless checks on Rust 1.95.0 and 1.99.0 in a pinned
Linux container. Run native compositor checks separately with `--native`.
Use the [testing guide](docs/testing.md) for prerequisites, selected checks,
rendering backends, appearance variants, and failure logs.

- [Code organization](docs/code-organization.md) maps module responsibilities.
- [GNOME UI design](docs/code-review.md) explains widgets, menus, focus, and accessibility.
- [Background sessions](docs/terminal-sessions.md) covers lifecycle, scheduling, transport, and persistence.
- [Terminal rendering](docs/renderer.md) covers Pango shaping, GSK nodes, caches, and renderer benchmarks.
- [Application performance](docs/performance.md) records benchmark methods and dated build comparisons.

## License

[MIT](LICENSE). The app icon adapts GNOME Console's artwork under [GPL-3.0-or-later](data/icons/COPYING.Console); see the [icon credits](data/icons/README.md). The bundled branch icon is [CC0](data/icons/COPYING.md). Ghostty and the GNOME libraries retain their own licenses.
