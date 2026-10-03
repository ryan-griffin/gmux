#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -f .deps/ghostty/include/ghostty/vt/terminal.h && -z ${GHOSTTY_PREFIX:-} ]]; then
    ./scripts/bootstrap-ghostty.sh
fi
cargo build --locked --bins
exec target/debug/gmux "$@"
