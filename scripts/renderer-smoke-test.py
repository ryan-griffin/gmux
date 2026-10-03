#!/usr/bin/env python3
"""Check terminal GSK rendering, independent text agreement, and retained rows.

Run through dbus-run-session on a native display after building examples.
Use GSK_RENDERER=vulkan, gl, or cairo to check GTK's different backends.
Requires gtk4-rendernode-tool.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, help="renderer-benchmark binary")
parser.add_argument("--check-binary", type=Path)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
binary = args.binary or root / "target/debug/examples/renderer-benchmark"
check_binary = args.check_binary or root / "target/debug/examples/renderer-check"
output = args.output or root / "artifacts/renderer-validation"
output.mkdir(parents=True, exist_ok=True)
with (output / "check.log").open("w") as log:
    subprocess.run([str(check_binary), "--output", str(output / "checks")],
                   check=True, stdout=log, stderr=log)
with (output / "benchmark.log").open("w") as log:
    subprocess.run([str(binary), "--frames", "10", "--cols", "80", "--rows", "24",
                    "--output", str(output)], check=True, stdout=log, stderr=log)

checks = json.loads((output / "checks/checks.json").read_text())
report = json.loads((output / "results.json").read_text())
assert checks["gtk_renderer"] == report["gtk_renderer"]
assert len(checks["glyph_cases"]) == 16
for result in report["results"]:
    assert result["terminal_renderer"] == "gsk"
    prefix = f"{result['fixture']}-{result['workload']}-gsk"
    info = subprocess.check_output(
        ["gtk4-rendernode-tool", "info", str(output / f"{prefix}.node")], text=True)
    assert not re.search(r"^\s+cairo:\s+[1-9]", info, re.MULTILINE), info
    assert re.search(r"^\s+text:\s+[1-9]", info, re.MULTILINE), info
    rows = dict(static=0, **{"single-row": 1, "full-screen": 1})
    assert result["rebuilt_rows"] == report["frames"] * rows[result["workload"]], result
    assert result["cached_glyphs"] <= 4096, result

print(f"PASS: {report['gtk_renderer']}, {len(checks['glyph_cases'])} independent glyph comparisons, "
      + ", ".join(checks["checks"]) + ", direct text nodes without Cairo nodes")
