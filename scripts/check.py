#!/usr/bin/env python3
"""Run Rust checks and smoke tests. Add --native for the isolated Wayland suite."""
import argparse
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--native", action="store_true")
parser.add_argument("--skip-build", action="store_true")
parser.add_argument("--only", nargs="+", help="Smoke-test names without the -smoke-test.py suffix")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent


def run(command):
    print("Running " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, check=True, timeout=300)


if args.native:
    from wayland_test_support import ensure_wayland_session
    ensure_wayland_session()

if not args.skip_build:
    run(["cargo", "fmt", "--check"])
    run(["cargo", "test", "--locked", "--all-targets"])
    run(["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"])
    run(["cargo", "build", "--locked", "--bins", "--examples"])
if args.native:
    names = ["daemon", "control", "icon", "state-recovery", "smoke", "audit", "workspace", "sidebar-search", "branch", "title", "scheduling", "resize", "font",
             "find", "commands", "main-menu", "terminal-context-menu", "terminal-scrollbar",
             "terminal-renderer", "terminal-input", "notifications", "notification-focus", "renderer"]
else:
    names = ["daemon", "control", "icon"]
for name in args.only or names:
    file = "smoke-test.py" if name == "smoke" else f"{name}-smoke-test.py"
    assert (root / "scripts" / file).is_file(), f"Unknown test: {name}"
    command = [sys.executable, str(root / "scripts" / file)]
    if args.native and name in ("smoke", "daemon"):
        command.append("--native")
    run(command)
print("All requested checks passed.")
