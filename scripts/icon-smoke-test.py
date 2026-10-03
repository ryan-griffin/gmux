#!/usr/bin/env python3
"""Check app icons through GdkPixbuf's format detection and SVG loader.

Run with python3 scripts/icon-smoke-test.py. Requires PyGObject and the
GdkPixbuf SVG loader, but no display or running application.
"""
import argparse
from pathlib import Path

import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--data-dir", type=Path,
                    default=Path(__file__).resolve().parent.parent / "data")
args = parser.parse_args()

for name in ["dev.gmux.Gmux.svg", "dev.gmux.Gmux-symbolic.svg"]:
    path = args.data_dir / name
    for size in [16, 32, 64, 128, 256]:
        # Specifying a loader type would bypass the format detection that
        # rejected the app icon when a long comment preceded its SVG header.
        pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(str(path), size, size, True)
        assert (pixbuf.get_width(), pixbuf.get_height()) == (size, size), path
        if pixbuf.get_has_alpha():
            assert any(pixbuf.get_pixels()[3::4]), f"{path} is fully transparent"
    print(f"PASS: {name} loads and renders at 16, 32, 64, 128, and 256px")
