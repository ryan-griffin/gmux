"""Select Cargo artifacts, or resolve existing native binaries without rebuilding."""
from functools import cache
import json
import os
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
ARTIFACT_ENV = "GMUX_TEST_BINARIES"


@cache
def cargo_target_dir():
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"], cwd=ROOT)
    return Path(json.loads(metadata)["target_directory"])


@cache
def native_output_dir():
    # Stable Cargo cannot query its merged config. Follow its documented search
    # order and legacy-file precedence, rejecting settings we cannot resolve.
    directories = [path / ".cargo" for path in (ROOT, *ROOT.parents)]
    directories.append(Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")))
    target = None
    for directory in directories:
        legacy, modern = directory / "config", directory / "config.toml"
        config = legacy if legacy.is_file() else modern
        if not config.is_file():
            continue
        settings = tomllib.loads(config.read_text())
        if settings.get("include"):
            raise SystemExit(f"Cannot resolve Cargo config includes in {config}. "
                             "Use explicit --binary/--check-binary paths, or a config without includes.")
        if target is None:
            target = settings.get("build", {}).get("target")
    target = os.environ.get("CARGO_BUILD_TARGET", target)
    if target is not None:
        host = next(line.removeprefix("host: ") for line in
                    subprocess.check_output(["rustc", "-vV"], cwd=ROOT, text=True).splitlines()
                    if line.startswith("host: "))
        if not isinstance(target, str) or target not in (host, "host-tuple"):
            raise SystemExit(f"Smoke defaults execute a single native host target ({host}); "
                             f"Cargo build.target is {target!r}. Remove that setting or use explicit "
                             "--binary/--check-binary paths to already-built native executables. "
                             "Multiple, custom, and cross targets are not supported by the defaults.")
        return cargo_target_dir() / host
    return cargo_target_dir()


def cargo_binary(name, *, example=False, profile="debug"):
    directory = native_output_dir() / profile
    if recorded := os.environ.get(ARTIFACT_ENV):
        key = f"{profile}/{'example' if example else 'bin'}/{name}"
        artifacts = json.loads(recorded)
        if key not in artifacts:
            raise SystemExit(f"Cargo did not report {key} in this build; use an explicit binary path.")
        return Path(artifacts[key])
    if example:
        directory /= "examples"
    return directory / name
