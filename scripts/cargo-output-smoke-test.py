#!/usr/bin/env python3
"""Build real Cargo fixtures and reject stale executable defaults."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

from cargo_test_support import ARTIFACT_ENV, compiler_artifacts

root = Path(__file__).resolve().parent.parent
host = next(line.removeprefix("host: ") for line in
            subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
            if line.startswith("host: "))
probe = """
import json, subprocess, sys
from pathlib import Path
import cargo_test_support as support
support.ROOT = Path(sys.argv[1])
binary = support.cargo_binary(sys.argv[2], example=sys.argv[3] == 'example', profile=sys.argv[4])
print(json.dumps({'binary': str(binary), 'output': subprocess.check_output([str(binary)], text=True).strip()}))
"""
names = [("gmux-target-review", "bin"), ("gmux-daemon", "bin"), ("renderer-check", "example")]
reports = []

for case in ("ordinary", "target-dir", "environment-target-dir", "environment-target", "config-target",
             "config-target-dir", "environment-overrides-config",
             "parent-config", "home-config", "legacy-config", "release-target"):
    with tempfile.TemporaryDirectory(prefix="gmux-cargo-output-") as directory:
        fixture = Path(directory)
        project = fixture / "project"
        project.mkdir()
        source = project / "src/main.rs"
        source.parent.mkdir()
        source.write_text('fn main() { println!("fresh {}", env!("CARGO_BIN_NAME")); }\n')
        (project / "src/daemon.rs").write_bytes(source.read_bytes())
        (project / "examples").mkdir()
        (project / "examples/renderer-check.rs").write_bytes(source.read_bytes())
        (project / "Cargo.toml").write_text(
            '[package]\nname="gmux-target-review"\nversion="0.0.0"\nedition="2024"\n'
            '[[bin]]\nname="gmux-target-review"\npath="src/main.rs"\n'
            '[[bin]]\nname="gmux-daemon"\npath="src/daemon.rs"\n'
            '[[example]]\nname="renderer-check"\npath="examples/renderer-check.rs"\n')
        env = dict(os.environ, CARGO_HOME=str(fixture / "cargo-home"),
                   PYTHONPATH=str(root / "scripts"))
        for key in (ARTIFACT_ENV, "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"):
            env.pop(key, None)
        target_dir = project / "target"
        settings = {}
        if case == "target-dir":
            target_dir = fixture / "custom-output"
            settings["target-dir"] = str(target_dir)
        elif case == "environment-target-dir":
            target_dir = fixture / "custom-output"
            env["CARGO_TARGET_DIR"] = str(target_dir)
        elif case in ("environment-target", "release-target"):
            env["CARGO_BUILD_TARGET"] = host
        else:
            if case != "ordinary":
                settings["target"] = host
            if case == "config-target-dir":
                target_dir = fixture / "custom-output"
                settings["target-dir"] = str(target_dir)
            if case == "environment-overrides-config":
                settings["target"] = "invalid-target-must-not-win"
                env["CARGO_BUILD_TARGET"] = host
        if settings:
            config_dir = (fixture / ".cargo" if case == "parent-config" else
                          Path(env["CARGO_HOME"]) if case == "home-config" else project / ".cargo")
            config_dir.mkdir(parents=True)
            config = config_dir / ("config" if case == "legacy-config" else "config.toml")
            config.write_text("[build]\n" + "".join(f"{key}={json.dumps(value)}\n" for key, value in settings.items()))
            if case == "legacy-config":
                (config_dir / "config.toml").write_text('[build]\ntarget="invalid-target-must-not-win"\n')
        profile = "release" if case == "release-target" else "debug"
        stale_marker = fixture / "stale-ran"
        for directory in {project / "target", target_dir}:
            for name, kind in names:
                binary = directory / profile / ("examples" if kind == "example" else "") / name
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_text(f"#!/bin/sh\nprintf stale > {shlex.quote(str(stale_marker))}\nprintf stale\n")
                binary.chmod(0o700)
        subprocess.run(["cargo", "generate-lockfile"], cwd=project, env=env, check=True)
        command = ["cargo", "build", "--locked", "--bins", "--examples", "--message-format=json-render-diagnostics"]
        if profile == "release":
            command.append("--release")
        build = subprocess.run(command, cwd=project, env=env, check=True, text=True, stdout=subprocess.PIPE)
        artifacts = compiler_artifacts(build.stdout, profile=profile)
        results = []
        for mode in ("direct-or-skip-build", "cargo-artifacts"):
            selection_env = dict(env)
            if mode == "cargo-artifacts":
                selection_env[ARTIFACT_ENV] = json.dumps(artifacts)
            for name, kind in names:
                selected = subprocess.check_output(
                    [sys.executable, "-c", probe, str(project), name, kind, profile],
                    cwd=project, env=selection_env, text=True)
                result = json.loads(selected)
                assert result["binary"] == artifacts[f"{profile}/{kind}/{name}"], (case, mode, result)
                assert result["output"] == f"fresh {name}", (case, mode, result)
                assert not stale_marker.exists(), (case, mode, "Ran a stale binary")
                results.append(dict(mode=mode, **result))
        # Unsupported defaults must reject before touching a planted stale file.
        other = "i686-unknown-linux-gnu" if host == "aarch64-unknown-linux-gnu" else "aarch64-unknown-linux-gnu"
        for unsupported in ('target=["' + host + '"]', f'target="{other}"',
                            'target="custom.json"'):
            config = project / ".cargo/config.toml"
            config.parent.mkdir(exist_ok=True)
            (config.parent / "config").unlink(missing_ok=True)
            config.write_text("[build]\n" + unsupported + "\n")
            rejected_env = dict(env)
            rejected_env.pop("CARGO_BUILD_TARGET", None)
            result = subprocess.run([sys.executable, "-c", probe, str(project), names[0][0], "bin", profile],
                                    cwd=project, env=rejected_env, text=True, capture_output=True)
            assert result.returncode != 0 and "single native host target" in result.stderr, result.stderr
            assert not stale_marker.exists()
        reports.append(dict(case=case, host=host, results=results))
        print(f"PASS: {case}, Cargo artifact and direct/skip-build app, daemon and example paths; unsupported targets reject", flush=True)

output = root / "artifacts/cargo-output.json"
output.parent.mkdir(exist_ok=True)
output.write_text(json.dumps(reports, indent=2) + "\n")
