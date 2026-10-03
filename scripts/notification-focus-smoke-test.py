#!/usr/bin/env python3
"""Check notification suppression with real pane and window focus on Wayland.

Run python3 scripts/notification-focus-smoke-test.py. The test uses an isolated
Mutter compositor, notification server, settings, sockets, and shells.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from daemon_test_support import stop_daemon
from notification_server import NotificationServer
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        try:
            if value := predicate():
                return value
        except (OSError, KeyError):
            pass
        time.sleep(0.025)
    raise AssertionError("Notification focus state did not settle")


with tempfile.TemporaryDirectory(prefix="gmux-notification-focus-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(
        restore=True, desktop_notifications=True, show_sidebar=False)))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"), GMUX_SOCKET=str(temp / "socket"),
               SHELL=str(shell), GTK_A11Y="none")
    app = other = notifications = None
    suppressed = []

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    def pane_state(pane):
        return next(p for tab in rpc("list")["tabs"] for p in tab["panes"] if p["id"] == pane)

    def focused(pane):
        info = rpc("renderer-info", pane=pane)
        return info["window_active"] and info["viewport"]["focused"]

    def suppress(pane, message, osc=None):
        suppressed.append(message)
        if osc is None:
            rpc("notify", pane=pane, message=message)
        else:
            marker = message + "_PROCESSED"
            rpc("send", pane=pane, paste=True, enter=True,
                text=f"printf '\\033]{osc}{message}\\007'; printf '{marker}\\n'")
            wait_for(lambda: marker in rpc("read", pane=pane)["text"])
        wait_for(lambda: rpc("_metadata", pane=pane)["meta"]["unread"] == 0)
        assert not pane_state(pane)["unread"], "A suppressed alert must stay read in the sidebar"
        assert not any(n["body"] == message for n in notifications.received), message

    def deliver(pane, message):
        rpc("notify", pane=pane, message=message)
        wait_for(lambda: any(n["body"] == message for n in notifications.received))
        assert pane_state(pane)["unread"], "An unattended pane must retain its unread alert"

    with (artifacts / "notification-focus-smoke.log").open("w") as log, WaylandInput() as native:
        try:
            notifications = NotificationServer()
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            first = rpc("new", name="Focused notification test", directory=str(temp))
            pane = first["pane"]
            rpc("send", pane=pane, paste=True, enter=True,
                text="printf '\\033[48;2;30;90;180mFOCUS_HERE\\033[0m\\n'")
            native.click_color((30, 90, 180))
            wait_for(lambda: focused(pane))
            suppress(pane, "FOCUSED_CLI")
            suppress(pane, "FOCUSED_OSC9", osc="9;")
            suppress(pane, "FOCUSED_OSC777", osc="777;notify;Test;")
            suppress(pane, "FOCUSED_CLI_AGAIN")

            sibling = rpc("split", pane=pane)["pane"]
            rpc("focus", pane=pane)
            wait_for(lambda: focused(pane))
            deliver(sibling, "SIBLING_ALERT")
            suppress(pane, "FOCUSED_WITH_UNREAD_SIBLING")
            assert pane_state(sibling)["unread"], "Suppressing an alert must preserve sibling unread state"
            assert next(t for t in rpc("list")["tabs"] if t["id"] == first["tab"])["unread"]

            rpc("focus", pane=sibling)
            wait_for(lambda: focused(sibling))
            wait_for(lambda: rpc("_metadata", pane=sibling)["meta"]["unread"] == 0)
            suppress(sibling, "NEWLY_FOCUSED_SIBLING")
            deliver(pane, "FORMERLY_FOCUSED_ALERT")

            background = rpc("new", name="Background tab", directory=str(temp))
            rpc("focus", pane=sibling)
            wait_for(lambda: focused(sibling))
            deliver(background["pane"], "BACKGROUND_TAB_ALERT")

            # The selected pane must still notify when another window has focus.
            other = subprocess.Popen([sys.executable, "-c", """
import gi
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, GLib
Gtk.init()
window = Gtk.Window(title='Other application')
window.set_default_size(320, 240)
window.present()
GLib.MainLoop().run()
"""], env=env, stdout=log, stderr=log)
            wait_for(lambda: not rpc("renderer-info", pane=sibling)["window_active"])
            assert rpc("list")["active_tab"] == first["tab"]
            assert next(t for t in rpc("list")["tabs"] if t["id"] == first["tab"])["active_pane"] == sibling
            deliver(sibling, "INACTIVE_WINDOW_ALERT")
            other.terminate()
            other.wait(timeout=8)
            other = None
            rpc("focus", pane=sibling)
            wait_for(lambda: focused(sibling))
            suppress(sibling, "REFOCUSED_WINDOW")

            # Marking suppressed alerts read must persist in the daemon.
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            assert rpc("_metadata", pane=sibling)["meta"]["unread"] == 0
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            wait_for(lambda: rpc("renderer-info")["gtk"])
            assert not pane_state(sibling)["unread"]
            assert pane_state(pane)["unread"] and pane_state(background["pane"])["unread"]
            assert not any(n["body"] in suppressed for n in notifications.received)
            print("PASS: focused CLI/OSC alerts stay read without desktop delivery, split siblings and "
                  "background tabs notify, inactive windows notify, focus changes update suppression, "
                  "and read acknowledgements survive reconnect")
        finally:
            for process in (other, app):
                if process and process.poll() is None:
                    process.terminate()
                    process.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
            if notifications:
                notifications.stop()
