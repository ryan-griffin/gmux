#!/usr/bin/env python3
"""Build binaries and select executable paths reported by that Cargo invocation."""
import argparse
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent


def compiler_artifacts(output, *, profile="debug"):
    """Read executable paths from cargo build --message-format=json output."""
    artifacts = {}
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        if message["profile"]["test"]:
            continue
        target = message["target"]
        for kind in ("bin", "example"):
            if kind not in target["kind"]:
                continue
            key = f"{profile}/{kind}/{target['name']}"
            executable = message["executable"]
            if key in artifacts and artifacts[key] != executable:
                raise SystemExit(f"Cargo reported multiple executables for {key}; "
                                 "select a single native target or explicit binary paths.")
            artifacts[key] = executable
    return artifacts


def build_artifacts(profile="debug", *, target=None, examples=False, headless=False,
                    timeout=None, log_path=None):
    command = ["cargo", "build", "--locked", "--bins", "--message-format=json-render-diagnostics"]
    if examples:
        command.append("--examples")
    if profile == "release":
        command.append("--release")
    if target:
        command.extend(["--target", target])
    # Build the daemon independently in both installation modes. GTK enables
    # additional features on shared dependencies; building it alongside the
    # daemon would otherwise produce a different daemon artifact.
    selections = [["-p", "gmux-core", "-p", "gmux"]]
    if not headless:
        selections.append(["-p", "gmux-gtk"])
    output = ""
    if log_path is not None:
        log_path.parent.mkdir(parents=True, exist_ok=True)
    for index, selection in enumerate(selections):
        invocation = command + selection
        print("Running " + " ".join(invocation), file=sys.stderr, flush=True)
        if log_path is None:
            result = subprocess.run(invocation, cwd=ROOT, check=True, timeout=timeout,
                                    text=True, stdout=subprocess.PIPE)
            output += result.stdout
        else:
            # Keep partial output for diagnostics when Cargo fails or times out.
            with log_path.open("a" if index else "w") as log:
                subprocess.run(invocation, cwd=ROOT, check=True, timeout=timeout, stdout=log)
    if log_path is not None:
        output = log_path.read_text()
    return compiler_artifacts(output, profile=profile)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--headless", action="store_true", help="Build the daemon and CLI without GTK")
    parser.add_argument("--target", help="Build for an explicit Cargo target")
    parser.add_argument("binaries", nargs="+", help="Binary names, in output order")
    args = parser.parse_args()
    profile = "release" if args.release else "debug"
    try:
        artifacts = build_artifacts(profile, target=args.target, headless=args.headless)
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode) from None
    keys = [f"{profile}/bin/{name}" for name in args.binaries]
    for key in keys:
        if key not in artifacts:
            raise SystemExit(f"Cargo did not report {key} in this build; refusing to use an older executable.")
    for key in keys:
        print(artifacts[key])


if __name__ == "__main__":
    main()
