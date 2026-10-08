"""Locate the binaries built by Cargo, including configured target directories."""
from functools import cache
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent


@cache
def cargo_target_dir():
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"], cwd=ROOT)
    return Path(json.loads(metadata)["target_directory"])


def cargo_binary(name, *, example=False, profile="debug"):
    directory = cargo_target_dir() / profile
    if example:
        directory /= "examples"
    return directory / name
