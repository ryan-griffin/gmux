#!/usr/bin/env python3
"""Check persistent workspaces and the flat, filtered native tab list.

Run with GTK_A11Y=none GDK_BACKEND=wayland dbus-run-session -- python3
scripts/workspace-smoke-test.py. Uses temporary settings and directories.
"""
import json
from test_support import rpc as shared_rpc, wait_for
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


with tempfile.TemporaryDirectory(prefix="gmux-workspaces-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    settings = dict(appearance="dark", desktop_notifications=False)
    (config / "settings.json").write_text(json.dumps(settings))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1='GMUX_READY> '\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
               GMUX_SOCKET=str(temp / "socket"), SHELL=str(shell), GTK_A11Y="none")

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def state():
        return rpc("list")

    def tab(identifier):
        return next(tab for tab in state()["tabs"] if tab["id"] == identifier)

    def capture(name):
        rpc("screenshot", path=str(artifacts / f"{name}.png"))

    def send(pane, text):
        rpc("send", pane=pane, text=text, paste=True, enter=True)

    def start(log):
        app = subprocess.Popen([str(cargo_binary("gmux"))], env=env, stdout=log, stderr=log)
        wait_for(lambda: rpc("daemon-info")["attached"])
        wait_for(state)
        return app

    def stop(app):
        app.terminate()
        app.wait(timeout=8)
        stop_daemon(env["GMUX_SOCKET"])
        wait_for(lambda: not (temp / "socket").exists())

    project = temp / "project"
    project.mkdir()
    other = temp / "other"
    other.mkdir()
    with (artifacts / "workspace-smoke.log").open("w") as log:
        app = start(log)
        try:
            assert state()["tabs"] == [] and state()["workspaces"] == []
            a = rpc("workspace-new", name="Project", directory=str(project))["workspace"]
            b = rpc("workspace-new", name="Other", directory=str(other))["workspace"]
            assert state()["tabs"] == [], "Adding a workspace must not create a shell"
            rpc("filter", workspace=a)
            assert state()["active_tab"] is None
            capture("workspace-empty-project")
            rpc("action", name="new")
            first = state()["tabs"][0]
            first_id, pane = first["id"], first["active_pane"]
            wait_for(lambda: "GMUX_READY>" in rpc("read", pane=pane)["text"])
            assert first["workspace"] == a
            assert first["panes"][0]["directory"] == str(project)
            right = rpc("split", pane=pane)["pane"]
            down = rpc("split", pane=right, down=True)["pane"]
            capture("workspace-tab-splits")
            layout = tab(first_id)["layout"]
            second = rpc("new", name="Second", workspace=a)
            third = rpc("new", name="Other tab", workspace=b)
            home = rpc("new", name="Unassigned tab", workspace=None)
            assert tab(home["tab"])["workspace"] is None
            assert tab(home["tab"])["panes"][0]["directory"] == str(Path.home())
            send(pane, f"cd '{other}'")
            wait_for(lambda: tab(first_id)["panes"][0]["directory"] == str(other))
            assert tab(first_id)["workspace"] == a, "cd must not change workspace membership"
            assert next(w for w in state()["workspaces"] if w["id"] == a)["directory"] == str(project)
            rpc("filter", workspace=a)
            assert {t["id"] for t in state()["tabs"] if t["visible"]} == {first_id, second["tab"]}
            rpc("action", name="new")
            fresh = state()["tabs"][0]
            assert [t["id"] for t in state()["tabs"]] == [fresh["id"], home["tab"], third["tab"], second["tab"], first_id]
            assert fresh["panes"][0]["directory"] == str(project), "New tabs must use the workspace's saved directory"
            rpc("action", name="next-tab")
            assert state()["active_tab"] in {first_id, second["tab"], fresh["id"]}
            rpc("filter", value="unassigned")
            assert [t["id"] for t in state()["tabs"] if t["visible"]] == [home["tab"]]
            rpc("filter", value="all")
            assert all(t["visible"] for t in state()["tabs"])
            rpc("focus", tab=first_id)
            capture("workspaces-all-tabs")
            time.sleep(1.1)
            capture("workspaces-all-tabs")
            rpc("filter", workspace=b)
            capture("workspaces-filtered")
            rpc("notify", pane=down, message="Keep unread through moves")
            assert tab(first_id)["unread"]
            rpc("move", tab=first_id, workspace=b)
            assert tab(first_id)["layout"] == layout, (layout, tab(first_id)["layout"])
            assert tab(first_id)["unread"]
            rpc("filter", workspace=a)
            # Focusing a hidden pane should reveal it and clear the filter.
            rpc("focus", pane=pane)
            assert state()["filter"] == "all"
            assert state()["active_tab"] == first_id
            assert tab(first_id)["unread"], "Focusing one pane must preserve its unread sibling"
            rpc("move", tab=first_id, workspace=a)
            send(pane, "printf 'ALIVE!\\n'; sleep 30")
            wait_for(lambda: "ALIVE!" in rpc("read", pane=pane)["text"])
            rpc("filter", workspace=a)
            rpc("workspace-delete", workspace=a)
            assert state()["filter"] == "unassigned"
            assert tab(first_id)["workspace"] is None
            assert tab(first_id)["layout"] == layout
            assert tab(first_id)["unread"]
            assert [p["id"] for p in tab(first_id)["panes"]] == [pane, right, down]
            assert not any(p["exited"] for p in tab(first_id)["panes"])
            rpc("send", pane=pane, text="\x03")
            send(pane, "printf 'KEPT!\\n'")
            wait_for(lambda: "KEPT!" in rpc("read", pane=pane)["text"])
            assert [w["id"] for w in state()["workspaces"]] == [b]
            # Removing all tabs keeps the independently saved workspace catalog.
            for current in state()["tabs"]:
                for current_pane in current["panes"]:
                    rpc("close", pane=current_pane["id"])
            assert state()["tabs"] == []
            assert [w["id"] for w in state()["workspaces"]] == [b]
            rpc("action", name="manage-workspaces")
            time.sleep(0.35)
            capture("workspaces-management")
        finally:
            stop(app)

        app = start(log)
        try:
            assert state()["tabs"] == []
            assert [w["id"] for w in state()["workspaces"]] == [b]
            rpc("filter", workspace=b)
            rpc("action", name="new")
            restored = state()["tabs"][0]
            assert restored["panes"][0]["directory"] == str(other)
            rpc("split", pane=restored["active_pane"])
            restored = tab(restored["id"])
            newest = rpc("new", name="Newest", workspace=b)
            saved_order = [t["id"] for t in state()["tabs"]]
            assert saved_order == [newest["tab"], restored["id"]]
            rpc("focus", tab=restored["id"])
        finally:
            stop(app)

        app = start(log)
        try:
            assert [t["id"] for t in state()["tabs"]] == saved_order
            actual_layout = tab(restored["id"])["layout"]
            assert actual_layout == restored["layout"], (restored["layout"], actual_layout)
            assert tab(restored["id"])["workspace"] == b
            assert state()["filter"] == {"workspace": b}
            assert state()["active_tab"] == restored["id"]
            capture("workspaces-restored")
            actual_layout = tab(restored["id"])["layout"]
            assert actual_layout == restored["layout"], (restored["layout"], actual_layout)
            # Hidden rows must not shift which visible neighbor takes focus.
            following = rpc("new", workspace=b)
            middle = rpc("new", workspace=b)
            rpc("new", workspace=None)
            rpc("new", workspace=b)
            rpc("filter", workspace=b)
            rpc("focus", tab=middle["tab"])
            rpc("close", pane=middle["pane"])
            assert state()["active_tab"] == following["tab"]
        finally:
            stop(app)
        print("PASS: persistent workspace catalog, flat All/No Workspace/workspace filters, saved default directories, stable membership across cd, moves without changing splits/unread state, revealing hidden panes, deletion without stopping processes, empty session restart, and filtered session restore")
