#!/usr/bin/env python3
"""Build the pinned libghostty-vt locally. Never changes system packages."""
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parent.parent
DEPS = ROOT / ".deps"
REVISION = "4da7523faba68ccb4042ea20585817098a51c015"
SOURCE_SHA256 = "61ea669deb246947d111c36527a87e37ab436cbe0cbe67ca71a970dba40b9b28"
ZIG_VERSION = "0.16.0"
DEPS.mkdir(exist_ok=True)


def download(url, destination, checksum=None):
    if not destination.exists():
        subprocess.run(["curl", "--fail", "--location", "--silent", "--show-error", "--retry", "3", url, "-o", str(destination)], check=True)
    if checksum and hashlib.sha256(destination.read_bytes()).hexdigest() != checksum:
        destination.unlink()
        raise SystemExit(f"Checksum mismatch for {url}; run bootstrap again to download a fresh copy.")


source_archive = DEPS / "ghostty.tar.gz"
download(f"https://codeload.github.com/ghostty-org/ghostty/tar.gz/{REVISION}", source_archive, SOURCE_SHA256)
source = DEPS / f"ghostty-{REVISION}"
if not source.exists():
    with tarfile.open(source_archive) as archive:
        archive.extractall(DEPS, filter="data")

arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(platform.machine())
if not arch or platform.system() != "Linux":
    raise SystemExit("Bootstrap supports x86_64 and aarch64 Linux. Set GHOSTTY_PREFIX for another platform.")
zig_dir = DEPS / f"zig-{arch}-linux-{ZIG_VERSION}"
zig = zig_dir / "zig"
if not zig.exists():
    index = subprocess.check_output(["curl", "-fsSL", "https://ziglang.org/download/index.json"])
    release = json.loads(index)[ZIG_VERSION][f"{arch}-linux"]
    zig_archive = DEPS / "zig.tar.xz"
    download(release["tarball"], zig_archive, release["shasum"])
    with tarfile.open(zig_archive) as archive:
        archive.extractall(DEPS, filter="data")

# curl honors HTTPS_PROXY in environments where Zig's downloader cannot
# resolve external hosts. Zig still verifies each dependency's package hash.
command = [str(zig), "build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "--prefix", str(DEPS / "ghostty"), "-j4"]
fetched = set()
for attempt in range(50):
    print("Building libghostty-vt...", flush=True)
    result = subprocess.run(command, cwd=source, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode == 0:
        print(f"libghostty-vt is ready in {DEPS / 'ghostty'}")
        break
    urls = re.findall(r'\.url\s*=\s*"(https://[^\"]+)"', result.stdout)
    urls = [url for url in urls if url not in fetched]
    if not urls:
        print(result.stdout)
        raise SystemExit(result.returncode)
    for url in urls:
        fetched.add(url)
        print(f"Fetching {url}", flush=True)
        archive = DEPS / ("dep-" + hashlib.sha256(url.encode()).hexdigest()[:16] + ".tar.gz")
        download(url, archive)
        subprocess.run([str(zig), "fetch", str(archive)], cwd=source, check=True)
else:
    raise SystemExit("Dependency fetch limit reached")
