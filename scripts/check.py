#!/usr/bin/env python3
"""Run Rust checks and smoke tests. Add --native for the isolated Wayland suite."""
import argparse
from pathlib import Path
import subprocess
import sys

HEADLESS = ["bootstrap", "daemon", "control", "icon"]
DESKTOP = ["state-recovery", "smoke", "audit", "workspace", "sidebar-search", "branch",
           "title", "scheduling", "resize", "font", "find", "commands", "main-menu", "window-close",
           "terminal-context-menu", "terminal-scrollbar", "terminal-renderer",
           "terminal-input", "notifications", "notification-focus", "notification-delivery", "renderer"]

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--native", action="store_true")
parser.add_argument("--skip-build", action="store_true")
parser.add_argument("--only", nargs="+", help="Smoke-test names without the -smoke-test.py suffix")
parser.add_argument("--list", action="store_true", help="List smoke tests without building or starting a compositor")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent

if args.list:
    for category, names in (("headless", HEADLESS), ("desktop", DESKTOP)):
        for name in names:
            print(f"{name} ({category})")
    sys.exit(0)

names = list(dict.fromkeys(args.only or (HEADLESS + DESKTOP if args.native else HEADLESS)))
unknown = [name for name in names if name not in HEADLESS + DESKTOP]
if unknown:
    parser.error("Unknown smoke tests: " + ", ".join(unknown) + ". Use --list for available tests.")
if not args.native and any(name in DESKTOP for name in names):
    parser.error("Desktop smoke tests require --native for the isolated Wayland fixture.")
files = [root / "scripts" / ("smoke-test.py" if name == "smoke" else f"{name}-smoke-test.py")
         for name in names]
missing = [str(file) for file in files if not file.is_file()]
if missing:
    parser.error("Missing smoke tests: " + ", ".join(missing))


def run(command):
    print("Running " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, check=True, timeout=300)


if args.native:
    from wayland_test_support import ensure_wayland_session
    ensure_wayland_session()

if not args.skip_build:
    run(["cargo", "fmt", "--check"])
    run(["cargo", "test", "--locked"])
    run(["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"])
    run(["cargo", "build", "--locked", "--bins", "--examples"])
for name, file in zip(names, files):
    command = [sys.executable, str(file)]
    if args.native and name in ("smoke", "daemon"):
        command.append("--native")
    run(command)
print("All requested checks passed.")
