#!/usr/bin/env python3
"""Check command search, native rows, and screen-reader announcements.

Run with GDK_BACKEND=wayland dbus-run-session -- python3
scripts/commands-smoke-test.py. Uses a private accessibility bus and settings.
"""
import json
from functools import partial
from test_support import rpc as shared_rpc, descendants, wait_for as shared_wait_for
import os
from pathlib import Path
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
context = GLib.MainContext.default()


wait_for = partial(shared_wait_for, timeout=10, context=context)


def focused(obj):
    # State can be cached before GTK transfers focus to the search entry.
    obj.clear_cache()
    return obj.get_state_set().contains(Atspi.StateType.FOCUSED)


bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
address = bus.call_sync(
    "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
    None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None,
).unpack()[0]
registry = subprocess.Popen(
    ["/usr/lib/at-spi2-registryd"],
    env=dict(os.environ, AT_SPI_BUS_ADDRESS=address, DBUS_SESSION_BUS_ADDRESS=address),
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
app = None
listener = None
test_directory = tempfile.TemporaryDirectory(prefix="gmux-commands-", delete=False)
try:
    with test_directory as directory:
        temp = Path(directory)
        config = temp / "config/gmux"
        config.mkdir(parents=True)
        appearance = os.environ.get("GMUX_REVIEW_APPEARANCE", "dark")
        (config / "settings.json").write_text(json.dumps(dict(
            restore=False, appearance=appearance, desktop_notifications=False,
        )))
        env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
                   GMUX_SOCKET=str(temp / "socket"), GTK_A11Y="atspi")
        with (artifacts / f"commands-{appearance}.log").open("w") as log:
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env,
                                   stdout=log, stderr=log)
            def rpc(method, **params):
                return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

            wait_for(lambda: rpc("daemon-info")["attached"])
            wait_for(lambda: rpc("renderer-info")["window_active"])
            desktop = Atspi.get_desktop(0)

            def find(name, role=None):
                return next((obj for obj in descendants(desktop)
                             if obj.get_name() == name
                             and (role is None or obj.get_role_name() == role)), None)

            announcements = []

            def announced(event, _data):
                announcements.append((event.source.get_name(), event.any_data))

            listener = Atspi.EventListener.new(announced, None)
            assert listener.register("object:announcement")
            rpc("action", name="palette")
            entry = wait_for(lambda: find("Search commands", "entry")
                            or find("Search commands", "text"))
            wait_for(lambda: focused(entry))
            first = wait_for(lambda: find("New Tab", "list item"))
            command_list = first.get_parent()
            selection = command_list.get_selection_iface()
            assert selection
            controls = [relation for relation in entry.get_relation_set()
                        if relation.get_relation_type() == Atspi.RelationType.CONTROLLER_FOR]
            assert controls and controls[0].get_target(0) == command_list
            assert "Up and Down" in entry.get_description()

            def selected(name):
                return selection.get_n_selected_children() == 1 and (
                    selection.get_selected_child(0).get_name() == name)

            def search(query, name):
                announcements.clear()
                assert entry.get_editable_text_iface().set_text_contents(query)
                wait_for(lambda: selected(name))
                wait_for(lambda: ("Search commands", name) in announcements)
                assert focused(entry)

            # Disabled matches remain visible but cannot be selected or run.
            announcements.clear()
            assert entry.get_editable_text_iface().set_text_contents("splti")
            wait_for(lambda: ("Search commands", "No Available Commands") in announcements)
            assert selection.get_n_selected_children() == 0
            split = wait_for(lambda: find("Split Right", "list item"))
            assert not split.get_state_set().contains(Atspi.StateType.SENSITIVE)
            rpc("new", name="Command Test")
            search("", "New Tab")
            for query, name in [
                ("settings", "Preferences"), ("settigns", "Preferences"),
                ("font", "Preferences"), ("keybindings", "Keyboard Shortcuts"),
                ("new directory", "New Tab in Directory…"),
                ("splti riht", "Split Right"), ("name", "Rename Tab…"),
            ]:
                search(query, name)

            search("", "New Tab")
            rows = [command_list.get_child_at_index(index)
                    for index in range(command_list.get_child_count())]
            rows = [row for row in rows if row.get_role_name() == "list item"]
            def uniform_heights():
                heights = {row.get_component_iface().get_extents(Atspi.CoordType.WINDOW).height
                           for row in rows}
                return len(rows) == 13 and len(heights) == 1 and next(iter(heights)) > 0

            wait_for(uniform_heights)
            for name in ["Add Workspace…", "Split Down", "Toggle Fullscreen", "New Tab"]:
                announcements.clear()
                row = find(name, "list item")
                assert selection.select_child(row.get_index_in_parent())
                wait_for(lambda: ("Search commands", name) in announcements)
                assert selected(name)
                assert focused(entry)
            rpc("screenshot", path=str(artifacts / f"commands-{appearance}.png"))

            announcements.clear()
            assert entry.get_editable_text_iface().set_text_contents("zzzzzz")
            wait_for(lambda: ("Search commands", "No Matching Commands") in announcements)
            assert selection.get_n_selected_children() == 0
            wait_for(lambda: find("No Matching Commands", "label"))
            search("new", "New Tab")
            print("PASS: command labels, related terms, typo matching, ranking, disabled matches, "
                  "uniform rows, retained search focus, selection and no-result announcements, "
                  "and search-to-list accessibility relation")
finally:
    if listener:
        listener.deregister("object:announcement")
    if app and app.poll() is None:
        app.terminate()
        app.wait(timeout=8)
        stop_daemon(env["GMUX_SOCKET"])
    registry.terminate()
    registry.wait(timeout=8)
    test_directory.cleanup()
