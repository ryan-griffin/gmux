#!/usr/bin/env python3
"""Exercise GTK's opt-in local execution alongside direct remote connections."""
import json
from pathlib import Path
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Atspi, Gdk

from cargo_test_support import cargo_binary
from daemon_test_support import stop_daemon
from test_support import AppFixture, AccessibilityFixture, descendants, scenario, wait_accessible, wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
daemon, gtk = cargo_binary("gmux"), cargo_binary("gmux-gtk")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "local-execution-smoke.log").open("w") as log, \
        AppFixture(env={"GTK_A11Y": "atspi"}) as local, AppFixture() as server, \
        AccessibilityFixture(), WaylandInput() as native:
    # Exercise Settings::default, rather than setting the opt-out explicitly.
    local.settings_path.unlink()
    app = local.start(gtk, stdout=log, stderr=log)
    wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))

    def objects():
        return [obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_state_set().contains(Atspi.StateType.SHOWING)]

    def find(name, role=None):
        return next((obj for obj in objects() if obj.get_name() == name
                     and (role is None or obj.get_role_name() == role)), None)

    def widget(name, role=None):
        return wait_accessible(lambda: find(name, role), message=f"Missing {name} ({role})")

    def key(symbol):
        native.key(symbol, True)
        native.key(symbol, False)

    def click(name, role=None):
        def ready():
            obj = find(name, role)
            return obj if obj and obj.get_state_set().contains(Atspi.StateType.SENSITIVE) else None

        obj = wait_accessible(ready, message=f"{name} did not become sensitive")
        action = obj.get_action_iface()
        if action and Atspi.Action.get_n_actions(action):
            assert Atspi.Action.do_action(action, 0), name
        else:
            for _ in range(24):
                obj.clear_cache()
                if obj.get_state_set().contains(Atspi.StateType.FOCUSED):
                    key(Gdk.KEY_space)
                    return
                key(Gdk.KEY_Tab)
                time.sleep(.04)
            raise AssertionError(f"Could not focus {name}")

    def manager_page():
        wait_accessible(lambda: not find("Local Connection", "dialog"))
        widget("Connections", "dialog")
        widget("Add Connection")

    def manager():
        if not find("Connections", "dialog"):
            local.rpc("action", name="manage-connections")
        manager_page()

    def editor():
        manager()
        click(local.rpc("connection-list")["local"]["name"], "list item")
        widget("Local Execution", "switch")

    def close_manager():
        dialog = widget("Connections", "dialog")
        close = next(obj for obj in descendants(dialog)
                     if obj.get_name() == "Close" and obj.get_role_name() == "button"
                     and obj.get_state_set().contains(Atspi.StateType.SHOWING))
        assert Atspi.Action.do_action(close.get_action_iface(), 0)
        wait_accessible(lambda: not find("Connections", "dialog"))

    def save():
        click("Save", "button")
        manager_page()

    def no_execution_files():
        assert not Path(local.env["GMUX_SOCKET"]).exists()
        assert not (local.config / "daemon-id.json").exists()
        assert not (local.config / "execution.json").exists()

    with scenario("fresh GTK starts without a daemon and workspaces work without connections"):
        widget("No Connections", "label")
        no_execution_files()
        workspace = local.rpc("workspace-add", name="Offline project")["workspace"]
        assert local.rpc("list")["workspaces"][0]["id"] == workspace
        no_execution_files()
        before = Path(f"/proc/{app.pid}/stat").read_text().split()
        time.sleep(1)
        after = Path(f"/proc/{app.pid}/stat").read_text().split()
        status = Path(f"/proc/{app.pid}/status").read_text().splitlines()
        (artifacts / "local-execution-idle.json").write_text(json.dumps(dict(
            cpu_ticks=sum(map(int, after[13:15])) - sum(map(int, before[13:15])),
            memory=[line for line in status if line.startswith(("VmRSS:", "Threads:"))]), indent=2) + "\n")

    with scenario("local settings and hosting can be saved without starting execution"):
        editor()
        click("Allow Remote Connections", "switch")
        entry = widget("Listening Address", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "127.0.0.1:0")
        save()
        assert local.rpc("connection-list")["hosting"]["enabled"]
        assert local.rpc("connection-list")["local"]["daemon"] is None
        no_execution_files()

    with scenario("runtime actions preserve drafts until Save or Cancel"):
        editor()
        start = widget("Start Daemon", "button")
        assert start.get_state_set().contains(Atspi.StateType.SENSITIVE)
        original = local.rpc("connection-list")["local"]["name"]
        entry = widget("Name", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "unsaved-name")
        wait_accessible(lambda: not start.get_state_set().contains(Atspi.StateType.SENSITIVE))
        no_execution_files()
        click("Cancel", "button")
        manager_page()
        assert local.rpc("connection-list")["local"]["name"] == original
        editor()
        start = widget("Start Daemon", "button")
        entry = widget("Listening Address", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "invalid")
        wait_accessible(lambda: not start.get_state_set().contains(Atspi.StateType.SENSITIVE))
        assert not widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        click("Cancel", "button")
        manager_page()

    with scenario("enabling local execution starts it and remotes connect independently"):
        editor()
        click("Local Execution", "switch")
        save()
        wait_for(lambda: local.rpc("daemon-info")["attached"])
        local_id = local.rpc("_hello", client=False)["daemon"]
        original = local.rpc("new", name="Local job", workspace=workspace)
        server.rpc("connection-edit", connection="local", hosting=dict(
            enabled=True, address="127.0.0.1:0", authentication=dict(mode="none")))
        server.start(daemon, "serve", stdout=log, stderr=log)
        address = wait_for(lambda: server.rpc("daemon-info")["listen"])
        remote = server.rpc("new", name="Remote job")
        local.rpc("connection-add", name="server", address=address, authentication={"mode": "none"})
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)

    with scenario("local daemon loss keeps every window and remote connection open"):
        close_manager()
        first_window = local.rpc("windows")["windows"][0]["id"]
        second_window = local.rpc("new-window")["window"]
        wait_for(lambda: local.rpc("daemon-info")["frontends"] == 2)
        wait_for(lambda: len(local.rpc("list", window=second_window)["tabs"]) == 2)
        stop_daemon(local.env["GMUX_SOCKET"])
        for window in (first_window, second_window):
            wait_for(lambda: any(tab["reference"]["daemon"] == local_id and not tab["connected"]
                                 for tab in local.rpc("list", window=window)["tabs"]))
        assert app.poll() is None
        assert server.rpc("list")["tabs"][0]["id"] == remote["tab"]
        editor()
        click("Start Daemon")
        manager_page()
        wait_for(lambda: local.rpc("daemon-info")["frontends"] == 2)

    with scenario("disabling local execution leaves its daemon and jobs running"):
        editor()
        click("Local Execution", "switch")
        save()
        wait_for(lambda: local.rpc("daemon-info")["attached"] == 0)
        assert local.rpc("list", client=False)["tabs"][0]["id"] == original["tab"]
        wait_for(lambda: [tab["name"] for tab in local.rpc("list")["tabs"]] == ["Remote job"])
        assert app.poll() is None
        for window in (first_window, second_window):
            assert [tab["name"] for tab in local.rpc("list", window=window)["tabs"]] == ["Remote job"]
        close_manager()
        local.rpc("detach", window=second_window)
        wait_for(lambda: len(local.rpc("windows")["windows"]) == 1)

    with scenario("stopping the local daemon keeps GTK and remote terminals open"):
        editor()
        click("Stop Daemon…")
        click("Stop Daemon", "button")
        wait_for(lambda: not Path(local.env["GMUX_SOCKET"]).exists())
        manager_page()
        assert app.poll() is None
        assert server.rpc("list")["tabs"][0]["id"] == remote["tab"]
        assert [tab["name"] for tab in local.rpc("list")["tabs"]] == ["Remote job"]

    with scenario("starting again restores local identity and the saved preference survives reopening"):
        editor()
        click("Start Daemon")
        manager_page()
        wait_for(lambda: local.rpc("daemon-info")["attached"])
        assert local.rpc("_hello", client=False)["daemon"] == local_id
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)
        assert next(tab for tab in local.rpc("list")["tabs"]
                    if tab["reference"]["daemon"] == local_id)["workspace"] == workspace
        wait_for(lambda: json.loads(local.settings_path.read_text())["local_execution"])
        if find("Connections", "dialog"):
            close_manager()
        local.rpc("detach")
        app.wait(timeout=8)
        stop_daemon(local.env["GMUX_SOCKET"])
        app = local.start(gtk, stdout=log, stderr=log)
        wait_for(lambda: local.rpc("daemon-info")["attached"])
        assert local.rpc("_hello", client=False)["daemon"] == local_id
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)

    with scenario("an existing local daemon stays unused when local execution is disabled"):
        editor()
        click("Local Execution", "switch")
        save()
        wait_for(lambda: local.rpc("daemon-info")["attached"] == 0)
        if find("Connections", "dialog"):
            close_manager()
        local.rpc("detach")
        app.wait(timeout=8)
        app = local.start(gtk, "--connection", "local", stdout=log, stderr=log)
        wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))
        wait_for(lambda: [tab["name"] for tab in local.rpc("list")["tabs"]] == ["Remote job"])
        assert local.rpc("daemon-info")["attached"] == 0
        assert local.rpc("_state")["daemon"] is None
        assert "error" in local.rpc("new", check=False)
        close_remote = local.start(gtk, "close-tab", "--connection", "server", "--tab", str(remote["tab"]), stdout=log, stderr=log)
        assert close_remote.wait(timeout=8) == 0
        wait_for(lambda: local.rpc("list")["tabs"] == [])
        server.rpc("quit")
        widget("Not Connected", "label")
        widget("Manage Connections…", "button")
        if find("Connections", "dialog"):
            close_manager()
        quit_client = local.start(gtk, "quit", stdout=log, stderr=log)
        assert quit_client.wait(timeout=8) == 0
        app.wait(timeout=8)
        assert local.rpc("daemon-info")["attached"] == 0


print("PASS: optional local execution, offline configuration, independent remotes, and daemon lifecycle")
