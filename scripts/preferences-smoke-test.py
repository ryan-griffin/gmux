#!/usr/bin/env python3
"""Check application preferences and local sidebar state across native windows."""
import fcntl
import ctypes
import json
import os
from pathlib import Path
import struct
import subprocess
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Atspi, Gdk, GdkPixbuf

from cargo_test_support import cargo_binary
from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible as wait_for
from wayland_test_support import WaylandInput, ensure_wayland_session

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "preferences-smoke.log").open("w") as log, AccessibilityFixture(), WaylandInput() as native, AppFixture(
    dict(restore=False, desktop_notifications=False, appearance="dark",
         use_system_font=True, font="Monospace", font_size=16, show_sidebar=True),
    prefix="gmux-preferences-", env={"GTK_A11Y": "atspi", "GSETTINGS_BACKEND": "keyfile"},
) as fixture:
    rpc = fixture.rpc
    subprocess.run(["gsettings", "set", "org.gnome.desktop.interface",
                    "monospace-font-name", "Monospace 9"], env=fixture.env, check=True)
    app = fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
    first = rpc("windows")["windows"][0]["id"]
    a = rpc("new", window=first, name="Window A", directory=str(root))
    second = rpc("new-window")["window"]
    b = rpc("new", window=second, name="Window B", directory=str(root))

    def frame(name):
        return next((obj for obj in descendants(Atspi.get_desktop(0))
                     if obj.get_process_id() == app.pid and obj.get_role_name() == "frame"
                     and obj.get_name() == f"{name} — Gmux"), None)

    def control(window, name, role):
        parent = frame(window)
        return parent and next((obj for obj in descendants(parent)
                               if (obj.get_name() == name or name == "Custom Font" and obj.get_name().startswith(name))
                               and obj.get_role_name() == role
                               and obj.get_action_iface().get_n_actions() > 0), None)

    def checked(window, name):
        obj = control(window, name, "switch")
        return obj and obj.get_state_set().contains(Atspi.StateType.CHECKED)

    def set_switch(window, name, value):
        obj = wait_for(lambda: control(window, name, "switch"))
        if obj.get_state_set().contains(Atspi.StateType.CHECKED) != value:
            assert obj.get_action_iface().do_action(0)
        wait_for(lambda: checked(window, name) == value)

    def metrics(window, pane):
        rpc("screenshot", window=window, path=str(artifacts / "preferences-grid.png"))
        return rpc("renderer-info", window=window, pane=pane)["viewport"]["cell_height"]

    def saved():
        return json.loads(fixture.settings_path.read_text())

    def combo(window, name):
        parent = frame(window)
        return parent and next((obj for obj in descendants(parent)
                               if obj.get_name() == name and obj.get_role_name() == "combo box"), None)

    def combo_value(window, name):
        row = combo(window, name)
        if row:
            labels = [obj.get_name() for obj in descendants(row) if obj.get_role_name() == "label"]
            return labels[-1] if labels else None

    def key(symbol):
        native.key(symbol, True)
        native.key(symbol, False)

    def choose(window_id, window, tab, name, value):
        rpc("focus", window=window_id, tab=tab)
        wait_for(lambda: rpc("renderer-info", window=window_id)["window_active"])
        row = wait_for(lambda: combo(window, name))
        # ComboRow does not expose an AT-SPI activation action or GrabFocus.
        # Reach it through normal keyboard navigation and open its dropdown.
        for _ in range(20):
            if any(obj.get_state_set().contains(Atspi.StateType.FOCUSED) for obj in descendants(row)):
                break
            key(Gdk.KEY_Tab)
            time.sleep(0.05)
        else:
            raise AssertionError(f"{name} did not receive keyboard focus")
        key(Gdk.KEY_space)

        def option():
            return next((obj for obj in descendants(row)
                         if obj.get_role_name() == "list item" and obj.get_action_iface().get_n_actions() > 0
                         and any(label.get_role_name() == "label" and label.get_name() == value
                                 for label in descendants(obj))), None)

        item = wait_for(option)
        assert item.get_parent().get_selection_iface().select_child(item.get_index_in_parent())
        key(Gdk.KEY_Return)
        wait_for(lambda: combo_value(window, name) == value)

    def viewport(window, pane):
        return rpc("renderer-info", window=window, pane=pane)["viewport"]

    def close_preferences(window):
        dialog = next(obj for obj in descendants(frame(window))
                      if obj.get_name() == "Preferences" and obj.get_role_name() == "dialog")
        button = next(obj for obj in descendants(dialog)
                      if obj.get_name() == "Close" and obj.get_role_name() == "button")
        assert button.get_action_iface().do_action(0)
        wait_for(lambda: not any(obj.get_role_name() == "dialog" and obj.get_name() == "Preferences"
                                 for obj in descendants(frame(window))))

    def background(window, pane):
        path = artifacts / "preferences-appearance.png"
        rpc("screenshot", window=window, path=str(path))
        view = viewport(window, pane)
        image = GdkPixbuf.Pixbuf.new_from_file(str(path))
        x = round(view["origin"][0] + view["width"] - 20)
        y = round(view["origin"][1] + view["height"] - 20)
        offset = y * image.get_rowstride() + x * image.get_n_channels()
        return tuple(image.get_pixels()[offset:offset + 3])

    rpc("action", window=second, name="preferences")

    with scenario("a sidebar toggle in another window preserves global preferences"):
        rpc("action", window=first, name="preferences")
        switch = wait_for(lambda: control("Window A", "Pane Title Bars", "switch"))
        assert switch.get_action_iface().do_action(0)
        wait_for(lambda: json.loads(fixture.settings_path.read_text()).get("show_pane_titles") is True)
        rpc("action", window=second, name="sidebar")
        wait_for(lambda: json.loads(fixture.settings_path.read_text()).get("show_sidebar") is False)
        assert json.loads(fixture.settings_path.read_text())["show_pane_titles"], "Sidebar restored stale global preferences"
        assert rpc("list", window=first)["sidebar_visible"]
        assert not rpc("list", window=second)["sidebar_visible"]
        for window, pane in ((first, a["pane"]), (second, b["pane"])):
            wait_for(lambda: viewport(window, pane)["title_bar_visible"],
                     message="Pane title bars did not update in both windows")

    with scenario("open dialogs and terminal fonts follow changes in either window"):
        before = [metrics(first, a["pane"]), metrics(second, b["pane"])]
        wait_for(lambda: checked("Window B", "Pane Title Bars"),
                 message="An already-open Preferences dialog did not receive the pane-title setting")
        set_switch("Window A", "Custom Font", True)
        wait_for(lambda: checked("Window B", "Custom Font"))
        wait_for(lambda: all(metrics(window, pane) > old for window, pane, old in
                             ((first, a["pane"], before[0]), (second, b["pane"], before[1]))),
                 message="Custom font did not update both terminal views")
        set_switch("Window B", "Desktop notifications", True)
        wait_for(lambda: checked("Window A", "Desktop notifications"))
        wait_for(lambda: saved()["desktop_notifications"])
        assert saved()["show_pane_titles"] and not saved()["use_system_font"]
        # Global font changes keep each terminal's existing zoom offset.
        rpc("action", window=first, name="zoom-in")
        set_switch("Window B", "Custom Font", False)
        wait_for(lambda: not checked("Window A", "Custom Font"))
        wait_for(lambda: metrics(first, a["pane"]) > metrics(second, b["pane"]))
        subprocess.run(["gsettings", "set", "org.gnome.desktop.interface",
                        "monospace-font-name", "Monospace 14"], env=fixture.env, check=True)
        wait_for(lambda: all(metrics(window, pane) > old for window, pane, old in
                             ((first, a["pane"], before[0]), (second, b["pane"], before[1]))),
                 message="System font changes did not update both views")
        set_switch("Window A", "Custom Font", True)
        wait_for(lambda: checked("Window B", "Custom Font"))
        wait_for(lambda: metrics(first, a["pane"]) > metrics(second, b["pane"]))

    with scenario("queued edits and new windows use current preferences before disk writes finish"):
        wait_for(lambda: not saved()["use_system_font"])
        # Hold the actual storage lock. The GTK thread must remain responsive,
        # and a new window must not reload the older preferences from disk.
        with (fixture.config / ".settings.json.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            on_disk = saved()
            choose(first, "Window A", a["tab"], "Style", "Light")
            wait_for(lambda: combo_value("Window B", "Style") == "Light")
            choose(second, "Window B", b["tab"], "Cursor Style", "Underline")
            wait_for(lambda: combo_value("Window A", "Cursor Style") == "Underline")
            for window, pane in ((first, a["pane"]), (second, b["pane"])):
                wait_for(lambda: viewport(window, pane)["cursor_style"] == 2)
            rpc("action", window=second, name="sidebar")
            rpc("action", window=first, name="sidebar")
            set_switch("Window B", "Restore tabs", True)
            wait_for(lambda: checked("Window A", "Restore tabs"))
            assert saved() == on_disk
            assert not rpc("list", window=first)["sidebar_visible"]
            assert rpc("list", window=second)["sidebar_visible"]
            third = rpc("new-window")["window"]
            c = rpc("new", window=third, name="Window C", directory=str(root))
            assert not rpc("list", window=third)["sidebar_visible"]
            rpc("action", window=third, name="preferences")
            wait_for(lambda: combo_value("Window C", "Style") == "Light")
            wait_for(lambda: combo_value("Window C", "Cursor Style") == "Underline")
            wait_for(lambda: checked("Window C", "Desktop notifications"))
            wait_for(lambda: checked("Window C", "Custom Font"))
            wait_for(lambda: viewport(third, c["pane"])["cursor_style"] == 2,
                     message="A new terminal used the older on-disk cursor style")
            close_preferences("Window A")
            close_preferences("Window B")
            close_preferences("Window C")
            for window, pane in ((first, a["pane"]), (second, b["pane"]), (third, c["pane"])):
                wait_for(lambda: background(window, pane) == (255, 255, 255),
                         message="Opening a new window reverted the current light appearance")
            assert rpc("list", window=first)["active_tab"] == a["tab"]
            assert rpc("list", window=second)["active_tab"] == b["tab"]
            assert saved() == on_disk
            fcntl.flock(lock, fcntl.LOCK_UN)
        expected = dict(appearance="light", cursor_style="underline", use_system_font=False,
                        font="Monospace", font_size=16, show_pane_titles=True,
                        desktop_notifications=True, restore=True, show_sidebar=False)
        wait_for(lambda: all(saved()[key] == value for key, value in expected.items()),
                 message="Ordered persistence lost a preference changed in another window")

    with scenario("pending preference writes finish when all windows close"):
        rpc("action", window=first, name="preferences")
        rpc("action", window=second, name="preferences")
        # Atomic publication renames settings.json into its directory. Count
        # those actual writes, rather than exposing worker internals to tests.
        libc = ctypes.CDLL(None, use_errno=True)
        events = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        assert events >= 0, ctypes.get_errno()
        try:
            assert libc.inotify_add_watch(events, os.fsencode(fixture.config), 0x80) >= 0, ctypes.get_errno()
            with (fixture.config / ".settings.json.lock").open("w") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                set_switch("Window A", "Pane Title Bars", False)
                set_switch("Window A", "Desktop notifications", False)
                # Many alternating edits would accumulate full snapshots in an
                # unbounded queue. Only the in-flight and final saves may run.
                for edit in range(40):
                    window = "Window A" if edit % 2 == 0 else "Window B"
                    set_switch(window, "Pane Title Bars", edit % 2 == 0)
                    set_switch(window, "Desktop notifications", edit % 2 == 0)
                assert saved()["show_pane_titles"] and saved()["desktop_notifications"]
                close_preferences("Window A")
                close_preferences("Window B")
                for window in (third, second, first):
                    rpc("detach", window=window)
                wait_for(lambda: not rpc("daemon-info")["attached"])
                assert app.poll() is None, "Application exited before pending preferences could be written"
                fcntl.flock(lock, fcntl.LOCK_UN)
            assert app.wait(timeout=8) == 0
            publications = 0
            while True:
                try:
                    data = os.read(events, 65536)
                except BlockingIOError:
                    break
                offset = 0
                while offset < len(data):
                    _, mask, _, length = struct.unpack_from("iIII", data, offset)
                    assert not mask & 0x4000, "File-event queue overflowed"
                    name = data[offset + 16:offset + 16 + length].rstrip(b"\0")
                    publications += bool(mask & 0x80 and name == b"settings.json")
                    offset += 16 + length
            assert 1 <= publications <= 2, f"Pending edits did not coalesce: {publications} settings writes"
        finally:
            os.close(events)
        expected.update(show_pane_titles=False, desktop_notifications=False)
        assert all(saved()[key] == value for key, value in expected.items()), saved()

print("PASS: global preferences, rendered views, ordered saves, and local sidebar state across native windows")
