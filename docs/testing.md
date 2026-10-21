# Testing

Run commands from the repository root. The [README](../README.md#build-and-run)
lists build prerequisites. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a build of the same source revision.

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
```

The runner checks formatting, Rust tests, Clippy, builds, linkage, and headless
smoke suites. Add `--headless` to build and test only `gmux-core`, `gmux-client`,
and `gmux`, without GTK, libadwaita, GLib, or `glib-compile-resources`.
Add `--native` to include desktop suites in a private Wayland session.
`--headless` and `--native` cannot be combined.

Check execution and the client library without a toolkit:

```sh
cargo test --locked -p gmux-core -p gmux-client --lib
cargo clippy --locked -p gmux-core -p gmux-client --all-targets -- -D warnings
```

## Select checks

```sh
python3 scripts/check.py --list
python3 scripts/check.py --only daemon scrollback
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
cc -O2 -std=c11 -I"$ghostty_prefix/include" \
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
