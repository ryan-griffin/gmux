#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ ! -f .deps/ghostty/include/ghostty/vt/terminal.h && -z ${GHOSTTY_PREFIX:-} ]]; then
    ./scripts/bootstrap-ghostty.sh
fi
exec cargo run -- "$@"
