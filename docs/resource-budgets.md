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
deltas. A full allowance suspends the reader asynchronously. FIFO admission
within each size class gives waiting connections a turn as consumers release
storage. Readers and writers run concurrently, so incoming backpressure does not stop outgoing input or heartbeats.
The last eight of a stream's 128 outgoing queue slots remain available to small
commands when bulk work fills the queue. Outgoing admission failures are reported
to the caller; accepted commands retain their order and reservations through the
socket write. Input cannot bypass an earlier paste on the same stream.

Protocol 43 records start with five little-endian u32 values: the remaining
wire length, JSON header length, record kind, JSON decode allowance, and cell
count. Kinds distinguish pane updates, execution snapshots, Git metadata, clipboard
writes/clears, and clipboard reads.
A four-byte zero remains a heartbeat acknowledgement and needs no reservation.
Readers validate the fixed framing before allocating the header. JSON headers
are limited to 16 MiB, clipboard headers to 16 KiB, and complete records to
64 MiB on the wire and 64 MiB of decode credit.

Admission charges each record for its validated JSON allowance, cell vector,
overflow text and Arc counters, binary clipboard payload, and queue transfer
storage. Encoded length alone does not bound decoded storage. Before typed JSON
parsing, an allocation-free scan checks a conservative bound for the current
stream payload schemas. It charges encoded strings, escape scratch, typed
objects, vector growth, boxed trees, and temporary hash maps. The record kind
selects a concrete payload before JSON decoding, without an untyped enum buffer.
Clipboard-read envelopes also decode directly into typed fields. Unknown
cross-kind fields are rejected. The clipboard deserializer checks descriptor
lengths against the framed payload before allocating binary buffers, without a
separate JSON traversal.
Cell counts must match validated viewport rows before cell allocation. Changes
to payload types must preserve or revise the allocation-bound audit in
`crates/protocol/src/transport/record.rs`.

The process allowance has two independent FIFO pools. Records needing at most
64 KiB use an 8 MiB pool; larger records use a 120 MiB pool. Bulk readers waiting
for credit, slow bulk payloads, and paused bulk consumers cannot block small
record admission. Exhausting the small pool still backpressures small readers.
The reservation keeps its original pool when it shrinks, so transferring a bulk
record to a queue never consumes small-record headroom.

A stream has at most one record decoding or awaiting local admission beyond its
queued packets. Headers larger than 8 KiB and clipboard records needing more
than 64 KiB of decode credit parse on the existing application worker pool, with
at most four workers across the process. Large packet storage accounting and clipboard content validation also use that pool. Waiting workers
retain decode credit. Ordinary small headers parse directly. Cell decoding reads
incrementally from the socket buffer, yielding at most every 8 KiB, without
buffering or copying the complete binary record. Each stream awaits its own
decode and enqueue before starting another record.

After decoding, the reservation shrinks to retained capacity and transfers to
the queue without acquiring shared storage again. The UI keeps it until it
finishes applying the packet. No shared allowance is acquired while holding
stream-local byte permits. Consumers release permits without acquiring another
allowance, so local admission cannot form a circular wait. Receiver closure wakes
admission waiters. Cancellation and deadlines release queued and in-flight
reservations; a running blocking worker keeps its reservation until its work and
result are destroyed. Blocking JSON parsing cannot be interrupted once started.

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
canonical per-view viewport/state snapshots, UI-owned clipboard contents, and kernel socket buffers. Decode credit covers JSON input
and scratch while a record is processing. An admitted record has a 30 second
decode/read deadline. Waiting for admission or queue space suspends the reader;
writers and heartbeats continue independently. Deadlines can cancel a worker's
caller but do not interrupt a running parse. The bound limits that parse's input,
nesting, and storage. Admission limits are conservative and can reject records
whose eventual retained storage would fit. Small-pool saturation and per-stream
ordering can still delay updates. These limits do not guarantee latency against
unlimited connections or CPU saturation.

GTK rendering scenes, glyph caches, terminal history, and daemon allocations also
sit outside these client allowances. Each daemon applies its own limits below.

## Event-loop work

Cooperative time and item limits bound work shared by ready panes and streams.
The [scheduling rules](terminal-sessions.md#ownership-and-scheduling) describe
per-stage daemon budgets, the shared GTK dispatcher, frame continuations, and
FIFO delivery. These limits do not preempt individual native operations or cap
readiness scans and dirty-state fan-out. Ready queues retain IDs rather than
copying terminal data. Each active frontend pass retains one pane-ID queue;
GTK retains at most one queued callback per window.

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
buffers, frontend input and paste buffers, map entries, and metadata are outside
the output and optional-screen allowances. Retained presentation state has the
separate policy below. Terminal engines, history, persistence,
and control requests have separate lifetimes and limits. Metadata-only panes
do not consume a view slot. Pane counts remain unrestricted. Cache pressure
can increase full-frame bandwidth and encoding work, and reconnecting clients
must reattach after slow-consumer disconnection.

## Retained views and searches

Each daemon shares these internal limits across its frontend streams and windows:

| Resource | Daemon allowance | Per view or query |
| --- | --- | --- |
| Retained presentation views, including hidden views | 256 views | One slot per stream/pane pair |
| Search index, query, and matcher storage | 64 MiB | 8 MiB |
| Native synchronized-output render allocations and hyperlink reservations | 128 MiB | Shared allowance |
| Search query input | Counted above after normalization | 4 KiB of UTF-8 before normalization |

Views acquire a slot before allocating the native view or Rust presentation.
Hiding a view keeps its slot, scroll, selection, and search state. It releases
only disposable sent-screen state and native render snapshots, following the
existing synchronized-output visibility rules. Closing a pane or disconnecting
its stream releases all of its view slots and storage. Admission never waits for
another view to release memory and never prunes terminal history.

Searches retain every match for navigation in both directions. The index uses
4 KiB pages of unsigned start-position deltas and range lengths. Typical short
matches need two or three bytes each, rather than two machine-sized offsets.
The encoding preserves full-width positions, overlapping cell ranges, and
separate matches within one Unicode grapheme. Page metadata supports ordinal
navigation and viewport lookup without decoding earlier pages. A lookup decodes
at most one preceding page plus the visible matches; it does not scan the whole
history.

Accounting includes allocated page capacity, directory capacity, normalized query
capacity, matcher text, and byte-to-cell mappings. Each page obtains credit
before allocation and is never resized. Only the small directory and matcher
vectors need geometric growth; they reserve replacement storage while the old
allocation remains charged. A rescan charges both the previous complete index
and the replacement under construction. It frees the old index and matcher
scratch when it completes. Query changes, cancellation, and failed admission free
index and scratch capacities immediately. Native row extraction scratch retains
one terminal row plus vector spare capacity per scanning view and is freed when
scanning stops. Row width and grapheme length have protocol limits; the view-count
limit bounds its aggregate retained size. Page slack, directory growth, and
simultaneous old/new indexes can still cause conservative admission failures.

If admission fails, the search releases both indexes and reports
`resource_limited: true`, `pending: false`, zero counts, and no highlights.
This is an explicit failure, never a truncated or completed search. GTK displays
"Search memory limit reached" beside the query and disables navigation. Edit the
query or press Enter to retry. Terminal output and routine UI refreshes do not
retry a failed search indefinitely. While an admitted rescan runs, navigation
continues to use the previous complete index. Search still scans at most 64 rows
per paced visit.
Adjacent and overlapping visual highlight ranges merge, with the selected match
painted over their union. Counts and navigation still include every text match,
including separate matches in one grapheme. Highlight vector growth is capped
before allocation at the viewport cell count, so duplicate grapheme matches
cannot multiply highlight storage. The daemon retains only the sent count/status
summary, not another copy of the viewport highlight vector. Completed rescans
send fresh highlights even when visible cells and counts stay unchanged.

Native render states use a pointer-stable shared allocator with admission before
every allocation. Old and replacement allocations remain charged during growth.
Before capturing each view's synchronized-output hyperlink map, the daemon also
reserves its packed-array ceiling, 704 KiB on supported 64-bit targets, against
the native allowance. After each capture it shrinks to actual packed-array
capacity, including spare capacity, so link-free views return the entire
reservation immediately. This avoids reserving every view's worst-case map
throughout the hold. Remaining map credit returns at hold completion, hiding,
or teardown. Actual hyperlink capacity also appears within `view_bytes`, so
these counters overlap.

A required snapshot that cannot allocate marks its view failed. Its next frontend
visit disconnects that stream before publishing a viewport. View-slot exhaustion
also disconnects the requesting stream. Existing views on other streams and PTY
execution continue. Native capture itself remains one nonpreemptible operation,
as in the scheduling policy.

`gmux status` adds these `frontend_resources` counters:

| Counter | Meaning |
| --- | --- |
| `views`, `view_limit` | All retained view slots and the admission limit |
| `view_bytes` | Rust presentation/view objects, search and row scratch capacity, native view objects and actual per-view hyperlink capacity |
| `search_bytes`, `search_limit`, `search_view_limit` | Search storage, aggregate limit, and local limit; search bytes are a subset of `view_bytes` |
| `view_render_bytes` | Actual native per-view render allocations |
| `view_native_reserved`, `view_native_limit` | Render bytes plus hyperlink credit, and their aggregate limit |

The allowances are not an RSS cap. Object storage and row scratch are bounded
by view count and existing row/grapheme limits, rather than reserved against the
search-byte pool. Temporary viewport highlights belong to packet construction
and then the output budget. Normalizing a query uses a bounded temporary string.
Allocator metadata, retained arenas, frontend pane-map capacity, and shared
terminal engines are excluded. A native view can own at most eight tracked grid
references for viewport, selection, output anchor, and history. Their fixed-size
Ghostty reference objects, page-list pins, and shared terminal reference-table
capacity use the terminal allocator and are excluded from `view_bytes`; view
admission bounds their count. Reference-table capacity can outlive a view until
its terminal dies. Shared terminal history, ordinary render state, held Rust
cell scratch, and hyperlink capture scratch are engine-owned, excluded from the
per-view counters, and never charged once per frontend. Row-scratch overflow
text includes its Arc header and is conservatively charged per retained cell
reference. Per-view search indexes are independent and charged once each. These
limits do not change actual scrollback retention.

## Measuring pressure

Run the isolated client storage probe in release mode:

```sh
cargo test -p gmux-client --release resource_pressure -- --ignored --nocapture
```

It fills 64 row caches and eight ordered queues, pauses consumers, measures
retained bytes and Linux RSS, checks executor responsiveness, copies cached
viewports, then drains every event in order. Producer-owned arguments awaiting
admission in this synthetic probe are outside its queue-byte measurement.

Measure packet admission and decode scheduling separately:

```sh
cargo test -p gmux-client --release packet_admission_pressure -- --ignored --nocapture
cargo test -p gmux-client --release clipboard_decode_pressure -- --ignored --nocapture
cargo test -p gmux-protocol --release header_decode_cost -- --ignored --nocapture
```

This uses the real client reader with bounded in-memory sockets. Two partial
records compete with an interactive stream, followed by thirty-two 8 MiB JSON
records alongside independent input and update traffic. It reports reserved
bytes, input/update latency, executor wake latency, CPU time, wall time, and sampled
Linux RSS. The clipboard variant uses thirty-two 4 MiB clipboard records.
`header_decode_cost` separates allocation-bound scanning, typed deserialization,
and complete header decoding for small, plain, escaped, and string-array headers.
CPU in the socket probe includes the interactive probes and socket copies.
Run alone for comparable measurements. It excludes daemon generation,
GTK application time, and display presentation. The normal tests separately
check bulk-pool saturation, worker cancellation, deadlines, and permit release.

For individual stream events, run:

```sh
cargo run --release -p gmux-protocol --example stream-codec
```

This standalone probe uses the counting allocator from `clipboard-benchmark`.
It reports separate encode/blocking-decode elapsed timings, Linux thread CPU time,
allocation/reallocation counts, cumulative allocated bytes, wire size, and
source/decoded retained-size estimates. Cases include metadata, one-row and
full-pane updates, execution snapshots with zero/one/eight tabs, Git metadata,
clipboard reads, clears, and offers up to 4 MiB. Buffers are reused after warmup. Retained estimates
include spare capacity and the full inline `Packet` enum size for every variant;
allocator bookkeeping is excluded. Keeping pane metadata inline avoids an extra
allocation per decoded pane update, but small events also occupy the largest
variant's inline size. These measurements exclude socket I/O, worker scheduling,
daemon generation, GTK application, and rendering. Run matching release builds
with the same dependencies, CPU affinity, and workloads to compare versions.

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

Measure frontend pane ownership separately:

```sh
cargo test -p gmux-daemon --release frontend_pane_workload -- --ignored --nocapture --test-threads=1
GMUX_BENCH_VISIBLE=0 cargo test -p gmux-daemon --release frontend_pane_workload -- --ignored --nocapture --test-threads=1
```

Run alone. This probe uses 32 panes and eight frontends. The default visits eight
panes per frontend, then hides four. The zero-visible variant keeps all panes
metadata-only. It counts retained Rust allocation sizes, including hash-table
buckets and spare capacity, while excluding socket output capacity, native
Ghostty allocations, and allocator overhead. It reports thread CPU time per
update and pane-owner teardown time with PTYs and shared terminal engines kept
alive outside the timed teardown. The default update alternates a row and
generates a viewport delta; the metadata-only update compares pane metadata.
Use the daemon pressure benchmark for native memory and disconnection behavior.

Measure dense searches and ordinary complete navigation separately:

```sh
python3 scripts/view-search-benchmark.py --binary /path/to/release/gmux \
  --output artifacts/view-search-dense.json
python3 scripts/view-search-benchmark.py --binary /path/to/release/gmux \
  --rows 600 --output artifacts/view-search-complete.json
cargo test -p gmux-protocol --release dense_search_storage -- --ignored --nocapture
GMUX_SEARCH_ROWS=600 cargo test -p gmux-protocol --release dense_search_storage -- --ignored --nocapture
```

The daemon probe uses four independent frontend views, dense history, a rescan,
a native synchronized-output hold, hidden views, disconnection, and pane teardown.
It records resource counters, Linux RSS/high-water memory, search wall time,
control latency, input-to-view latency on an unrelated PTY, and navigation
round trips. Use `--navigation-rounds` for more navigation samples. Python socket
decoding and observation are included; GTK and physical scanout are excluded.
Both workloads support complete results with the default four views. Add
`--assert-complete` to verify complete initial scans and rescans, or use
`--case sparse` for one match per generated row. Larger view counts and denser
retained histories can reach the explicit search limit. Existing scrollback
retention determines how much of the generated history survives. Run benchmarks
alone against matching release binaries; use `--protocol-version` when measuring a preserved older binary.
The protocol-only probe isolates allocated search capacities and excludes native
storage, allocator overhead, and process RSS. Use the native multi-window probe
above for GTK rendering and window-lifecycle measurements.
