#!/usr/bin/env python3
"""Exercise header, workspace, and tab menus in an isolated Wayland session."""
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import tempfile
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Atspi, Gdk, GdkPixbuf, Gio, GLib

from daemon_test_support import stop_daemon
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
appearance = os.environ.get("GMUX_REVIEW_APPEARANCE", "dark")
assert appearance in ("light", "dark"), appearance
variant = appearance + ("-hc" if os.environ.get("ADW_DEBUG_HIGH_CONTRAST") == "1" else "")
main_items = ("New Tab", "New Tab in Directory…", "New Tab in Workspace",
              "Show Tabs", "Fullscreen", "Manage Workspaces…", "Commands…", "Quit Gmux", "Preferences",
              "Keyboard Shortcuts", "About Gmux")
terminal_items = ("Split Right", "Split Down", "Find in Terminal…", "Zoom In", "Zoom Out",
                  "Reset Zoom", "Close Pane", "Close Tab")


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if result := predicate():
                return result
        except (OSError, GLib.Error):
            pass
        while GLib.MainContext.default().pending():
            GLib.MainContext.default().iteration(False)
        time.sleep(0.025)
    raise AssertionError("Main menu did not reach the expected state")


def descendants(element):
    element.clear_cache()
    yield element
    for index in range(element.get_child_count()):
        child = element.get_child_at_index(index)
        if child:
            yield from descendants(child)


with tempfile.TemporaryDirectory(prefix="gmux-main-menu-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    settings = config / "settings.json"
    settings.write_text(json.dumps(dict(
        use_system_font=False, font="Monospace", font_size=12, appearance=appearance,
        restore=False, show_sidebar=False, desktop_notifications=False)))
    env = dict(os.environ, GTK_A11Y="atspi", SHELL=str(shell),
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
    app = registry = pane = None

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    def find(name=None, role=None):
        return next((obj for obj in descendants(Atspi.get_desktop(0))
            if obj.get_process_id() == app.pid
            and (name is None or obj.get_name() == name)
            and (role is None or obj.get_role_name() == role)
            and obj.get_state_set().contains(Atspi.StateType.SHOWING)), None)

    def item(name):
        return next((obj for obj in descendants(Atspi.get_desktop(0))
            if obj.get_process_id() == app.pid and obj.get_name() == name
            and obj.get_role_name() in ("menu item", "check menu item", "radio menu item")
            and obj.get_state_set().contains(Atspi.StateType.SHOWING)), None)

    def checked(name):
        return (obj := item(name)) and obj.get_state_set().contains(Atspi.StateType.CHECKED)

    def workspace_choices(names):
        for name in names:
            wait_for(lambda: item(name))
        # AT-SPI exposes items before GTK finishes sliding to their submenu.
        # Wait for the transition before sending its next key or dismissal.
        time.sleep(0.3)

    def key(symbol):
        native.key(symbol, True)
        time.sleep(0.05)
        native.key(symbol, False)

    def chord(*symbols):
        for symbol in symbols:
            native.key(symbol, True)
        time.sleep(0.05)
        for symbol in reversed(symbols):
            native.key(symbol, False)

    def dismiss():
        if menu_visible():
            key(Gdk.KEY_Escape)
            wait_for(lambda: not menu_visible())

    def menu_visible():
        return find(role="menu")

    def activate(name):
        obj = wait_for(lambda: item(name))
        assert obj.get_state_set().contains(Atspi.StateType.SENSITIVE), name
        assert obj.get_action_iface().do_action(0), name

    def open_menu(terminal=False):
        dismiss()
        label = "Terminal Actions" if terminal else "Main Menu"
        button = wait_for(lambda: find(label, "toggle button") or find(label, "button"))
        assert button.get_action_iface().do_action(0)
        wait_for(lambda: item("Split Right" if terminal else "New Tab"))

    def open_options():
        dismiss()
        button = wait_for(lambda: find("New tab options", "toggle button")
            or find("New tab options", "button"))
        assert button.get_action_iface().do_action(0)
        wait_for(lambda: find("New tab options", "menu"))
        wait_for(lambda: item("New Tab"))

    def open_filter():
        dismiss()
        button = wait_for(lambda: find("Workspace Filter", "toggle button")
            or find("Workspace Filter", "button"))
        assert button.get_action_iface().do_action(0)
        wait_for(lambda: item("All Tabs"))

    def tab_row(name):
        for row in descendants(Atspi.get_desktop(0)):
            if (row.get_process_id() != app.pid or row.get_role_name() != "list item"
                    or not row.get_state_set().contains(Atspi.StateType.SHOWING)):
                continue
            # The first label is the title. A different tab can have the same
            # text in its directory or workspace metadata.
            title = next((obj for obj in descendants(row) if obj.get_role_name() == "label"), None)
            if title and title.get_name() == name:
                return row
        return None

    def open_tab_menu(name, keyboard=False):
        dismiss()
        row = wait_for(lambda: tab_row(name))
        if keyboard:
            # Start at the main menu button and reach the selected row with
            # native keyboard navigation to check the sidebar's focus behavior.
            open_menu()
            dismiss()
            for _ in range(12):
                chord(Gdk.KEY_Shift_L, Gdk.KEY_Tab)
                time.sleep(0.05)
                if tab_row(name).get_state_set().contains(Atspi.StateType.FOCUSED):
                    break
            else:
                raise AssertionError("Keyboard navigation must reach the selected tab row")
            # Keep the activating key pressed until Wayland accepts the popup
            # grab. Releasing it before GTK handles it can invalidate its serial.
            native.key(Gdk.KEY_Shift_L, True)
            native.key(Gdk.KEY_F10, True)
            try:
                wait_for(lambda: find("Tab Actions", "menu"))
            finally:
                native.key(Gdk.KEY_F10, False)
                native.key(Gdk.KEY_Shift_L, False)
        else:
            # This part runs fullscreen in the private compositor, so window
            # coordinates are monitor coordinates without window shadows.
            bounds = row.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
            native.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2)
            for pressed in (True, False):
                native.call("org.gnome.Mutter.RemoteDesktop", native.remote,
                    "org.gnome.Mutter.RemoteDesktop.Session", "NotifyPointerButton",
                    "(ib)", (273, pressed))
                if pressed:
                    time.sleep(0.05)
        wait_for(lambda: find("Tab Actions", "menu"))
        wait_for(lambda: item("Rename Tab…"))

    def viewport():
        return rpc("renderer-info", pane=pane)["viewport"]

    def capture(name):
        # AT-SPI marks the submenu visible before its native slide finishes.
        deadline = time.monotonic() + 0.5
        while time.monotonic() < deadline:
            frame = native.frame(timeout=0.1, fresh=False)
            time.sleep(0.02)
        pixels = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(frame.data),
            GdkPixbuf.Colorspace.RGB, True, 8, frame.width, frame.height, frame.stride)
        pixels.savev(str(artifacts / f"main-menu-{name}.png"), "png", [], [])

    with (artifacts / f"main-menu-{variant}.log").open("w") as log, WaylandInput() as native:
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            address = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                "GetAddress", None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE,
                5000, None).unpack()[0]
            registry = subprocess.Popen(["/usr/lib/at-spi2-registryd"], stdout=log, stderr=log,
                env=dict(env, AT_SPI_BUS_ADDRESS=address, DBUS_SESSION_BUS_ADDRESS=address))
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])

            # A colored terminal marker establishes native keyboard focus before
            # checking the empty window. Input never reaches the user's desktop.
            pane = rpc("new", name="Main menu focus")["pane"]
            rpc("send", pane=pane, text="printf '\\033[48;2;230;30;45mMENU\\033[0m\\n'",
                paste=True, enter=True)
            native.click_color((230, 30, 45))
            wait_for(lambda: rpc("renderer-info")["window_active"])
            key(Gdk.KEY_F10)
            wait_for(lambda: find("Main Menu", "menu"))
            dismiss()
            chord(Gdk.KEY_Shift_L, Gdk.KEY_Tab)
            wait_for(lambda: (button := find("Terminal Actions", "toggle button"))
                and button.get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_space)
            wait_for(lambda: find("Terminal Actions", "menu"))
            dismiss()
            rpc("close", pane=pane)
            wait_for(lambda: not rpc("list")["tabs"])
            open_menu()
            for name in main_items:
                wait_for(lambda: item(name))
            assert not item("New Tab in Workspace").get_state_set().contains(Atspi.StateType.SENSITIVE)
            assert not checked("Show Tabs") and not checked("Fullscreen")
            assert not any(item(name) for name in terminal_items), "Terminal actions belong in their own menu"
            open_menu(terminal=True)
            for name in terminal_items:
                wait_for(lambda: item(name))
                assert not item(name).get_state_set().contains(Atspi.StateType.SENSITIVE), name
            assert not any(item(name) for name in main_items), "App actions belong in the main menu"

            # The empty-window SplitButton has the same accessible menu model.
            open_options()
            for name in main_items[:3]:
                wait_for(lambda: item(name))
            assert not item("New Tab in Workspace").get_state_set().contains(Atspi.StateType.SENSITIVE)

            # Tab creation remains available with the sidebar hidden.
            open_menu()
            key(Gdk.KEY_n)
            tabs = wait_for(lambda: rpc("list")["tabs"])
            assert len(tabs) == 1, tabs
            pane = tabs[0]["panes"][0]["id"]
            wait_for(lambda: viewport()["focused"] and viewport()["cell_height"] > 0)
            base = viewport()["cell_height"]
            workspaces = []
            for name in ("Project", "Product", "My_Project"):
                folder = temp / name
                folder.mkdir()
                workspace = rpc("workspace-new", name=name, directory=str(folder))["workspace"]
                workspaces.append((workspace, name, folder))
            open_menu()
            wait_for(lambda: item("New Tab").get_state_set().contains(Atspi.StateType.FOCUSED))
            for name in main_items[1:]:
                key(Gdk.KEY_Down)
                wait_for(lambda: item(name).get_state_set().contains(Atspi.StateType.FOCUSED))
            open_menu()
            native.frame(timeout=2)
            capture(variant)
            open_menu(terminal=True)
            wait_for(lambda: item("Split Right").get_state_set().contains(Atspi.StateType.FOCUSED))
            for name in terminal_items[1:]:
                key(Gdk.KEY_Down)
                wait_for(lambda: item(name).get_state_set().contains(Atspi.StateType.FOCUSED))
            open_menu(terminal=True)
            capture(variant + "-terminal")

            # View actions operate on the active pane and preserve zoom reset.
            for name, predicate in (("Zoom In", lambda: viewport()["cell_height"] > base),
                                    ("Reset Zoom", lambda: viewport()["cell_height"] == base),
                                    ("Zoom Out", lambda: viewport()["cell_height"] < base),
                                    ("Reset Zoom", lambda: viewport()["cell_height"] == base)):
                open_menu(terminal=True)
                activate(name)
                wait_for(predicate)
                wait_for(lambda: not menu_visible())

            open_menu()
            key(Gdk.KEY_t)
            wait_for(lambda: json.loads(settings.read_text())["show_sidebar"])
            open_menu()
            wait_for(lambda: checked("Show Tabs"))
            dismiss()
            chord(Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_b)
            wait_for(lambda: not json.loads(settings.read_text())["show_sidebar"])
            open_menu()
            assert not checked("Show Tabs"), "Sidebar shortcut must update the menu checkmark"

            before = viewport()["height"]
            activate("Fullscreen")
            wait_for(lambda: viewport()["height"] > before)
            open_menu()
            wait_for(lambda: checked("Fullscreen"))
            dismiss()
            key(Gdk.KEY_F11)
            wait_for(lambda: viewport()["height"] == before)
            open_menu()
            assert not checked("Fullscreen"), "F11 must update the menu checkmark"

            # The secondary menu targets the active pane. Find must move focus
            # to its entry and restore the terminal after dismissal.
            initial_height = viewport()["height"]
            open_menu(terminal=True)
            key(Gdk.KEY_f)
            wait_for(lambda: (entry := find("Find..."))
                and entry.get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Escape)
            wait_for(lambda: viewport()["focused"] and viewport()["height"] == initial_height)
            native.frame(timeout=2)
            original_tab = rpc("list")["active_tab"]

            def tab(tab_id):
                return next(tab for tab in rpc("list")["tabs"] if tab["id"] == tab_id)

            open_menu(terminal=True)
            key(Gdk.KEY_r)
            wait_for(lambda: len(tab(original_tab)["panes"]) == 2)
            right = tab(original_tab)["active_pane"]
            assert right != pane
            open_menu(terminal=True)
            key(Gdk.KEY_d)
            wait_for(lambda: len(tab(original_tab)["panes"]) == 3)
            down = tab(original_tab)["active_pane"]
            assert down not in (pane, right)

            ready = temp / "pane-busy-ready"
            command = f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(60)"
            rpc("send", pane=down, text="python3 -c " + shlex.quote(command), paste=True, enter=True)
            wait_for(ready.exists)
            for mnemonic, heading in ((Gdk.KEY_p, "Close terminal?"), (Gdk.KEY_t, "Close tab?")):
                open_menu(terminal=True)
                key(mnemonic)
                wait_for(lambda: find(heading))
                key(Gdk.KEY_Escape)
                wait_for(lambda: find(heading) is None)
                assert len(tab(original_tab)["panes"]) == 3, "Cancel must preserve all panes"
            open_menu(terminal=True)
            activate("Close Pane")
            button = wait_for(lambda: find("Close terminal", "button"))
            assert button.get_action_iface().do_action(0)
            wait_for(lambda: len(tab(original_tab)["panes"]) == 2)
            assert {p["id"] for p in tab(original_tab)["panes"]} == {pane, right}
            wait_for(lambda: find("Close terminal?") is None)

            # Closing a whole tab must confirm a busy inactive pane, close all
            # its splits, and preserve other tabs and their terminals.
            open_menu()
            activate("New Tab")
            wait_for(lambda: len(rpc("list")["tabs"]) == 2)
            temporary_tab = rpc("list")["active_tab"]
            temporary_pane = tab(temporary_tab)["active_pane"]
            open_menu(terminal=True)
            activate("Split Right")
            wait_for(lambda: len(tab(temporary_tab)["panes"]) == 2)
            assert tab(temporary_tab)["active_pane"] != temporary_pane
            ready = temp / "tab-busy-ready"
            command = f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(60)"
            rpc("send", pane=temporary_pane, text="python3 -c " + shlex.quote(command), paste=True, enter=True)
            wait_for(ready.exists)
            open_menu(terminal=True)
            activate("Close Tab")
            wait_for(lambda: find("Close tab?"))
            key(Gdk.KEY_Escape)
            wait_for(lambda: find("Close tab?") is None)
            assert len(tab(temporary_tab)["panes"]) == 2
            open_menu(terminal=True)
            activate("Close Tab")
            button = wait_for(lambda: find("Close Tab", "button"))
            assert button.get_action_iface().do_action(0)
            wait_for(lambda: len(rpc("list")["tabs"]) == 1)
            assert rpc("list")["tabs"][0]["id"] == original_tab
            assert {p["id"] for p in tab(original_tab)["panes"]} == {pane, right}
            wait_for(lambda: find("Close tab?") is None)
            rpc("focus", pane=pane)

            # Workspace choices share the existing action and stay available
            # with the sidebar hidden, even with another workspace selected.
            for (workspace, name, folder), mnemonic in zip(workspaces, (Gdk.KEY_p, Gdk.KEY_r, Gdk.KEY_m)):
                rpc("filter", workspace=workspaces[0][0])
                open_menu()
                key(Gdk.KEY_s)
                workspace_choices(label for _, label, _ in workspaces)
                if name == "Project":
                    capture(variant + "-workspaces")
                key(mnemonic)
                wait_for(lambda: find("Main Menu", "menu") is None)
                wait_for(lambda: len(rpc("list")["tabs"]) == 2)
                state = rpc("list")
                created = next(tab for tab in state["tabs"] if tab["id"] == state["active_tab"])
                assert created["workspace"] == workspace, created
                created_pane = created["panes"][0]["id"]
                wait_for(lambda: next(tab for tab in rpc("list")["tabs"]
                    if tab["id"] == created["id"])["panes"][0]["directory"] == str(folder))
                assert not rpc("list")["sidebar_visible"]
                rpc("close", pane=created_pane)
                wait_for(lambda: len(rpc("list")["tabs"]) == 1)

            # The workspace selector keeps radio choices separate from commands,
            # escapes names, and reserves the management commands' mnemonics.
            dismiss()
            rpc("filter", value="all")
            rpc("action", name="sidebar")
            wait_for(lambda: rpc("list")["sidebar_visible"])
            open_filter()
            for name in ("All Tabs", "No Workspace", "Add Workspace…", "Manage Workspaces…"):
                wait_for(lambda: item(name))
            for _, name, _ in workspaces:
                wait_for(lambda: item(name))
            assert checked("All Tabs")
            capture(variant + "-filter")
            key(Gdk.KEY_o)
            wait_for(lambda: rpc("list")["filter"] == {"workspace": workspaces[2][0]})
            open_filter()
            assert checked("My_Project") and not checked("All Tabs")
            key(Gdk.KEY_a)
            wait_for(lambda: rpc("list")["filter"] == "all")
            for mnemonic, label in ((Gdk.KEY_w, "Add Workspace"), (Gdk.KEY_m, "Workspaces")):
                open_filter()
                key(mnemonic)
                wait_for(lambda: find(label))
                key(Gdk.KEY_Escape)
                wait_for(lambda: find(role="dialog") is None)

            # The sidebar SplitButton also exposes named items in its submenu.
            open_options()
            key(Gdk.KEY_s)
            workspace_choices(name for _, name, _ in workspaces)
            key(Gdk.KEY_m)
            wait_for(lambda: len(rpc("list")["tabs"]) == 2)
            created = tab(rpc("list")["active_tab"])
            assert created["workspace"] == workspaces[2][0]
            created_pane = created["active_pane"]
            wait_for(lambda: tab(created["id"])["panes"][0]["directory"] == str(workspaces[2][2]))
            rpc("close", pane=created_pane)
            wait_for(lambda: len(rpc("list")["tabs"]) == 1)
            rpc("filter", value="all")
            rpc("focus", pane=pane)

            # Context actions target the clicked tab, preserving its complete
            # pane set while another tab is active. Also check keyboard invocation.
            before = viewport()["height"]
            rpc("action", name="fullscreen")
            wait_for(lambda: viewport()["height"] > before)
            # GTK allocates the fullscreen size before Mutter finishes moving
            # the window. Pointer tests below use its final window coordinates.
            time.sleep(0.5)
            target_name = tab(original_tab)["name"]
            other = rpc("new", name="Workspace menu other", workspace=None)
            expected_panes = {pane, right}
            open_tab_menu(target_name)
            assert rpc("list")["active_tab"] == other["tab"], "Right-click must preserve the active tab"
            for name in ("Rename Tab…", "Move to Workspace", "Move to New Workspace…", "Close Tab"):
                wait_for(lambda: item(name))
            assert item("Remove from Workspace") is None
            wait_for(lambda: item("Rename Tab…").get_state_set().contains(Atspi.StateType.FOCUSED))
            for name in ("Move to Workspace", "Move to New Workspace…", "Close Tab"):
                key(Gdk.KEY_Down)
                wait_for(lambda: item(name).get_state_set().contains(Atspi.StateType.FOCUSED))
            capture(variant + "-tab")
            key(Gdk.KEY_r)
            wait_for(lambda: find("Rename Tab"))
            key(Gdk.KEY_Escape)
            wait_for(lambda: find(role="dialog") is None)
            open_tab_menu(target_name)
            key(Gdk.KEY_m)
            workspace_choices(name for _, name, _ in workspaces)
            key(Gdk.KEY_p)
            wait_for(lambda: tab(original_tab)["workspace"] == workspaces[0][0])
            assert tab(other["tab"])["workspace"] is None
            assert {p["id"] for p in tab(original_tab)["panes"]} == expected_panes
            rpc("focus", pane=pane)
            open_tab_menu(target_name, keyboard=True)
            dismiss()
            key(Gdk.KEY_Up)
            wait_for(lambda: rpc("list")["active_tab"] == other["tab"])
            wait_for(lambda: tab_row("Workspace menu other").get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Down)
            wait_for(lambda: rpc("list")["active_tab"] == original_tab)
            wait_for(lambda: tab_row(target_name).get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Return)
            wait_for(lambda: viewport()["focused"])
            bounds = tab_row("Workspace menu other").get_component_iface().get_extents(Atspi.CoordType.WINDOW)
            native.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2)
            native.button(True)
            native.button(False)
            wait_for(lambda: rpc("renderer-info", pane=other["pane"])["viewport"]["focused"])
            rpc("focus", pane=pane)
            open_tab_menu(target_name, keyboard=True)
            wait_for(lambda: item("Remove from Workspace"))
            key(Gdk.KEY_m)
            workspace_choices(("Product", "My_Project"))
            assert item("Project") is None, "Current workspace must not be a move destination"
            key(Gdk.KEY_r)
            wait_for(lambda: tab(original_tab)["workspace"] == workspaces[2][0])
            open_tab_menu(target_name)
            key(Gdk.KEY_f)
            wait_for(lambda: tab(original_tab)["workspace"] is None)
            open_tab_menu(target_name)
            key(Gdk.KEY_n)
            wait_for(lambda: find("Add Workspace"))
            wait_for(lambda: find("This tab will move to the new workspace. New tabs start in its directory."))
            entry = wait_for(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_role_name() in ("entry", "text")
                and obj.get_state_set().contains(Atspi.StateType.FOCUSED)), None))
            assert entry.get_editable_text_iface().set_text_contents("New_Context")
            assert wait_for(lambda: find("Add", "button")).get_action_iface().do_action(0)
            new_workspace = wait_for(lambda: next((w for w in rpc("list")["workspaces"]
                if w["name"] == "New_Context"), None))
            wait_for(lambda: tab(original_tab)["workspace"] == new_workspace["id"])
            assert {p["id"] for p in tab(original_tab)["panes"]} == expected_panes
            assert tab(other["tab"])["workspace"] is None
            open_tab_menu("Workspace menu other")
            key(Gdk.KEY_c)
            wait_for(lambda: len(rpc("list")["tabs"]) == 1)
            assert rpc("list")["tabs"][0]["id"] == original_tab
            rpc("workspace-delete", workspace=new_workspace["id"])
            rpc("action", name="fullscreen")
            rpc("action", name="sidebar")
            wait_for(lambda: not rpc("list")["sidebar_visible"])
            rpc("filter", value="all")
            rpc("focus", pane=pane)

            # Removing workspaces must remove their destinations from every
            # shared menu and restore the disabled entry after the final removal.
            for workspace, name, _ in workspaces:
                rpc("workspace-delete", workspace=workspace)
                open_menu()
                remaining = rpc("list")["workspaces"]
                if remaining:
                    activate("New Tab in Workspace")
                    workspace_choices(workspace["name"] for workspace in remaining)
                    assert item(name) is None, "Removed workspaces must disappear"
                else:
                    assert not item("New Tab in Workspace").get_state_set().contains(Atspi.StateType.SENSITIVE)
            dismiss()
            rpc("filter", value="all")
            rpc("focus", pane=pane)

            open_menu()
            activate("New Tab")
            wait_for(lambda: len(rpc("list")["tabs"]) == 2)
            open_menu()
            key(Gdk.KEY_d)
            wait_for(lambda: find("New Tab in Directory"))
            key(Gdk.KEY_Escape)
            wait_for(lambda: find("New Tab in Directory") is None)
            assert len(rpc("list")["tabs"]) == 2, "Cancel must not create a tab"

            # Standard items and Commands remain reachable by unique mnemonics.
            for mnemonic in (Gdk.KEY_m, Gdk.KEY_p, Gdk.KEY_k, Gdk.KEY_a, Gdk.KEY_c):
                open_menu()
                key(mnemonic)
                wait_for(lambda: find(role="dialog"))
                if mnemonic == Gdk.KEY_m:
                    assert not rpc("list")["sidebar_visible"]
                    wait_for(lambda: find("Add Workspace…", "button"))
                key(Gdk.KEY_Escape)
                wait_for(lambda: find(role="dialog") is None)

            # Quit retains the existing confirmation for busy background work.
            ready = temp / "busy-ready"
            command = f"from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(60)"
            rpc("send", pane=pane, text="python3 -c " + shlex.quote(command), paste=True, enter=True)
            wait_for(ready.exists)
            open_menu()
            key(Gdk.KEY_q)
            wait_for(lambda: find("Quit Gmux?"))
            key(Gdk.KEY_Escape)
            wait_for(lambda: find("Quit Gmux?") is None)
            assert rpc("daemon-info")["attached"] and len(rpc("list")["tabs"]) == 2
            print(f"PASS ({variant}): accessible main menu, empty-window sensitivity, "
                  "tab and workspace creation, dynamic workspace choices and mnemonics, "
                  "directory dialog, separate terminal menu, active-pane splits and zoom, "
                  "Find focus, pane and whole-tab closure with busy confirmations, synchronized "
                  "visibility and fullscreen checkmarks, workspace filter and creation dropdowns, "
                  "clicked-tab workspace moves and creation, keyboard invocation and navigation, "
                  "workspace management, standard dialogs, and Quit confirmation")
        except Exception:
            capture("failed")
            print("Visible accessible items:", [(obj.get_role_name(), obj.get_name())
                  for obj in descendants(Atspi.get_desktop(0))
                  if app and obj.get_process_id() == app.pid
                  and obj.get_state_set().contains(Atspi.StateType.SHOWING)], flush=True)
            print("Focused accessible items:", [(obj.get_role_name(), obj.get_name())
                  for obj in descendants(Atspi.get_desktop(0))
                  if app and obj.get_process_id() == app.pid
                  and obj.get_state_set().contains(Atspi.StateType.FOCUSED)], flush=True)
            raise
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
            if registry is not None:
                registry.terminate()
                registry.wait(timeout=8)
