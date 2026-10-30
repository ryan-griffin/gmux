# Remote daemons

Each daemon owns execution on its machine. Clients connect directly to a local
Unix socket or an authenticated remote TCP listener. A daemon does not aggregate
other daemons or store outgoing connections. The CLI and GTK app both use
`gmux-client` for saved destinations, authentication, and direct connections.

## Serve execution

Install the standalone daemon with `./scripts/install.sh --headless`, then run:

```sh
gmux serve
```

This restores local terminals and serves local Unix sockets using saved local
connection settings. To accept TCP clients, configure incoming access and choose
authentication explicitly, before starting or while the daemon is running:

```sh
gmux connection edit local --remote-access true --listen-address 100.64.0.10 --auth none
# Or use the same token file contents on the server and client:
gmux connection edit local --remote-access true --listen-address 0.0.0.0 --auth token --token-file ~/.config/gmux/token
```

Listener and destination addresses use TCP port `7331` when no port is specified.
Remote destinations accept hostnames or IP addresses, such as `server` or
`100.64.0.10`. Override the port with `server:8400` or `100.64.0.10:8400`.
IPv6 addresses can be bare or bracketed without a port; use `[::1]:8400`
to specify a port. Listener addresses must be IP addresses.
Saved addresses include the port in JSON. The connection dialogs hide port `7331`
and show custom ports.

TCP is not encrypted. Use a private network or an encrypted tunnel, such as
Tailscale or an SSH tunnel. Token authentication controls access; it does not
provide transport encryption. Token files contain 32 to 1024 bytes without
whitespace, optionally followed by a newline. Configuration stores file paths,
never token contents. Token files must be regular files; symlinks to regular files
are accepted.

`gmux serve` runs in the foreground. Add `--background` to detach it and return
once it is ready:

```sh
gmux serve --background
gmux status
gmux stop
```

A background daemon writes errors to `daemon.log` in the Gmux configuration
directory. Startup failures return a nonzero exit status. Both forms reject an
already running or starting local daemon. `serve` accepts no listener or
authentication overrides; use `gmux connection edit local` to change incoming
access settings. These edits persist and apply immediately to a running daemon.
GTK starts or attaches to a daemon when local execution is requested. Detached
starts and GTK acquisition wait up to eight seconds for readiness. Canceling a
GTK acquisition leaves the daemon running. Reading
client configuration, adding a remote, or connecting to one never starts a local
daemon.

## Connect from the CLI

Add a named connection on the client machine, then select it with `--connection`:

```sh
gmux connection add 100.64.0.10 --name server --auth none
gmux connection list
gmux --connection server list
gmux --connection server new --name build --directory /srv/project
gmux --connection server split --pane 2 --down
gmux --connection server send 'make test' --pane 2 --enter
gmux --connection server read --pane 2
```

All these commands are short-lived clients. They do not require `gmux serve`
on the client machine. The destination must be running a daemon. Remote pane
commands require an explicit `--pane`.
Remote directories must be absolute or begin with `~/`; the destination expands
its own home directory. Local commands resolve relative paths in the calling shell
and may use `GMUX_PANE` for an implicit pane target.

Names default to the server's hostname. Adding a destination authenticates it and
pins its persistent daemon identity. Address and credential edits must reach the
same identity. Renaming or removing a destination works while it is offline.
Removing it leaves its execution running. To stop that daemon and its processes,
use `gmux --connection server stop`.

Use `gmux connection edit NAME --name NEW_NAME` to rename a connection. The same
command accepts `--address`, `--auth`, and `--token-file` for remote settings.
Use `gmux connection remove NAME` to remove a saved remote connection.

`gmux connection` manages the same local and remote entries as GTK's Manage
Connections. `--connection NAME` selects a destination in both binaries and can
appear before or after a command. Omitting it selects the local daemon in `gmux`
and the window's default connection in `gmux-gtk`. `--connection local` always
selects this computer; its current display name also works. Selecting it does not
start a daemon. The local entry can be renamed, but cannot be removed.
Targets inherited from `GMUX_PANE` or `GMUX_TAB` select local execution even when
the window defaults to a remote. To select a remote target instead, pass
`--connection` and an explicit ID.

## Graphical clients

`gmux-gtk` starts with local execution disabled and connects directly to saved
remotes. It does not require a local daemon, including when no connections exist.
To select a remote as the window's default destination:

```sh
gmux-gtk --connection server
```

This selects the named remote as the window's default destination and connects to
other saved remotes. If GTK is already open, it changes the active window's default.
Add `--new-window` to open another window with that default; otherwise, new windows
inherit the active window's default. `--directory PATH` opens a tab on that
connection, resolving local relative paths from the calling shell and remote `~/`
paths on the destination. Local execution follows its separate saved preference.
The GTK client can use remotes without the `gmux` executable installed.

Saved remotes connect when a window opens and retry automatically after failed
connections or network loss. Retry delays grow from about one second to at most
thirty seconds, with small random variations between clients. A connection must
stay healthy for ten seconds before its retry delay resets. Manage Connections
shows Reconnecting and lets you retry immediately while waiting. Authentication,
version, identity, and invalid credential-file errors pause automatic retries;
correct the settings or retry manually. Removing a connection or closing the
window cancels its retries. Reconnection restores the current terminal state
without replaying input or commands. Jobs continue if the remote daemon stayed
running; a restarted daemon restores fresh shells.

Local and remote terminal streams send a heartbeat every ten seconds. Thirty
seconds without a reply or terminal output closes the stream and starts recovery,
including when the host still accepts TCP data but its daemon has stopped
responding. Remote streams also use TCP keepalive and a thirty-second timeout
for unacknowledged data. CLI commands remain short-lived and do not retry
execution requests automatically.

Open **Manage Connections…** to add or check remote daemons. To run
terminals locally, open **This Computer**, turn on **Local Daemon**, and Save.
Use **Allow Remote Connections** to configure incoming access separately.
See [connection dialogs](ui.md#dialogs-and-accessibility) for dialog behavior.
Connections added or edited from the CLI also appear in running GTK windows.

Workspaces can contain tabs from several connections, with a starting directory
for each:

```sh
gmux-gtk workspace add --name Project --connection-directory 'server=/srv/project'
gmux-gtk --connection server new --workspace 1
gmux-gtk workspace edit 1 --name Build --connection-directory 'server=/srv/build'
gmux-gtk workspace remove 1
```

Repeat `--connection-directory CONNECTION=PATH` for several destinations.
`--directory PATH` sets the workspace's local starting directory. Removing a
workspace keeps its tabs open.

Closing a window detaches its streams and leaves execution running. Quit Gmux
stops local execution when that window uses it. In a remote-only window it exits
the app and leaves remote execution running.

## Saved state

Connection settings live under `$XDG_CONFIG_HOME/gmux/`, normally
`~/.config/gmux/`. Clients save outgoing connections in `connections.json`;
the daemon saves incoming access settings in `remote-access.json`.
See [saved state](terminal-sessions.md#persistence-and-recovery) for terminal
history and GTK settings.

`gmux connection list` includes this computer, saved remotes, local daemon status,
and remote access settings, even when no daemon is running. `remote_access` holds
the saved configuration; `running` reports the local daemon's current state and
`bound_address` reports its active TCP listener, or null. Rename the local entry
or restore its hostname with:

```sh
gmux connection edit local --name desktop
gmux connection edit local --use-hostname
```

Edit the local connection to configure incoming access:

```sh
gmux connection list
gmux connection edit local --remote-access true --listen-address 100.64.0.10 --auth none
gmux connection edit local --remote-access false
```

Enabling remote access requires an explicit `--auth`. Unspecified settings keep
their saved values. Use `--listen-address` for the local listener and `--address` for a
saved remote destination. Authentication applies to incoming clients when editing
local, and to the destination daemon when editing a remote connection.

Remote access edits persist separately from outgoing connections and work while the
daemon is stopped. They do not start local execution. A running daemon applies
them immediately; a stopped daemon uses them on its next `serve`. Disabling
remote access, changing its listening address, or rotating its token disconnects
existing TCP clients without stopping terminals. Address edits can keep the same
port when switching between loopback and wildcard interfaces. Failed edits preserve
the saved settings and restore the previous listener and its connected clients.
`gmux serve` always reads these saved settings. If the TCP listener
cannot start, local execution remains available and `gmux connection list` reports
the error so the settings can be corrected.

See [daemon API](daemon-api.md) for protocol details and
[client library](code-organization.md#client-library) for Rust integration.
