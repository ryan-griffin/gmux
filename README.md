# Gmux

A GNOME terminal inspired by [cmux](https://cmux.com/), built with Rust, GTK 4,
libadwaita, and libghostty-vt.

Gmux has workspaces, split panes, a searchable tab sidebar, Git branch labels,
and notifications for background commands. Terminal sessions keep running when
you close the window.

This project is pre-alpha. Saved data formats may change between builds.

## Build and run

Requires Linux, Rust 1.99+, GTK 4.22+, libadwaita 1.9+, a C compiler, libclang,
pkg-config, `glib-compile-resources`, curl, and Python 3.12+.
The checked-in Rust toolchain file selects the development compiler with rustup.

```sh
./scripts/run.sh
./scripts/run.sh --directory ~/Projects/my-app
```

The first run downloads Zig and builds the pinned Ghostty library in `.deps/`.
To install in `~/.local`, run `./scripts/install.sh`. Pass a different prefix as
its first argument.

## Use

Create tabs with the plus button and split panes through Terminal Actions.
Manage workspaces through the main menu. Use the command palette or Keyboard
Shortcuts dialog to find other actions.

| Action | Shortcut |
| --- | --- |
| New window | Ctrl+Shift+N |
| New tab | Ctrl+Shift+T |
| Split right / down | Ctrl+Shift+D / Ctrl+Shift+E |
| Close pane | Ctrl+Shift+W |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab |
| Copy / paste | Ctrl+Shift+C / Ctrl+Shift+V |
| Find | Ctrl+Shift+F |
| Command palette | Ctrl+Shift+P |

Closing a window leaves its terminals running. Reopen Gmux to reconnect, or use
**Quit Gmux**, Ctrl+Shift+Q, or `gmux quit` to stop all terminals. After a daemon
restart, Gmux restores the saved layout and text history with fresh shells.
Commands and jobs do not resume.

Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`.

## Shell automation

With Gmux running, its CLI can create panes, send input, and deliver alerts:

```sh
gmux list
gmux new --name server --directory ~/Projects/my-app
gmux split --down
gmux send 'cargo test' --enter
gmux notify 'Tests finished'
```

Run `gmux --help` for all commands. Pane commands use the calling shell's pane,
or the active pane when called outside Gmux. They also work with the window closed.

## Development

```sh
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
python3 scripts/check.py --native
```

See [testing](docs/testing.md) for prerequisites and selected checks, and
[code organization](docs/code-organization.md) for the architecture.

## License

[MIT](LICENSE). The app icon adapts GNOME Console's artwork under
[GPL-3.0-or-later](data/icons/COPYING.Console). See the
[icon credits](data/icons/README.md). Ghostty and the GNOME libraries retain
their own licenses.
