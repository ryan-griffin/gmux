# Gmux

A GNOME terminal inspired by [cmux](https://cmux.com/), built with Rust, GTK 4, libadwaita, and libghostty-vt.

- Flat, searchable tab sidebar with workspace filters and Git branch information.
- Split terminal panes, background sessions, layout restoration, and a command palette.
- Adwaita light and dark themes, GNOME's monospace font, and configurable cursor and pane title bars.
- Desktop notifications with unread indicators and a local CLI for shell automation.

## Run

Requires Linux, Rust stable, a C compiler, pkg-config, GTK 4.22+, libadwaita 1.9+, GLib's `glib-compile-resources`, curl, and Python 3.12+.

```sh
./scripts/run.sh
# Start a tab in a specific directory:
./scripts/run.sh --directory ~/Projects/my-app
```

The first run downloads Zig and builds the pinned Ghostty library under `.deps/`. Nothing is installed system-wide.

To install in `~/.local`, run:

```sh
./scripts/install.sh
```

Pass another installation prefix as the script's first argument.

## Use

Closing the window keeps panes and commands running in a headless daemon. Reopen Gmux to reconnect to the same terminals. Use **Quit Gmux**, Ctrl+Shift+Q, or `gmux quit` to stop them.

Create tabs with the plus button. Use the sidebar filter to add or manage workspaces. Sidebar visibility is remembered when you reopen Gmux. Right-click a tab to rename it or move it to a workspace; middle-click to close it. Deleting a workspace keeps its tabs open and unassigned. Exiting a shell closes its pane.

| Action | Shortcut |
| --- | --- |
| New tab | Ctrl+Shift+T |
| New tab in directory | Ctrl+Shift+O |
| Split right / down | Ctrl+Shift+D / Ctrl+Shift+E |
| Close pane | Ctrl+Shift+W |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab |
| Copy / paste | Ctrl+Shift+C / Ctrl+Shift+V |
| Find in visible terminal | Ctrl+Shift+F |
| Commands | Ctrl+Shift+P |
| Preferences | Ctrl+, |

The app's Keyboard Shortcuts dialog lists the remaining shortcuts. Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`. After the daemon stops, restored tabs start fresh shells. Scrollback currently survives window closure, but not daemon shutdown or a reboot.

## Shell automation

With the Gmux daemon running, including while its window is closed, use `gmux` inside a pane or `target/debug/gmux` from another terminal:

```sh
gmux list
gmux new --name server --directory ~/Projects/my-app
gmux workspace new --name my-app --directory ~/Projects/my-app
gmux split --down
gmux send 'cargo test' --enter
gmux notify 'Tests finished'
```

Pane commands default to the calling shell's pane, or the active pane when called outside Gmux. Run `gmux --help` for all commands and `gmux <command> --help` for options.

## Development

```sh
./scripts/bootstrap-ghostty.sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Native regression tests are in [scripts/](scripts/). The [keyboard and clipboard test](docs/renderer.md#validation) runs in an isolated Wayland compositor with XWayland disabled. See the [code and GNOME UI review](docs/code-review.md) for widget choices, library integration, validation, and known limitations.

Run `python3 scripts/icon-smoke-test.py` to check the app icons with GdkPixbuf's format detection and SVG loader. This check requires PyGObject and the GdkPixbuf SVG loader, and runs without a display.

Terminal processes and state live in a GTK-independent [session manager](docs/terminal-sessions.md). A headless daemon owns that manager and sends changed rows to the GTK frontend.

Gmux uses Ghostty's terminal engine with Pango font shaping and GSK rendering. GTK selects a GPU backend or its software backend; see the [renderer design and benchmarks](docs/renderer.md). Terminal image protocols are not supported; Find and selection currently operate on the visible viewport.

The [application benchmark](docs/performance.md) measures frontend and daemon CPU, resident memory, and native Wayland input-to-composited-frame latency with several busy panes.

## License

[MIT](LICENSE). The app icon adapts GNOME Console's artwork under [GPL-3.0-or-later](data/icons/COPYING.Console); see the [icon credits](data/icons/README.md). The bundled branch icon is [CC0](data/icons/COPYING.md). Ghostty and the GNOME libraries retain their own licenses.
