#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
gmux_install_prefix="$HOME/.local"
gmux_headless=false
gmux_prefix_set=false
while (( $# )); do
    case "$1" in
        --headless) gmux_headless=true ;;
        --help|-h)
            printf 'Usage: %s [--headless] [PREFIX]\n' "$0"
            exit 0 ;;
        -*) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
        *)
            if "$gmux_prefix_set"; then
                printf 'Only one installation prefix is allowed.\n' >&2
                exit 2
            fi
            gmux_install_prefix=$1
            gmux_prefix_set=true ;;
    esac
    shift
done
gmux_ghostty_prefix=${GHOSTTY_PREFIX:-"$PWD/.deps/ghostty"}
if [[ ! -f "$gmux_ghostty_prefix/include/ghostty/vt/terminal.h" ]]; then
    ./scripts/bootstrap-ghostty.sh
fi
gmux_build_options=(--release)
if "$gmux_headless"; then gmux_build_options+=(--headless); fi
gmux_binary_names=(gmux)
if ! "$gmux_headless"; then gmux_binary_names+=(gmux-gtk); fi
gmux_binaries=$(python3 ./scripts/cargo_build.py "${gmux_build_options[@]}" "${gmux_binary_names[@]}")
mapfile -t gmux_binaries <<< "$gmux_binaries"
install -Dm755 "${gmux_binaries[0]}" "$gmux_install_prefix/bin/gmux"
if ! "$gmux_headless"; then install -Dm755 "${gmux_binaries[1]}" "$gmux_install_prefix/bin/gmux-gtk"; fi
mkdir -p "$gmux_install_prefix/lib/gmux"
cp -L "$gmux_ghostty_prefix/lib/libghostty-vt.so" "$gmux_install_prefix/lib/gmux/libghostty-vt.so.0"
install -Dm644 LICENSE "$gmux_install_prefix/share/doc/gmux/LICENSE"
mkdir -p "$gmux_install_prefix/share/gmux"
cp -R data/shell-integration "$gmux_install_prefix/share/gmux/"
if ! "$gmux_headless"; then
    install -Dm644 data/dev.gmux.Gmux.desktop "$gmux_install_prefix/share/applications/dev.gmux.Gmux.desktop"
    install -Dm644 data/dev.gmux.Gmux.svg "$gmux_install_prefix/share/icons/hicolor/scalable/apps/dev.gmux.Gmux.svg"
    install -Dm644 data/dev.gmux.Gmux-symbolic.svg "$gmux_install_prefix/share/icons/hicolor/symbolic/apps/dev.gmux.Gmux-symbolic.svg"
    install -Dm644 data/dev.gmux.Gmux.metainfo.xml "$gmux_install_prefix/share/metainfo/dev.gmux.Gmux.metainfo.xml"
    install -Dm644 data/icons/README.md "$gmux_install_prefix/share/doc/gmux/icons/README.md"
    install -Dm644 data/icons/COPYING.Console "$gmux_install_prefix/share/doc/gmux/icons/COPYING.Console"
    install -Dm644 data/icons/COPYING.md "$gmux_install_prefix/share/doc/gmux/icons/COPYING.md"
    if command -v gtk4-update-icon-cache >/dev/null; then
        gtk4-update-icon-cache -f -t "$gmux_install_prefix/share/icons/hicolor"
    elif command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -f -t "$gmux_install_prefix/share/icons/hicolor"
    fi
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database "$gmux_install_prefix/share/applications"
    fi
fi
printf 'Installed Gmux in %s\n' "$gmux_install_prefix"
