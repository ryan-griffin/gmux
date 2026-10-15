# Gmux

A persistent terminal daemon with a management CLI and an optional GNOME client,
inspired by [cmux](https://cmux.com/). The daemon uses Rust and libghostty-vt;
the graphical client uses GTK 4 and libadwaita.

The daemon owns terminal processes, tabs, split panes, history, and notifications.
Its CLI works without a graphical client, and clients can connect over local
Unix sockets or authenticated TCP. Terminal sessions keep running when clients
disconnect. The GNOME client adds workspaces, a searchable sidebar, and Git labels.

This project is pre-alpha. Saved data formats may change between builds.

## Build and run

The daemon requires Linux, Rust 1.99+, a C compiler, libclang, pkg-config,
curl, and Python 3.12+. Building the GNOME client also requires GTK 4.22+,
libadwaita 1.9+, and `glib-compile-resources`. The checked-in Rust toolchain file
selects the development compiler with rustup.

The first installation downloads Zig and builds the pinned Ghostty library in
`.deps/`. The default installation prefix is `~/.local`; pass a different prefix
to choose another directory.

To install the standalone daemon and its built-in CLI:

```sh
./scripts/install.sh --headless
```

This installs `gmux` and the terminal engine. It uses the same daemon
package as the graphical installation and requires no GTK or desktop resources.
Start the daemon in the background under your user account:

```sh
gmux serve --background
```

The command returns when the daemon is ready. Errors go to `daemon.log` in the
Gmux configuration directory. Plain `gmux serve` runs in the foreground, including
when managed by a service manager. Both forms use saved local connection settings
and fail if the daemon is already running.

Manage its terminals:

```sh
gmux new --name server --directory ~/Projects/my-app
gmux list
gmux status
gmux stop
```

Stop checkpoints history and terminates the daemon's terminal processes.
To build the daemon alone, use `cargo build --locked -p gmux`.
Bare `gmux` shows CLI help; `gmux serve` runs the daemon. The default workspace
build also builds the graphical client, `gmux-gtk`.

## GNOME client

To install the daemon with the optional graphical client, run `./scripts/install.sh`.
To build and run the graphical client from the checkout:

```sh
./scripts/run.sh
./scripts/run.sh --directory ~/Projects/my-app
```

The app opens without starting or connecting to a local daemon. In **Manage
Connections…**, open the **This Computer** entry, enable **Local Execution**, and
Save to start or attach to the local daemon. Gmux remembers this choice for future
launches. You can also add remote connections and use them without local execution.

Create tabs with the plus button and split panes through Terminal Actions.
Manage workspaces through the main menu. Use the command palette or Keyboard
Shortcuts dialog to find other actions.

| Action | Shortcut |
| --- | --- |
| New window | Ctrl+Shift+N |
| New tab | Ctrl+Shift+T |
| Split right / down | Ctrl+Shift+D / Ctrl+Shift+E |
| Close pane | Ctrl+Shift+W |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab |
| Copy / paste | Ctrl+Shift+C / Ctrl+Shift+V |
| Find | Ctrl+Shift+F |
| Command palette | Ctrl+Shift+P |

Closing a window leaves its terminals running. Reopen Gmux to reconnect, or use
**Quit Gmux** or Ctrl+Shift+Q to stop the app's connected local daemon and its
terminals, including those used by other clients. Remote terminals keep running.
`gmux stop` stops the addressed daemon even when GTK is closed. After a daemon restart, Gmux restores the saved layout
and text history with fresh shells.
Commands and jobs do not resume.

Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`.
The daemon saves execution and history; the frontend saves workspace organization
and presentation separately under `clients/gtk/`.

## Shell automation

With the daemon running, its CLI can create panes, send input, and deliver alerts:

```sh
gmux list
gmux new --name server --directory ~/Projects/my-app
gmux split --down
gmux send 'cargo test' --enter
gmux notify 'Tests finished'
```

Run `gmux --help` for commands. Pane commands use the calling shell's
`GMUX_PANE`; otherwise pass `--pane` when more than one pane exists. Commands
address the daemon directly, regardless of whether a GUI is open.

The optional `gmux-gtk` client CLI handles workspace organization and window actions.
Its terminal commands use the client's active pane and workspace defaults.
Both binaries support `rename --tab ID NAME`, `reset-name --tab ID`, and
`close-tab --tab ID`. Use `--connection NAME` to target a saved connection.
`gmux-gtk quit` and workspace or window actions require an open app. Use them
without `--connection`; `gmux --connection NAME stop` stops a particular daemon.

## Remote daemons

Serve a machine's terminals over a private network such as Tailscale:

```sh
gmux connection edit local --allow-remote true --listen 100.64.0.10:7331 --auth none
gmux serve --background
```

Add a connection on the client machine and manage its terminals, with no local
daemon required:

```sh
gmux connection add 100.64.0.10:7331 --name server --auth none
gmux connection list
gmux --connection server new --directory /srv/my-app
gmux --connection server list
gmux --connection server send 'cargo test' --pane 2 --enter
gmux --connection server read --pane 2
```

`gmux connection` manages the same entries as **Manage Connections…** in GTK.
Use `edit` and `remove` to update saved connections. Use `--connection NAME` in
either binary to select a saved connection, or
`--connection local` for this computer. Removing a connection leaves its terminals
running. `gmux connection edit local` also configures incoming access with
`--allow-remote true|false`, `--listen`, and `--auth`, including while the daemon is
stopped. `gmux connection list` reports its saved settings and live listener state.

Run `gmux-gtk --connection server` to select a remote as the window's default
connection, including when GTK is already open. Add `--new-window` to select it in
a new window. Local execution follows its separate saved setting and is off by
default. The CLI and GTK app share the `gmux-client` library for connections;
workspaces belong to the GTK app. Use `gmux-gtk workspace add`, `edit`, and
`remove` to manage them from a shell.

Local and remote connections share one tab sidebar. Each tab shows its connection
name and an icon for the computer's hardware type. Disconnected connections use
an offline icon and warning color. Open **Manage Connections…** to edit the local
connection, enable local execution, add remote connections, choose authentication,
reconnect, or remove saved connections. Names default to hostnames. Disabling **Local Execution** disconnects the app and leaves the daemon and its
terminals running. **Stop Daemon…** stops local execution while keeping the app
and remote connections open. The **Local Connection** dialog can enable or disable
**Allow Remote Connections** independently, without restarting. Set its
listening address and optional authentication, then Save. The sidebar header
filters by workspace or shows All Tabs. New Tab uses the window's default connection. The plus button's
dropdown lists local and remote connections under **New Tab via Connection**.
Choose a connection to create a tab there. Disconnected connections are disabled
in the menu; use **Manage Connections…** to reconnect.
Both actions assign new tabs to the filtered workspace when one is selected.
Workspaces can contain tabs from several connections, with a starting directory
for each connection. Unconfigured directories default to the destination's home
directory. Each client connects directly to its chosen daemons.
Each window releases its remote connections when it closes, leaving
terminals running. Closing the last window releases the receiver's retained
control connections. `gmux-gtk quit` stops local execution; remote terminals keep
running.

Listeners are off by default. Authentication must be chosen explicitly; `--auth
token --token-file /path/to/token` enables a shared token. TCP transport uses
the network's encryption, such as Tailscale's. See [remote
daemons](docs/remote-daemons.md) for setup, ownership, and the
[daemon API](docs/daemon-api.md).

## Development

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native
```

See [testing](docs/testing.md) for prerequisites and selected checks, and [code
organization](docs/code-organization.md) for the architecture.

## License

[MIT](LICENSE). The app icon adapts GNOME Console's artwork under
[GPL-3.0-or-later](data/icons/COPYING.Console). See the [icon
credits](data/icons/README.md). Ghostty and the GNOME libraries retain their own
licenses.
