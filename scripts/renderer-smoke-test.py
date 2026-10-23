#!/usr/bin/env python3
"""Check terminal GSK rendering, independent text agreement, and retained rows.

Starts the private Wayland fixture after building examples.
Use GSK_RENDERER=vulkan, gl, or cairo to check GTK's different backends.
Requires gtk4-rendernode-tool.
"""
import argparse
import json
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import re
import subprocess
import time
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, help="renderer-benchmark binary")
parser.add_argument("--check-binary", type=Path)
parser.add_argument("--output", type=Path)
parser.add_argument("--selection", action="store_true", help="Measure changing linear and rectangular selections")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
binary = args.binary or cargo_binary("renderer-benchmark", example=True)
check_binary = args.check_binary or cargo_binary("renderer-check", example=True)
output = args.output or root / "artifacts/renderer-validation"
output.mkdir(parents=True, exist_ok=True)
render_env = dict(os.environ, GTK_A11Y="none")
activation = output / "checks/cursor-activation.json"
activation.unlink(missing_ok=True)
activated = output / "checks/cursor-activated"
activated.unlink(missing_ok=True)
with (output / "check.log").open("w") as log, WaylandInput() as native:
    process = subprocess.Popen([str(check_binary), "--cursor-lifecycle", "--output", str(output / "checks")],
                               stdout=log, stderr=log, env=render_env)
    try:
        serial = 0
        deadline = time.monotonic() + 60
        while process.poll() is None:
            assert time.monotonic() < deadline, "Renderer check timed out"
            if activation.is_file():
                request = json.loads(activation.read_text())
                if request["serial"] != serial:
                    native.click_color(tuple(request["color"]))
                    serial = request["serial"]
                    activated.write_text(str(serial))
            time.sleep(.005)
        assert process.returncode == 0, f"Renderer check failed; see {output / 'check.log'}"
    except BaseException:
        print("Cursor activation request:", activation.read_text() if activation.exists() else "absent", file=log)
        frame = native.frame(fresh=False)
        import gi
        gi.require_version("GdkPixbuf", "2.0")
        from gi.repository import GdkPixbuf, GLib
        GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(frame.data), GdkPixbuf.Colorspace.RGB,
            True, 8, frame.width, frame.height, frame.stride).savev(
                str(output / "cursor-failure.png"), "png", [], [])
        raise
    finally:
        if process.poll() is None:
            process.kill()
        process.wait()
with (output / "benchmark.log").open("w") as log:
    subprocess.run([str(binary), "--frames", "10", "--cols", "80", "--rows", "24",
                    "--output", str(output), *(["--selection"] if args.selection else [])],
                   check=True, stdout=log, stderr=log, env=render_env)

checks = json.loads((output / "checks/checks.json").read_text())
report = json.loads((output / "results.json").read_text())
assert checks["cursor_lifecycle"], checks
assert checks["gtk_renderer"] == report["gtk_renderer"]
assert checks["cache_renderer"] == "GskCairoRenderer", checks
assert len(checks["glyph_cases"]) == 16
for result in report["results"]:
    assert result["terminal_renderer"] == "gsk"
    prefix = f"{result['fixture']}-{result['workload']}-gsk"
    info = subprocess.check_output(
        ["gtk4-rendernode-tool", "info", str(output / f"{prefix}.node")], text=True)
    assert not re.search(r"^\s+cairo:\s+[1-9]", info, re.MULTILINE), info
    assert re.search(r"^\s+text:\s+[1-9]", info, re.MULTILINE), info
    rows = dict(static=0, **{"single-row": 1, "full-screen": 1, "all-rows": report["rows"], "cursor-blink": 1})
    if result["workload"] == "rectangle-selection":
        assert result["rebuilt_rows"] == result["expected_rebuilt_rows"], result
        assert 0 < result["rebuilt_rows"] <= report["frames"] * report["rows"], result
    else:
        rows["linear-selection"] = 1
        assert result["rebuilt_rows"] == report["frames"] * rows[result["workload"]], result
    assert result["cached_shapes"] <= 4096, result
    assert result["cached_shape_bytes"] <= 1024 * 1024, result

print(f"PASS: {report['gtk_renderer']}, {len(checks['glyph_cases'])} independent glyph comparisons, "
      + ", ".join(checks["checks"]) + f", cache oracle {checks['cache_renderer']}"
      + ", direct text nodes without Cairo nodes")
