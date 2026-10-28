#!/usr/bin/env python3
"""Build the pinned libghostty-vt locally. Never changes system packages."""
import hashlib
import json
from pathlib import Path
import platform
import re
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEPS = ROOT / ".deps"
REVISION = "8f0dd3709050b1026f6324368805033197d8b4a5"
SOURCE_SHA256 = "f4869fc667ea0a1c93705e363ceca06d394b67ad1d3dde756a7fa5c15691f9cb"
# Must match the source's build.zig.zon and src/build/zig.zig requirement.
ZIG_VERSION = "0.16.0"
PATCHES = [ROOT / "scripts/patches/ghostty-media-keys.patch"]


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


def extract(archive_path, destination, *, tree_name=None, patches=()):
    # Publish only a complete tree so a failed extraction is retried next time.
    with tempfile.TemporaryDirectory(dir=destination.parent, prefix=f".{destination.name}.") as directory:
        with tarfile.open(archive_path) as archive:
            archive.extractall(directory, filter="data")
        tree = Path(directory, tree_name or destination.name)
        if not tree.is_dir():
            raise SystemExit(f"{archive_path} does not contain {tree.name}")
        for patch in patches:
            subprocess.run(["patch", "--batch", "--forward", "--fuzz=0", "-p1", "-i", str(patch)],
                           cwd=tree, check=True)
        tree.replace(destination)


def main():
    arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(platform.machine())
    if not arch or platform.system() != "Linux":
        raise SystemExit("Bootstrap supports x86_64 and aarch64 Linux. Set GHOSTTY_PREFIX for another platform.")
    DEPS.mkdir(exist_ok=True)

    # Keep patched sources separate from pristine or older patched caches.
    patch_hash = hashlib.sha256(b"".join(patch.read_bytes() for patch in PATCHES)).hexdigest()
    build_id = hashlib.sha256(Path(__file__).read_bytes() + patch_hash.encode() + arch.encode()).hexdigest()
    stamp = DEPS / "ghostty/.bootstrap-id"
    if (stamp.is_file() and stamp.read_text() == build_id
            and (DEPS / "ghostty/include/ghostty/vt/terminal.h").is_file()
            and (DEPS / "ghostty/lib/libghostty-vt.so").is_file()):
        print(f"libghostty-vt is ready in {DEPS / 'ghostty'}")
        return
    stamp.unlink(missing_ok=True)

    source_archive = DEPS / "ghostty.tar.gz"
    download(f"https://codeload.github.com/ghostty-org/ghostty/tar.gz/{REVISION}", source_archive, SOURCE_SHA256)
    source = DEPS / f"ghostty-{REVISION}-{patch_hash[:12]}"
    if not source.is_dir():
        extract(source_archive, source, tree_name=f"ghostty-{REVISION}", patches=PATCHES)

    zig_dir = DEPS / f"zig-{arch}-linux-{ZIG_VERSION}"
    zig = zig_dir / "zig"
    if not zig_dir.is_dir():
        index = subprocess.check_output(["curl", "-fsSL", "https://ziglang.org/download/index.json"])
        release = json.loads(index)[ZIG_VERSION][f"{arch}-linux"]
        zig_archive = DEPS / "zig.tar.xz"
        download(release["tarball"], zig_archive, release["shasum"])
        extract(zig_archive, zig_dir)

    # curl honors HTTPS_PROXY in environments where Zig's downloader cannot
    # resolve external hosts. Zig still verifies each dependency's package hash.
    # Cached installations must run on other CPUs of the same architecture.
    command = [str(zig), "build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "-Dcpu=baseline", "--prefix", str(DEPS / "ghostty"), "-j4"]
    fetched = set()
    for _ in range(50):
        print("Building libghostty-vt...", flush=True)
        result = subprocess.run(command, cwd=source, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode == 0:
            stamp.write_text(build_id)
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
