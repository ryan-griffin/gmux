"""Use the executable paths reported by Cargo for smoke tests and benchmarks."""
from functools import cache
import json
import os
from pathlib import Path

from cargo_build import build_artifacts as cargo_build

ARTIFACT_ENV = "GMUX_TEST_BINARIES"


@cache
def build_artifacts(profile="debug"):
    return cargo_build(profile, examples=True)


def cargo_binary(name, *, example=False, profile="debug"):
    recorded = os.environ.get(ARTIFACT_ENV)
    artifacts = json.loads(recorded) if recorded else build_artifacts(profile)
    key = f"{profile}/{'example' if example else 'bin'}/{name}"
    if key not in artifacts:
        raise SystemExit(f"Cargo did not report {key}; run scripts/check.py to build it.")
    return Path(artifacts[key])
