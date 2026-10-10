#!/usr/bin/env python3
"""Build real Cargo fixtures and reject stale executable defaults."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from cargo_build import compiler_artifacts
from cargo_test_support import ARTIFACT_ENV

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
names = [("gmux", "bin"), ("gmux-daemon", "bin"), ("renderer-check", "example")]
reports = []
real_cargo = shutil.which("cargo")


def check_scripts(fixture, project, env, case, stale_marker):
    """Run the actual entrypoints with real builds, isolated installs and a Cargo spy."""
    scripts = project / "scripts"
    scripts.mkdir()
    for name in ("run.sh", "install.sh", "cargo_build.py"):
        shutil.copy2(root / "scripts" / name, scripts / name)
    shutil.copytree(root / "data", project / "data")
    shutil.copy2(root / "LICENSE", project / "LICENSE")
    ghostty = fixture / "ghostty with spaces"
    (ghostty / "include/ghostty/vt").mkdir(parents=True)
    (ghostty / "include/ghostty/vt/terminal.h").touch()
    (ghostty / "lib").mkdir()
    (ghostty / "lib/libghostty-vt.so").write_bytes(b"fixture Ghostty library")
    tools = fixture / "tools"
    tools.mkdir()
    cargo = tools / "cargo"
    cargo.write_text(f"#!{sys.executable}\n" + '''
import json, os, subprocess, sys
from pathlib import Path
with Path(os.environ["GMUX_CARGO_CALLS"]).open("a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\\n")
build = subprocess.run([os.environ["GMUX_REAL_CARGO"], *sys.argv[1:]], text=True, stdout=subprocess.PIPE)
output = build.stdout
mode = os.environ.get("GMUX_CARGO_REPORT_MODE")
if build.returncode == 0 and mode in ("missing-daemon", "ambiguous-app"):
    messages = [json.loads(line) for line in output.splitlines()]
    if mode == "missing-daemon":
        messages = [message for message in messages if message.get("target", {}).get("name") != "gmux-daemon"]
    else:
        duplicate = next(message.copy() for message in messages
                         if message.get("executable") and message["target"]["name"] == "gmux")
        duplicate["executable"] = os.environ["GMUX_STALE_BINARY"]
        messages.append(duplicate)
    output = "".join(json.dumps(message) + "\\n" for message in messages)
Path(os.environ["GMUX_CARGO_REPORT"]).write_text(output)
sys.stdout.write(output)
if mode == "failed-after-artifacts":
    print("fixture build failed after emitting artifacts", file=sys.stderr)
    sys.exit(73)
sys.exit(build.returncode)
''')
    cargo.chmod(0o700)
    calls = fixture / "cargo-calls.jsonl"
    report = fixture / "cargo-report.jsonl"
    arguments = fixture / "arguments"
    pid = fixture / "pid"
    stale_binary = fixture / "stale-gmux"
    stale_binary.write_text(f"#!/bin/sh\nprintf stale > {shlex.quote(str(stale_marker))}\n")
    stale_binary.chmod(0o700)
    script_env = dict(env, PATH=str(tools) + os.pathsep + env["PATH"], GHOSTTY_PREFIX=str(ghostty),
                      GMUX_REAL_CARGO=real_cargo, GMUX_CARGO_CALLS=str(calls),
                      GMUX_CARGO_REPORT=str(report), GMUX_STALE_BINARY=str(stale_binary),
                      GMUX_FIXTURE_ARGUMENTS=str(arguments), GMUX_FIXTURE_PID=str(pid))

    def check_build(release, *, target=None):
        commands = [json.loads(line) for line in calls.read_text().splitlines()]
        assert len(commands) == 1 and commands[0][0] == "build", (case, commands)
        assert ("--release" in commands[0]) == release, (case, commands)
        assert "--locked" in commands[0] and "--bins" in commands[0], (case, commands)
        assert ("--target" in commands[0]) == (target is not None), (case, commands)
        if target is not None:
            assert commands[0][commands[0].index("--target") + 1] == target, (case, commands)
        calls.unlink()

    app_args = ["--directory", "folder with spaces", "", "quote'\"$()", "--target", host, "--", "line\nbreak"]
    with subprocess.Popen([str(scripts / "run.sh"), *app_args], cwd=fixture, env=script_env,
                          text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        stdout, stderr = process.communicate(timeout=60)
        assert process.returncode == 0 and stdout.strip() == "fresh gmux", (case, stdout, stderr)
        assert int(pid.read_text()) == process.pid, "run.sh must exec the selected application"
    assert arguments.read_bytes() == b"".join(argument.encode() + b"\0" for argument in app_args), case
    check_build(False)
    assert not stale_marker.exists(), (case, "run.sh executed a stale default")

    # Preserve a stale release default when Cargo uses a custom directory or host subdirectory.
    for name in ("gmux", "gmux-daemon"):
        binary = project / "target/release" / name
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.write_bytes(stale_binary.read_bytes())
        binary.chmod(0o700)
    prefix = fixture / "install prefix with spaces"
    installed = subprocess.run([str(scripts / "install.sh"), str(prefix)], cwd=fixture,
                               env=script_env, text=True, capture_output=True, timeout=60)
    assert installed.returncode == 0, (case, installed.stdout, installed.stderr)
    check_build(True)
    release_artifacts = compiler_artifacts(report.read_text(), profile="release")
    for name in ("gmux", "gmux-daemon"):
        binary = prefix / "bin" / name
        assert binary.read_bytes() == Path(release_artifacts[f"release/bin/{name}"]).read_bytes(), case
        assert subprocess.check_output([str(binary)], text=True, env=env).strip() == f"fresh {name}", case
    assert (prefix / "lib/gmux/libghostty-vt.so.0").read_bytes() == b"fixture Ghostty library", case
    assert not stale_marker.exists(), (case, "install.sh copied a stale release default")

    # An explicit native --target must also select only Cargo's reported paths.
    selected = subprocess.run([sys.executable, str(scripts / "cargo_build.py"), "--target", host,
                               "gmux", "gmux-daemon"], cwd=fixture, env=script_env,
                              text=True, capture_output=True, timeout=60)
    assert selected.returncode == 0, (case, selected.stderr)
    check_build(False, target=host)
    explicit_artifacts = compiler_artifacts(report.read_text())
    assert selected.stdout.splitlines() == [explicit_artifacts[f"debug/bin/{name}"]
                                            for name in ("gmux", "gmux-daemon")], case

    if case == "ordinary":
        for mode, code, diagnostic in (("failed-after-artifacts", 73, "fixture build failed"),
                                       ("missing-daemon", 1, "Cargo did not report"),
                                       ("ambiguous-app", 1, "Cargo reported multiple executables"),
                                       ("compile-error", 101, "error:")):
            rejected_env = dict(script_env, GMUX_CARGO_REPORT_MODE=mode)
            if mode == "compile-error":
                (project / "src/daemon.rs").write_text("this is not valid Rust\n")
            for script in ("run.sh", "install.sh"):
                arguments.unlink(missing_ok=True)
                pid.unlink(missing_ok=True)
                failed_prefix = fixture / "failed installation"
                command = [str(scripts / script)]
                if script == "install.sh":
                    command.append(str(failed_prefix))
                rejected = subprocess.run(command, cwd=fixture, env=rejected_env,
                                          text=True, capture_output=True, timeout=60)
                assert rejected.returncode == code and diagnostic in rejected.stderr, (mode, script, rejected)
                assert not arguments.exists() and not pid.exists(), (mode, script, "Launched after failure")
                assert not failed_prefix.exists(), (mode, script, "Installed after failure")
                assert not stale_marker.exists(), (mode, script, "Used a stale binary after failure")
                check_build(script == "install.sh")
        print("PASS: run/install reject failed builds, missing daemon artifacts and ambiguous app artifacts", flush=True)
    print(f"PASS: {case}, run arguments and exec, release install, explicit native --target, single-profile builds", flush=True)

for case in ("ordinary", "target-dir", "environment-target-dir", "environment-target", "config-target",
             "config-target-dir", "environment-overrides-config",
             "parent-config", "home-config", "legacy-config", "release-target"):
    with tempfile.TemporaryDirectory(prefix="gmux-cargo-output-") as directory:
        fixture = Path(directory)
        project = fixture / "project with spaces"
        project.mkdir()
        source = project / "src/main.rs"
        source.parent.mkdir()
        source.write_text('''use std::io::Write;
fn main() {
    println!("fresh {}", env!("CARGO_BIN_NAME"));
    if let Ok(path) = std::env::var("GMUX_FIXTURE_ARGUMENTS") {
        let mut file = std::fs::File::create(path).unwrap();
        for argument in std::env::args().skip(1) {
            file.write_all(argument.as_bytes()).unwrap();
            file.write_all(b"\\0").unwrap();
        }
    }
    if let Ok(path) = std::env::var("GMUX_FIXTURE_PID") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
}
''')
        (project / "src/daemon.rs").write_bytes(source.read_bytes())
        (project / "examples").mkdir()
        (project / "examples/renderer-check.rs").write_bytes(source.read_bytes())
        (project / "Cargo.toml").write_text(
            '[package]\nname="gmux"\nversion="0.0.0"\nedition="2024"\n'
            '[[bin]]\nname="gmux"\npath="src/main.rs"\n'
            '[[bin]]\nname="gmux-daemon"\npath="src/daemon.rs"\n'
            '[[example]]\nname="renderer-check"\npath="examples/renderer-check.rs"\n')
        env = dict(os.environ, CARGO_HOME=str(fixture / "cargo-home"),
                   PYTHONPATH=str(root / "scripts"))
        for key in (ARTIFACT_ENV, "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"):
            env.pop(key, None)
        target_dir = project / "target"
        settings = {}
        if case == "target-dir":
            target_dir = fixture / "custom output"
            settings["target-dir"] = str(target_dir)
        elif case == "environment-target-dir":
            target_dir = fixture / "custom output"
            env["CARGO_TARGET_DIR"] = str(target_dir)
        elif case in ("environment-target", "release-target"):
            env["CARGO_BUILD_TARGET"] = host
        else:
            if case != "ordinary":
                settings["target"] = host
            if case == "config-target-dir":
                target_dir = fixture / "custom output"
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
        if case in ("ordinary", "environment-target-dir", "environment-target", "config-target-dir"):
            check_scripts(fixture, project, env, case, stale_marker)
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
