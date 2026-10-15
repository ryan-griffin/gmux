#!/usr/bin/env python3
"""Check application startup, PTY workflows, and populated and empty sessions.

Run with scripts/check.py --native --only smoke for the isolated Wayland fixture.
"""
import argparse
from contextlib import ExitStack
from pathlib import Path
from cargo_test_support import cargo_binary
import shlex
import os
import subprocess
import tempfile

from test_support import AppFixture, scenario, stop_process, wait_for

parser = argparse.ArgumentParser()
parser.add_argument("--native", action="store_true")
parser.add_argument("--binary", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
binary = args.binary.resolve() if args.binary else cargo_binary("gmux-gtk")
assert binary.is_file(), "Run cargo build first"
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def tab(fixture, identifier):
    return next(t for t in fixture.rpc("list")["tabs"] if t["id"] == identifier)


def command(fixture, pane, text):
    wait_for(lambda: "GMUX_SMOKE>" in fixture.rpc("read", pane=pane)["text"])
    fixture.rpc("send", pane=pane, text=text, paste=True, enter=True)


def empty_startup(fixture, log):
    fixture.start_app(binary, cwd=root, stdout=log, stderr=log)
    assert fixture.rpc("list")["tabs"] == []
    assert fixture.rpc("list")["active_tab"] is None
    assert fixture.rpc("renderer-info")["terminal"] is None
    assert "error" in fixture.rpc("read", check=False)
    for name in ("next-pane", "previous-pane", "rename", "split-right", "find", "browser"):
        assert "error" in fixture.rpc("action", name=name, check=False), name
    for method, params in (("read", {"pane": 999999}), ("unknown", {}),
                           ("browser", {"url": "about:blank"}),
                           ("new", {"directory": str(fixture.path / "missing")})):
        assert "error" in fixture.rpc(method, check=False, **params), method
    fixture.rpc("action", name="new")
    pane = fixture.rpc("list")["tabs"][0]["active_pane"]
    fixture.rpc("close", pane=pane)
    assert fixture.rpc("list")["tabs"] == [], "Closing the last pane created another shell"


def pty_workflow(fixture, log):
    fixture.start_app(binary, "--directory", root, cwd=root, stdout=log, stderr=log)
    first = fixture.rpc("list")["tabs"][0]
    pane = first["active_pane"]
    assert len(first["panes"]) == 1
    assert fixture.rpc("renderer-info", pane=pane)["terminal"] == "gsk"
    command(fixture, pane, "printf '\\033[2J\\033[H\\033[32mGMUX_PTY_OK\\033[0m\\n'")
    wait_for(lambda: "GMUX_PTY_OK" in fixture.rpc("read", pane=pane)["text"])
    right = fixture.rpc("split", pane=pane)["pane"]
    down = fixture.rpc("split", pane=right, down=True)["pane"]
    assert len(tab(fixture, first["id"])["panes"]) == 3
    command(fixture, right, "printf '\\nGMUX_SPLIT_OK\\n'")
    wait_for(lambda: "GMUX_SPLIT_OK" in fixture.rpc("read", pane=right)["text"])
    project = fixture.path / "project"
    project.mkdir()
    subdir = project / "src"
    subdir.mkdir()
    other = fixture.rpc("new", name="Smoke project", directory=str(project))
    command(fixture, other["pane"], "cd " + shlex.quote(str(subdir)))
    wait_for(lambda: tab(fixture, other["tab"])["panes"][0]["directory"] == str(subdir))
    fixture.rpc("close", pane=down)
    fixture.rpc("close", pane=pane)
    assert len(tab(fixture, first["id"])["panes"]) == 1
    command(fixture, right, "exit 7")
    wait_for(lambda: all(t["id"] != first["id"] for t in fixture.rpc("list")["tabs"]))
    assert fixture.rpc("list")["active_tab"] == other["tab"]
    assert "error" in fixture.rpc("send", pane=right, text="echo should fail", check=False)
    if args.native:
        fixture.rpc("screenshot", path=str(artifacts / "smoke-native.png"))


def reconnect_and_empty_restore(fixture, log):
    app = fixture.start_app(binary, "--directory", root, cwd=root, stdout=log, stderr=log)
    first = fixture.rpc("list")["tabs"][0]
    fixture.rpc("split", pane=first["active_pane"], down=True)
    fixture.rpc("new", name="Saved tab", directory=str(fixture.path))
    expected = fixture.rpc("list")
    daemon_pid = fixture.rpc("daemon-info")["pid"]
    fixture.rpc("detach")
    assert app.wait(timeout=8) == 0
    assert fixture.rpc("daemon-info")["pid"] == daemon_pid
    assert not fixture.rpc("daemon-info")["attached"]
    fixture.start_app(binary, cwd=root, stdout=log, stderr=log)
    restored = fixture.rpc("list")
    assert restored["active_tab"] == expected["active_tab"]
    assert len(restored["tabs"]) == len(expected["tabs"])
    for old, current in zip(expected["tabs"], restored["tabs"]):
        for key in ("id", "name", "layout", "active_pane"):
            assert current[key] == old[key], key
        assert [(p["id"], p["directory"]) for p in current["panes"]] == [
            (p["id"], p["directory"]) for p in old["panes"]]
        assert not any(p["exited"] for p in current["panes"])
        for pane in current["panes"]:
            fixture.rpc("close", pane=pane["id"])
    assert fixture.rpc("list")["tabs"] == []
    fixture.stop()
    app = fixture.start_app(binary, cwd=root, stdout=log, stderr=log)
    assert fixture.rpc("list")["tabs"] == [], "A full restart populated an empty session"
    assert fixture.rpc("list")["active_tab"] is None
    assert app.poll() is None


cases = [("empty startup and invalid requests", empty_startup),
         ("PTY output, splits, directories, and pane closure", pty_workflow),
         ("populated reconnect and empty session restart", reconnect_and_empty_restore)]
with ExitStack() as cleanup:
    env = {"GTK_A11Y": "none"}
    if not args.native:
        runtime = Path(cleanup.enter_context(tempfile.TemporaryDirectory(prefix="gmux-broadway-")))
        env.update(GDK_BACKEND="broadway", BROADWAY_DISPLAY=":0", XDG_RUNTIME_DIR=str(runtime))
        server = subprocess.Popen(["gtk4-broadwayd", "--unixsocket=" + str(runtime / "http.socket"), ":0"],
                                  env=dict(os.environ, **env),
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        cleanup.callback(stop_process, server)

        def broadway_ready():
            assert server.poll() is None, "Broadway server failed to start"
            return (runtime / "broadway1.socket").is_socket()

        wait_for(broadway_ready)
    with (artifacts / ("smoke-native.log" if args.native else "smoke.log")).open("w") as log:
        for name, test in cases:
            with scenario(name), AppFixture(dict(desktop_notifications=False),
                prefix="gmux-smoke-", env=env,
                shell_script="#!/bin/sh\nexport PS1='GMUX_SMOKE> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n",
            ) as fixture:
                test(fixture, log)
