#!/usr/bin/env python3
"""Check that invalid saved state produces a visible, actionable startup error."""
import os
from pathlib import Path
import subprocess

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib

from test_support import AppFixture, descendants, wait_for

root = Path(__file__).resolve().parent.parent
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
address = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
    None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
registry = subprocess.Popen(["/usr/lib/at-spi2-registryd"], stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL, env=dict(os.environ, AT_SPI_BUS_ADDRESS=address,
    DBUS_SESSION_BUS_ADDRESS=address))
try:
    for name in ["settings.json", "workspaces.json", "session.json"]:
        with AppFixture(settings={"restore": True}) as fixture:
            fixture.env["GTK_A11Y"] = "atspi"
            path = fixture.path / "config/gmux" / name
            damaged = b'{"broken":'
            path.write_bytes(damaged)
            app = fixture.start(root / "target/debug/gmux", stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL)
            def elements():
                return [node for node in descendants(Atspi.get_desktop(0)) if node.get_process_id() == app.pid]
            wait_for(lambda: any(node.get_name() == "Could Not Start Gmux" for node in elements()))
            wait_for(lambda: any(str(path) in node.get_name() and "unchanged" in node.get_name() for node in elements()))
            assert path.read_bytes() == damaged
            close = wait_for(lambda: next((node for node in elements() if node.get_name() == "Close" and node.get_role_name() == "button"), None))
            assert close.get_action_iface().do_action(0)
            assert app.wait(timeout=8) != 0
finally:
    registry.terminate()
    registry.wait(timeout=8)
print("PASS: damaged settings, workspace, and session files stay unchanged and show recovery instructions in the startup window")
