#!/usr/bin/env python3
"""Check terminal find controls through native GTK accessibility.

Run with GDK_BACKEND=wayland dbus-run-session -- python3
scripts/find-smoke-test.py. Uses its own app, settings, and accessibility bus.
"""
import json
from test_support import rpc as shared_rpc, wait_for, descendants
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


def find(name, role):
    return next((element for element in descendants(Atspi.get_desktop(0))
                 if element.get_name() == name and element.get_role_name() == role), None)


def sensitive(element):
    element.clear_cache()
    return element.get_state_set().contains(Atspi.StateType.SENSITIVE)


bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
address = bus.call_sync(
    "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", None,
    GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None,
).unpack()[0]

with (artifacts / "find-smoke.log").open("w") as log:
    registry = subprocess.Popen(
        ["/usr/lib/at-spi2-registryd"], stdout=log, stderr=log,
        env=dict(os.environ, AT_SPI_BUS_ADDRESS=address, DBUS_SESSION_BUS_ADDRESS=address),
    )
    try:
        for appearance in ["dark", "light"]:
            with tempfile.TemporaryDirectory(prefix="gmux-find-") as directory:
                temp = Path(directory)
                config = temp / "config/gmux"
                config.mkdir(parents=True)
                (config / "settings.json").write_text(json.dumps({
                    "restore": False, "desktop_notifications": False,
                    "appearance": appearance,
                }))
                shell = temp / "shell"
                # No echoed commands or prompt text to inflate the match count.
                shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
                shell.chmod(0o700)
                env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
                           GMUX_SOCKET=str(temp / "socket"), SHELL=str(shell), GTK_A11Y="atspi")
                # A compositor may pause an occluded test window's frame clock.
                # Keep reveal animations running without changing desktop settings.
                env["GDK_DEBUG"] = env.get("GDK_DEBUG", "") + ":no-vsync"
                app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
                try:
                    def rpc(method, **params):
                        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

                    wait_for(lambda: rpc("daemon-info")["attached"])
                    first = rpc("new", name="Find Test")
                    time.sleep(0.5)
                    rpc("send", pane=first["pane"], text="printf '\\033[2J\\033[HAlpha beta ALPHA\\nalpha\\n'", paste=True, enter=True)
                    wait_for(lambda: "Alpha beta ALPHA" in rpc("read", pane=first["pane"])["text"])
                    rpc("action", name="find")
                    entry = wait_for(lambda: (find("Find...", "entry") or find("Find...", "text")))
                    previous = wait_for(lambda: find("Previous match (Ctrl+Shift+G)", "button"))
                    following = wait_for(lambda: find("Next match (Ctrl+G)", "button"))
                    assert not sensitive(previous) and not sensitive(following)
                    assert entry.get_editable_text_iface().set_text_contents("alpha")
                    wait_for(lambda: find("1 of 3", "label"))
                    assert sensitive(previous) and sensitive(following)
                    assert previous.get_action_iface().do_action(0)
                    wait_for(lambda: find("3 of 3", "label"))
                    assert following.get_action_iface().do_action(0)
                    wait_for(lambda: find("1 of 3", "label"))
                    assert following.get_action_iface().do_action(0)
                    wait_for(lambda: find("2 of 3", "label"))
                    rpc("screenshot", path=str(artifacts / f"find-{appearance}.png"))
                    field = entry.get_parent()
                    field_bounds = field.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
                    count_bounds = find("2 of 3", "label").get_component_iface().get_extents(Atspi.CoordType.WINDOW)
                    assert count_bounds.x > field_bounds.x
                    assert count_bounds.x + count_bounds.width < field_bounds.x + field_bounds.width
                    # The compact controls must also fit a narrow window.
                    rpc("resize", width=540, height=480)
                    time.sleep(0.5)
                    rpc("screenshot", path=str(artifacts / f"find-{appearance}-narrow.png"))
                    wait_for(lambda: find("2 of 3", "label"))
                    assert entry.get_component_iface().get_extents(Atspi.CoordType.WINDOW).width > 80
                    rpc("resize", width=1120, height=740)
                    time.sleep(0.5)

                    # New output updates the total without changing the current match.
                    rpc("send", pane=first["pane"], text="printf 'ALPHA\\n'", paste=True, enter=True)
                    wait_for(lambda: find("2 of 4", "label"))
                    # Reopen Find to focus the entry after native window resizing.
                    rpc("action", name="find")
                    rpc("action", name="find")
                    assert entry.get_editable_text_iface().set_text_contents("missing")
                    wait_for(lambda: not sensitive(previous) and not sensitive(following))
                    assert not sensitive(previous) and not sensitive(following)
                    assert not find("0 of 0", "label")
                    assert not find("2 of 4", "label")
                    entry.clear_cache()
                    assert entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)
                    rpc("screenshot", path=str(artifacts / f"find-{appearance}-not-found.png"))
                    assert entry.get_editable_text_iface().set_text_contents("alpha")
                    wait_for(lambda: find("1 of 4", "label"))
                    entry.clear_cache()
                    assert not entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)

                    # Counts follow the active pane and tab, rather than stale output.
                    rpc("new", name="Other Tab")
                    wait_for(lambda: not sensitive(previous) and not sensitive(following))
                    rpc("focus", pane=first["pane"])
                    wait_for(lambda: find("1 of 4", "label"))
                    rpc("split", pane=first["pane"])
                    wait_for(lambda: not sensitive(previous) and not sensitive(following))
                    rpc("focus", pane=first["pane"])
                    wait_for(lambda: find("1 of 4", "label"))
                    clear = wait_for(lambda: find("Clear search", "button"))
                    assert clear.get_action_iface().do_action(0)
                    wait_for(lambda: not find("1 of 4", "label"))
                    assert not sensitive(previous) and not sensitive(following)
                    entry.clear_cache()
                    assert not entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)
                    rpc("action", name="find")
                    rpc("action", name="find")
                    entry = wait_for(lambda: (find("Find...", "entry") or find("Find...", "text")))
                    assert Atspi.Text.get_text(entry, 0, -1) == ""
                    assert rpc("list")["active_tab"] == first["tab"]
                    # Find must scan retained history and reveal an offscreen match.
                    rpc("send", pane=first["pane"], text="printf '\\033[3J\\033[2J\\033[HSCROLLBACK_FIND_界\\n'; seq 1 200", paste=True, enter=True)
                    wait_for(lambda: "200" in rpc("read", pane=first["pane"])["text"])
                    assert "SCROLLBACK_FIND_界" not in rpc("read", pane=first["pane"])["text"]
                    assert entry.get_editable_text_iface().set_text_contents("scrollback_find_界")
                    wait_for(lambda: find("1 of 1", "label"))
                    assert following.get_action_iface().do_action(0)
                    wait_for(lambda: "SCROLLBACK_FIND_界" in rpc("read", pane=first["pane"])["text"])
                    # Place the query across the terminal's current wrap boundary.
                    cols = rpc("_metadata", pane=first["pane"])["meta"]["size"][0]
                    wrapped = "x" * (cols - 2) + "needle"
                    rpc("send", pane=first["pane"], text="printf '\\033[3J\\033[2J\\033[H" + wrapped + "\\n'", paste=True, enter=True)
                    wait_for(lambda: wrapped in rpc("read", pane=first["pane"])["text"].replace("\n", ""))
                    assert entry.get_editable_text_iface().set_text_contents("needle")
                    wait_for(lambda: find("1 of 1", "label"))
                finally:
                    app.terminate()
                    app.wait(timeout=8)
                    stop_daemon(env["GMUX_SOCKET"])
    finally:
        registry.terminate()
        registry.wait(timeout=8)

print("PASS: dark/light find bars, Unicode history search, offscreen navigation, match counts, wrapping, live output, tab/pane switching, clearing and reopening")
