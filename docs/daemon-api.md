# Daemon API

`gmux` is a standalone terminal service. Clients manage execution through
its control API and attach terminal views through its stream API. GTK is one
client; workspaces, window selection, and presentation belong to that client.

The `gmux-client` Rust library exposes control types, stream commands, packet
codecs, and authenticated TCP helpers without GTK. Clients in other languages
can implement the wire protocol below. This project is pre-alpha; protocol
changes may require client updates. Protocol version checks reject incompatible
clients before attachment.

## Rust clients

The Linux client library separates execution from saved connection configuration.
`ipc::Endpoint::remote` selects a saved connection. `ipc::Endpoint::tcp` accepts
`network::Credentials` directly, so another client can use its own secret store
without writing a token file. Both Unix and TCP endpoints provide the same typed
`state`, `command`, and `execute` operations. `command_result` returns only result
IDs and warnings, avoiding a local execution snapshot when it is unnecessary:

```rust
use gmux_client::{control::Command, ipc::Endpoint, network::Credentials};

let endpoint = Endpoint::tcp(address, Credentials::token(token)?, None);
let reply = endpoint.command(Command::New {
    directory: Some("/srv/project".into()),
    name: Some("Build".into()),
}, None)?;
let daemon = reply.daemon;
```

Create a `connection::Connection` with the daemon identity, endpoint, and an
`ipc::Wake`, then call `connect`. Setup runs outside the caller's event loop.
When the wake descriptor becomes readable, drain it and call `poll`. Updates
contain connection status changes, execution state, and pane changes. The
connected session manager provides terminal handles with borrowed `Frame` and
`Cell` snapshots, plus input, resize, scroll, selection, and search operations.
Rendering, workspaces, and window state belong to the client.

Connection cancellation interrupts DNS waits, TCP setup, and handshakes. DNS
lookups use a bounded worker pool because the system resolver cannot be canceled.
Connecting and reading client configuration never start a local daemon.
Explicit local lifecycle helpers live in `local`; endpoint `stop` waits for
shutdown before returning.

## Local connections

The control socket is `$GMUX_SOCKET`, normally `$XDG_RUNTIME_DIR/gmux.sock`.
Send one UTF-8 JSON object followed by a newline. Read one JSON reply followed
by a newline. Errors contain an `error` string. The daemon socket always
addresses that daemon's execution, even when a GUI is running.

```json
{"method":"new","directory":"/srv/project","name":"Build"}
```

Local layout replies contain their result IDs and the daemon identity:

```json
{"tab":1,"pane":2,"daemon":"..."}
```

Use `daemon-info` to discover the daemon identity and protocol version. IDs are
scoped to a persistent daemon identity and are never reused by that daemon.
A process restart preserves identity; a new installation at the same address
may have a different identity.

The `.stream` companion socket carries a persistent terminal connection. Send
`attach` with the discovered version and identity before other commands.
The frontend's `.ui` socket is a separate, optional client API for workspaces
and window actions. It is not part of the daemon API.

## Direct TCP connections

TCP listening is opt-in. The connection begins with one newline-terminated JSON
handshake. The current protocol version is 18:

```json
{"version":18,"channel":"control","token":"shared-token","daemon":"expected-identity"}
```

Choose `control` or `stream` for `channel`. Omit `token` for a listener configured
with no authentication. Omit `daemon` for initial discovery; subsequent
connections should supply the identity the client selected. The reply contains
`version`, `daemon`, `hostname`, `home_directory`, and `machine_kind`, or an
`error`. A mismatch in version, authentication, or expected identity fails the
handshake.

A control connection accepts multiple newline-terminated JSON requests, each
with one newline-terminated JSON reply. Direct TCP clients do not need a local
daemon. TCP provides no encryption; use an encrypted private network.

The Rust helper `network::call(address, credentials, expected, request)`
opens an authenticated control connection and returns the execution response.
Connection setup and the reply share a ten-second deadline, with five-second
limits on each phase. `network::connect_stream` returns an authenticated TCP
stream and its connection metadata. It leaves attachment and subscriptions to
the caller.

## Execution requests

Local and TCP control connections support these methods:

- `daemon-info` reports identity, version, process ID, listener address, attached
  clients, and execution statistics.
- `list` returns this daemon's tabs, topology, and pane metadata. Tab and pane
  references contain `daemon` and `id`.
- `new` creates a tab. Optional `directory` and `name` fields choose its working
  directory and shared name. Omitted directories use the daemon's home directory.
- `split` creates a pane beside `pane`. Set `down` to true for a vertical split.
- `close` terminates `pane`; `close-tab` terminates every pane in `tab`.
- `rename` sets `tab`'s shared `name`; `remove-tab-name` restores its automatic name.
- `send` writes `text` to `pane`. Set `paste` for terminal paste handling and
  `enter` to submit the text as a command.
- `read` returns a pane's visible terminal contents in `text`.
- `notify` delivers `message` to clients viewing `pane` and records an unread alert.
- `quit` checkpoints history, stops the addressed daemon, and terminates all of
  its terminal processes. The CLI calls this operation `stop`.

Both transports also expose `state`, `busy`, `metadata`, and `copy` for execution
clients. `state` returns the execution snapshot and revision. Layout mutations
over TCP return a snapshot with `result` containing the operation's output.
The Rust control helper unwraps those results for CLI-style requests.

Pane targets must be explicit over TCP. Destination directories must be absolute
or begin with `~/`, which the destination daemon expands. Local CLI commands
resolve relative paths against the calling shell. The daemon CLI never uses a
local shell's `GMUX_PANE` to select a remote pane.

Hosting configuration uses `hosting-get` and `hosting-set` on the local control
socket. Saved outgoing connections belong to the client library and use no daemon
API. Incoming TCP connections address only the serving daemon, including its
shutdown operation. They cannot manage hosting or client state.

## Terminal streams

After a successful TCP stream handshake, send:

```json
{"method":"attach","version":18,"daemon":"returned-identity"}
{"method":"subscribe","panes":[2]}
{"method":"resize","pane":2,"cols":80,"rows":24}
```

The same commands work on the local `.stream` socket after local identity
and version discovery. Attachment is required once per connection. Each view
owns its subscriptions, scroll position, selection, search, and paste assembly.
The daemon shares terminal output and processes across views. The most recently
focused view controls a pane's PTY size.

Use `input` with a byte array to write terminal input, or `key`, `mouse`, and
`paste` for terminal-aware input. `focused` declares the connection's focus.
`scroll`, `scroll-to`, `find`, `find-advance`, `select`, `clear-selection`,
`copy`, and `mark-read` operate on an explicit `pane`. `git-subscribe` chooses
panes whose Git metadata the client needs. The Rust `transport::StreamCommand`
and `StreamRequest` types define the complete command schema;
`transport::command` bounds its size and writes a newline-terminated command.

Replies are binary viewport packets. Each packet begins with a little-endian
32-bit payload length. The payload begins with a little-endian 32-bit JSON
header length, then that many UTF-8 JSON bytes, then cell bytes. The header
contains pane `meta`, an optional `frame`, changed `rows`, and optional execution
`state`, Git metadata, notifications, search results, selection, copy text, or
errors. A full viewport arrives on subscription; later packets contain changed
rows. Metadata-only packets have no cell data.

For each row listed in the header, there are `frame.cols` cells in column order.
Each cell has 71 bytes: 64 bytes of zero-padded UTF-8 text, three RGB foreground
bytes, three RGB background bytes, and one flags byte. The flag bits are bold,
italic, faint, underline, strike, invisible, and a wide-character continuation.
The Rust `transport::Header` and `Packet::read` expose the complete header and
validate packet dimensions and row indices before decoding cells.

Handshakes are limited to 4 KiB; control and stream commands to 1 MiB; replies
and viewport packets to 16 MiB. Pastes use UTF-8 chunks with a 4 MiB total limit.
See [remote daemons](remote-daemons.md) for authentication, connection budgets,
and saved destinations, and [background sessions](terminal-sessions.md) for
persistence and scheduling.
