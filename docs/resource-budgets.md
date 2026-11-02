# Resource budgets

## Client storage

Every `gmux-client` process shares the following allowances across its daemon
connections and GTK windows. These limits are internal defaults.

| Storage | Process allowance | Local ceiling |
| --- | --- | --- |
| Incoming packets, including decode reservations | 128 MiB | 64 MiB and 64 packets queued per stream |
| Scroll prediction rows and displayed prediction copies | 32 MiB | 2 MiB and 512 cached rows per pane view |
| Outgoing bulk input and clipboard replies | 32 MiB | 8 MiB of queued/in-flight payload per connection |
| Small outgoing commands, including keys and clipboard read chunks | 4 MiB | 64 KiB of additional payload headroom per connection |

Incoming packets preserve stream order, including clipboard, state, and viewport
deltas. A full allowance suspends the reader asynchronously. FIFO admission gives
waiting connections a turn as consumers release storage. Readers and writers run
concurrently, so incoming backpressure does not stop outgoing input or heartbeats.
The last eight of a stream's 128 outgoing queue slots remain available to small
commands when bulk work fills the queue. Outgoing admission failures are reported
to the caller; accepted commands retain their order and reservations through the
socket write. Input cannot bypass an earlier paste on the same stream.

A reader consumes the fixed four-byte record prefix before requesting a 64 MiB
decode reservation. Heartbeat acknowledgements need no reservation.
At most two readers can hold these reservations at once. A stream can have one
packet decoding or awaiting local admission in addition to its queued packets;
both consume the process allowance. After decoding, the reservation shrinks to
the packet's retained storage and transfers to the queue
without acquiring shared storage a second time. The UI keeps that reservation
until it finishes applying the packet. No shared allowance is acquired while
holding stream-local byte permits. Consumers release permits without acquiring
another allowance, so local admission cannot form a circular wait. Closing a
receiver wakes admission waiters; task cancellation releases decode, queued-write,
and in-flight-write reservations.

The prediction pool evicts whole row caches from unfocused windows first, then
least recently used caches at the same priority. Hidden tabs release their rows
and displayed predictions immediately. A displayed prediction remains reserved
until reconciliation, hiding, or destruction, even if its source rows are evicted.
If optional storage is unavailable, scrolling requests authoritative rows and
keeps any already displayed prediction until it can reconcile. Clearing or closing
a manager releases optional storage even when a terminal widget retains a handle.

Accounting includes vector and string capacity, row metadata, and overflow
text. Shared overflow text is conservatively charged for each retained cell
reference. Cache updates can temporarily allocate one extra local cache before
synchronous eviction completes.

These allowances are not an RSS cap. They exclude allocator bookkeeping and
retained allocator arenas, queue bookkeeping, each socket reader's 8 KiB buffer,
JSON decode scratch storage, canonical per-view viewport/state snapshots, UI-owned
clipboard contents, and kernel socket buffers. A wire packet can expand during
JSON decoding before its retained-size rejection;
only two readers can do this concurrently. An admitted decoder has a 30 second
read deadline. FIFO admission of full decode reservations can delay another
connection behind a partial packet or paused consumer. Large synchronous JSON
parses still run on the existing I/O thread and can delay executor scheduling.
Separate outgoing budgets keep input and control work independent of that
admission wait.

GTK rendering scenes, glyph caches, terminal history, and daemon allocations also
sit outside these client allowances. Each daemon applies its own limits below.

## Daemon frontend storage

Each daemon shares these internal allowances across local Unix streams and
remote TCP streams forwarded through its Unix listener:

| Resource | Daemon allowance | Per-stream ceiling |
| --- | --- | --- |
| Retained encoded output buffer capacity | 128 MiB | 64 MiB |
| Optional sent-screen snapshots for delta generation | 32 MiB | Shared allowance |
| Admitted frontend streams, including unattached peers | 64 | One slot |

Output accounting charges allocated capacity, including consumed prefixes and
spare space. A drained buffer retains at most 256 KiB for reuse. Larger drained
buffers release their allocation. On aggregate exhaustion, the daemon first
releases idle output allocations, then disconnects the stream with the largest
unsent backlog. Capacity breaks ties. Local and remote streams use the same
policy, regardless of focus. A rejected enqueue closes its stream and releases
its output allocation, including capacity grown by a failed encoder. The daemon
never skips an accepted packet to admit a later reply. Partial writes, viewport
deltas, clipboard offers, clipboard clears, and heartbeat replies keep their
stream order until disconnection. Clipboard reads from disconnected recipients
are cancelled by the daemon's existing clipboard maintenance.

The optional allowance includes sent-screen cell and row-flag vector capacity,
cell overflow text, and hyperlink storage. Shared cell overflow text is charged
for every retained reference. The daemon evicts whole snapshots from streams with
observed socket backpressure first, then unfocused streams, then the least recently used view.
Eviction frees allocations rather than clearing vectors. It preserves the engine
view's scroll position, selection, search, appearance, and synchronized-output
hold. On the next requested or terminal-driven update, the daemon sends all rows,
advances the cache generation, invalidates previous prefetched rows, and sends the
current hyperlink map, including an empty map. Eviction alone schedules no frame,
so idle views do not repeatedly regenerate each other's caches. A viewport that
cannot fit in the allowance uses full snapshots on subsequent updates.

Excess stream connections close immediately, before frontend buffers or views are
created. The listener still accepts at most eight connections per event-loop turn.
The existing TCP listener separately allows 32 active connections, including
handshakes and control channels. TCP terminal streams also consume a daemon
frontend slot. The platform control server remains separate, with at most 64
active handlers, a 32-request channel, and its existing request sizes and timeouts.
Frontend pressure does not wait for storage, block PTY reads, or suspend terminal
execution. Closing streams returns admission slots through normal teardown.

`gmux status` reports `frontend_resources`, including current stream count,
retained output capacity, optional screen-cache bytes, and their limits.

These allowances are not an RSS cap. Output encoding can temporarily grow one
buffer before synchronous capacity enforcement, and snapshot extraction can
allocate one viewport before eviction. Packet construction, shared execution
broadcast encoding, allocator overhead and arenas, kernel socket and TCP bridge
buffers, frontend input and paste buffers, map entries, metadata, and semantic
engine-view state are outside the storage allowances. Engine views retain search
state and native snapshots required for synchronized output. Terminal engines,
history, persistence, and control requests have separate lifetimes and limits.
The number of panes and views per stream remains unrestricted. Cache pressure
can increase full-frame bandwidth and encoding work, and reconnecting clients
must reattach after slow-consumer disconnection.

## Measuring pressure

Run the isolated client storage probe in release mode:

```sh
cargo test -p gmux-client --release resource_pressure -- --ignored --nocapture
```

It fills 64 row caches and eight ordered queues, pauses consumers, measures
retained bytes and Linux RSS, checks executor responsiveness, copies cached
viewports, then drains every event in order. Producer-owned arguments awaiting
admission in this synthetic probe are outside its queue-byte measurement.

For the native workload, run:

```sh
python3 scripts/resource-budget-benchmark.py --output artifacts/resource-budget-native.json
```

This uses the isolated Wayland fixture and measures four windows, three daemons,
18 panes, 72 pane views, and three raw subscribers that stop reading. It reports
visible/hidden row-cache bytes, client and daemon RSS, control round trips,
input-to-view latency, and scroll acknowledgements. The latency measurements
include RPC overhead and 25 ms observation polling. They do not measure time to
screen scanout. The native workload covers ordinary view lifecycles; use the
storage probe to saturate the aggregate allowances.

Measure daemon pressure without GTK:

```sh
python3 scripts/cargo_build.py --release --headless gmux
python3 scripts/daemon-resource-benchmark.py --binary /path/reported/by/cargo/gmux \
  --verify --output artifacts/daemon-resource.json
```

The benchmark runs twelve slow or nonreading 160×50 subscribers alongside local
and TCP interactive readers, then twenty-four 512×128 views to exhaust optional
storage. It reports retained capacities, Linux RSS and high-water memory,
control round trips, and input-to-view latency. It also checks full viewport
recovery, selection copying, Unicode text, hyperlink clearing, and teardown.
Latency includes socket and Python observation overhead, not screen scanout.
The same workload can measure a pre-budget binary with `--verify` omitted if
that binary exposes the `frontend_resources` counters. The headless
`daemon-resource` smoke suite also checks admission saturation and recovery.
