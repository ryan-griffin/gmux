# Gmux

A GNOME terminal inspired by [cmux](https://cmux.com/), built with Rust, GTK 4, libadwaita, and libghostty-vt.

- Flat, searchable tab sidebar with workspace filters and Git branch information.
- Split terminal panes, session layout restoration, and a command palette.
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

Create tabs with the plus button. Use the sidebar filter to add or manage workspaces. Right-click a tab to rename it or move it to a workspace; middle-click to close it. Deleting a workspace keeps its tabs open and unassigned. Exiting a shell closes its pane.

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

The app's Keyboard Shortcuts dialog lists the remaining shortcuts. Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`. Restored tabs start fresh shells.

## Shell automation

With Gmux running, use `gmux` inside a pane or `target/debug/gmux` from another terminal:

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

Native regression tests are in [scripts/](scripts/). See the [code and GNOME UI review](docs/code-review.md) for widget choices, library integration, validation, and known limitations.

Gmux uses a custom Pango/Cairo renderer with Ghostty's terminal engine. Terminal image protocols are not supported; Find and selection currently operate on the visible viewport.

## License

[MIT](LICENSE). The bundled branch icon is [CC0](data/icons/COPYING.md). Ghostty and the GNOME libraries retain their own licenses.
