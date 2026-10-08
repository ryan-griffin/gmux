#!/usr/bin/env bash
# Run inside the official Fedora 44 container, including local validation.
set -euo pipefail
mkdir -p artifacts
exec > >(tee artifacts/packages.log) 2>&1
dnf --assumeyes --setopt=install_weak_deps=False install \
    gcc gcc-c++ clang clang-libs pkgconf-pkg-config gtk4-devel libadwaita-devel \
    glib2-devel curl git tar gzip zstd rustup python3 python3-gobject gdk-pixbuf2 librsvg2
cat /etc/os-release
dnf repolist --enabled
rpm -qa | sort > artifacts/packages.txt
rpm -q gtk4 gtk4-devel libadwaita libadwaita-devel clang clang-libs rustup \
    python3 python3-gobject gdk-pixbuf2 librsvg2
pkg-config --atleast-version=4.22 gtk4
pkg-config --atleast-version=1.9 libadwaita-1
python3 - <<'PY'
import ctypes
import ctypes.util
from pathlib import Path
import subprocess
import sys
assert sys.version_info >= (3, 12)
clang = ctypes.util.find_library("clang")
assert clang, "libclang is not discoverable"
ctypes.CDLL(clang)
print(f"Python {sys.version}; libclang {clang}")
resource = subprocess.check_output(["clang", "-print-resource-dir"], text=True).strip()
assert Path(resource, "include/stdbool.h").is_file(), resource
subprocess.run(["clang", "-x", "c", "-fsyntax-only", "-"],
               input="#include <stdbool.h>\n", text=True, check=True)
print(f"Clang resource headers: {resource}")
import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf
assert any("svg" in f.get_extensions() for f in GdkPixbuf.Pixbuf.get_formats())
PY
