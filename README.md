# Gmux

A GNOME terminal inspired by [cmux](https://cmux.com/), built with Rust, GTK 4,
libadwaita, and libghostty-vt.

Gmux has workspaces, split panes, a searchable tab sidebar, Git branch labels,
and notifications for background commands. Terminal sessions keep running when
you close the window. Connect to local or remote machines, or use the standalone
daemon and CLI without a desktop.

This project is pre-alpha. Saved data formats may change between builds.

## Build and run

Requires Linux, Rust 1.99+, a C compiler, libclang, pkg-config, curl, patch, and Python
3.12+. The GNOME client also requires GTK 4.22+, libadwaita 1.9+, and
`glib-compile-resources`. The checked-in Rust toolchain file selects the
development compiler with rustup.

```sh
./scripts/run.sh
./scripts/run.sh --directory ~/Projects/my-app
```

The first run downloads Zig 0.17.0 and builds the pinned Ghostty 1.4.0 library
in `.deps/`. See [native library](docs/native-library.md) for the source pin,
local build caches, and saved-history behavior on dependency updates.
To install in `~/.local`, run `./scripts/install.sh`. Pass a different prefix as
its first argument. Launch the installed app with `gmux-gtk`.

For the daemon and CLI alone, install with `./scripts/install.sh --headless` and
start it with `gmux serve --background`. See [remote daemons](docs/remote-daemons.md)
for connection setup and authentication.

## Use

In **Manage Connections…**, open **This Computer**, turn on **Local Daemon**, and
Save to run terminals locally. You can also add remote connections there.

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
| Previous / next prompt | Ctrl+Shift+Up / Ctrl+Shift+Down |
| Command palette | Ctrl+Shift+P |

Closing a window leaves its terminals running. Reopen Gmux to reconnect.

**Quit Gmux** or Ctrl+Shift+Q stops the app's connected local daemon and its
terminals. Remote terminals keep running. After a daemon restart, Gmux restores
the saved layout and text history with fresh shells. Commands and jobs do not resume.

Media and volume keys delivered by the desktop work in terminal programs that
enable the Kitty keyboard protocol. See [keyboard input](docs/keyboard.md) for
supported keys and browser-key limitations.

Copying terminal selections or command output offers plain text and styled
HTML together. Compatible destinations preserve terminal colors and formatting;
text destinations receive plain text. See [clipboard behavior](docs/clipboard.md)
for limits and formatting support.

Terminal programs can write text with OSC 52 or iTerm2 Copy, and offer PNG,
HTML, and plain-text alternatives together with Kitty OSC 5522, including over
remote connections. Gmux sends each write to one window viewing the pane, preferring
the last focused eligible window, then the earliest connected window. The `c`
destination uses the system clipboard and `p` uses primary selection where the
desktop supports it. An empty OSC 52 payload clears the destination. Secondary
selection and clipboard reads are unsupported. Application writes are
limited to 4 MiB of decoded data and discarded when no eligible window is
connected. They are never replayed on attachment or session restore.

Settings and saved layouts live in `~/.config/gmux/`, or `$XDG_CONFIG_HOME/gmux/`.

## Shell automation

With the daemon running, its CLI can create panes, send input, and deliver alerts:

```sh
gmux list
gmux new --name server --directory ~/Projects/my-app
gmux split --down
gmux send 'cargo test' --enter
gmux metadata
gmux command-output
gmux notify 'Tests finished'
```

Use `gmux scrollback` to inspect the daemon's saved history limits, or
`gmux scrollback --max-bytes 10000000 --max-lines 10000` to change them for all
its panes. See [scrollback retention](docs/scrollback.md) for defaults, remote
controls, and pruning behavior.

Run `gmux --help` for commands. Pane commands use the calling shell's pane;
otherwise pass `--pane` to choose one. Use `gmux-gtk` for workspace and window actions.

Bash and Zsh shells report command lifecycle and exit status automatically.
Use Terminal Actions or the command palette to select or copy command output
without its prompt. See [shell integration](docs/shell-integration.md) for setup,
nested shells, and socket commands.

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
