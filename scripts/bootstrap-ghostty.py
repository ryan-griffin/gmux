#!/usr/bin/env python3
"""Build the pinned libghostty-vt locally. Never changes system packages."""
import hashlib
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEPS = ROOT / ".deps"
VERSION = "1.0.0"
# Peeled vt-1.0.0 tag. Checkpoints require this exact source revision.
REVISION = "0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec"
SOURCE_SHA256 = "4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676"
DEPS_SHA256 = "4d5ea5e448e4c7ea8fb55ccbaa8668f2a97d44f438cee831e2b7ecd162d85c29"
# Must match the source's build.zig.zon and src/build/zig.zig requirement.
ZIG_VERSION = "0.17.0"
ZIG_SHA256 = {
    "x86_64": "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026",
    "aarch64": "9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8",
}
PATCHES = [ROOT / "scripts/patches/ghostty-media-keys.patch",
           ROOT / "scripts/patches/ghostty-clipboard-reads.patch",
           ROOT / "scripts/patches/ghostty-snapshot-state.patch"]


def download(url, destination, checksum):
    def valid(path):
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
            and (DEPS / "ghostty/lib/libghostty-vt.so").is_file()
            and (DEPS / "ghostty/lib/libghostty-vt.so.1").is_file()):
        print(f"libghostty-vt is ready in {DEPS / 'ghostty'}")
        return
    stamp.unlink(missing_ok=True)

    release_url = f"https://release.files.ghostty.org/libghostty-vt/{VERSION}"
    source_name = f"libghostty-vt-{VERSION}"
    source_archive = DEPS / f"{source_name}.tar.gz"
    download(f"{release_url}/{source_archive.name}", source_archive, SOURCE_SHA256)
    source = DEPS / f"{source_name}-{patch_hash[:12]}"
    if not source.is_dir():
        extract(source_archive, source, tree_name=source_name, patches=PATCHES)

    dependencies = DEPS / f"{source_name}-deps"
    dependency_archive = DEPS / f"{dependencies.name}.tar.gz"
    download(f"{release_url}/{dependency_archive.name}", dependency_archive, DEPS_SHA256)
    if not dependencies.is_dir():
        extract(dependency_archive, dependencies)

    zig_dir = DEPS / f"zig-{arch}-linux-{ZIG_VERSION}"
    zig = zig_dir / "zig"
    if not zig_dir.is_dir():
        zig_archive = DEPS / f"{zig_dir.name}.tar.xz"
        download(f"https://ziglang.org/download/{ZIG_VERSION}/{zig_archive.name}",
                 zig_archive, ZIG_SHA256[arch])
        extract(zig_archive, zig_dir)

    # --system resolves every package from the release dependency bundle without
    # network access. Keep this compiler cache separate from older Zig fetch caches:
    # Zig 0.17 can fail when its global cache contains package archives.
    env = dict(os.environ, ZIG_GLOBAL_CACHE_DIR=str(DEPS / "zig-build-cache"),
               ZIG_LOCAL_CACHE_DIR=str(source / ".zig-cache"))
    # Cached installations must run on other CPUs of the same architecture.
    command = [str(zig), "build", "--system", str(dependencies), "-Dsimd=true",
               "-Doptimize=ReleaseFast", "-Dcpu=baseline", "--prefix", str(DEPS / "ghostty"), "-j4"]
    print(f"Building libghostty-vt {VERSION}...", flush=True)
    subprocess.run(command, cwd=source, env=env, check=True)
    stamp.write_text(build_id)
    print(f"libghostty-vt is ready in {DEPS / 'ghostty'}")


if __name__ == "__main__":
    main()
