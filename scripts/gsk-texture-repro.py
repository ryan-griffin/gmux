#!/usr/bin/env python3
"""Build and run the GTK/Pango texture probe in the private Wayland fixture."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess

from wayland_test_support import ensure_wayland_session

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "scripts/gsk-texture-repro.c"


def bounded_count(value):
    count = int(value)
    if not 1 <= count <= 1000:
        raise argparse.ArgumentTypeError("Expected a count between 1 and 1000")
    return count


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inventory(command):
    if not shutil.which(command[0]):
        return {"command": command, "available": False}
    result = subprocess.run(command, capture_output=True, text=True, timeout=30)
    return {"command": command, "exit": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=("vulkan", "gl", "cairo"), default="vulkan")
    parser.add_argument("--runs", type=bounded_count, default=20,
                        help="Fresh processes in a fixed measurement group; failed runs are never retried")
    parser.add_argument("--sequence", choices=("full", "scroll", "fixed"), default="fixed")
    parser.add_argument("--rounds", type=bounded_count, default=100)
    parser.add_argument("--keep-node", action="store_true")
    parser.add_argument("--same-node", action="store_true")
    parser.add_argument("--no-prelude", action="store_true")
    parser.add_argument("--no-context", action="store_true")
    parser.add_argument("--no-churn", action="store_true")
    parser.add_argument("--output", type=Path,
                        default=ROOT / "artifacts/gsk-texture-repro")
    args = parser.parse_args()
    if args.keep_node and args.sequence != "fixed":
        parser.error("--keep-node requires --sequence fixed")
    ensure_wayland_session()
    output = args.output.resolve()
    # Do not erase a previous failure when measuring another group.
    output.mkdir(parents=True, exist_ok=False)
    binary = output / "gsk-texture-repro"
    flags = shlex.split(subprocess.check_output(
        ["pkg-config", "--cflags", "--libs", "gtk4"], text=True))
    build = [*shlex.split(os.environ.get("CC", "cc")), "-Wall", "-Wextra", "-Werror", "-O0", "-g",
             str(SOURCE), "-o", str(binary), *flags, "-lm"]
    with (output / "build.log").open("w") as log:
        subprocess.run(build, stdout=log, stderr=log, check=True, timeout=60)
    # Capture inherited switches as well as the selected backend. Inventory
    # commands run in the private session and do not modify packages/settings.
    env = dict(os.environ, GSK_RENDERER=args.backend, GTK_A11Y="none")
    prefixes = ("GSK_", "GDK_", "GTK_", "MESA_", "VK_", "LIBGL_", "PANGO_", "FONTCONFIG_")
    metadata = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "options": {**vars(args), "output": str(output)},
        "source_sha256": sha256(SOURCE), "binary_sha256": sha256(binary), "build": build,
        "environment": {key: value for key, value in env.items() if key.startswith(prefixes)},
        "inventory": [inventory(command) for command in (
            ["git", "-C", str(ROOT), "rev-parse", "HEAD"], ["uname", "-a"],
            ["pkg-config", "--modversion", "gtk4", "pango", "glib-2.0", "cairo", "fontconfig"],
            ["mutter", "--version"], ["vulkaninfo", "--summary"],
            ["lspci", "-nnk", "-d", "::03xx"],
            ["fc-match", "Monospace"], ["fc-match", "emoji"],
        )],
    }
    (output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    switches = [f"--{name}" for name in ("keep-node", "same-node", "no-prelude", "no-context", "no-churn")
                if getattr(args, name.replace("-", "_"))]
    expected_renderer = {"vulkan": "GskVulkanRenderer", "gl": "GskGLRenderer", "cairo": "GskCairoRenderer"}[args.backend]
    runs = []
    for index in range(1, args.runs + 1):
        directory = output / f"run-{index:03d}"
        directory.mkdir()
        command = [str(binary), "--output", str(directory), "--sequence", args.sequence,
                   "--rounds", str(args.rounds), *switches]
        timeout = False
        with (directory / "run.log").open("w") as log:
            try:
                process = subprocess.run(command, env=env, stdout=log, stderr=log, timeout=120)
                exit_code = process.returncode
            except subprocess.TimeoutExpired:
                timeout = True
                exit_code = None
        report = json.loads((directory / "result.json").read_text()) if (directory / "result.json").exists() else None
        mismatch = json.loads((directory / "mismatch.json").read_text()) if (directory / "mismatch.json").exists() else None
        valid_backend = bool(report and report["gtk_renderer"] == expected_renderer and
                             report["gdk_display"] == "GdkWaylandDisplay")
        passed = exit_code == 0 and valid_backend and report["equal"]
        runs.append({"run": index, "exit": exit_code, "timeout": timeout, "passed": passed,
                     "valid_backend": valid_backend, "result": report, "mismatch": mismatch})
        summary = {"runs": runs, "requested_runs": args.runs, "completed_runs": len(runs),
                   "failed_runs": sum(not run["passed"] for run in runs),
                   "mismatches": sum(run["mismatch"] is not None for run in runs),
                   "complete": len(runs) == args.runs}
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"{args.backend} {args.sequence} run {index}/{args.runs}: "
              f"{'PASS' if passed else 'FAIL'} exit={exit_code}", flush=True)
        if not passed:
            print((directory / "run.log").read_text()[-2000:], flush=True)
    print(f"{summary['failed_runs']}/{args.runs} failed; {output}/summary.json", flush=True)
    return int(summary["failed_runs"] != 0)


if __name__ == "__main__":
    raise SystemExit(main())
