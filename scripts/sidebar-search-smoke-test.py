#!/usr/bin/env python3
"""Check native sidebar search, workspace filters, and terminal focus.

Run with GTK_A11Y=none GDK_BACKEND=wayland dbus-run-session -- python3
scripts/sidebar-search-smoke-test.py. Uses temporary settings and directories.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        try:
            if result := predicate():
                return result
        except (FileNotFoundError, ConnectionRefusedError):
            pass
        time.sleep(0.05)
    raise AssertionError("Sidebar search did not reach the expected state")


with tempfile.TemporaryDirectory(prefix="gmux-sidebar-search-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps({
        "appearance": "dark", "restore": True, "desktop_notifications": False,
    }))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
               GMUX_SOCKET=str(temp / "socket"), SHELL=str(shell), GTK_A11Y="none")
    project = temp / "project-a"
    project.mkdir()
    src = project / "src"
    src.mkdir()
    other = temp / "project-b"
    other.mkdir()
    subprocess.run(["git", "init", "--quiet", "--initial-branch=feature/search-panel", str(project)], check=True)

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    def state():
        return rpc("list")

    def tab(identifier):
        return next(tab for tab in state()["tabs"] if tab["id"] == identifier)

    def visible():
        return {tab["id"] for tab in state()["tabs"] if tab["visible"]}

    def search(query, expected):
        rpc("search-tabs", query=query)
        wait_for(lambda: visible() == set(expected))
        assert state()["tab_search"] == query

    def capture(name):
        rpc("screenshot", path=str(artifacts / f"{name}.png"))

    def start(log):
        app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
        wait_for(state)
        return app

    def stop(app):
        app.terminate()
        app.wait(timeout=8)
        wait_for(lambda: not (temp / "socket").exists())

    with (artifacts / "sidebar-search-smoke.log").open("w") as log:
        app = start(log)
        try:
            search("missing tab", [])
            assert state()["active_tab"] is None
            capture("sidebar-search-empty-unchanged")
            search("", [])
            a = rpc("workspace-new", name="Alpha", directory=str(project))["workspace"]
            b = rpc("workspace-new", name="Beta", directory=str(other))["workspace"]
            server = rpc("new", name="Server", directory=str(src), workspace=a)
            sibling = rpc("split", pane=server["pane"])["pane"]
            client = rpc("new", name="Client", workspace=a)
            database = rpc("new", name="Database", workspace=b)
            scratch = rpc("new", name="Scratch", workspace=None)
            rpc("notify", pane=sibling, message="Keep unread while searching")
            search("sErVeR", [server["tab"]])
            assert state()["active_tab"] == scratch["tab"], "Typing must not replace the active terminal"
            assert tab(server["tab"])["unread"], "Filtering must not mark hidden alerts as read"
            capture("sidebar-search-title")
            search("Alpha", [server["tab"], client["tab"]])
            search("project-a/src", [server["tab"]])
            search("ALPHA SEARCH-PANEL", [server["tab"], client["tab"]])
            search("Server src", [server["tab"]])
            rpc("filter", workspace=b)
            assert visible() == set(), "Text and workspace filters must both match"
            assert state()["active_tab"] is None
            capture("sidebar-search-filter-no-matches")
            search("database", [database["tab"]])
            assert state()["active_tab"] is None, "Typing must not replace the empty page"
            rpc("focus", tab=database["tab"])
            assert state()["active_tab"] == database["tab"]
            search("", [database["tab"]])
            rpc("filter", workspace=a)
            search("alpha", [server["tab"], client["tab"]])
            rpc("focus", tab=server["tab"])
            rpc("action", name="next-tab")
            assert state()["active_tab"] == client["tab"]
            search("database", [])
            rpc("action", name="next-tab")
            assert state()["active_tab"] == client["tab"], "No matches must not make tab navigation crash"
            capture("sidebar-search-no-matches")
            rpc("notify", pane=sibling, message="Keep sibling unread after revealing a hidden pane")
            rpc("focus", pane=server["pane"])
            assert state()["tab_search"] == ""
            assert state()["filter"] == {"workspace": a}
            assert state()["active_tab"] == server["tab"]
            assert tab(server["tab"])["unread"]
            search("server", [server["tab"]])
            rpc("move", tab=server["tab"], workspace=b)
            assert visible() == set()
            rpc("filter", value="all")
            assert visible() == {server["tab"]}
            search("alpha", [client["tab"]])
            search("beta server", [server["tab"]])
            rpc("action", name="new")
            assert state()["tab_search"] == "", "Creating a tab must reveal it"
            assert tab(state()["active_tab"])["visible"]
            search("alpha", [client["tab"]])
            rpc("focus", tab=client["tab"])
            rpc("send", pane=client["pane"], text="exit", paste=True, enter=True)
            wait_for(lambda: all(t["id"] != client["tab"] for t in state()["tabs"]))
            assert state()["active_tab"] is None and visible() == set()
            search("database", [database["tab"]])
            assert state()["active_tab"] is None, "Typing must not replace the empty page"
            rpc("focus", tab=database["tab"])
            assert state()["active_tab"] == database["tab"]
        finally:
            stop(app)
        app = start(log)
        try:
            assert state()["tab_search"] == "", "Search text should not hide restored tabs"
            assert all(tab["visible"] for tab in state()["tabs"])
            capture("sidebar-search-restored")
        finally:
            stop(app)
        print("PASS: native title/workspace/directory/branch search, case-insensitive words across fields, combined workspace filters, no-match states, preserving terminal focus and unread alerts, navigation, revealing hidden panes and new tabs, membership updates, shell exit, and restored tabs")
