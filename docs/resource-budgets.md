# Client resource budgets

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
sit outside these client allowances. Each daemon frontend still has its own
64 MiB pending-output limit and per-visible-view snapshots for delta generation.
Slow frontend output can disconnect that frontend without blocking terminal
execution. A client budget does not bound total daemon memory or the number of
panes and snapshots a user opens.

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
