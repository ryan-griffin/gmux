# Testing

Run commands from the repository root. The [README](../README.md#build-and-run)
lists build prerequisites. Bootstrap the pinned Ghostty library before the first
build, or set `GHOSTTY_PREFIX` to a build of the same source revision.

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native
```

The runner checks formatting, Rust tests, Clippy, builds, and four headless smoke
suites. `--native` adds five desktop suites in a private Wayland session.
Rust tests cover terminal parsing, search, layout changes, transport limits,
state writes, and scheduling. Integration suites exercise real PTYs and GTK.

## Select checks

```sh
python3 scripts/check.py --list
python3 scripts/check.py --only daemon scrollback
python3 scripts/check.py --native --skip-build --only renderer multi-window
```

`--only` selects suites by filename without `-smoke-test.py`; use `smoke` for
`smoke-test.py`. Rust checks still run unless `--skip-build` is set.
That option uses executable paths recorded by the previous runner build in
`artifacts/cargo-build.jsonl`. Rerun without it after source or build-config changes.
Direct smoke scripts build their executables through Cargo unless given an
explicit binary path. Cargo handles target directories and build configuration.

Headless suites cover daemon lifecycle, shared clients, input backpressure and
UTF-8 paste, and durable history. Desktop suites cover application workflows,
multiple windows, native input and clipboard, notifications, and rendering.

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

## CI and failures

GitHub Actions runs the headless checks in the Fedora container pinned in
`.github/workflows/check.yml`, using the development Rust toolchain from
`rust-toolchain.toml`. It does not run desktop checks.
`ci/install-deps.sh` and `ci/check.sh` provide the same steps for a container run.

Logs, screenshots, and renderer reports go under the Git-ignored `artifacts/`
directory. The runner stops at the first failure. Its default deadlines are
1200 seconds per Cargo command and 300 seconds per smoke suite; change them with
`--build-timeout` and `--smoke-timeout`.
