# Testing

Run commands from the repository root. The [README](../README.md#build-and-run)
lists build prerequisites. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a build of the same source revision with all
three bundled integration patches applied:

- The [media-key patch](../scripts/patches/ghostty-media-keys.patch) supplies Kitty media/audio encoding.
- The [clipboard-read patch](../scripts/patches/ghostty-clipboard-reads.patch) supplies the owned deferred-read APIs.
- The [snapshot-state patch](../scripts/patches/ghostty-snapshot-state.patch) omits unfinished parser input from history checkpoints.

The bootstrap uses the architecture's baseline CPU target so cached libraries
remain portable between machines. Ghostty's SIMD dispatch remains enabled.

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
```

The runner checks formatting, Rust tests, Clippy, builds, linkage, and headless
smoke suites. Add `--headless` to build and test the protocol, platform support,
daemon, client library, and CLI packages, without GTK, libadwaita, GLib, or `glib-compile-resources`.
Add `--native` to include desktop suites in a private Wayland session.
`--headless` and `--native` cannot be combined.

Check execution and the client library without a toolkit:

```sh
cargo test --locked -p gmux-protocol -p gmux-platform -p gmux-daemon -p gmux-client --lib
cargo clippy --locked -p gmux-protocol -p gmux-platform -p gmux-daemon -p gmux-client --all-targets
```

Test the protocol and client without Ghostty or a C compiler:

```sh
cargo test --locked -p gmux-protocol -p gmux-platform -p gmux-client
```

All workspace packages inherit the lint policy in the root `Cargo.toml`.
Compiler warnings fail normal checks, builds, and tests. Clippy warnings fail
when running `cargo clippy`; normal builds do not run Clippy.

## Select checks

```sh
python3 scripts/check.py --list
python3 scripts/check.py --only daemon scrollback
python3 scripts/check.py --headless --only progress
python3 scripts/check.py --native --only progress-ui
python3 scripts/check.py --native --skip-build --only renderer multi-window
```

`--only` selects suites by filename without `-smoke-test.py`; use `smoke` for
`smoke-test.py`. Desktop suites require `--native`.

`--skip-build` skips Rust checks and reuses executable paths recorded by the
previous runner build in `artifacts/cargo-build.jsonl`, or
`artifacts/cargo-build-headless.jsonl` for `--headless`. Use the same `--headless`
setting as that build. Rebuild after source or build-configuration changes.
Running a smoke script directly builds its executables through Cargo unless
given explicit binary paths.

`--skip-smoke` runs Rust, build, and linkage checks without smoke suites.

The `hyperlinks` headless suite checks OSC 8 maps through the daemon, including
target-only packets, independent viewports, holds, reconnect and clear. It also
compiles a native ownership check for hold release, timeout and host clear.
The native `terminal-input` suite checks rectangular drag preview with paused and
delayed replies, Unicode and blank block copy, physical soft wraps, accessible
row ranges, mouse-tracking overrides, cached-history scrolling,
compositor-generated keyboard repeats, Kitty media/volume bytes, unsupported browser keys,
event negotiation, legacy typing, modifiers, composition, shortcuts, and
key tracking across pane and window focus changes. It also installs a private
test URI handler and checks hover, contextual copy and modifier-click against
delayed and held frames. It never launches a browser. The renderer suite checks
shared hover/OSC 22 pointer ownership alongside fractional alignment, retained
rows and cursor scheduling.

Engine tests check exact media/audio codes, modifier parameters, and negotiated
press/repeat/release behavior. The `remote` suite checks media events and silent
browser events through local and remote socket streams. Those checks bypass GTK;
the native `terminal-input` cases exercise compositor input through GDK and GTK
to the terminal program. Unsupported-key checks also observe the frontend's
requests, so a swallowed event cannot pass as a correctly silent key.
Native browser checks use the keysyms present in the fixture's keymap; `Refresh`
and browser `Stop` have engine coverage only. The private compositor delivering
a key does not imply that a desktop with system key grabs will deliver it.

The `text-blink` headless suite calls libghostty's style API directly to check
SGR 5/6 and 25/0. It also checks mixed wide/combining text through Unix and TCP
streams, synchronized-output holds, cached history, scrolling, reconnect and
screen lifetime. Renderer fixtures check hidden/shown pixels and row retention,
colored decorations, hover, selections, search and block/bar/underline cursors.
Native lifecycle fixtures check reduced-animation preferences, focus-independent
text, inactive windows, hidden/unrooted widgets and source removal on teardown.

`terminal-input` also checks focus-entry Num Lock after an external toggle,
active/inactive keypad input, native repeat/release, shifted punctuation and the
ISO extra key. Engine tests separately check international layout data and exact
negotiated encoder output. The Wayland fixture uses its default US keymap;
non-US layout cases currently have direct encoder/model coverage.

The `scrollback-limits` headless suite checks saved policy and restart, complete
request validation before mutation, blocked and failed saves, successive partial
updates while saving, new and existing panes, and immediate local/remote cache updates with queued replies. Engine tests
cover Unicode/style history, linear and rectangular selections, Find restarts,
prompt/output anchors, independent viewports, alternate screens, pruning during
synchronized-output holds, and byte/line zero semantics. Client tests exercise cache predictions and rebasing
through delayed replies and pruning, and verify that wire-valid supplemental rows
can exceed local retention without exceeding its memory budget. Decoded-packet
session tests check that overlapping supplemental rows cannot replace the
authoritative viewport or break subsequent row shifts when retention is disabled.
Protocol tests check row and encoded-byte reservations before reading cells, and
fragmented
blocking/async reads with supplemental long graphemes and following packets.
The maximum-grapheme grid plus the wire-limit history window also passes client
queue admission. Daemon tests cover prefetch windows across supported dimensions
and engine extraction at the wire boundary.

Engine retention tests exercise scroll-clear cursor migration with zero and small
byte/line limits, bold RGB text, OSC 8, and both explicit and shell-prompt clears.
They also check one-row line-limit pruning after reflow. Saved-history tests
check colors and gaps across reflow, current formatter normalization, buffer
reuse, expansion budgets, and rejection of other terminal operations. Destructive-erasure shell
fixtures put non-prompt text on the bottom row so ED 2 discards the active screen;
prompt-preserving clears separately verify that captured output survives. Native
coordinate fixtures erase history after ED 2 so its prompt-preserving behavior
cannot shift their absolute row-zero assertions. The shell suite runs
Bash/Zsh directory-reporting fixtures in new sessions without a controlling TTY.
It captures all output and checks backslashes, raw controls, hostnames, and Unicode
in both C and UTF-8 locales, alongside the existing startup and user-hook tests.

The `compression` headless suite checks attached and detached multi-pane
compression, input during incremental work, transparent history access, and
deadline removal after completion and pane teardown. It observes idle periods
longer than the five-second compression delay without sending daemon requests.
See [performance measurements](performance.md#idle-scrollback-compression) for
release-build comparisons and larger retained-history workloads.

The `clipboard` headless suite checks OSC 52, iTerm2 Copy and atomic Kitty writes through local and remote streams,
recipient focus and visibility, explicit copy ordering, decoded size limits,
detach/reconnect without replay, and bounded output when a frontend stops
reading. Binary cases preserve actual PNG bytes with NUL, HTML/plain alternatives,
empty values, clear, unsupported/oversized whole-request rejection and expanded
alias budgets. It also compiles the native normalized-request policy and reply check.
Rich-copy cases compare independently parsed HTML with plain text over both
transports, including colors, escapes, long graphemes, soft wraps, page boundaries,
rectangles, blank padding, and bounded styled expansion. A daemon queue test
checks rich-offer admission and rejection rollback without losing prior offers
or a following clear event. The `shell` suite also
checks rich command-output requests and the CLI.
The native `terminal-input` suite reads both MIME representations from actual
GTK clipboard offers, checks Unicode OSC 52 writes, and pastes primary selection
in the private Wayland session. `multi-window` verifies each window's selection;
`remote-window` reads a remote rich copy through GTK. These reads outlive the
transient daemon packet and exercise provider ownership.
The `clipboard-ui` suite uses the same native Wayland fixture to read exact PNG,
HTML and plain bytes from local and remote GTK offers. It checks atomic rejection,
no invented text, empty versus clear, and command-output copy. It does not render
application clipboard data.

The `clipboard-read` headless suite exercises real PTYs over local and remote
frontend streams. It checks asynchronous ordering, 4 MiB binary results,
continued service to another pane, one-use paste grants, metadata exemptions,
destination and generation boundaries, detach, and partial-transfer deadlines.
Engine tests also check that partial requests, deferred response bytes, and paste
grants never enter history checkpoints. `scripts/clipboard-read-check.c` compares
native synchronous and stepped encoding at base64 and packet boundaries, checks
deferred request/reply ownership and stale IDs, and counts native allocations
through teardown. It also bounds storage for skewed MIME sizes and repeated
requested entries. The
`clipboard-read-ui` suite uses native Wayland and AT-SPI to approve and deny real
GTK requests. It observes clipboard transfer signals to verify no content is
requested before consent, checks binary HTML without rendering it, and exercises
local and remote genuine-paste authorization. Split-pane cases check that deliberate
ordinary and Kitty pastes supersede unanswered consent in another pane without
reading data for the cancelled request. Its `clipboard-banner-check`
compares banner measurement and placement against the installed `AdwBanner`
with one visible action, then checks the two-action adaptation, both text
directions, larger fonts, keyboard focus, and cleanup after hiding.

The `shell` headless suite requires Bash and Zsh. It checks real login startup,
existing prompt/preexec hooks, scalar and array `PROMPT_COMMAND`, disabled Bash
history, relative resource paths, repeated sourcing, multiline input, builtin busy
state, exit codes, frontend reconnect, fresh-shell restoration, independent prompt
navigation and output copy, partial
markers, alternate screens, reset, and foreground-process fallback. Engine tests
cover reflow, pruning, anchor limits, command identity across active-area reuse,
and metadata reads during idle compression.

The `program-status` headless suite covers OSC 7501 ownership, local/remote
metadata, reconnect, shared acknowledgement, process exit with background PTY
holders while detached, record bounds,
coalescing, hidden-pane extraction and input responsiveness. Engine tests exercise
the pinned native callback, detection reply, every report split including its OSC
introducer, replacement, inheritance, clearing,
eviction, reset/screen lifetime and fresh-shell restoration. The native
`program-status-ui` suite checks sidebar status and metadata attribution, both
protocols feeding one progress bar and icon, combined unread badges, independent
alert acknowledgement, plain accessible labels, unobstructed terminal text,
hidden tabs, reconnect and shared windows. Its widget fixtures check unknown progress values, mapped-only animation,
viewport allocation and stable sidebar title positions at normal/large fonts and
both text directions.

## Native prerequisites

The native suites need PyGObject and these installed components:

- `dbus-run-session`, Mutter with headless Wayland support, PipeWire, and
  WirePlumber 0.5+.
- GStreamer with `pipewiresrc` and `videoconvert`, and introspection data for
  GStreamer, GstVideo, GDK 4, and AT-SPI 2.
- The AT-SPI accessibility bus and registry service files.
- `gtk4-rendernode-tool` for rendering checks.

The fixture starts a 1280 × 960 headless Mutter monitor with XWayland disabled.
D-Bus, settings, sockets, shells, and capture services are private. Input stays
inside this session. It uses Mutter's private RemoteDesktop and ScreenCast
interfaces, so a Mutter update may require fixture changes.

Use `GSK_RENDERER=vulkan`, `gl`, or `cairo` to select a rendering backend.
Set `GMUX_TEST_REFRESH_RATE=144` to exercise the native suites on a 144 Hz
virtual monitor; the default is 60 Hz.

## CI and failures

GitHub Actions uses the Fedora container pinned in `.github/workflows/check.yml`
and the development Rust toolchain from `rust-toolchain.toml`. CI runs the
headless smoke suites once, then checks the full workspace and verifies that its
daemon artifact matches the headless build. It does not run native desktop suites.
`ci/install-deps.sh` and `ci/check.sh` provide the same steps for a container run.

Logs, screenshots, and renderer reports go under the Git-ignored `artifacts/`
directory. The runner stops at the first failure. Its default deadlines are
1200 seconds per Cargo command and 300 seconds per smoke suite; change them with
`--build-timeout` and `--smoke-timeout`.

## Synchronized-output cache benchmark

The native benchmark replays a 160 × 48 colored Unicode terminal with visible
and previously visited hidden views. It checks complete frames during holds and
updated frames after release. Build both revisions with the same compiler flags
and Ghostty library:

```sh
ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
mkdir -p artifacts
cc -O2 -std=c11 -DGMUX_VERSION='"benchmark"' -I"$ghostty_prefix/include" \
  scripts/sync-output-benchmark.c src/ghostty_bridge.c \
  -L"$ghostty_prefix/lib" -Wl,-rpath,"$ghostty_prefix/lib" \
  -Wl,--wrap=ghostty_render_state_new -lghostty-vt -o artifacts/sync-output-benchmark
artifacts/sync-output-benchmark 2 24 2000 0
artifacts/sync-output-benchmark 2 24 2000 1
```

Arguments select visible views, hidden views, iterations, and allocation counting.
Use counting off for process CPU measurements with the production allocator.
Counting on substitutes a render-state allocator and reports allocation calls,
requested bytes, and live bytes after warmup. These counts exclude terminal,
Rust, transport, and GTK allocations. This benchmark measures the native engine;
it does not measure complete daemon or desktop frame latency.
