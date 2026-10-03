#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
gmux_install_prefix=${1:-"$HOME/.local"}
gmux_ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
if [[ ! -f "$gmux_ghostty_prefix/include/ghostty/vt/terminal.h" ]]; then
    ./scripts/bootstrap-ghostty.sh
fi
cargo build --release --locked
install -Dm755 target/release/gmux-daemon "$gmux_install_prefix/bin/gmux-daemon"
install -Dm755 target/release/gmux "$gmux_install_prefix/bin/gmux"
mkdir -p "$gmux_install_prefix/lib/gmux"
cp -L "$gmux_ghostty_prefix/lib/libghostty-vt.so" "$gmux_install_prefix/lib/gmux/libghostty-vt.so.0"
install -Dm644 data/dev.gmux.Gmux.desktop "$gmux_install_prefix/share/applications/dev.gmux.Gmux.desktop"
install -Dm644 data/dev.gmux.Gmux.svg "$gmux_install_prefix/share/icons/hicolor/scalable/apps/dev.gmux.Gmux.svg"
install -Dm644 data/dev.gmux.Gmux-symbolic.svg "$gmux_install_prefix/share/icons/hicolor/symbolic/apps/dev.gmux.Gmux-symbolic.svg"
install -Dm644 data/dev.gmux.Gmux.metainfo.xml "$gmux_install_prefix/share/metainfo/dev.gmux.Gmux.metainfo.xml"
install -Dm644 LICENSE "$gmux_install_prefix/share/doc/gmux/LICENSE"
install -Dm644 data/icons/README.md "$gmux_install_prefix/share/doc/gmux/icons/README.md"
install -Dm644 data/icons/COPYING.Console "$gmux_install_prefix/share/doc/gmux/icons/COPYING.Console"
install -Dm644 data/icons/COPYING.md "$gmux_install_prefix/share/doc/gmux/icons/COPYING.md"
if command -v gtk4-update-icon-cache >/dev/null; then
    gtk4-update-icon-cache -f -t "$gmux_install_prefix/share/icons/hicolor"
elif command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -f -t "$gmux_install_prefix/share/icons/hicolor"
fi
if command -v update-desktop-database >/dev/null; then update-desktop-database "$gmux_install_prefix/share/applications"; fi
printf 'Installed Gmux in %s\n' "$gmux_install_prefix"
