# Testing

Run commands from the repository root. [Build prerequisites](../README.md#run)
apply to all Rust checks. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a compatible installation.

After changing the Ghostty pin, bootstrap it in that worktree and validate
against the resulting `.deps/ghostty` installation. An installation built from
the previous pin cannot validate the upgrade. Update `SOURCE_SHA256` with the
source archive hash and check `ZIG_VERSION` against upstream's `build.zig.zon`
and `src/build/zig.zig` requirements.

The build also needs libclang, which bindgen uses to read
[`src/ghostty_bridge.h`](../src/ghostty_bridge.h). Set `LIBCLANG_PATH` to its
directory if it is not discoverable. Every Rust build checks the generated FFI
struct sizes, alignments, and field offsets; the C compiler checks exported
function prototypes. See the [boundary documentation](code-organization.md#shared-data-and-protocols)
for the source of truth.

Bindings use Cargo's target, including x86_64 and aarch64 Linux. Cross builds
need a target C toolchain and sysroot, target GTK and Ghostty libraries, and the
Rust target. Bindgen receives the C compiler's flags, including target header
search paths and ABI options. They must also be accepted by libclang. Use
`BINDGEN_EXTRA_CLANG_ARGS_<TARGET>` for additional Clang header search paths
if needed; avoid overriding the shared ABI options there.

## Prerequisites

The headless smoke tests need Python 3.12+, PyGObject, GdkPixbuf 2 introspection
data, and its SVG loader. They use real PTYs and private settings and sockets.
They require no display or running Gmux instance.

`multi-client` checks two connections sharing input, output, layouts, and
workspaces while keeping scrolling, search, selection, and clipboard replies
independent. It also checks PTY sizing and disconnect isolation. `multi-window`
uses the native Wayland fixture to check CLI/action window creation, shared tab
and split changes, independent navigation and zoom, and closing individual
windows without stopping their shells. It also launches the checked-in desktop
file's New Window action through GIO, checking its label and reuse of the same
daemon and terminals. GNOME uses this action for the app icon's context menu and
middle-click.

`preferences` changes settings through native GTK controls in multiple windows.
It checks shared fonts, cursor style, pane title bars, appearance, and open
Preferences dialogs while sidebar visibility, navigation, and zoom stay local.
It holds the settings-file lock to check ordered saves, new windows opened before
a save finishes, and pending writes when the application closes. Many alternating
edits must publish at most two snapshots after unlocking, covering the in-flight
save and the latest pending preferences.

The `main-menu` suite activates New Window through the visible menu item, its
W mnemonic, Ctrl+Shift+N from a focused terminal, and Enter in the command palette.
It checks availability in an empty window and the collapsed sidebar overlay,
preservation of the source window's navigation, and reuse of existing terminals.
New Window must stay out of the New Tab dropdowns. The `commands` suite checks
its search terms, enabled state, accelerator label, and matching help in the
Application shortcut section. Run the menu suite in light, dark, and high-contrast
appearances using the options below.

The native suite and application benchmark also need:

- `dbus-run-session`, Mutter with `--headless`, `--no-x11`, and
  `--virtual-monitor` support, PipeWire, and WirePlumber 0.5+.
- GStreamer with `pipewiresrc` and `videoconvert`, plus introspection data for
  GStreamer 1.0, GstVideo 1.0, GDK 4, GdkPixbuf 2, and AT-SPI 2.
- The AT-SPI accessibility bus and registry D-Bus service files, and GNOME
  desktop settings schemas for the font tests. The fixture activates the registry
  by its bus name. If activation returns `ServiceUnknown`, it reports the failure
  and launches the installed accessibility service file's `Exec` command from
  `XDG_DATA_DIRS`. This supports private `dbus-broker` sessions without a systemd
  user manager, without assuming a distribution's executable location.
- `gtk4-rendernode-tool` for renderer checks, Git for branch checks, and Vim
  and `top` for interactive terminal checks.

The native fixture uses Mutter's private RemoteDesktop and ScreenCast D-Bus
interfaces. The host-native validation used Mutter 51.0, PipeWire 1.6.9,
WirePlumber 0.5.18, and GStreamer 1.28.7. These are tested versions, not minimum
requirements beyond those stated above. This was separate from Fedora headless
CI and does not establish Fedora native coverage.

## Rust requirements

`Cargo.toml` declares Rust 1.95 as the minimum supported compiler.
`src/remote.rs` uses `AtomicUsize::try_update`, which
[became stable in Rust 1.95](https://github.com/rust-lang/rust/blob/1.95.0/library/core/src/sync/atomic.rs).
Rust 1.94 fails to compile that call with `E0658` for `atomic_try_update`.
The locked GTK and GLib crates also declare Rust 1.92 as their minimum.
The locked bindgen 0.73.2 dependency declares Rust 1.71 and does not raise
the minimum. Its generated declarations use the bridge compiler's target C
flags and undergo layout checks with both supported compilers.
Rust 2024 edition support alone does not satisfy these requirements.

[`rust-toolchain.toml`](../rust-toolchain.toml) pins development to Rust 1.99.0
and installs rustfmt and Clippy with rustup. CI reads both TOML files to select
its compilers. Both compilers run the full headless runner with the locked
dependencies. Compiler and dependency updates must keep the minimum run passing
or deliberately raise `rust-version`.

To check the minimum compiler locally after bootstrapping Ghostty:

```sh
rustup toolchain install 1.95.0 --profile minimal --component rustfmt --component clippy
RUSTUP_TOOLCHAIN=1.95.0 python3 scripts/check.py
```

## Linux container and CI

[`Validation`](../.github/workflows/check.yml) uses the official x86_64
Fedora 44 container pinned by digest. Fedora 44 is a
[supported stable release](https://fedoraproject.org/workstation/download/).
Its normal signed `fedora` and `updates` repositories supply
[GTK 4.22.5](https://packages.fedoraproject.org/pkgs/gtk4/gtk4/)
and [libadwaita 1.9.4](https://packages.fedoraproject.org/pkgs/libadwaita/libadwaita-devel/).
Fedora 43's GTK 4.20 does not meet the requirement.

GitHub's `ubuntu-24.04` runner supplies the Docker host. Dependency installation,
Ghostty bootstrap, Rust toolchain installation, builds, and checks all run inside
the Fedora job container. There is no custom image build, image publication, or
self-hosted runner. The container uses its existing root account and home;
rootless Podman maps that account to the local caller without creating users.
Docker bind mounts can leave root-owned cache and artifact files on the host.

[`ci/install-deps.sh`](../ci/install-deps.sh) installs GCC, Clang and its built-in headers, libclang,
pkg-config, GTK and libadwaita development packages, GLib resource tools,
Python 3.12+, PyGObject, GdkPixbuf introspection data and SVG decoding, curl,
Git, and cache archive tools through DNF. It verifies the library minimums,
libclang discovery/loading, Python, and SVG loader availability. Fedora packages
`rustup-init`; [`ci/check.sh`](../ci/check.sh) uses it to install Cargo's proxies
and then the selected compiler, rustfmt, and Clippy.

Run the same environment with rootless Podman from the repository root:

```sh
# Copy this digest from .github/workflows/check.yml when updating the image.
gmux_ci_image=registry.fedoraproject.org/fedora:44@sha256:cd3513b19e87220eb6fba1aeb041cf88f9c00c3b1de213c36db7532c383f4a1c
podman run --rm --init --volume "$PWD:$PWD:Z" --workdir "$PWD" \
  --env CARGO_TARGET_DIR="$PWD/.ci-cache/target" \
  "$gmux_ci_image" bash -c 'bash ci/install-deps.sh && bash ci/check.sh'
# Test the minimum compiler in the same image:
podman run --rm --init --volume "$PWD:$PWD:Z" --workdir "$PWD" \
  --env CARGO_TARGET_DIR="$PWD/.ci-cache/target-minimum" \
  --env RUSTUP_TOOLCHAIN=1.95.0 \
  "$gmux_ci_image" bash -c 'bash ci/install-deps.sh && bash ci/check.sh'
```

Docker accepts the same commands without the SELinux `:Z` volume suffix.
The checkout stores ignored caches in `.ci-cache/` and `.deps/`. Start with fresh
caches when comparing environments. CI keys caches by architecture, compiler,
workflow/install/bootstrap inputs, and the installed RPM manifest; Cargo's
lockfile also keys Rust build outputs. Package updates therefore cannot restore
outputs built against an older system environment. The two compiler jobs use
independent Rust caches.

The container always bootstraps this checkout's pinned Ghostty revision. It sets
`GHOSTTY_PREFIX` and `LD_LIBRARY_PATH` to that installation, overriding inherited
values, and checks both binaries' resolved Ghostty linkage. Bootstrap verifies
the source archive's checked-in SHA-256 on every run, including cache hits.
It verifies a newly downloaded Zig archive against the pinned release's upstream
SHA-256, and Zig verifies its dependency package hashes.

The image digest, Rust versions, Cargo lockfile, Ghostty revision and source
checksum, Zig version, and action commit IDs are fixed inputs. DNF deliberately
uses Fedora's supported stable package updates, with signature verification;
this is not a frozen RPM archive or a claim of bit-for-bit build reproducibility.
Each run preserves the full installed RPM manifest and enabled repositories.
Review image, compiler, action, and source-pin updates in PRs and rerun both
compiler jobs. Move to a supported Fedora release before this one reaches end of
life. A package update that breaks a check must be investigated, not hidden by
loosening assertions or skipping a suite.

The independent Rust 1.95.0 and 1.99.0 jobs run formatting, locked Rust tests,
Clippy with warnings denied, binary and example builds, and every headless suite
selected by `scripts/check.py`. New headless suites are included automatically.
The jobs run on pushes to main, pull requests, and manual dispatches.

CI does not run `--native` or validate frontend drawing, input, accessibility,
notifications, GPU drivers, or compositor integration. It does not cover aarch64
or cross compilation. Run native checks separately with the prerequisites above.
The headless job does not install Mutter, PipeWire, or the remaining native
fixture services. Native font tests compare metric changes, not one machine's
fixed pixel values. Renderer cache comparisons retain the exact Cairo oracle
and the optional selected-GPU diagnostic described in [renderer validation](renderer.md#validation).

The container writes installation output to `artifacts/packages.log`, its RPM
manifest to `artifacts/packages.txt`, and bootstrap, runner, compiler and linkage
output to `artifacts/ci.log`. GitHub uploads `artifacts/` after success or failure
and retains reports for 14 days. Failures before checkout remain in Actions step
logs. Validate workflow edits with:

```sh
actionlint .github/workflows/check.yml
bash -n ci/install-deps.sh ci/check.sh
```

## Run checks

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native
```

The runner checks formatting, runs Rust unit, integration, and documentation
tests, runs Clippy with warnings denied, and builds both binaries and the
examples. It then runs the bootstrap, Cargo output, daemon, scrollback, control, and icon smoke tests.
`--native` includes the desktop suite in an isolated headless Wayland compositor
with XWayland disabled.

The engine tests cover wide-character clipping without reflow on active and
inactive alternate screens and with wraparound disabled, editing the clipped
column, same-grid resize reports and synchronized output, and palette reset to
the latest configured theme. After a Ghostty upgrade, run
`python3 scripts/check.py --native --skip-build --only resize font renderer terminal-renderer`
after the headless checks to cover sizing and rendering through the frontend.

The renderer examples include source modules whose unit tests already run in
the application target. The runner skips repeating those unit tests for the
examples; the `renderer` smoke suite runs the tools to check drawing and cache
behavior.

The fixture creates a 1280 × 960 virtual monitor and private D-Bus, PipeWire,
settings, runtime sockets, home, and XDG config/cache/state/data directories.
It clears inherited display sockets, accessibility and session bus addresses,
user fontconfig overrides, and GTK theme overrides. System fonts and schemas
come from the selected distribution. `C.UTF-8` fixes translated toolkit labels
and UTF-8 text behavior. Monitor scale starts at 100%; scale cases explicitly
change the private monitor. `GSK_RENDERER`, appearance, and high-contrast
selections remain deliberate inputs. Keyboard, pointer, and clipboard input
stay in that compositor. The fixture stops its processes when the run ends.

## Select smoke tests

```sh
python3 scripts/check.py --list
python3 scripts/check.py --native --only find terminal-input
python3 scripts/check.py --native --skip-build --only renderer terminal-renderer
python3 scripts/check.py --skip-build --only control icon
```

`--only` selects smoke tests by filename without the `-smoke-test.py` suffix.
Use `smoke` for `smoke-test.py`. Rust checks still run unless `--skip-build` is
set; that option skips all formatting, Rust tests, Clippy, and build steps.
Use it after a successful build while checking an unchanged binary.
Fresh runner builds select executable paths from Cargo's `compiler-artifact`
JSON messages and save that output to `artifacts/cargo-build.jsonl`. The runner
passes those paths to smoke tests for this invocation. It clears any inherited
artifact selection before setup, so an earlier build cannot supply it.
Cargo writes directly to that artifact during the build, preserving partial
diagnostics if the command fails or reaches its deadline.

Direct smoke/benchmark defaults and `--skip-build` do not rebuild the application.
They resolve Cargo's output directory with locked `cargo metadata` and read the
current environment and Cargo config hierarchy to select either the ordinary
output or a single explicit native host target's subdirectory. This includes
`CARGO_TARGET_DIR`, config `build.target-dir`, `CARGO_BUILD_TARGET`, and config
`build.target`, with parent/home configs and legacy `config` precedence.
They do not fall back to `target/debug` if the selected binary is missing.

Defaults execute native host binaries only. Multiple-target arrays, custom and
cross targets, and config includes are rejected with an actionable error.
For a separate manual build using command-line-only `--target` or `--config`,
pass its actual executable paths through `--binary` and `--check-binary` where
available. Those explicit overrides remain unchanged and bypass default path
selection; they do not provide cross-execution or emulator support.
The `cargo-output` smoke suite builds isolated tiny Cargo packages and verifies
application, daemon and example selection with stale default binaries present.
It tests environment/config host targets, ordinary and redirected outputs,
config precedence, release output, and rejection of unsupported targets.
It also runs `run.sh` and `install.sh` against those fixtures, checking argument
forwarding, release installation, and rejection of failed builds or missing and
ambiguous artifacts. The scripts share the runner's artifact parser and select
both binaries from one successful Cargo build in the requested profile.
`--skip-build` still skips the repository's Rust/build checks; this explicitly
selected smoke suite compiles only its temporary fixtures.
Include `--native` for desktop tests so the runner provides their isolated
display and services.

`--list` shows every suite and whether it needs the desktop fixture, without
building or starting services. Unknown names and desktop selections without
`--native` fail before setup. Repeated names run once.

| Smoke tests | What they cover |
| --- | --- |
| `cargo-output` | Real Cargo executable artifacts and native host target/config selection, redirected output, launch argument forwarding, release installation, stale-binary rejection, build failures, and missing or ambiguous artifacts. |
| `bootstrap` | Interrupted downloads and extraction, recovery of older incomplete caches, cache reuse, checksum verification, and temporary-file cleanup. |
| `startup-error` | A private incompatible-daemon handshake, content-sized standalone alerts, repeated CLI and desktop activation/open including `--new-window` during startup failure, initial focus, keyboard/pointer/system dismissal, keyboard scrolling of long plain-text error details with a fixed Close response, AT-SPI error descriptions, saved light/dark appearance, screenshots with and without high contrast, and neutral button fill. |
| `daemon`, `control`, `state-recovery` | Session lifecycle, command ordering, persistence failures, transport validation, and startup recovery. |
| `scrollback` | Durable colored history across Quit/SIGTERM, exact indexed/RGB foreground/background and attribute cells, resets and fresh shell output, Unicode wraps, bounded versioned snapshots, corruption and startup recovery, and deleted-pane cleanup. |
| `workspace`, `sidebar-search`, `branch`, `title`, `scheduling` | Workspace state, sidebar behavior, process metadata, focus, and background work. |
| `find`, `commands`, `font`, `resize`, `split-resize` | Search, palette accessibility, live settings, and sizing. |
| `terminal-copy` | Clipboard publication during row resize with daemon replies held, and clipboard preservation when no selection exists. |
| `main-menu`, `terminal-context-menu` | Menu actions, keyboard navigation, and busy-process confirmation. |
| `terminal-input`, `terminal-scrollbar`, `terminal-renderer`, `renderer` | Input, clipboard, selection, scrolling, interactive applications, and render-node correctness. |
| `notifications`, `notification-focus`, `notification-delivery` | Alert queues, unread acknowledgements, reconnects, focus suppression, desktop delivery before pane attachment, and click navigation. |
| `smoke`, `audit`, `icon` | Application workflows, pane lifecycle cases, and icon decoding. |
| `screenshot` | Fresh mapped, occluded, and revealed captures, window decoration, paused frame clocks, and logical PNG dimensions at 100% and 125% monitor scale. |

The `find` suite runs both dark and light appearance, including retained and
checkpoint-restored colored Unicode and wrapped history, offscreen navigation,
grid reflow, and the selected sidebar row's directory after pane focus changes.
It checks exact restored background pixels in a composited Wayland frame and
copies a restored line across reflow using native pointer selection and the
Wayland clipboard. A new clipboard owner and the exact text must arrive within
the existing deadline, even when a previous test has left clipboard contents.
Before dragging after Find closes, the suite waits for its observed closed
vertical allocation, frontend and daemon grid agreement, and a new compositor frame.
The `terminal-copy` suite holds daemon updates at a barrier while row growth or
shrink invalidates the displayed selection. Copy must still publish the exact
tracked Unicode text with a new Wayland clipboard owner. Copy with no backend
selection must return the daemon's rejection and preserve the existing text
and owner. The headless `daemon`
suite checks ordered selection and copy requests, including selected whitespace
that legitimately formats to an empty string.
It waits for native window dimensions and the expanded sidebar after widening;
terminal width alone can change when a collapsed sidebar hides. Engine unit
tests also check copying restored history across reflow, and `terminal-input`
checks native selection and clipboard copying across scrollback.

Engine checkpoint tests compare historical cells across several widths,
including indexed and RGB colors, text attributes, wide/combining characters,
styled spaces, unwritten gaps, internal blank lines, and search/copy results.
They verify current theme behavior and discarded saved palette overrides, a clean new output row,
restricted rendition controls, and rejection before installation when styles
expand a valid snapshot beyond the replay budget. CRC-valid payloads containing
control characters or excessive page dimensions also fail before installation.

Detailed rendering checks are in [renderer validation](renderer.md#validation).
Backend cases are in [session validation](terminal-sessions.md#validation).
Test counts come from the runner output; the suite changes as regressions are
added.

The general `smoke` suite covers startup, PTY workflows, and session reconnects.
The runner selects its native mode. A direct invocation without `--native`
uses Broadway with private runtime and HTTP Unix sockets, avoiding fixed display
sockets and TCP ports; that mode does not validate native Wayland integration.
Notification edge cases belong to the notification suites. The `audit` suite
runs each regression in a separate application fixture. Named scenarios print
their status so failures identify the behavior being checked.

The `resize` suite checks grid overlays and their dismissal deadlines at 100%
and 125% monitor scale. It waits for actual GTK window allocations and requires
one-pixel canvas changes in both width and height while retaining rows and
columns. Independent `TIOCGWINSZ` probes check PTY pixel sizes. Each pixel-only
case observes overlay absence and stable geometry throughout the dismissal
interval, catching delayed compositor replies that undo a resize. Its empty
saved session starts at a size that fits the logical monitor.

The `split-resize` suite checks horizontal and vertical proportions during
one-pixel window growth and shrinkage at 100% and 125% monitor scale. Equal
splits must stay within one pixel, and resizing must preserve saved proportions.
It waits for native allocations on each step and captures diagnostic screenshots
at scenario checkpoints.
It also checks native divider dragging, nested unequal splits, session restore,
and recovery of the requested proportions after minimum sizes constrain a pane.
The fixture enables pane title bars so their controls impose the minimum width
used by the clamping scenario, independently of the application's default.

The `screenshot` suite uses a solid fullscreen cover and compositor frames to
verify that Gmux is fully occluded. It changes only the private fixture monitor's
scale through Mutter's DisplayConfig interface and restores it after each case.
It checks GTK's reported surface scale because `GDK_SCALE` does not override
Wayland's preferred scale in the tested environment. See the
[screenshot investigation](screenshot-diagnostics.md) for API alternatives.

## Backends and appearance

Check GTK's Vulkan, OpenGL, and software backends through the same fixture:

```sh
for backend in vulkan gl cairo; do
  GSK_RENDERER="$backend" python3 scripts/check.py --native --skip-build --only renderer
done
```

The menu tests also accept light and high-contrast appearances:

```sh
GMUX_REVIEW_APPEARANCE=light python3 scripts/check.py --native --skip-build --only main-menu terminal-context-menu
ADW_DEBUG_HIGH_CONTRAST=1 python3 scripts/check.py --native --skip-build --only main-menu terminal-context-menu
```

## Failures and artifacts

The runner stops at the first failed command. Each Cargo command has a
20-minute build deadline; each smoke suite retains a five-minute deadline.
Use `--build-timeout SECONDS` and `--smoke-timeout SECONDS` to configure them.
These command deadlines do not change native readiness or dismissal assertions.
The final output names the smoke suites that passed, reports omitted Rust checks
with `--skip-build`, and states when native suites were not selected. The optional
`btop` title case reports its omission when that executable is absent.
Read the command's output and its files under `artifacts/`. Native
fixture logs include `check-session.log` and `check-wayland.log`; individual
checks write their own logs, screenshots, and render-node reports.

`artifacts/` is ignored by Git. A fresh checkout has none of the local reports
referenced in design or benchmark notes. Preserve reports separately when sharing
results. [Application performance](performance.md) and
[renderer benchmarks](renderer.md#benchmarks) describe how to generate them.

Shared socket calls, waits, accessibility traversal, and process fixtures live
in [`scripts/test_support.py`](../scripts/test_support.py). Native input and
capture helpers live in
[`scripts/wayland_test_support.py`](../scripts/wayland_test_support.py).

`AppFixture` tracks processes before waiting for startup, supplies private state
and a shell, and shuts down the daemon even if the frontend has already exited.
Shell fixtures write history within their temporary directories.
`AccessibilityFixture` activates and waits for the registry's D-Bus name.
An activated registry belongs to the private accessibility bus and ends with
that bus. If activation is unavailable, the fixture owns and stops only the
fallback process it launched. An existing registry is reused without killing it.
Readiness checks wait for observable state. Timed
observations remain where a check needs to verify dismissal deadlines or the
absence of delayed updates.
