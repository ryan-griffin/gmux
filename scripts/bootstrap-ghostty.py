#!/usr/bin/env python3
"""Build the pinned libghostty-vt locally. Never changes system packages."""
import hashlib
import os
from pathlib import Path
import platform
import re
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEPS = ROOT / ".deps"
# Ghostty v1.4.0, peeled release tag. Never use the moving tag as a cache key.
REVISION = "83fb95624b0439ee660c2679b43b38c8b63769cd"
SOURCE_SHA256 = "7759929eadaddf602d6fd5ad6b3403d854b91b7083975cd984bb525bc8275187"
# Must match the source's build.zig.zon and src/build/zig.zig requirement.
ZIG_VERSION = "0.17.0"
ZIG_SHA256 = {
    "x86_64": "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026",
    "aarch64": "9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8",
}
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


def fetch_dependency(zig, source, archive, env):
    # Fetch an extracted package root. Zig 0.17 can retain the archive's wrapper
    # directory when repacking a local tarball into its global package cache.
    # A directory fetch keeps the cached layout identical to the hashed source.
    with tempfile.TemporaryDirectory(dir=DEPS, prefix=".fetch-") as directory:
        with tarfile.open(archive) as package:
            package.extractall(directory, filter="data")
        entries = list(Path(directory).iterdir())
        package_root = entries[0] if len(entries) == 1 and entries[0].is_dir() else Path(directory)
        subprocess.run([str(zig), "fetch", str(package_root)], cwd=source, env=env, check=True)


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

    source_archive = DEPS / f"ghostty-{REVISION}.tar.gz"
    download(f"https://codeload.github.com/ghostty-org/ghostty/tar.gz/{REVISION}", source_archive, SOURCE_SHA256)
    source = DEPS / f"ghostty-{REVISION}-{patch_hash[:12]}"
    if not source.is_dir():
        extract(source_archive, source, tree_name=f"ghostty-{REVISION}", patches=PATCHES)

    zig_dir = DEPS / f"zig-{arch}-linux-{ZIG_VERSION}"
    zig = zig_dir / "zig"
    if not zig_dir.is_dir():
        zig_archive = DEPS / f"{zig_dir.name}.tar.xz"
        download(f"https://ziglang.org/download/{ZIG_VERSION}/{zig_archive.name}",
                 zig_archive, ZIG_SHA256[arch])
        extract(zig_archive, zig_dir)

    # curl honors HTTPS_PROXY in environments where Zig's downloader cannot
    # resolve external hosts. Zig still verifies each dependency's package hash.
    # Keep compiler artifacts and downloaded packages in this worktree even
    # when the caller exports cache paths for a different checkout.
    env = dict(os.environ, ZIG_GLOBAL_CACHE_DIR=str(DEPS / "zig-cache"),
               ZIG_LOCAL_CACHE_DIR=str(source / ".zig-cache"))
    # Cached installations must run on other CPUs of the same architecture.
    command = [str(zig), "build", "-Demit-lib-vt=true", "-Dsimd=true", "-Doptimize=ReleaseFast", "-Dcpu=baseline", "--prefix", str(DEPS / "ghostty"), "-j4"]
    fetched = set()
    for _ in range(50):
        print("Building libghostty-vt...", flush=True)
        result = subprocess.run(command, cwd=source, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
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
            fetch_dependency(zig, source, archive, env)
    else:
        raise SystemExit("Dependency fetch limit reached")


if __name__ == "__main__":
    main()
