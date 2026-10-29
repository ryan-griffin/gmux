#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p artifacts
exec > >(tee artifacts/ci.log) 2>&1
export CARGO_HOME="${CARGO_HOME:-$PWD/.ci-cache/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$PWD/.ci-cache/rustup}"
export PATH="$CARGO_HOME/bin:$PATH"
# Fedora packages the installer; it creates Cargo's proxies without a curl pipe.
if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
    rustup-init -y --no-modify-path --default-toolchain none
fi

# Use the checked-in development toolchain unless explicitly overridden.
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')}"
rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rustfmt --component clippy
rustc --version
cargo --version
# CI always validates this checkout's newly bootstrapped library.
export GHOSTTY_PREFIX="$PWD/.deps/ghostty"
export LD_LIBRARY_PATH="$GHOSTTY_PREFIX/lib"
python3 - <<'PY'
import runpy
pins = runpy.run_path("scripts/bootstrap-ghostty.py")
print(f"Ghostty {pins['REVISION']}; Zig {pins['ZIG_VERSION']}")
PY
./scripts/bootstrap-ghostty.sh
python3 - <<'PY'
import hashlib
from pathlib import Path
import runpy
pins = runpy.run_path("scripts/bootstrap-ghostty.py")
archive = Path(".deps") / f"ghostty-{pins['REVISION']}.tar.gz"
with archive.open("rb") as source:
    checksum = hashlib.file_digest(source, "sha256").hexdigest()
assert checksum == pins["SOURCE_SHA256"]
print(f"{checksum}  {archive}")
PY
python3 scripts/check.py --headless
python3 - <<'PY'
import hashlib
from pathlib import Path
import sys
sys.path.insert(0, "scripts")
from cargo_build import compiler_artifacts
artifacts = compiler_artifacts(Path("artifacts/cargo-build-headless.jsonl").read_text())
daemon = Path(artifacts["debug/bin/gmux"])
Path("artifacts/daemon-headless.sha256").write_text(hashlib.sha256(daemon.read_bytes()).hexdigest())
PY
# The headless pass already exercised this same daemon. Check the full workspace
# and its GTK linkage without repeating those smoke suites.
python3 scripts/check.py --skip-smoke
python3 - <<'PY'
from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys
sys.path.insert(0, "scripts")
from cargo_build import compiler_artifacts
from cargo_test_support import ARTIFACT_ENV, cargo_binary
os.environ[ARTIFACT_ENV] = json.dumps(compiler_artifacts(Path("artifacts/cargo-build.jsonl").read_text()))
for name in ("gmux-gtk", "gmux"):
    binary = cargo_binary(name)
    print(f"Built binary: {binary}", flush=True)
    linkage = subprocess.check_output(["ldd", str(binary)], text=True)
    print(linkage)
    ghostty = [line for line in linkage.splitlines() if "libghostty-vt.so" in line]
    # Terminal parsing lives in the daemon; the frontend may not link Ghostty.
    if name == "gmux":
        assert len(ghostty) == 1, "The daemon must link the bootstrapped Ghostty library"
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == Path("artifacts/daemon-headless.sha256").read_text(), \
            "Headless and desktop builds must ship the same daemon artifact"
    else:
        assert not ghostty, "GTK must run without the native execution library"
    for library in ghostty:
        assert str(Path(".deps/ghostty/lib").resolve()) + "/" in library, library
PY
