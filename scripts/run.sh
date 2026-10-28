#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -z ${GHOSTTY_PREFIX:-} ]]; then
    ./scripts/bootstrap-ghostty.sh
fi
gmux_binaries=$(python3 ./scripts/cargo_build.py gmux-gtk gmux)
mapfile -t gmux_binaries <<< "$gmux_binaries"
exec "${gmux_binaries[0]}" "$@"
