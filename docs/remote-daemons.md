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
GTK starts or attaches to a daemon when local execution is requested. Reading
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
The GTK executable links the client library without PTYs or libghostty. It can
run without the `gmux` executable when local execution is not requested.

Manage Connections always includes one **This Computer** entry. Its **Local Daemon**
toggle stays temporary until Save; Cancel discards it. Saving it on starts or
attaches in every GTK window, remembers the choice for later launches, and
restarts the daemon if it stops. Failed starts retry with backoff. Saving it off
asks for confirmation, cancels recovery, and stops local terminals while keeping
GTK and remote connections open. Canceling the confirmation keeps the draft open
and leaves the saved settings and daemon unchanged.

**Allow Remote Connections**, the listening address, and authentication control
incoming TCP access separately. They can be saved while local execution is off
without starting a daemon. Enabling remote access takes effect when the daemon starts.
Unix sockets remain accessible only to their owning user.

Manage Connections edits the client catalog and shows live connection status.
Reconnect restores views of existing remote execution. Each connection has its
own requests and terminal stream, so a stalled remote does not block another
connection. Catalog edits from the CLI are observed by running GTK windows.

Workspaces, assignments, per-connection starting directories, split proportions,
filters, and window defaults belong to the GTK app. They are absent from the
client library and daemon protocol. Workspaces can span connections:

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

Files live under `$XDG_CONFIG_HOME/gmux/`, normally `~/.config/gmux/`:

- `connections.json` belongs to clients. It contains the local display alias and
  named remote addresses, credential file paths, machine kinds, and pinned identities.
- `remote-access.json` belongs to the daemon. It contains its saved TCP listener settings.
- `clients/gtk/state.json` contains GTK workspace organization and presentation.
- `daemon-id.json`, `execution.json`, and `scrollback/` belong to local execution.

Client catalog edits use a writer lock and atomic replacement. Invalid files
remain untouched. A daemon never loads the client catalog or GTK state, so damaged
client settings cannot prevent local execution from starting.

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

## Client and socket APIs

Use `gmux_client::connections::Catalog` to inspect saved destinations,
`connections::call` for CLI-style configuration and execution calls, and
`ipc::Endpoint` for direct Unix or TCP operations. Its `state` and `command`
methods return typed execution replies; `execute` accepts `control::ExecutionRequest`.
`connection::Connection` owns asynchronous setup, retries, and disconnect status.
It exposes cached terminal views through `remote::SessionManager` and returns
state and pane updates from `poll`. `connections::Watch` reports catalog edits.
`local::ensure_daemon` is an explicit, cancellable local-launch helper.
It is never invoked by remote operations or catalog loading.

Both transports expose the same execution model and viewport protocol. TCP adds
a versioned authentication handshake and requires explicit targets. Incoming
clients cannot access outgoing client configuration, workspace state, or local
remote access administration. Local daemon remote access administration uses `remote-access-get`
and `remote-access-set` on the Unix control socket. The GTK `.ui` socket separately
handles workspace and window operations.

See [daemon API](daemon-api.md) for execution requests, authentication handshakes,
viewport packet encoding, and limits. See [background sessions](terminal-sessions.md)
for persistence, process ownership, and view scheduling.
