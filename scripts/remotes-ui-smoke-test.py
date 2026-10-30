#!/usr/bin/env python3
"""Operate connection management menus, forms, and reconnection in native GTK."""
import json
from pathlib import Path
import shlex
import socket
import tempfile
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Atspi, Gdk, GLib

from cargo_test_support import cargo_binary
from daemon_test_support import stop_daemon
from test_support import AppFixture, AccessibilityFixture, descendants, scenario, wait_accessible, wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
daemon, cli = cargo_binary("gmux"), cargo_binary("gmux-gtk")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


with (artifacts / "remotes-ui-smoke.log").open("w") as log, \
        AppFixture(dict(desktop_notifications=False), env={"GTK_A11Y": "atspi"}) as local, \
        AppFixture() as server, AppFixture() as secured, AccessibilityFixture(), WaylandInput() as native, \
        tempfile.TemporaryDirectory(prefix="gmux-token-", dir=artifacts) as token_directory:
    for fixture in (local, server, secured):
        home = fixture.path / "home"
        home.mkdir()
        fixture.env["HOME"] = str(home)
    token = secured.path / "token"
    token.write_text("a" * 64 + "\n")
    server.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    secured.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="token", file=str(token))))
    local.start(daemon, "serve", cwd=local.path, stdout=log, stderr=log)
    remote = server.start(daemon, "serve", stdout=log, stderr=log)
    secured.start(daemon, "serve", stdout=log, stderr=log)
    address = wait_for(lambda: server.rpc("status")["bound_address"])
    secure_address = wait_for(lambda: secured.rpc("status")["bound_address"])
    kinds = {fixture: fixture.rpc("status")["machine_kind"] for fixture in (local, server, secured)}
    kind_labels = dict(unknown="Computer", desktop="Desktop", laptop="Laptop", convertible="Convertible",
                       server="Server", tablet="Tablet", phone="Phone", container="Container",
                       watch="Watch", embedded="Embedded Computer",
                       **{"virtual-machine": "Virtual Machine"})
    hostname = local.rpc("connection-list")["hostname"]
    local_id = local.rpc("_hello")["daemon"]
    pane = server.rpc("new", name="Remote job")["pane"]
    app = local.start_app(cli, cwd=artifacts.parent, stdout=log, stderr=log)

    def objects():
        return [obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_state_set().contains(Atspi.StateType.SHOWING)]

    def find(name, role=None):
        return next((obj for obj in objects() if obj.get_name() == name
                     and (role is None or obj.get_role_name() == role)), None)

    def widget(name, role=None):
        try:
            editors = {
                "Connections": {"Local Connection", "Add Connection", "Edit Connection"},
                "Workspaces": {"Add Workspace", "Edit Workspace"},
            }
            if role == "dialog" and name in editors:
                wait_accessible(lambda: not any(obj.get_role_name() == "dialog"
                    and obj.get_name() in editors[name] for obj in objects()),
                    message=f"The editor over {name} did not close")
            return wait_accessible(lambda: find(name, role), message=f"Missing visible widget: {name} ({role})")
        except AssertionError:
            print([(obj.get_role_name(), obj.get_name()) for obj in objects()], flush=True)
            raise

    def activate(obj):
        name = obj.get_name()
        assert obj.get_state_set().contains(Atspi.StateType.SENSITIVE), name
        action = obj.get_action_iface()
        if action and Atspi.Action.get_n_actions(action):
            assert Atspi.Action.do_action(action, 0), (name, obj.get_role_name())
        else:
            keyboard_focus(name, obj.get_role_name())
            key(Gdk.KEY_Return)

    def click(name, role=None):
        activate(widget(name, role))

    def close_dialog(title):
        dialog = widget(title, "dialog")
        close = next(obj for obj in descendants(dialog)
                     if obj.get_name() == ("Close" if title in {"Connections", "Workspaces"} else "Cancel")
                     and obj.get_role_name() == "button"
                     and obj.get_state_set().contains(Atspi.StateType.SHOWING))
        activate(close)
        wait_accessible(lambda: not find(title, "dialog"))

    def key(symbol):
        native.key(symbol, True)
        native.key(symbol, False)

    def chord(modifier, symbol):
        native.key(modifier, True)
        key(symbol)
        native.key(modifier, False)

    def type_text(text):
        for char in text:
            key(Gdk.unicode_to_keyval(ord(char)))

    def fill(name, text):
        obj = widget(name, "text")
        assert Atspi.EditableText.set_text_contents(obj.get_editable_text_iface(), text), name

    def save_enabled(expected):
        wait_accessible(lambda: widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE) == expected,
                        message=f"Save sensitivity did not become {expected}")

    def choose_authentication(token_file):
        keyboard_focus("Authentication", "combo box")
        key(Gdk.KEY_space)
        widget("Token", "label")
        key(Gdk.KEY_End if token_file else Gdk.KEY_Home)
        time.sleep(.05)
        key(Gdk.KEY_Return)
        wait_accessible(lambda: bool(find("Token File", "text")) == token_file)

    def keyboard_focus(name, role):
        obj = widget(name, role)
        for _ in range(16):
            obj.clear_cache()
            if obj.get_state_set().contains(Atspi.StateType.FOCUSED):
                return
            key(Gdk.KEY_Tab)
            time.sleep(.05)
        raise AssertionError(f"Keyboard navigation did not focus {name}")

    def shown_error(fragment):
        return any(fragment in obj.get_name() for obj in objects() if obj.get_role_name() == "label")

    def creation_menu(disconnected=(), *, submenu=False):
        widget("New Tab via Connection", "menu item" if submenu else "label")
        registry = local.rpc("connection-list")
        expected = [registry["local"]["name"]] + sorted(remote["name"] for remote in registry["remotes"])
        for name in expected:
            widget(name, "menu item")
            wait_accessible(lambda: widget(name, "menu item").get_state_set().contains(Atspi.StateType.SENSITIVE)
                            == (name not in disconnected), message=f"Incorrect connection availability: {name}")
        items = [obj.get_name() for obj in objects() if obj.get_role_name() == "menu item"
                 and obj.get_name() != "New Tab via Connection"]
        assert items == expected, items

    def open_manager():
        if find("Connections", "dialog"):
            widget("Connections", "dialog")
            widget("Add Connection")
            return
        click("Main Menu", "toggle button")
        click("Manage Connections…")
        widget("Add Connection")

    def screenshot(name):
        # Let dialog and focus scrolling animations finish before inspecting the layout.
        time.sleep(.6)
        frame = native.frame(timeout=.2, fresh=False)
        texture = Gdk.MemoryTexture.new(frame.width, frame.height, Gdk.MemoryFormat.R8G8B8A8,
                                       GLib.Bytes.new(frame.data), frame.stride)
        assert texture.save_to_png(str(artifacts / name))

    def dialog_size(title):
        obj = widget(title, "dialog")
        bounds = Atspi.Component.get_extents(obj.get_component_iface(), Atspi.CoordType.WINDOW)
        return bounds.width, bounds.height

    def idle_resources():
        status = Path(f"/proc/{app.pid}/status").read_text().splitlines()
        return {key.rstrip(":"): int(next(line.split()[1] for line in status if line.startswith(key)))
                for key in ("VmRSS:", "Threads:")}

    def cpu_ticks():
        fields = Path(f"/proc/{app.pid}/stat").read_text().split()
        return int(fields[13]) + int(fields[14])

    with scenario("workspace creation from the menu and command palette opens only its editor"):
        click("Tab Filter", "toggle button")
        click("Add Workspace…", "menu item")
        widget("Add Workspace", "dialog")
        assert not find("Workspaces", "dialog")
        assert not find("Back", "button")
        fill("Name", "Uncommitted workspace")
        local.rpc("action", name="new-workspace")
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == "Uncommitted workspace"
        click("Cancel", "button")
        wait_accessible(lambda: not find("Add Workspace", "dialog"))
        assert not find("Workspaces", "dialog")
        assert local.rpc("list")["workspaces"] == []

        local.rpc("action", name="palette")
        widget("Commands", "dialog")
        click("Add Workspace…", "list item")
        widget("Add Workspace", "dialog")
        wait_accessible(lambda: not find("Commands", "dialog"))
        assert not find("Workspaces", "dialog")
        fill("Name", "Palette workspace")
        click("Add", "button")
        wait_accessible(lambda: not find("Add Workspace", "dialog"))
        assert not find("Workspaces", "dialog")
        created = next(w["id"] for w in local.rpc("list")["workspaces"] if w["name"] == "Palette workspace")
        local.rpc("workspace-remove", workspace=created)

    with scenario("the local row edits its name while the workspace filter stays unchanged"):
        local.rpc("filter", value="all")
        widget("No Tabs", "label")
        open_manager()
        widget("This Computer", "label")
        widget("Connected", "label")
        machines_size = dialog_size("Connections")
        assert not find(f"Remove {hostname}")
        local_retry = widget(f"Retry connection to {hostname} now", "button")
        assert not local_retry.get_state_set().contains(Atspi.StateType.SENSITIVE)
        click(hostname, "list item")
        widget("Local Connection", "dialog")
        assert dialog_size("Local Connection") == machines_size
        assert not find("Address", "text")
        widget("Allow Remote Connections", "switch")
        widget("Listening Address", "text")
        widget("Authentication", "combo box")
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == hostname
        assert not widget("Use Hostname", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        assert not find("Remove Connection…", "button")
        save_enabled(False)
        fill("Name", "cancelled")
        save_enabled(True)
        fill("Name", hostname)
        save_enabled(False)
        fill("Name", hostname + " ")
        save_enabled(False)
        fill("Name", "cancelled")
        local.rpc("action", name="manage-connections")
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == "cancelled"
        assert len([obj for obj in objects() if obj.get_name() == "Local Connection"
                    and obj.get_role_name() == "dialog"]) == 1
        close_dialog("Local Connection")
        widget("Connections", "dialog")
        close_dialog("Connections")
        assert not find("Connections", "dialog"), "Close should dismiss the whole manager"
        assert local.rpc("connection-list")["local"]["name"] == hostname
        open_manager()
        click(hostname, "list item")
        fill("Name", "desktop")
        keyboard_focus("Save", "button")
        screenshot("edit-local-machine.png")
        click("Save", "button")
        wait_accessible(lambda: not find("Local Connection", "dialog"))
        widget("No Tabs", "label")
        assert local.rpc("list")["filter"] == "all"
        assert json.loads((local.config / "connections.json").read_text())["local_name"] == "desktop"
        open_manager()
        click("desktop", "list item")
        fill("Name", "")
        assert not widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        click("Use Hostname", "button")
        wait_accessible(lambda: Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == hostname)
        assert not widget("Use Hostname", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        assert widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        click("Cancel", "button")
        widget("Connections", "dialog")
        assert local.rpc("connection-list")["local"]["name"] == "desktop", "Reset must wait for Save"
        click("desktop", "list item")
        click("Use Hostname", "button")
        screenshot("reset-machine-name.png")
        click("Save", "button")
        wait_accessible(lambda: not find("Local Connection", "dialog"))
        widget("No Tabs", "label")
        assert "local_name" not in json.loads((local.config / "connections.json").read_text())
        local.rpc("filter", value="all")

    with scenario("the local editor saves live remote access and Cancel returns to the same manager"):
        open_manager()
        click(hostname, "list item")
        widget("Local Connection", "dialog")
        assert not find("Back", "button")
        save_enabled(False)
        original_address = local.rpc("connection-list")["remote_access"]["listen_address"]
        assert Atspi.Text.get_text(widget("Listening Address", "text").get_text_iface(), 0, -1) == "127.0.0.1"
        click("Allow Remote Connections", "switch")
        save_enabled(True)
        fill("Listening Address", "invalid")
        save_enabled(False)
        fill("Listening Address", "127.0.0.1")
        save_enabled(True)
        fill("Listening Address", original_address)
        save_enabled(True)
        choose_authentication(True)
        save_enabled(False)
        fill("Token File", str(token))
        save_enabled(True)
        choose_authentication(False)
        click("Allow Remote Connections", "switch")
        save_enabled(False)
        click("Allow Remote Connections", "switch")
        fill("Listening Address", address)
        saved = (local.config / "connections.json").read_bytes()
        click("Save", "button")
        wait_accessible(lambda: shown_error("Address already in use"))
        assert (local.config / "connections.json").read_bytes() == saved
        fill("Listening Address", "127.0.0.1:0")
        click("Save", "button")
        widget("Connections", "dialog")
        assert local.rpc("connection-list")["remote_access"]["enabled"]
        assert "local_name" not in json.loads((local.config / "connections.json").read_text()), "Saving default settings pinned the hostname"
        listening = local.rpc("status")["bound_address"]
        with socket.create_connection(("127.0.0.1", int(listening.rsplit(":", 1)[1])), timeout=4) as peer:
            peer.sendall((json.dumps(dict(version=local.rpc("_hello")["version"], channel="control")) + "\n").encode())
            reader = peer.makefile("rb")
            assert json.loads(reader.readline())["daemon"] == local_id
            click(hostname, "list item")
            assert Atspi.Text.get_text(widget("Listening Address", "text").get_text_iface(), 0, -1) == "127.0.0.1:0"
            click("Allow Remote Connections", "switch")
            save_enabled(True)
            click("Save", "button")
            widget("Connections", "dialog")
            assert reader.readline() == b"", "Disabling remote access retained an authenticated connection"
            reader.close()
        assert local.rpc("status")["bound_address"] is None
        assert not local.rpc("connection-list")["remote_access"]["enabled"]
        click(hostname, "list item")
        assert not widget("Allow Remote Connections", "switch").get_state_set().contains(Atspi.StateType.CHECKED)
        key(Gdk.KEY_Escape)
        widget("Connections", "dialog")
        widget("Add Connection")
        wait_accessible(lambda: len([obj for obj in objects() if obj.get_role_name() == "dialog"]) == 1,
                        message="Cancel did not return to one management dialog")

    with scenario("menus open one manager and empty forms require their fields"):
        open_manager()
        assert local.rpc("connection-list")["remotes"] == []
        local.rpc("action", name="manage-connections")
        assert len([obj for obj in objects() if obj.get_name() == "Connections"
                    and obj.get_role_name() == "dialog"]) == 1
        initial_resources = idle_resources()
        click("Add Connection")
        widget("Add Connection", "dialog")
        assert not find("Back", "button")
        assert not widget("Add", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        assert not find("Token File", "text"), "Token input should be hidden for None"
        assert dialog_size("Add Connection") == machines_size
        screenshot("add-remote.png")
        fill("Name", "invalid name")
        fill("Address", address)
        click("Add", "button")
        wait_accessible(lambda: shown_error("Connection name must contain"))
        assert local.rpc("connection-list")["remotes"] == [], "A failed form saved a definition"
        fill("Name", "server")
        fill("Address", "127.0.0.1:0")
        click("Add", "button")
        wait_accessible(lambda: shown_error("Connection refused"))
        assert local.rpc("connection-list")["remotes"] == []

    with scenario("None authentication and a blank name use the remote hostname"):
        fill("Address", address)
        fill("Name", "")
        click("Add", "button")
        wait_accessible(lambda: not find("Add Connection", "dialog"))
        default_name = hostname[:62] + "-2"
        assert local.rpc("connection-list")["remotes"][0]["name"] == default_name
        open_manager()
        click(default_name, "list item")
        fill("Name", "server")
        click("Save", "button")
        wait_accessible(lambda: not find("Edit Connection", "dialog"))
        open_manager()
        widget("server", "list item")
        widget(kind_labels[kinds[server]], "image")
        wait_for(lambda: any(t["connection"] == "server" and t["connected"] for t in local.rpc("list")["tabs"]))
        widget("Connected", "label")
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Add Connection"))
        assert local.rpc("connection-list")["remotes"][0]["authentication"] == {"mode": "none"}
        open_manager()

    with scenario("token authentication exposes a file chooser and rejects a wrong token"):
        click("Add Connection")
        fill("Name", "secured")
        fill("Address", secure_address)
        click("Add", "button")
        wait_accessible(lambda: shown_error("Authentication failed"))
        keyboard_focus("Authentication", "combo box")
        key(Gdk.KEY_space)
        widget("Token", "label")
        key(Gdk.KEY_End)
        time.sleep(.05)
        key(Gdk.KEY_Return)
        widget("Token File", "text")
        assert not widget("Add", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        wrong = local.path / "wrong-token"
        wrong.write_text("b" * 64)
        fill("Token File", str(wrong))
        click("Add", "button")
        wait_accessible(lambda: shown_error("Authentication failed"))
        click("Choose Token File", "button")
        widget("Open", "button")
        chord(Gdk.KEY_Control_L, Gdk.KEY_l)
        type_text(str(token))
        key(Gdk.KEY_Return)
        wait_accessible(lambda: not find("Open", "button"))
        wait_accessible(lambda: Atspi.Text.get_text(widget("Token File", "text").get_text_iface(), 0, -1) == str(token))
        assert dialog_size("Add Connection") == machines_size
        screenshot("add-remote-token.png")
        click("Add", "button")
        wait_accessible(lambda: not find("Add Connection", "dialog"))
        open_manager()
        widget("secured", "list item")
        wait_for(lambda: secured.rpc("status")["remote_clients"] == 1)
        screenshot("machines.png")
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Connections", "dialog"))
        workspace = local.rpc("workspace-add", name="Work", directory=str(local.path))["workspace"]
        local.rpc("action", name="manage-workspaces")
        widget("Add Workspace")
        workspaces_size = dialog_size("Workspaces")
        screenshot("workspaces.png")
        click("Work", "list item")
        widget("Edit Workspace", "dialog")
        assert dialog_size("Edit Workspace") == workspaces_size
        assert Atspi.Text.get_text(widget(f"Directory on {hostname}", "text").get_text_iface(), 0, -1) == str(local.path)
        widget("Choose Directory", "button")
        screenshot("edit-workspace.png")
        assert not find("Back", "button")
        fill("Name", "Discarded")
        click("Cancel", "button")
        widget("Workspaces", "dialog")
        widget("Work", "list item")
        click("Add Workspace")
        assert dialog_size("Add Workspace") == workspaces_size
        assert not find("Remove Workspace…", "button")
        fill("Name", "Temporary")
        fill(f"Directory on {hostname}", "")
        assert not widget("Add", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        fill("Name", "")
        picked_directory = local.path / "picked directory"
        picked_directory.mkdir()
        click("Choose Directory", "button")
        widget("Select", "button")
        chord(Gdk.KEY_Control_L, Gdk.KEY_l)
        type_text(str(picked_directory))
        click("Select", "button")
        wait_accessible(lambda: not find("Select", "button"))
        assert Atspi.Text.get_text(widget(f"Directory on {hostname}", "text").get_text_iface(), 0, -1) == str(picked_directory)
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == picked_directory.name
        fill("Name", "Temporary")
        fill(f"Directory on {hostname}", str(local.path / "missing directory"))
        click("Add", "button")
        wait_accessible(lambda: shown_error("Choose an existing directory"))
        assert all(w["name"] != "Temporary" for w in local.rpc("list")["workspaces"])
        typed_directory = Path(local.env["HOME"]) / "typed directory"
        typed_directory.mkdir()
        fill(f"Directory on {hostname}", "~/typed directory")
        screenshot("add-workspace.png")
        click("Add", "button")
        widget("Workspaces", "dialog")
        widget("Temporary", "list item")
        temporary = next(w["id"] for w in local.rpc("list")["workspaces"] if w["name"] == "Temporary")
        assert next(w for w in local.rpc("list")["workspaces"] if w["id"] == temporary)["directories"][local_id] == str(typed_directory)
        click("Temporary", "list item")
        assert Atspi.Text.get_text(widget(f"Directory on {hostname}", "text").get_text_iface(), 0, -1) == "~/typed directory"
        fill(f"Directory on {hostname}", str(picked_directory))
        fill("Name", "Renamed workspace")
        keyboard_focus("Name", "text")
        key(Gdk.KEY_Return)
        widget("Workspaces", "dialog")
        widget("Renamed workspace", "list item")
        assert next(w for w in local.rpc("list")["workspaces"] if w["id"] == temporary)["directories"][local_id] == str(picked_directory)
        local.rpc("workspace-edit", workspace=temporary, name="External rename", directory=str(Path(local.env["HOME"])))
        widget("External rename", "list item")
        assert not find("Renamed workspace", "list item")
        temporary_pane = local.rpc("new", workspace=temporary)["pane"]
        click("External rename", "list item")
        click("Remove Workspace…", "button")
        confirmation = widget("Remove External rename?", "alert")
        activate(next(obj for obj in descendants(confirmation)
                      if obj.get_name() == "Cancel" and obj.get_role_name() == "button"))
        wait_accessible(lambda: not find("Remove External rename?", "alert"))
        assert any(w["id"] == temporary for w in local.rpc("list")["workspaces"])
        click("Remove Workspace…", "button")
        confirmation = widget("Remove External rename?", "alert")
        activate(next(obj for obj in descendants(confirmation)
                      if obj.get_name() == "Remove Workspace" and obj.get_role_name() == "button"))
        widget("Workspaces", "dialog")
        wait_accessible(lambda: not find("External rename", "list item"))
        kept = next(t for t in local.rpc("list")["tabs"] if t["active_pane"] == temporary_pane)
        assert kept["workspace"] is None, "Removing a workspace should leave its tabs open"
        assert "error" not in local.rpc("read", pane=temporary_pane)
        local.rpc("close", pane=temporary_pane)
        click("Work", "list item")
        fill("Name", "Unsaved close")
        close_dialog("Edit Workspace")
        widget("Workspaces", "dialog")
        close_dialog("Workspaces")
        assert not find("Workspaces", "dialog")
        assert next(w for w in local.rpc("list")["workspaces"] if w["id"] == workspace)["name"] == "Work"
        local.rpc("action", name="manage-workspaces")
        widget("Workspaces", "dialog")
        # Presentation can still be animating when the accessible dialog appears.
        screenshot("workspaces.png")
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Workspaces", "dialog"))
        saved = next(r for r in local.rpc("connection-list")["remotes"] if r["name"] == "secured")
        assert saved["authentication"] == {"mode": "token", "file": str(token)}
        assert saved["machine_kind"] == kinds[secured]

    with scenario("clicking a remote opens an editor dialog and Cancel preserves its saved settings"):
        catalog = (local.config / "connections.json").read_bytes()
        open_manager()
        click("server", "list item")
        widget("Edit Connection", "dialog")
        assert not find("Back", "button")
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == "server"
        assert Atspi.Text.get_text(widget("Address", "text").get_text_iface(), 0, -1) == address
        assert not find("Token File", "text")
        save_enabled(False)
        fill("Name", "server ")
        save_enabled(False)
        keyboard_focus("Name", "text")
        key(Gdk.KEY_Return)
        widget("Edit Connection", "dialog")
        assert (local.config / "connections.json").read_bytes() == catalog
        fill("Name", "server")
        fill("Address", "127.0.0.1:1")
        save_enabled(True)
        fill("Address", address)
        save_enabled(False)
        choose_authentication(True)
        save_enabled(False)
        fill("Token File", str(token))
        save_enabled(True)
        choose_authentication(False)
        save_enabled(False)
        screenshot("edit-remote.png")
        fill("Name", "cancelled")
        save_enabled(True)
        click("Cancel", "button")
        wait_accessible(lambda: not find("Edit Connection", "dialog"))
        assert (local.config / "connections.json").read_bytes() == catalog

    with scenario("saving a rename preserves the selected machine, tab, workspace, and execution"):
        local.rpc("filter", value="all")
        local.rpc("focus", connection="server", pane=pane)
        local.rpc("move", workspace=workspace)
        local.rpc("filter", workspace=workspace)
        before = local.rpc("list")
        assert "desktop" not in server.rpc("_state")
        open_manager()
        click("server", "list item")
        fill("Name", "secured")
        click("Save", "button")
        wait_accessible(lambda: shown_error("Connection name is already in use"))
        assert (local.config / "connections.json").read_bytes() == catalog
        fill("Name", "build-box")
        click("Save", "button")
        wait_accessible(lambda: not find("Edit Connection", "dialog"))
        wait_for(lambda: any(t["connection"] == "build-box" for t in local.rpc("list")["tabs"]))
        after = local.rpc("list")
        assert after["active_reference"] == before["active_reference"]
        assert after["filter"] == before["filter"]
        assert next(t for t in after["tabs"] if t["reference"] == after["active_reference"])["workspace"] == workspace
        assert "desktop" not in server.rpc("_state")
        assert server.rpc("status")["remote_clients"] == 1
        local.rpc("send", text="printf '\\nEDITED_REMOTE_INPUT\\n'", paste=True, enter=True)
        wait_for(lambda: "EDITED_REMOTE_INPUT" in server.rpc("read", pane=pane)["text"].splitlines())
        open_manager()
        click("build-box", "list item")
        fill("Address", "127.0.0.1:0")
        click("Save", "button")
        wait_accessible(lambda: shown_error("Connection refused"))
        assert next(r for r in local.rpc("connection-list")["remotes"] if r["name"] == "build-box")["address"] == address
        changed_address = "localhost:" + address.rsplit(":", 1)[1]
        fill("Address", changed_address)
        click("Save", "button")
        wait_accessible(lambda: not find("Edit Connection", "dialog"))
        # Read the displayed viewport and send through its retained terminal widget.
        # A fresh control connection alone would hide a stale stream attachment.
        server.rpc("send", pane=pane, text="printf '\\nADDRESS_EDIT_OUTPUT_OK\\n'", enter=True)
        wait_for(lambda: "ADDRESS_EDIT_OUTPUT_OK" in local.rpc("read")["text"].splitlines())
        local.rpc("send", text="printf '\\nADDRESS_EDIT_INPUT_OK\\n'", paste=True, enter=True)
        wait_for(lambda: "ADDRESS_EDIT_INPUT_OK" in server.rpc("read", pane=pane)["text"].splitlines())
        local.rpc("move", workspace=None)
        local.rpc("connection-edit", connection="build-box", name="server", address=address)
        wait_for(lambda: any(t["connection"] == "server" for t in local.rpc("list")["tabs"]))

    with scenario("token settings are prefilled and edited credentials are checked before saving"):
        open_manager()
        click("secured", "list item")
        assert Atspi.Text.get_text(widget("Token File", "text").get_text_iface(), 0, -1) == str(token)
        save_enabled(False)
        fill("Token File", str(wrong))
        save_enabled(True)
        fill("Token File", str(token))
        save_enabled(False)
        choose_authentication(False)
        save_enabled(True)
        choose_authentication(True)
        save_enabled(False)
        catalog = (local.config / "connections.json").read_bytes()
        fill("Token File", str(wrong))
        click("Save", "button")
        wait_accessible(lambda: shown_error("Authentication failed"))
        assert (local.config / "connections.json").read_bytes() == catalog
        replacement_token = Path(token_directory) / "replacement-token"
        replacement_token.write_text(token.read_text())
        fill("Token File", str(replacement_token.relative_to(artifacts.parent)))
        click("Save", "button")
        wait_accessible(lambda: not find("Edit Connection", "dialog"))
        saved = next(r for r in local.rpc("connection-list")["remotes"] if r["name"] == "secured")
        assert saved["authentication"]["file"] == str(replacement_token)
        assert token.read_text().strip() not in (local.config / "connections.json").read_text()
        open_manager()
        local.rpc("connection-edit", connection="local", name="renamed-local")
        widget("renamed-local", "list item")
        assert not find(hostname, "list item")
        local.rpc("connection-edit", connection="local", name=None)
        widget(hostname, "list item")
        local.rpc("connection-edit", connection="server", address=changed_address)
        wait_accessible(lambda: shown_error(changed_address))
        local.rpc("connection-edit", connection="server", address=address)
        wait_accessible(lambda: not shown_error(changed_address))
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Connections", "dialog"))

    with scenario("plus menu creates a remote's first tab and retains the receiving workspace"):
        local.rpc("filter", value="all")
        own = local.rpc("new", name="Local job", directory=str(local.path), workspace=workspace)
        local.rpc("focus", pane=own["pane"])
        local.rpc("filter", workspace=workspace)
        assert secured.rpc("list")["tabs"] == []
        click("New Tab Options", "toggle button")
        creation_menu()
        widget(hostname, "menu item")
        widget("server", "menu item")
        widget("secured", "menu item")
        # Popovers have their own Wayland surfaces; capture the compositor.
        time.sleep(.25)
        frame = native.frame(timeout=.2, fresh=False)
        texture = Gdk.MemoryTexture.new(frame.width, frame.height, Gdk.MemoryFormat.R8G8B8A8,
                                       GLib.Bytes.new(frame.data), frame.stride)
        assert texture.save_to_png(str(artifacts / "new-tab-machine.png"))
        click("secured", "menu item")

        def selected_remote():
            view = local.rpc("list")
            return next((t for t in view["tabs"] if t["reference"] == view["active_reference"]
                         and t["connection"] == "secured" and t["connected"]), None)

        created = wait_for(selected_remote)
        secure_pane = created["active_pane"]
        assert created["workspace"] == workspace
        assert created["machine_kind"] == kinds[secured]
        widget(kind_labels[kinds[secured]], "image")
        assert local.rpc("list")["filter"] == {"workspace": workspace}
        assert len(local.rpc("_state", client=False)["execution"]["tabs"]) == 1, "Remote creation spawned a local terminal"
        host_tab = secured.rpc("_state")["execution"]["tabs"][0]
        assert "workspace" not in host_tab, "The remote inherited a receiver-only workspace"
        assert secured.rpc("list")["tabs"][0]["panes"][0]["directory"] == str(Path(secured.env["HOME"]))

    with scenario("credential file edits reattach an existing remote terminal"):
        local.rpc("connection-edit", connection="secured", authentication={"mode": "token", "file": str(token)})
        secured.rpc("send", pane=secure_pane, text="printf '\\nCREDENTIAL_EDIT_OUTPUT_OK\\n'", enter=True)
        wait_for(lambda: "CREDENTIAL_EDIT_OUTPUT_OK" in local.rpc("read")["text"].splitlines())
        local.rpc("send", text="printf '\\nCREDENTIAL_EDIT_INPUT_OK\\n'", paste=True, enter=True)
        wait_for(lambda: "CREDENTIAL_EDIT_INPUT_OK" in secured.rpc("read", pane=secure_pane)["text"].splitlines())

    with scenario("remote path labels abbreviate the remote home and preserve raw paths"):
        reference = selected_remote()["reference"]
        local.rpc("rename", connection="secured", tab=reference["id"], name="Remote path labels")

        def path_label(value):
            row = next((obj for obj in objects() if obj.get_role_name() == "list item"
                and any(child.get_name() == "Remote path labels" for child in descendants(obj))), None)
            return row and any(child.get_role_name() == "label" and child.get_name() == value
                               for child in descendants(row))

        wait_accessible(lambda: path_label("~"))
        home = Path(secured.env["HOME"])
        assert selected_remote()["panes"][0]["directory"] == str(home)
        project = home / "nested project"
        project.mkdir()
        local.rpc("send", text="cd " + shlex.quote(str(project)), paste=True, enter=True)
        wait_for(lambda: selected_remote()["panes"][0]["directory"] == str(project))
        wait_accessible(lambda: path_label("~/nested project"))
        # A path equal to the receiving machine's home is literal on the remote.
        local.rpc("send", text="cd " + shlex.quote(local.env["HOME"]), paste=True, enter=True)
        wait_for(lambda: selected_remote()["panes"][0]["directory"] == local.env["HOME"])
        wait_accessible(lambda: path_label(local.env["HOME"]))
        local.rpc("reset-name", connection="secured", tab=reference["id"])

    with scenario("remote creation ignores the selected remote tab's working directory"):
        local.rpc("send", text="cd " + shlex.quote(str(secured.path)), paste=True, enter=True)
        wait_for(lambda: selected_remote()["panes"][0]["directory"] == str(secured.path))
        click("New Tab Options", "toggle button")
        creation_menu()
        click("secured", "menu item")
        created = wait_for(lambda: (tab if (tab := selected_remote())
                                   and tab["active_pane"] != secure_pane else None))
        assert created["workspace"] == workspace
        assert created["panes"][0]["directory"] == str(Path(secured.env["HOME"]))
        assert local.rpc("list")["filter"] == {"workspace": workspace}
        latest_pane = created["active_pane"]

    with scenario("Move to New Workspace prefills the remote machine's current directory"):
        local.rpc("focus", connection="secured", pane=secure_pane)
        wait_for(lambda: selected_remote()["panes"][0]["directory"] == str(secured.path))
        secured.rpc("rename", tab=selected_remote()["reference"]["id"], name="Remote directory source")
        source = wait_accessible(lambda: next((obj for obj in objects()
            if obj.get_role_name() == "list item" and any(child.get_name() == "Remote directory source"
                for child in descendants(obj))), None))
        local.rpc("action", name="search-tabs")
        for _ in range(32):
            source.clear_cache()
            if source.get_state_set().contains(Atspi.StateType.FOCUSED):
                break
            key(Gdk.KEY_Tab)
            time.sleep(.05)
        else:
            raise AssertionError("Keyboard navigation did not focus the remote tab")
        key(Gdk.KEY_Menu)
        click("Move to New Workspace…")
        widget("Add Workspace", "dialog")
        assert Atspi.Text.get_text(widget("Directory on secured", "text").get_text_iface(), 0, -1) == str(secured.path)
        assert Atspi.Text.get_text(widget(f"Directory on {hostname}", "text").get_text_iface(), 0, -1) == "~"
        assert Atspi.Text.get_text(widget("Directory on server", "text").get_text_iface(), 0, -1) == "~"
        fill("Name", "Remote project")
        screenshot("remote-workspace-directory.png")
        click("Add", "button")
        wait_accessible(lambda: not find("Add Workspace", "dialog"))
        assert not find("Workspaces", "dialog")
        remote_workspace = next(w for w in local.rpc("list")["workspaces"] if w["name"] == "Remote project")
        secure_id = secured.rpc("_hello")["daemon"]
        assert remote_workspace["directories"] == {secure_id: str(secured.path)}
        moved = next(t for t in local.rpc("list")["tabs"] if t["active_pane"] == secure_pane and t["connection"] == "secured")
        assert moved["workspace"] == remote_workspace["id"]
        assert "catalog" not in secured.rpc("_state")
        local.rpc("filter", workspace=remote_workspace["id"])
        click("New Tab Options", "toggle button")
        creation_menu()
        click("secured", "menu item")
        created = wait_for(lambda: (tab if (tab := selected_remote()) and tab["active_pane"] != secure_pane else None))
        assert created["workspace"] == remote_workspace["id"]
        assert created["panes"][0]["directory"] == str(secured.path)
        local.rpc("close")
        wait_for(lambda: len(secured.rpc("list")["tabs"]) == 2)
        local.rpc("workspace-remove", workspace=remote_workspace["id"])
        local.rpc("focus", connection="secured", pane=secure_pane)
        local.rpc("move", workspace=workspace)
        local.rpc("focus", connection="secured", pane=latest_pane)
        local.rpc("filter", workspace=workspace)

    with scenario("remote workspace directories can be edited and reset in the manager"):
        remote_directory = Path(secured.env["HOME"]) / "remote project"
        remote_directory.mkdir()
        secured_id = secured.rpc("_hello")["daemon"]
        local.rpc("workspace-edit", workspace=workspace, name="Work",
                  directories={local_id: str(local.path), secured_id: secured.env["HOME"]})
        local.rpc("action", name="manage-workspaces")
        widget("Workspaces", "dialog")
        click("Work", "list item")
        assert Atspi.Text.get_text(widget("Directory on secured", "text").get_text_iface(), 0, -1) == "~"
        fill("Directory on secured", "relative")
        click("Save", "button")
        wait_accessible(lambda: shown_error("Remote directories"))
        fill("Directory on secured", "~/remote project")
        click("Save", "button")
        widget("Workspaces", "dialog")
        definition = next(w for w in local.rpc("list")["workspaces"] if w["id"] == workspace)
        assert definition["directories"] == {local_id: str(local.path), secured.rpc("_hello")["daemon"]: "~/remote project"}
        close_dialog("Workspaces")
        local.rpc("filter", workspace=workspace)
        click("New Tab Options", "toggle button")
        creation_menu()
        click("secured", "menu item")
        created = wait_for(lambda: (tab if (tab := selected_remote()) and tab["active_pane"] != latest_pane else None))
        assert created["panes"][0]["directory"] == str(remote_directory)
        local.rpc("close")
        wait_for(lambda: len(secured.rpc("list")["tabs"]) == 2)
        local.rpc("workspace-edit", workspace=workspace, name="Work",
                  directories={local_id: str(local.path), secured_id: str(remote_directory)})
        local.rpc("action", name="manage-workspaces")
        click("Work", "list item")
        assert Atspi.Text.get_text(widget("Directory on secured", "text").get_text_iface(), 0, -1) == "~/remote project"
        row = widget("Directory on secured", "text").get_parent()
        while row.get_role_name() != "list item":
            row = row.get_parent()
        reset = next(obj for obj in descendants(row) if obj.get_name() == "Use Home Directory" and obj.get_role_name() == "button")
        activate(reset)
        wait_accessible(lambda: Atspi.Text.get_text(widget("Directory on secured", "text").get_text_iface(), 0, -1) == "~")
        click("Save", "button")
        widget("Workspaces", "dialog")
        definition = next(w for w in local.rpc("list")["workspaces"] if w["id"] == workspace)
        assert definition["directories"] == {local_id: str(local.path)}
        close_dialog("Workspaces")
        local.rpc("focus", connection="secured", pane=latest_pane)

    with scenario("repeated workspace editors release their machine directory rows"):
        local.rpc("action", name="manage-workspaces")
        widget("Workspaces", "dialog")
        before_workspace_editors = idle_resources()
        for _ in range(12):
            click("Work", "list item")
            widget("Edit Workspace", "dialog")
            widget("Directory on secured", "text")
            click("Cancel", "button")
            widget("Workspaces", "dialog")
        close_dialog("Workspaces")
        time.sleep(.5)
        after_workspace_editors = idle_resources()
        assert after_workspace_editors["Threads"] <= before_workspace_editors["Threads"], (before_workspace_editors, after_workspace_editors)
        assert after_workspace_editors["VmRSS"] <= before_workspace_editors["VmRSS"] + 2048, (before_workspace_editors, after_workspace_editors)

    with scenario("New Tab uses the workspace filter and opens locally while a remote tab is selected"):
        local.rpc("filter", workspace=workspace)
        assert selected_remote()
        click("Main Menu", "toggle button")
        click("New Tab", "menu item")

        def selected_local():
            view = local.rpc("list")
            return next((t for t in view["tabs"] if t["reference"] == view["active_reference"]
                         and t["reference"]["daemon"] == local_id), None)

        created = wait_for(selected_local)
        assert created["workspace"] == workspace
        assert created["panes"][0]["directory"] == str(local.path)
        assert created["machine_kind"] == kinds[local]
        widget(kind_labels[kinds[local]], "image")
        screenshot("machine-icons-tabs.png")
        assert len(local.rpc("_state", client=False)["execution"]["tabs"]) == 2
        assert len(secured.rpc("list")["tabs"]) == 2

    with scenario("the connection menu uses the local name and keeps the selected workspace"):
        local.rpc("filter", workspace=workspace)
        local.rpc("focus", connection="secured", pane=latest_pane)
        assert selected_remote()
        local.rpc("connection-edit", connection="local", name="desktop")
        wait_for(lambda: any(t["connection"] == "desktop" for t in local.rpc("list")["tabs"]))
        click("New Tab Options", "toggle button")
        creation_menu()
        widget("desktop", "menu item")
        assert not find(hostname, "menu item"), "An old local name remained in the connection menu"
        click("desktop", "menu item")
        created = wait_for(selected_local)
        assert created["workspace"] == workspace
        assert created["panes"][0]["directory"] == str(local.path)
        assert local.rpc("list")["filter"] == {"workspace": workspace}
        assert len(local.rpc("_state", client=False)["execution"]["tabs"]) == 3
        assert len(secured.rpc("list")["tabs"]) == 2
        local.rpc("close", pane=created["active_pane"])
        local.rpc("connection-edit", connection="local", name=None)
        wait_for(lambda: len(local.rpc("_state", client=False)["execution"]["tabs"]) == 2)

    with scenario("the local connection choice uses home in All Tabs regardless of the selected remote tab"):
        local.rpc("filter", value="all")
        local.rpc("focus", connection="secured", pane=latest_pane)
        assert selected_remote()
        server_tabs = len(server.rpc("list")["tabs"])
        click("New Tab Options", "toggle button")
        creation_menu()
        click(hostname, "menu item")
        created = wait_for(selected_local)
        assert created["workspace"] is None and created["visible"]
        assert created["panes"][0]["directory"] == str(Path(local.env["HOME"]))
        assert local.rpc("list")["filter"] == "all"
        assert len(server.rpc("list")["tabs"]) == server_tabs
        local.rpc("close", pane=created["active_pane"])
        wait_for(lambda: len(local.rpc("_state", client=False)["execution"]["tabs"]) == 2)

    with scenario("the filter menu only offers workspaces and removed creation actions stay unavailable"):
        click("Tab Filter", "toggle button")
        widget("All Tabs", "menu item")
        widget("Work", "menu item")
        assert not find("Manage Connections…", "menu item")
        for name in (hostname, "server", "secured"):
            assert not find(name, "menu item"), "A connection remained in the filter menu"
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("All Tabs", "menu item"))
        assert "error" in local.rpc("filter", connection="server", check=False)
        assert local.rpc("list")["filter"] == "all"
        assert "error" in local.rpc("action", name="open-folder", check=False)
        click("New Tab Options", "toggle button")
        creation_menu()
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("New Tab via Connection", "label"))

    with scenario("an explicit remote destination uses home in All Tabs and keeps all hosts visible"):
        local.rpc("filter", value="all")
        local.rpc("focus", pane=own["pane"])
        assert selected_local()
        server_tabs = len(server.rpc("list")["tabs"])
        click("New Tab Options", "toggle button")
        creation_menu()
        click("secured", "menu item")
        created = wait_for(lambda: (tab if (tab := selected_remote())
                                   and tab["active_pane"] != latest_pane else None))
        assert created["workspace"] is None
        assert created["visible"], "The new remote tab was hidden in All Tabs"
        assert len(server.rpc("list")["tabs"]) == server_tabs
        assert len(secured.rpc("list")["tabs"]) == 3
        open_manager()

    with scenario("CLI changes refresh an open manager and removal preserves execution"):
        assert not find("Remove Connection…", "button"), "Remove belongs in the editor dialog"
        click("server", "list item")
        click("Remove Connection…", "button")
        confirmation = widget("Remove server?", "alert")
        activate(next(obj for obj in descendants(confirmation)
                      if obj.get_name() == "Cancel" and obj.get_role_name() == "button"))
        wait_accessible(lambda: not find("Remove server?", "alert"))
        assert len(local.rpc("connection-list")["remotes"]) == 2
        click("Remove Connection…", "button")
        confirmation = widget("Remove server?", "alert")
        activate(next(obj for obj in descendants(confirmation)
                      if obj.get_name() == "Remove Connection" and obj.get_role_name() == "button"))
        widget("Connections", "dialog")
        wait_accessible(lambda: not find("server", "list item"))
        wait_for(lambda: all(t["connection"] != "server" for t in local.rpc("list")["tabs"]))
        assert "error" not in server.rpc("read", pane=pane), "Removal killed the remote pane"
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Connections", "dialog"))
        click("New Tab Options", "toggle button")
        creation_menu()
        widget("secured", "menu item")
        assert not find("server", "menu item"), "Removed remote remained in the plus menu"
        click("New Tab Options", "toggle button")
        wait_accessible(lambda: not find("secured", "menu item"))
        open_manager()
        local.rpc("connection-add", name="server", address=address, authentication={"mode": "none"})
        widget("server", "list item")
        wait_for(lambda: any(t["connection"] == "server" and t["connected"] for t in local.rpc("list")["tabs"]))

    with scenario("disconnection offers manual retry and reconnects when the server returns"):
        stop_daemon(server.env["GMUX_SOCKET"])
        remote.wait(timeout=8)
        retry = widget("Retry connection to server now", "button")
        widget("Reconnecting…", "label")
        wait_accessible(lambda: retry.get_state_set().contains(Atspi.StateType.SENSITIVE))
        screenshot("connections-reconnecting.png")
        # Retry while the server is still offline. After restarting it, automatic
        # recovery can connect first and correctly disable the retry button.
        wait_accessible(lambda: retry.get_state_set().contains(Atspi.StateType.SENSITIVE))
        activate(retry)
        server.rpc("connection-edit", connection="local", remote_access=dict(
            enabled=True, listen_address=address, authentication=dict(mode="none")))
        server.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: server.rpc("status")["bound_address"] == address)
        wait_for(lambda: any(t["connection"] == "server" and t["connected"] for t in local.rpc("list")["tabs"]))
        wait_accessible(lambda: not retry.get_state_set().contains(Atspi.StateType.SENSITIVE))

    with scenario("permanent credential failures show Disconnected and manual retry restores Connected"):
        credentials = replacement_token.read_bytes()
        try:
            replacement_token.write_text("invalid")
            # Changing authentication closes the existing stream and triggers recovery.
            secured.rpc("connection-edit", connection="local", remote_access=dict(
                enabled=True, listen_address=secure_address, authentication=dict(mode="none")))
            widget("Disconnected", "label")
            retry = widget("Retry connection to secured now", "button")
            assert retry.get_state_set().contains(Atspi.StateType.SENSITIVE)
            screenshot("connections-error.png")
        finally:
            replacement_token.write_bytes(credentials)
            secured.rpc("connection-edit", connection="local", remote_access=dict(
                enabled=True, listen_address=secure_address,
                authentication=dict(mode="token", file=str(token))))
        click("Retry connection to secured now", "button")
        wait_accessible(lambda: not retry.get_state_set().contains(Atspi.StateType.SENSITIVE))
        wait_accessible(lambda: any(obj.get_role_name() == "label" and obj.get_name() == "Connected"
                        for obj in descendants(widget("secured", "list item"))))

    with scenario("closing and reopening the manager releases dialog resources"):
        for name in ("server", "secured"):
            local.rpc("connection-remove", connection=name)
        assert local.rpc("connection-list")["remotes"] == []
        assert "error" not in secured.rpc("read", pane=secure_pane)
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Add Connection"))
        click("New Tab Options", "toggle button")
        creation_menu()
        widget(hostname, "menu item")
        assert not find("server", "menu item") and not find("secured", "menu item")
        click("New Tab Options", "toggle button")
        wait_accessible(lambda: not find(hostname, "menu item"))
        time.sleep(.5)
        before_repeated_dialogs = idle_resources()
        for _ in range(12):
            open_manager()
            assert local.rpc("connection-list")["remotes"] == []
            click(hostname, "list item")
            widget("Local Connection", "dialog")
            click("Cancel", "button")
            wait_accessible(lambda: not find("Local Connection", "dialog"))
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("Connections", "dialog"))
        # GTK's shared worker pool can grow while animating pages and retire
        # those threads after its idle timeout. Check retained workers.
        closed_resources = wait_for(lambda: (resources if (resources := idle_resources())["Threads"]
            <= before_repeated_dialogs["Threads"] else None), timeout=20,
            message=f"Dialog workers remained alive: {before_repeated_dialogs} -> {idle_resources()}")
        before = cpu_ticks()
        time.sleep(1)
        closed_ticks = cpu_ticks() - before
        open_manager()
        assert local.rpc("connection-list")["remotes"] == []
        time.sleep(.5)
        before = cpu_ticks()
        time.sleep(1)
        open_ticks = cpu_ticks() - before
        (artifacts / "remotes-ui-resources.json").write_text(json.dumps(dict(
            initial=initial_resources, before_repeated_dialogs=before_repeated_dialogs,
            before_workspace_editors=before_workspace_editors, after_workspace_editors=after_workspace_editors,
            after_repeated_dialogs=closed_resources,
            idle_cpu_ticks_closed=closed_ticks, idle_cpu_ticks_open=open_ticks), indent=2) + "\n")

    with scenario("all connection menus disable disconnected remotes and enable them after reconnecting"):
        close_dialog("Connections")
        empty_workspace = local.rpc("workspace-add", name="Empty menu test")["workspace"]
        local.rpc("filter", workspace=empty_workspace)
        local.rpc("connection-add", name="server", address=address, authentication={"mode": "none"})
        local.rpc("connection-add", name="secured", address=secure_address,
                  authentication={"mode": "token", "file": str(token)})
        wait_for(lambda: {t["connection"] for t in local.rpc("list")["tabs"] if t["connected"]}
                 >= {"server", "secured"})
        remote_ids = {fixture.rpc("_hello")["daemon"] for fixture in (server, secured)}
        empty_button = widget("New Tab", "button")
        empty_options = next(obj for obj in descendants(empty_button.get_parent())
                             if obj.get_name() == "New Tab Options" and obj.get_role_name() == "toggle button")
        activate(empty_options)
        creation_menu()
        for fixture in (server, secured):
            stop_daemon(fixture.env["GMUX_SOCKET"])
        wait_for(lambda: {t["reference"]["daemon"] for t in local.rpc("list")["tabs"] if not t["connected"]}
                 >= remote_ids)
        creation_menu(disconnected=("server", "secured"))
        keyboard_focus(hostname, "menu item")
        key(Gdk.KEY_Down)
        wait_accessible(lambda: widget(hostname, "menu item").get_state_set().contains(Atspi.StateType.FOCUSED))
        screenshot("new-tab-empty-disconnected.png")
        key(Gdk.KEY_Escape)
        wait_accessible(lambda: not find("New Tab via Connection", "label"))
        click("New Tab Options", "toggle button")
        creation_menu(disconnected=("server", "secured"))
        key(Gdk.KEY_Escape)
        click("Main Menu", "toggle button")
        click("New Tab via Connection", "menu item")
        creation_menu(disconnected=("server", "secured"), submenu=True)
        key(Gdk.KEY_Escape)
        server.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: server.rpc("status")["bound_address"] == address)
        wait_for(lambda: any(t["connection"] == "server" and t["connected"] for t in local.rpc("list")["tabs"]))
        activate(empty_options)
        creation_menu(disconnected=("secured",))
        click(hostname, "menu item")
        wait_for(lambda: any(t["workspace"] == empty_workspace and t["reference"]["daemon"] == local_id
                             for t in local.rpc("list")["tabs"]))
        wait_accessible(lambda: not find("New Tab via Connection", "label"))

print("PASS: native remote management, plus menu destinations, optional authentication, reconnect, and non-destructive removal")
