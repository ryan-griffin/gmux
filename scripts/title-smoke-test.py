#!/usr/bin/env python3
"""Test pane titles against real foreground jobs and OSC output.

Run on a native display with GTK_A11Y=none dbus-run-session -- python3
scripts/title-smoke-test.py. Shell hooks and application settings are isolated.
"""
import json
from test_support import rpc as shared_rpc, wait_for
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


with tempfile.TemporaryDirectory(prefix="gmux-title-") as temp:
    temp = Path(temp)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    job = temp / "job.py"
    job.write_text("""import os, sys, time
mode = sys.argv[1]
if mode != 'silent':
    os.write(1, b'\\x1b]2;OpenCode\\x07')
if mode == 'clear':
    time.sleep(0.7)
    os.write(1, b'\\x1b]2;\\x07')
if mode == 'exec':
    time.sleep(0.7)
    os.execl('/usr/bin/sleep', 'sleep', '30')
time.sleep(30)
""")
    env = os.environ.copy()
    env.update(XDG_CONFIG_HOME=str(temp / "config"), SHELL=str(shell),
               GMUX_SOCKET=str(temp / "gmux.sock"), GTK_A11Y="none",
               PS1="GMUX_TEST> ", PROMPT_COMMAND="")

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def title(pane):
        return next(p for w in rpc("list")["tabs"] for p in w["panes"]
                    if p["id"] == pane)["title"]

    def expect(pane, expected, tab_name=None):
        def updated():
            for tab in rpc("list")["tabs"]:
                for terminal in tab["panes"]:
                    if terminal["id"] == pane and terminal["title"] == expected:
                        return tab
        tab = wait_for(updated)
        if tab["active_pane"] == pane:
            assert tab["name"] == (tab_name or expected), (
                "The tab title must update in the same event as its selected pane", tab)

    def run(pane, command):
        rpc("send", pane=pane, text=command, paste=True, enter=True)

    def interrupt(pane, tab_name=None):
        rpc("send", pane=pane, text="\x03")
        expect(pane, "gmux", tab_name)

    def ready(pane):
        run(pane, "printf 'GMUX_READY\\n'")
        wait_for(lambda: "GMUX_READY" in rpc("read", pane=pane)["text"].splitlines())

    with (artifacts / "title-smoke.log").open("w") as log:
        app = subprocess.Popen([str(root / "target/debug/gmux"), "--directory", str(root)],
                               env=env, cwd=root, stdout=log, stderr=log)
        try:
            tab = wait_for(lambda: rpc("list")["tabs"])[0]
            pane = tab["active_pane"]
            ready(pane)
            expect(pane, "gmux")
            for mode in ["title", "title", "clear", "exec"]:
                run(pane, f"python3 {shlex.quote(str(job))} {mode}")
                expect(pane, "OpenCode")
                if mode == "clear":
                    expect(pane, "python3")
                if mode == "exec":
                    expect(pane, "sleep")
                interrupt(pane)
            run(pane, "sleep 30")
            expect(pane, "sleep")
            interrupt(pane)
            # Reproduce the report with the actual monitor when installed.
            if shutil.which("btop"):
                run(pane, "btop")
                expect(pane, "btop")
                interrupt(pane)
            second = rpc("split", pane=pane)["pane"]
            ready(second)
            run(pane, f"python3 {shlex.quote(str(job))} title")
            expect(pane, "OpenCode")
            run(second, "sleep 30")
            expect(second, "sleep")
            assert title(pane) == "OpenCode", "Other panes must keep their own title"
            assert rpc("list")["tabs"][0]["name"] == "sleep"
            rpc("focus", pane=pane)
            assert rpc("list")["tabs"][0]["name"] == "OpenCode"
            rpc("focus", pane=second)
            interrupt(pane)
            interrupt(second)
            subdir = temp / "changed-directory"
            subdir.mkdir()
            run(second, f"cd {shlex.quote(str(subdir))}")
            expect(second, "changed-directory")
            assert rpc("list")["tabs"][0]["name"] == "changed-directory"
            named = rpc("new", name="Pinned title", directory=str(root))
            ready(named["pane"])
            run(named["pane"], f"python3 {shlex.quote(str(job))} title")
            expect(named["pane"], "OpenCode", "Pinned title")
            assert next(t for t in rpc("list")["tabs"] if t["id"] == named["tab"])["name"] == "Pinned title"
            interrupt(named["pane"], "Pinned title")
            print("PASS: foreground titles, repeated OSC titles, empty title reset, exec, "
                  "silent commands, shell directory fallback, independent split titles, "
                  "tab titles immediately following the selected pane, and explicit tab names")
        finally:
            app.terminate()
            app.wait(timeout=10)
            stop_daemon(env["GMUX_SOCKET"])
