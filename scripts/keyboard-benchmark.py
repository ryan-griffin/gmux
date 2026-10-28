#!/usr/bin/env python3
"""Measure the C encoder bridge and production GDK metadata lookup in a private Wayland display."""
import argparse
import hashlib
import platform
import json
import os
from pathlib import Path
import statistics
import subprocess

from cargo_build import compiler_artifacts
from wayland_test_support import ensure_wayland_session

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--baseline-source", type=Path, help="Original ghostty_bridge.c, with its matching header alongside it")
parser.add_argument("--metadata-binary", type=Path, help="Use an already-built release keyboard-benchmark example")
parser.add_argument("--samples", type=int, default=7)
args = parser.parse_args()
assert args.samples > 0
ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty")).resolve()
if args.metadata_binary:
    metadata = args.metadata_binary.resolve()
else:
    result = subprocess.check_output(["cargo", "build", "--locked", "--release", "-p", "gmux-gtk", "--example", "keyboard-benchmark", "--message-format=json-render-diagnostics"], cwd=root, text=True)
    metadata = Path(compiler_artifacts(result, profile="release")["release/example/keyboard-benchmark"])

binaries = {}
for label, source in [("current", root / "src/ghostty_bridge.c"), ("baseline", args.baseline_source)]:
    if source is None:
        continue
    binary = artifacts / f"keyboard-bridge-{label}"
    command = ["cc", "-O3", "-std=c11", "-I", str(prefix / "include"), "-I", str(root / "src"),
               '-DGMUX_BRIDGE_SOURCE="' + str(source.resolve()) + '"', str(root / "scripts/keyboard-benchmark.c"),
               "-L", str(prefix / "lib"), "-Wl,-rpath," + str(prefix / "lib"), "-lghostty-vt", "-o", str(binary)]
    if label == "baseline": command.append("-DGMUX_KEYBOARD_BASELINE")
    subprocess.run(command, check=True)
    binaries[label] = binary
samples = []
for _ in range(args.samples):
    for label, binary in binaries.items():
        for row in subprocess.check_output([str(binary)], text=True).splitlines():
            samples.append(dict(kind="bridge", revision=label, **json.loads(row)))

for _ in range(args.samples):
    samples.append(dict(kind="metadata", **json.loads(subprocess.check_output([str(metadata)], text=True))))
summary = []
for key in sorted({(r["kind"], r.get("revision", r.get("backend")), r.get("mode", "")) for r in samples}):
    rows = [r for r in samples if (r["kind"], r.get("revision", r.get("backend")), r.get("mode", "")) == key]
    summary.append(dict(kind=key[0], implementation=key[1], mode=key[2],
                        median_cpu_ns=statistics.median(r["cpu_ns_per_key"] for r in rows),
                        median_wall_ns=statistics.median(r["wall_ns_per_key"] for r in rows),
                        maximum_allocations=max(r.get("native_key_allocations", r.get("rust_allocations", 0)) for r in rows)))
report = dict(samples=samples, summary=summary, machine=platform.machine(),
              cpu=next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown"),
              gtk=subprocess.check_output(["pkg-config", "--modversion", "gtk4"], text=True).strip(),
              bridge_source_sha256=hashlib.sha256((root / "src/ghostty_bridge.c").read_bytes()).hexdigest(),
              baseline_source_sha256=hashlib.sha256(args.baseline_source.read_bytes()).hexdigest() if args.baseline_source else None,
              metadata_binary_sha256=hashlib.sha256(metadata.read_bytes()).hexdigest())
(artifacts / "keyboard-benchmark.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(summary, indent=2))
