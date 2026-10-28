#!/usr/bin/env python3
"""Run Rust checks and smoke tests. Add --native for the isolated Wayland suite."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

HEADLESS = ["serve", "daemon", "ownership", "remote", "git", "multi-client", "frontend-input", "scrollback", "scrollback-limits", "compression", "progress", "hyperlinks", "clipboard", "shell", "program-status", "text-blink"]
DESKTOP = ["local-execution", "smoke", "multi-window", "remote-window", "remotes-ui", "terminal-input", "notifications", "renderer", "progress-ui", "program-status-ui", "clipboard-ui"]

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--native", action="store_true")
parser.add_argument("--headless", action="store_true", help="Check the GTK-free daemon and CLI build")
parser.add_argument("--skip-build", action="store_true")
parser.add_argument("--skip-smoke", action="store_true", help="Run Rust, build, and linkage checks without smoke suites")
parser.add_argument("--only", nargs="+", help="Smoke-test names without the -smoke-test.py suffix")
parser.add_argument("--list", action="store_true", help="List smoke tests without building or starting a compositor")
parser.add_argument("--build-timeout", type=int, default=1200,
                    help="Deadline in seconds for each Cargo command (default: 1200)")
parser.add_argument("--smoke-timeout", type=int, default=300,
                    help="Deadline in seconds for each smoke suite (default: 300)")
args = parser.parse_args()
if args.build_timeout <= 0 or args.smoke_timeout <= 0:
    parser.error("Timeouts must be positive numbers of seconds.")
if args.headless and args.native:
    parser.error("--headless excludes the GTK frontend and cannot be combined with --native.")
if args.skip_smoke and (args.only or args.native or args.skip_build):
    parser.error("--skip-smoke cannot be combined with --only, --native, or --skip-build.")
root = Path(__file__).resolve().parent.parent

if args.list:
    for category, names in (("headless", HEADLESS), ("desktop", DESKTOP)):
        for name in names:
            print(f"{name} ({category})")
    sys.exit(0)

names = [] if args.skip_smoke else list(dict.fromkeys(args.only or (HEADLESS + DESKTOP if args.native else HEADLESS)))
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


def run(command, timeout):
    print("Running " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, timeout=timeout, check=True)


from cargo_build import build_artifacts, compiler_artifacts
from cargo_test_support import ARTIFACT_ENV
# Never import an artifact selection from a previous invocation.
os.environ.pop(ARTIFACT_ENV, None)


if args.native:
    from wayland_test_support import ensure_wayland_session
    ensure_wayland_session()

artifact_log = root / "artifacts" / ("cargo-build-headless.jsonl" if args.headless else "cargo-build.jsonl")

if not args.skip_build:
    cargo_options = ["-p", "gmux-core", "-p", "gmux-client", "-p", "gmux"] if args.headless else ["--workspace"]
    run(["cargo", "fmt", "--all", "--check"], args.build_timeout)
    client_dependencies = subprocess.check_output(
        ["cargo", "tree", "--locked", "-p", "gmux-client", "--edges", "normal,build", "--prefix", "none"],
        cwd=root, text=True, timeout=args.build_timeout)
    included = {line.split()[0] for line in client_dependencies.splitlines() if line.strip()}
    forbidden = {"gmux-core", "gtk4", "libadwaita", "portable-pty", "bindgen", "cc", "glib"}
    assert not included & forbidden, "Client library includes execution or GUI dependencies: " + str(included & forbidden)
    if args.headless:
        dependencies = subprocess.check_output(
            ["cargo", "tree", "--locked", *cargo_options, "--edges", "normal,build", "--prefix", "none"],
            cwd=root, text=True, timeout=args.build_timeout)
        forbidden = {"gtk4", "gtk4-sys", "libadwaita", "libadwaita-sys", "glib", "glib-sys", "gobject-sys", "gio-sys"}
        included = {line.split()[0] for line in dependencies.splitlines() if line.strip()}
        assert not included & forbidden, "Headless build includes frontend dependencies: " + str(included & forbidden)
    run(["cargo", "test", "--locked", *cargo_options], args.build_timeout)
    run(["cargo", "clippy", "--locked", "--all-targets", *cargo_options, "--", "-D", "warnings"], args.build_timeout)
    artifacts = build_artifacts(examples=True, headless=args.headless, timeout=args.build_timeout,
                                log_path=artifact_log)
    os.environ[ARTIFACT_ENV] = json.dumps(artifacts)
else:
    if not artifact_log.is_file():
        parser.error("--skip-build needs a previous scripts/check.py build with the same --headless setting.")
    os.environ[ARTIFACT_ENV] = json.dumps(compiler_artifacts(artifact_log.read_text()))
from cargo_test_support import cargo_binary
linkage = subprocess.check_output(["ldd", str(cargo_binary("gmux"))], text=True,
                                   timeout=args.build_timeout)
assert "not found" not in linkage, linkage
assert not any(library in linkage for library in ("libgtk", "libadwaita", "libglib", "libgobject", "libgio")), linkage
if not args.headless:
    client_linkage = subprocess.check_output(["ldd", str(cargo_binary("gmux-gtk"))], text=True,
                                            timeout=args.build_timeout)
    assert "libghostty" not in client_linkage, client_linkage
help_text = subprocess.check_output([str(cargo_binary("gmux")), "--help"], text=True)
assert all(command not in help_text for command in ("workspace", "screenshot", "focus", "move")), help_text
print("The standalone daemon and its management CLI exclude GTK and client organization commands.", flush=True)
for name, file in zip(names, files):
    command = [sys.executable, str(file)]
    if args.native and name in ("smoke", "daemon"):
        command.append("--native")
    if args.headless and name == "daemon":
        command.append("--headless")
    run(command, args.smoke_timeout)
print("Passed smoke suites: " + ", ".join(names) + "." if names else "Smoke suites skipped (--skip-smoke).")
print("Rust checks skipped (--skip-build)." if args.skip_build else "Rust checks passed.")
if not args.skip_smoke and not any(name in DESKTOP or (args.native and name == "daemon") for name in names):
    print("Native desktop suites were not selected; use --native to run them.")
