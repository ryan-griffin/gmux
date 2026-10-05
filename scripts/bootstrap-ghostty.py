#!/usr/bin/env python3
"""Build the pinned libghostty-vt locally. Never changes system packages."""
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEPS = ROOT / ".deps"
REVISION = "4da7523faba68ccb4042ea20585817098a51c015"
SOURCE_SHA256 = "61ea669deb246947d111c36527a87e37ab436cbe0cbe67ca71a970dba40b9b28"
ZIG_VERSION = "0.16.0"
EXTRACTED = ".gmux-extracted"


def download(url, destination, checksum=None):
    def valid(path):
        if checksum is None:
            return True
        with path.open("rb") as archive:
            return hashlib.file_digest(archive, "sha256").hexdigest() == checksum

    mismatch = f"Checksum mismatch for {url}; run bootstrap again to download a fresh copy."
    if destination.exists():
        if not valid(destination):
            destination.unlink()
            raise SystemExit(mismatch)
        return
    with tempfile.NamedTemporaryFile(dir=destination.parent, prefix=f".{destination.name}.",
                                     suffix=".tmp", delete=False) as file:
        temporary = Path(file.name)
    try:
        subprocess.run(["curl", "--fail", "--location", "--silent", "--show-error", "--retry", "3", url, "-o", str(temporary)], check=True)
        if not valid(temporary):
            raise SystemExit(mismatch)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)


def extract(archive_path, destination):
    # Publish only a complete tree so a failed extraction is retried next time.
    with tempfile.TemporaryDirectory(dir=destination.parent, prefix=f".{destination.name}.") as directory:
        with tarfile.open(archive_path) as archive:
            archive.extractall(directory, filter="data")
        tree = Path(directory, destination.name)
        if not tree.is_dir():
            raise SystemExit(f"{archive_path} does not contain {destination.name}")
        (tree / EXTRACTED).touch()
        # Replace partial trees left by older bootstrap attempts.
        if destination.exists():
            shutil.rmtree(destination)
        tree.replace(destination)


def main():
    arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(platform.machine())
    if not arch or platform.system() != "Linux":
        raise SystemExit("Bootstrap supports x86_64 and aarch64 Linux. Set GHOSTTY_PREFIX for another platform.")
    DEPS.mkdir(exist_ok=True)

    source_archive = DEPS / "ghostty.tar.gz"
    download(f"https://codeload.github.com/ghostty-org/ghostty/tar.gz/{REVISION}", source_archive, SOURCE_SHA256)
    source = DEPS / f"ghostty-{REVISION}"
    if not (source / EXTRACTED).is_file():
        extract(source_archive, source)

    zig_dir = DEPS / f"zig-{arch}-linux-{ZIG_VERSION}"
    zig = zig_dir / "zig"
    if not zig.exists() or not (zig_dir / EXTRACTED).is_file():
        index = subprocess.check_output(["curl", "-fsSL", "https://ziglang.org/download/index.json"])
        release = json.loads(index)[ZIG_VERSION][f"{arch}-linux"]
        zig_archive = DEPS / "zig.tar.xz"
        download(release["tarball"], zig_archive, release["shasum"])
        extract(zig_archive, zig_dir)

    # curl honors HTTPS_PROXY in environments where Zig's downloader cannot
    # resolve external hosts. Zig still verifies each dependency's package hash.
    command = [str(zig), "build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "--prefix", str(DEPS / "ghostty"), "-j4"]
    fetched = set()
    for _ in range(50):
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


if __name__ == "__main__":
    main()
