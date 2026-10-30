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

Use `status` to discover the daemon identity and protocol version. IDs are
scoped to a persistent daemon identity and are never reused by that daemon.
A process restart preserves identity; a new installation at the same address
may have a different identity.

The `.stream` companion socket carries a persistent terminal connection. Send
`attach` with the discovered version and identity before other commands.
The frontend's `.ui` socket is a separate, optional client API for workspaces
and window actions. It is not part of the daemon API.

## Direct TCP connections

Rust client address arguments accept a hostname or IP address with an optional
port. Omitting the port uses `network::DEFAULT_PORT`, which is `7331`. Use
`server:8400` or `[::1]:8400` to override it.

TCP listening is opt-in. The connection begins with one newline-terminated JSON
handshake. The current protocol version is 40:

```json
{"version":40,"channel":"control","token":"shared-token","daemon":"expected-identity"}
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

- `status` reports identity, version, process ID, `hostname`, `bound_address`,
  attached Unix and TCP stream counts in `local_clients` and `remote_clients`,
  and execution statistics. Connection display names are client-owned aliases; `hostname`
  always identifies the daemon's host.
- `scrollback` inspects or updates the daemon's saved per-terminal byte and
  physical-line limits. Optional `max_bytes` and `max_lines` fields accept an
  unsigned integer, `"default"`, or `"unlimited"`. Omission leaves a dimension
  unchanged. Updates save before pruning existing history. See
  [scrollback retention](scrollback.md) for zero semantics, responses, and failures.
- `list` returns this daemon's tabs, topology, and pane metadata. Tab and pane
  references contain `daemon` and `id`.
- `new` creates a tab. Optional `directory` and `name` fields choose its working
  directory and shared name. Omitted directories use the daemon's home directory.
- `split` creates a pane beside `pane`. Set `down` to true for a vertical split.
- `close` terminates `pane`; `close-tab` terminates every pane in `tab`.
- `rename` sets `tab`'s shared `name`; `reset-name` restores its automatic name.
- `send` writes `text` to `pane`. Set `paste` for terminal paste handling and
  `enter` to append a carriage return and submit the text as a command.
  `enter` works with both raw input and paste handling.
- `read` returns a pane's visible terminal contents in `text`.
- `links` returns `frame` and `hyperlinks` for the daemon's default viewport.
  `gmux links --pane ID` prints the same JSON. This inspection does not select
  a frontend's scrolled view; use its stream snapshot or `gmux-gtk links` for that.
- `notify` delivers `message` to clients viewing `pane` and records an unread alert.
- `stop` checkpoints history, stops the addressed daemon, and terminates all of
  its terminal processes. `quit` belongs to the GTK app API.

Both transports also expose `state`, `busy`, `metadata`, and `copy` for execution
clients. `state` returns the execution snapshot in `execution`, together with
the daemon's identity, `hostname`, revision, and pane metadata. Layout mutations
over TCP return the same snapshot with `result` containing the operation's
output. `execution` is an `ExecutionSnapshot` with a single `tabs` array.
Each tab contains `id`, `created_at`, `name`, `custom_name`, `layout`, and
`directories`. Checkpoint filenames and ID allocation counters belong only to
the daemon's saved execution file. Stream state packets use the same reply shape.
The Rust control helper unwraps those results for CLI-style requests.
GTK's separate client API uses `session` for its assembled window view.

The `rpc` subcommand in both binaries prints the complete JSON reply, including
the `text` field returned by `read` and `copy`. The ordinary `read` subcommand
prints terminal text directly.

Pane targets must be explicit over TCP. Destination directories must be absolute
or begin with `~/`, which the destination daemon expands. Local CLI commands
resolve relative paths against the calling shell. The daemon CLI never uses a
local shell's `GMUX_PANE` to select a remote pane.

Remote access configuration uses `remote-access-get` and `remote-access-set` on
the local control socket. The `remote_access` object contains `enabled`,
`listen_address`, and `authentication`. These are saved settings that can be
edited while the daemon is running or stopped. `bound_address` reports the
actual listener address, or null when no listener is running;
`remote_access_error` reports a listener startup failure. The configured
`listen_address` accepts an IP address with an optional port, defaulting to
`7331`. An explicit port zero requests an available port on each start. Saved
settings retain zero; `bound_address` reports the assigned port.

Saved outgoing connections belong to the client library and use no daemon API.
Incoming TCP connections address only the serving daemon, including its shutdown
operation. They cannot manage remote access or client state. Client lifecycle
helpers `local-daemon-start` and `local-daemon-stop` start or attach to the local
daemon and wait for its shutdown, respectively. They are handled by the client
library and cannot be sent directly to a daemon. `local-daemon-stop` also succeeds
when no daemon is running, so local execution can be disabled after a failed start.

## Terminal streams

After a successful TCP stream handshake, send:

```json
{"method":"attach","version":40,"daemon":"returned-identity"}
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
Each `key` requires `action`: `"press"`, `"repeat"`, or `"release"`, alongside
optional `name`, `physical`, `unshifted`, `consumed_mods`, `mods`, and `text`.
`physical` is a Linux evdev keycode, independent of the layout. `unshifted` is
the Unicode scalar at the active layout's base level. Zero means unavailable
for either field. `text` is the produced UTF-8 string. For example, Shift+1 on US:

```json
{"method":"key","pane":2,"name":"exclam","physical":2,"unshifted":49,"consumed_mods":1,"mods":1,"action":"repeat","text":"!"}
```

Modifier bits are Shift `1`, Control `2`, Alt `4`, Super `8`, Caps Lock `16`,
and Num Lock `32`. `consumed_mods` uses the same bits for modifiers used to
produce text. Lock bits describe the active state, including on subsequent keys.
Callers without a native event should omit physical/layout information they
cannot supply. A text-only commit uses `name: ""`, zero modifiers and layout
fields, and `action: "press"`. Gmux does not guess a US physical key or lowercase
produced text. See [keyboard input](keyboard.md) for platform limits.

The originating frontend classifies hardware key events independently of other
windows and connections. The daemon encodes each action using the terminal's
negotiated keyboard mode. Kitty repeat and release event types require the
[event reporting flag](https://sw.kovidgoyal.net/kitty/keyboard-protocol/#event-types).
Legacy modes encode repeats as ordinary key presses and omit releases.

`frame-interval` sets this connection's viewport, search, progress, and program status pacing with a
`micros` integer from 1,000 to 1,000,000. It defaults to 16,000. GTK updates it
from the display frame clock; one client's interval does not pace another client.
`scroll`, `scroll-to`, `find`, `find-advance`, `select`, `clear-selection`,
`copy`, and `mark-read` operate on an explicit `pane`. `git-subscribe` chooses
panes whose Git metadata the client needs. The Rust `transport::StreamCommand`
and `StreamRequest` types define the complete command schema;
`transport::command` bounds its size and writes a newline-terminated command.

`select` takes inclusive `[column, physical_row]` endpoints in retained terminal
history. `mode` is `"cell"` (the default), `"word"`, `"line"`, or `"rectangle"`.
For example, this stream command selects columns 4 through 8 on physical rows
10 through 12:

```json
{"method":"select","pane":1,"start":[4,10],"end":[8,12],"mode":"rectangle","serial":7}
```

A viewport header's `selection` is null or an object such as
`{"start":804,"end":968,"shape":"rectangle"}` for an 80-column grid.
Indices are inclusive cell offsets in physical history rows, with rectangle
corners normalized to top-left and bottom-right. `shape` is `"linear"` or
`"rectangle"`. `selection_origin` retains the tracked drag anchor, and
`selection_serial` acknowledges the latest selection or clear command.
Clients should preview drags locally and ignore older acknowledgements.
The toolkit-independent `selection::Selection` supplies viewport clipping and
row geometry without allocating a cell mask.

Each stream owns its selection, including its shape. `copy` on that stream
copies that view's current selection. Rectangles keep physical row breaks,
selected blank cells and trailing spaces; a wide glyph intersecting an edge
is copied whole. The current tracked corners define the rectangle after reflow.
Pruned references invalidate both highlight and copy. A selection is available
only on the screen where it was made. These rules also apply to the control
API's `copy` request, which chooses the focused frontend view, then the newest
frontend as a fallback. `gmux rpc '{"method":"copy","pane":1}'` returns its
text; it does not write the desktop clipboard. Control and stream `copy`
requests accept `formats: "text"` (the default) or `"text-html"`. The latter
returns plain `text` and optional `html` from the same selection. Only requested
representations are generated. `command-output` accepts the same `formats`.
For stream `select-output`, `copy` is null to select only, `"text"` to copy
plain text, or `"text-html"` to copy both representations.

A frontend that handles terminal clipboard writes advertises
`{"method":"clipboard-writes","enabled":true}`. The default is disabled, so
headless observers do not become clipboard recipients. An eligible connection
must also subscribe to the pane. The last focused eligible connection wins,
with connection order as the fallback. Detached writes are dropped immediately.

Replies are binary viewport packets. Each packet begins with a little-endian
32-bit payload length. The payload begins with a little-endian 32-bit JSON
header length, then that many UTF-8 JSON bytes, then cell bytes or a dedicated
clipboard payload. The header
contains pane `meta`, an optional `frame`, changed `rows`, and optional execution
`state`, Git metadata, notifications, search results, selection, clipboard offers, or
errors. A full viewport arrives on subscription; later packets contain changed
rows. Before applying changed rows, shift the previous viewport by `row_shift`
rows, positive toward smaller viewport indices. Newly exposed rows are always
included in `rows`. Apply the same shift to soft-wrap flags. Metadata-only
packets have no cell data.

Clipboard packets are dedicated transient events. Their `copy` field is an
object with `destination` (`"clipboard"` or `"primary"`) and `content`.
A null `content` clears that destination. Otherwise `content` is a nonempty list
of descriptors, for example `[{"mime":"image/png","len":70},
{"mime":"text/plain","len":5}]`. The bytes for each representation follow the
JSON header in descriptor order, without separators or padding. The record
contains exactly the sum of these lengths and no cell data. A descriptor with
`len: 0` offers an empty value; an empty descriptor list is invalid. Apply each
event in wire order across all panes, without applying its default viewport or
selection metadata. Both explicit `copy` replies and unsolicited writes use
these packets. `SessionManager::poll_with_clipboard` delivers them in order.
They are neither coalesced into pane updates nor saved or replayed on reconnect.
Supported exact MIME names are `image/png`, `text/html`, `text/plain`, and
`text/plain;charset=utf-8`. Plain text must be NUL-free UTF-8; other formats
preserve arbitrary bytes. Both plain aliases must agree when supplied together.
Names are unique, and a packet carries at most four representations. Plain
text is limited to 4 MiB per representation and HTML/PNG to 8 MiB each, with
12 MiB total data and 16 KiB of clipboard metadata within the 64 MiB stream packet limit.
Application admission has a stricter 4 MiB total limit, including aliases.
Explicit user copy still omits HTML if generation or normalization fails or its
control-JSON budget would be exceeded. Clipboard stream data is always binary.
See [clipboard behavior](clipboard.md) for formatter limitations.
A stalled frontend's output is bounded by the 64 MiB transport queue and is
detached on overflow. The client reader separately bounds retained storage to
64 MiB and 64 packets, including representation capacities.

Clipboard reads use a separate capability,
`{"method":"clipboard-reads","enabled":true}`. A desktop frontend advertises it
only if it can mediate consent and serve asynchronous reads. After attachment it
reports each destination's monotonically increasing clipboard `epoch` using
`clipboard-changed` with `destination` and `epoch`. Equal epochs are idempotent;
older epochs are invalid. Capabilities and epochs belong to this stream
connection and must be advertised again after reconnect.

Read events are dedicated metadata-only packets with `meta.id` identifying the
pane and `clipboard_read` containing one of these objects:

- `{"kind":"request","request":{"id":42,"destination":"clipboard","mimes":["text/plain"],"list":false,"name":"","granted":false,"epoch":null}}`.
- `{"kind":"cancel","id":42}`.
- `{"kind":"paste_text","serial":1,"destination":"primary","epoch":3}`.

Request IDs identify a transient native request, independently of the program's
Kitty `id`. A true `granted` is a hint, not desktop authority. The frontend must
independently match it to a recent local user-paste gesture for the same pane,
connection, destination, and clipboard epoch, and consume that authorization
once. It must also validate `paste_text` against its own gesture serial. A server
flag or program-supplied name cannot authorize reading the desktop. Targets-only requests do not read
content and do not consume the one-use paste authorization. All other data reads
require explicit consent. `SessionManager::poll_with_clipboard_events` delivers
reads, cancellation, and writes in their shared wire order.

The frontend answers with `clipboard-read-result`, containing a `result` object
with `id`, `status`, `epoch`, `available` MIME names, and `contents` descriptors
such as `[{"mime":"text/plain","len":5}]`. Status is `success`, `denied`,
`unsupported`, `busy`, or `io_error`. Failures have no contents or targets.
Success can contain no matching representations. Metadata is limited to 16 KiB,
requested contents to four representations and 4 MiB total, and available targets
to 16 names and 4096 joined bytes. Names are at most 256 bytes. Read MIME data is
independent of the write-format allowlist.

For each payload chunk, send a newline-terminated JSON command
`{"method":"clipboard-read-data","pane":7,"id":42,"size":5}` followed
immediately by exactly `size` raw bytes. Chunks contain at most 16 KiB. The next
JSON command starts after those raw bytes, regardless of embedded NUL or newline.
Bytes follow descriptor order and must total exactly the declared lengths.
Finish with `clipboard-read-end` and the same pane and ID. This is the only raw
binary extension to the frontend command stream; attach, authentication,
heartbeats, and other commands retain their existing framing. Partial binary
chunks and result assembly have five-second deadlines. Invalid transfers can
detach the frontend. Late or foreign request IDs never complete another read.

A genuine desktop paste uses `user-paste` with a new connection-wide `serial`,
`destination`, `epoch`, and bounded `mimes`. It supersedes unanswered reads routed
to this frontend, including a read in another pane. If native mode 5522 is active, the
daemon emits a native paste event and records a short-lived authorization for
one data read. Otherwise it returns `paste_text`. The frontend asynchronously
reads bounded text and returns `user-paste-text` with `serial`, `epoch`, and
`text`. The Rust client streams larger text using `user-paste-begin` with `size`,
then ordinary `paste-data` chunks and `paste-end`. Scripted `paste` always inserts
text and cannot create clipboard grants. A client claiming desktop capability
is a trusted clipboard broker within the existing socket/authentication boundary;
a JSON field cannot prove a physical gesture.

`generation` identifies cached history for this pane and view. Clears, reflow,
screen switches, and resolved palette or appearance changes advance it. Discard
old cached rows when a nonzero generation changes, including on metadata-only
packets. Zero means the packet does not report cache state.

Within a generation, `cache_origin` counts rows pruned from the front. Identify
cached rows by `cache_origin + row` so surviving rows retain their identity.
Discard identities below the new origin and adjust pending local scroll targets
by its advance. `cache_valid_before`, when present, invalidates previously cached
rows at or beyond that screen row. Apply it before inserting the packet's viewport
and supplemental rows. Ordinary output invalidates the previously mutable screen
rows while preserving older history, even if the displayed viewport is unchanged.

An optional `cache` object supplies a starting history row and `row_flags`, one
flag per extra row. These cells follow the changed viewport cells in the packet. Cache
rows preserve the viewport's resolved colors, decorations, graphemes, and wide
cells. Each packet admits at most 512 supplemental rows and reserves at most
16 MiB for their encoded cell data, excluding JSON metadata. Each row reserves
`cols * (13 + 260)` bytes, including the maximum text length in every cell, even
when the actual text is shorter. Thus a 1,000-column packet admits at most 61
supplemental rows. Readers check this limit before allocating or decoding cells.
These wire limits are independent of client retention and Rust cell layout.

The daemon chooses up to five screens and 49,152 cells within the wire limit.
It skips prefetch when that window cannot cover a viewport, and sends only
missing parts as the window moves. Immutable history can be prefetched during
output.
Live screen prefetch waits for 100 ms without output. Synchronized-output holds
suspend prefetching.

A frame can carry a replacement `hyperlinks` object with `targets`, an array of
OSC 8 URI strings, and `spans`, sorted disjoint `[start, end, target_index]`
ranges. Start is inclusive and end is exclusive, in row-major displayed cell
indices. Wide continuations are included. Adjacent ranges with one target may
cross a wrapped row. The target comes from Ghostty's grid-reference API, not
from the printed text. For example, `{"targets":["https://example.org/"],"spans":[[0,5,0]]}`
links the first five displayed cells.

Apply the replacement atomically with the packet's frame and row deltas.
An absent map keeps the displayed map; an empty map clears it. Reset maps on a
new connection or grid size unless the packet supplies a replacement. A map
requires a frame, but a target-only change can arrive with no changed rows.
Targets are captured with each client's viewport, including synchronized-output
holds. Hover and activation can resolve locally through
`ScreenSnapshot::hyperlinks.target_at(cell)`, without socket requests. Consumers
should retain the map and geometry they actually display before accepting link
actions, and refresh stationary hover when that displayed snapshot changes.
Locally predicted scroll snapshots expose an empty map until an authoritative
viewport is displayed. Their opaque hyperlink revision differs from the
authoritative snapshot, including when a late reply leaves prediction active.
Supplemental cached rows carry no targets. Hyperlink spans always index the
frame's viewport, not appended cached rows; apply a replacement after the
packet's row shift and row deltas without shifting the map separately.

Maps allow at most 16,384 spans and targets, 16 KiB per URI and 256 KiB of total
URI bytes. Invalid UTF-8, control characters, lookup failures or exceeded native
capture limits produce an empty map for the entire viewport. The decoder rejects
malformed map indices, overlapping or out-of-grid ranges and oversized maps.
Reusable Rust maps bound retained URI capacity to 256 KiB; target and span
vectors have the same count limits. Empty transitions release map allocations.

`scroll-to` takes an absolute history `row` and an increasing `serial`. Viewport
replies acknowledge the last applied serial as `scroll_serial`. The Rust client
draws cached history immediately. With a page of cached rows available on
either side, it sends the final target after 100 ms without input. A refill already
in flight also coalesces further cached targets. Returning to the live bottom
skips this debounce so the daemon's current cursor state can arrive promptly.
Near cache edges it requests a refill; cache misses pipeline at the connection's
frame interval while replies are outstanding, without waiting for a network round
trip. Commands that depend on the viewport flush the queued target first.
Apply row deltas from older replies too, since later packets depend on them.
Keep the latest local scroll target while reconciling older viewport replies.

The Rust client history cache retains at most 512 rows and 2 MiB per visible
pane, usually five screens. Row storage and overflow text count toward that budget. Clients
may evict or ignore supplemental rows without affecting authoritative viewport
deltas. Hidden panes release their cache. Unknown rows and search
highlights require an authoritative viewport reply.

Send the connection command `{"method":"ping"}` to check that the daemon's
event loop is responding. It replies with a zero-length record, four zero bytes,
without changing pane state or an in-progress paste. The Rust session client sends
a ping every ten seconds and closes streams that deliver no replies or output for
thirty seconds. Heartbeat replies do not wake the UI. `Packet::read_message`
returns None for these records; `Packet::read` skips them.

Pane metadata includes required `progress` with named `state` and a validated
integer `percentage` from 0 through 100. States are `inactive`, `determinate`,
`error`, `indeterminate`, and `paused`; inactive requires zero. The stream
sends progress changes without a frame or rows. State snapshots and `metadata`
include the retained latest progress, including for hidden panes. `list` also
includes each pane's progress and each tab's selected source pane and progress.
See [terminal progress](progress.md) for transitions, expiry, and selection.

Pane metadata also includes nullable `program_status`, containing one representative
`record` and its pane-wide `count`. Tab lists add the selected source `pane`.
An explicit `metadata` reply adds `program_status_records`, an ID-sorted array of
all retained records with their original app values. Ordinary metadata carries
only the representative, with app inheritance resolved. Status-only changes use
metadata packets without cell rows and share connection frame pacing.
See [program status](program-status.md) for selection, limits and acknowledgement.

Pane metadata uses `unread_sequence` for the newest unread alert's sequence
number, or zero after acknowledgement. Send that sequence as `through` in
`mark-read` to acknowledge alerts without clearing newer ones. Listings use
the boolean `unread` field to indicate whether alerts remain.

For each row listed in the header, there are `frame.cols` cells in column order.
Each cell starts with a 13-byte prefix: a little-endian `u16` UTF-8 byte length,
three RGB foreground bytes, three RGB background bytes, one flags byte, one
decoration byte, and three RGB underline-color bytes. The complete UTF-8 text
immediately follows, without zero padding or a terminator. Length zero represents
an empty cell. Lengths above 260, malformed UTF-8, embedded NUL, truncated cells, and trailing
cell data are rejected. This is stream protocol version 40; clients must match
the daemon's version.

Flag bits 0-5 mean bold, italic, faint, blink, strike, and invisible.
Bits 6 and 7 mark wide-character spacer tails and heads. These native layout
flags determine occupied columns, independently of the text's byte or codepoint
count. Decoration bits 0-2 encode no underline, single, double, curly,
dotted, or dashed as 0-5. Bit 3 enables overline. Bit 4 selects the independent
underline color; otherwise the underline follows the displayed text color.

Frames include `cursor_blink`, the effective Ghostty blink state, and `mouse_shape`,
the pinned Ghostty W3C pointer-shape enum. Shape 8 is text and shape 0 is the
ordinary pointer. OSC 22 with an empty name restores text; unknown names leave
the previous shape intact. Pointer changes remain live during synchronized-output
holds. The `style` stream command takes `style` and `blink`
to configure per-view cursor defaults. Explicit program changes take precedence
until a program reset selects these defaults again.
The Rust `transport::Header` and `Packet::read` expose the complete header and
validate packet dimensions and row indices before decoding cells.

Handshakes are limited to 4 KiB; control and stream commands to 1 MiB; replies
and viewport packets to 64 MiB. Pastes use UTF-8 chunks with a 4 MiB total limit.
See [remote daemons](remote-daemons.md) for authentication, connection budgets,
and saved destinations, and [background sessions](terminal-sessions.md) for
persistence and scheduling.
