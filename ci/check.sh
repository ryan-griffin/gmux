#!/usr/bin/env bash
# Invoked by ci/run.sh as the checkout owner, with a valid container home.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p artifacts
exec > >(tee artifacts/ci.log) 2>&1

# CI sets this to the package minimum or the development toolchain. Local
# container runs default to the checked-in development toolchain.
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')}"
rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rustfmt --component clippy
rustc --version
cargo --version
pacman -Q gtk4 libadwaita clang python python-gobject gdk-pixbuf2 librsvg

# Always run bootstrap, even after a cache hit, to verify the source archive
# checksum and rebuild the library from this checkout's pin.
python3 - <<'PY'
import runpy
pins = runpy.run_path("scripts/bootstrap-ghostty.py")
print(f"Ghostty {pins['REVISION']}; Zig {pins['ZIG_VERSION']}")
PY
./scripts/bootstrap-ghostty.sh
python3 scripts/check.py
