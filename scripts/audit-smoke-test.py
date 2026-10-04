#!/usr/bin/env python3
"""Regression checks for tab focus, directory tracking, and app activation.

Run with GTK_A11Y=none dbus-run-session -- python3 scripts/audit-smoke-test.py.
Uses an isolated configuration and a shell without prompt hooks.
"""
import json
from functools import partial
from test_support import rpc as shared_rpc, wait_for as shared_wait_for
import os
from pathlib import Path
import shlex
import resource
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(shared_wait_for, timeout=8)


with tempfile.TemporaryDirectory(prefix="gmux-audit-") as temp:
    temp = Path(temp)
    shell = temp / "shell"
    rcfile = temp / "bashrc"
    rcfile.write_text("PS1='GMUX_AUDIT> '\nPROMPT_COMMAND=''\n")
    shell.write_text(f"#!/bin/sh\nexec /bin/bash --noprofile --rcfile {shlex.quote(str(rcfile))} -i\n")
    shell.chmod(0o700)
    env = os.environ.copy()
    env.update(XDG_CONFIG_HOME=str(temp / "config"), SHELL=str(shell),
               GMUX_SOCKET=str(temp / "gmux.sock"), GTK_A11Y="none")

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def state():
        return rpc("list")

    def tab(identifier):
        return next(w for w in state()["tabs"] if w["id"] == identifier)

    def run(pane, command):
        rpc("send", pane=pane, text=command, paste=True, enter=True)

    failures = []

    def check(name, test):
        try:
            test()
            print(f"PASS: {name}", flush=True)
        except AssertionError as error:
            failures.append(name)
            print(f"FAIL: {name}: {error}", flush=True)

    with (artifacts / "audit-smoke.log").open("w") as log:
        app = subprocess.Popen([str(root / "target/debug/gmux"), "--directory", str(root)],
                               env=env, cwd=root, stdout=log, stderr=log)
        try:
            wait_for(lambda: rpc("daemon-info")["attached"])
            first = wait_for(lambda: state()["tabs"])[0]
            a = first["active_pane"]
            b = rpc("split", pane=a)["pane"]
            c = rpc("split", pane=b)["pane"]
            rpc("focus", pane=c)
            rpc("close", pane=b)

            def selected_pane():
                assert tab(first["id"])["active_pane"] == c, state()

            check("closing an inactive pane preserves the selected pane", selected_pane)
            rpc("focus", pane=c)
            second = rpc("new", name="Keep selected", directory=str(root))
            rpc("close", pane=a)

            def selected_tab():
                assert state()["active_tab"] == second["tab"], state()

            check("closing a background pane preserves the selected tab", selected_tab)
            # Closing the last pane of a background tab has the same rule.
            third = rpc("new", name="Third", directory=str(root))
            rpc("close", pane=c)

            def last_background_pane():
                assert state()["active_tab"] == third["tab"], state()

            check("closing a background tab preserves focus", last_background_pane)

            pane = third["pane"]
            changed = temp / "new-directory"
            changed.mkdir()
            run(pane, f"printf '\\033]7;{root.as_uri()}\\007'; cd {shlex.quote(str(changed))}")

            def directory():
                wait_for(lambda: tab(third["tab"])["panes"][0]["directory"] == str(changed), timeout=4)
                new = rpc("split", pane=pane)["pane"]
                assert next(p for p in tab(third["tab"])["panes"] if p["id"] == new)["directory"] == str(changed)

            check("stale OSC directory does not override the shell's current directory", directory)

            def captures_current_output():
                before = artifacts / "audit-before.png"
                after = artifacts / "audit-after.png"
                rpc("screenshot", path=str(before))
                run(pane, "printf '\\033]11;#345678\\007\\033[2J\\033[HCAPTURE_UPDATED\\n'")
                wait_for(lambda: rpc("read", pane=pane)["text"].startswith("CAPTURE_UPDATED"))
                rpc("screenshot", path=str(after))
                assert before.read_bytes() != after.read_bytes(), "Screenshot returned a stale frame"

            check("screenshots capture current terminal output", captures_current_output)
            count = len(state()["tabs"])
            opened = temp / "open-directory"
            opened.mkdir()
            result = subprocess.run([str(root / "target/debug/gmux"), "--directory", str(opened)],
                                    env=env, cwd=root, stdout=log, stderr=log, timeout=10)

            def activation():
                assert result.returncode == 0
                wait_for(lambda: len(state()["tabs"]) == count + 1, timeout=3)
                assert state()["tabs"][0]["panes"][0]["directory"] == str(opened)

            check("opening a directory in an existing instance creates a tab", activation)

            def shell_exit():
                selected = state()["active_tab"]
                workspace = rpc("workspace-new", name="Keep workspace", directory=str(root))["workspace"]
                exiting = rpc("new", name="Exit checks", workspace=workspace)
                left = exiting["pane"]
                middle = rpc("split", pane=left)["pane"]
                right = rpc("split", pane=middle, down=True)["pane"]
                run(left, "printf 'STILL_HERE\\n'; (exit 7)")
                wait_for(lambda: "STILL_HERE" in rpc("read", pane=left)["text"].splitlines())
                assert len(tab(exiting["tab"])["panes"]) == 3, "A command or subshell exiting must not close its parent shell's pane"
                run(left, "exit")
                wait_for(lambda: len(tab(exiting["tab"])["panes"]) == 2)
                assert tab(exiting["tab"])["active_pane"] == right
                assert tab(exiting["tab"])["layout"]["Split"]["first"] == {"Pane": middle}
                run(right, "exit 7")
                wait_for(lambda: tab(exiting["tab"])["layout"] == {"Pane": middle})
                assert tab(exiting["tab"])["active_pane"] == middle
                rpc("send", pane=middle, text="\x04")
                wait_for(lambda: all(t["id"] != exiting["tab"] for t in state()["tabs"]))
                assert state()["active_tab"] == selected
                assert any(w["id"] == workspace for w in state()["workspaces"])

            check("shell exit closes active and inactive splits, Ctrl+D closes the final pane, and workspaces stay saved", shell_exit)
        finally:
            app.terminate()
            app.wait(timeout=10)
            stop_daemon(env["GMUX_SOCKET"])

        session_path = temp / "config/gmux/session.json"
        saved = json.loads(session_path.read_text())
        valid = saved["tabs"]
        invalid = dict(id=2**64 - 1, name="Invalid tab", custom_name=True, workspace=None, layout={"Pane": 42},
                       directories={"42": str(root)}, active=42)
        duplicate = dict(valid[0], name="Duplicate tab")
        duplicate_pane = dict(duplicate, id=10_000, name="Duplicate pane")
        saved["tabs"] = [invalid, valid[0], duplicate, duplicate_pane] + valid[1:]
        session_path.write_text(json.dumps(saved))
        app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, cwd=root,
                               stdout=log, stderr=log)
        try:
            def restore():
                wait_for(lambda: rpc("daemon-info")["attached"])
                def restored():
                    assert app.poll() is None, "Invalid saved IDs crashed the application"
                    return state()["tabs"]
                tabs = wait_for(restored)
                assert [w["name"] for w in tabs] == [w["name"] for w in valid]
                assert [w["id"] for w in tabs] == [w["id"] for w in valid]
                assert [w["layout"] for w in tabs] == [w["layout"] for w in valid]
                assert [w["workspace"] for w in tabs] == [w.get("workspace") for w in valid]
                for old, current in zip(valid, tabs):
                    assert {str(p["id"]): p["directory"] for p in current["panes"]} == old["directories"]

            check("invalid saved IDs are skipped while valid tabs restore", restore)

            def exit_last_tab():
                for current in state()["tabs"]:
                    for pane in current["panes"]:
                        # Bash can flush queued input during startup. A restored
                        # pane exists before its new shell is ready for commands.
                        try:
                            wait_for(lambda: "GMUX_AUDIT>" in rpc("read", pane=pane["id"])["text"])
                        except AssertionError:
                            print("Shell not ready:", pane, rpc("read", pane=pane["id"]), flush=True)
                            raise
                        run(pane["id"], "exit")
                try:
                    wait_for(lambda: state()["tabs"] == [])
                except AssertionError:
                    print("Panes remaining after exit:", state(), flush=True)
                    for current in state()["tabs"]:
                        for pane in current["panes"]:
                            print("Remaining pane output:", pane["id"], rpc("read", pane=pane["id"]), flush=True)
                    raise
                assert state()["active_tab"] is None
                assert app.poll() is None, "Exiting the last pane should leave the app's empty state open"
                assert state()["workspaces"], "Exiting shells must not delete workspace definitions"

            check("exiting every shell leaves the empty state open with saved workspaces", exit_last_tab)
        finally:
            if app.poll() is None:
                app.terminate()
                app.wait(timeout=10)
                stop_daemon(env["GMUX_SOCKET"])
    assert not failures, failures
