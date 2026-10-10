#!/usr/bin/env python3
"""Build binaries and select executable paths reported by that Cargo invocation."""
import argparse
import json
from pathlib import Path
import subprocess


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--target", help="Build for an explicit Cargo target")
    parser.add_argument("binaries", nargs="+", help="Binary names, in output order")
    args = parser.parse_args()
    command = ["cargo", "build", "--locked", "--bins", "--message-format=json-render-diagnostics"]
    if args.release:
        command.append("--release")
    if args.target:
        command.extend(["--target", args.target])
    try:
        build = subprocess.run(command, cwd=Path(__file__).resolve().parent.parent,
                               check=True, text=True, stdout=subprocess.PIPE)
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode) from None
    profile = "release" if args.release else "debug"
    artifacts = compiler_artifacts(build.stdout, profile=profile)
    keys = [f"{profile}/bin/{name}" for name in args.binaries]
    for key in keys:
        if key not in artifacts:
            raise SystemExit(f"Cargo did not report {key} in this build; refusing to use an older executable.")
    for key in keys:
        print(artifacts[key])


if __name__ == "__main__":
    main()
