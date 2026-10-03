#!/usr/bin/env python3
"""Test GNOME font tracking on a native display with isolated desktop settings.

Run with GTK_A11Y=none dbus-run-session -- python3 scripts/font-smoke-test.py.
The keyfile backend keeps every GNOME setting change inside a temporary directory.
"""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

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
    raise AssertionError("Timed out waiting for font update")


with tempfile.TemporaryDirectory(prefix="gmux-font-") as temp:
    temp = Path(temp)
    env = os.environ.copy()
    env.update(GSETTINGS_BACKEND="keyfile", XDG_CONFIG_HOME=str(temp / "config"),
               GMUX_SOCKET=str(temp / "gmux.sock"), SHELL="/bin/sh", GTK_A11Y="none")
    settings_path = temp / "config/gmux/settings.json"
    settings_path.parent.mkdir(parents=True)

    def set_font(size):
        subprocess.run(["gsettings", "set", "org.gnome.desktop.interface",
                        "monospace-font-name", f"Monospace {size}"], env=env, check=True)

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    serial = 0

    def grid(pane):
        global serial
        serial += 1
        # Test windows may be occluded, pausing GTK's frame clock. Capture the
        # current UI to finish layout/drawing before inspecting its PTY size.
        rpc("screenshot", path=str(artifacts / "font-grid.png"))
        marker = f"FONT_SIZE_{serial}"
        rpc("send", pane=pane, text=f"printf '{marker} '; stty size", paste=True, enter=True)
        match = wait_for(lambda: re.search(rf"^{marker} (\d+) (\d+)$",
                                          rpc("read", pane=pane)["text"], re.MULTILINE))
        return tuple(map(int, match.groups()))

    with (artifacts / "font-smoke.log").open("w") as log:
        def start(settings):
            settings_path.write_text(json.dumps(settings))
            process = subprocess.Popen([str(root / "target/debug/gmux"), "--directory", str(root)],
                                       env=env, cwd=root, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            wait_for(lambda: rpc("list")["tabs"])
            time.sleep(0.5)
            return process

        app = None
        try:
            set_font(9)
            app = start(dict(use_system_font=True, font="Monospace", font_size=12))
            first = rpc("list")["tabs"][0]["active_pane"]
            second = rpc("split", pane=first)["pane"]
            time.sleep(0.5)
            small = [grid(first), grid(second)]
            set_font(18)
            time.sleep(0.6)
            large = [grid(first), grid(second)]
            assert all(b[0] < a[0] and b[1] < a[1] for a, b in zip(small, large)), (small, large)
            rpc("action", name="zoom-in")
            time.sleep(0.3)
            assert grid(second)[1] < large[1][1]
            rpc("action", name="zoom-reset")
            time.sleep(0.3)
            assert grid(second) == large[1], "Zoom reset must return to the GNOME size"
            rpc("action", name="preferences")
            time.sleep(0.4)
            set_font(11)
            time.sleep(0.6)
            rpc("screenshot", path=str(artifacts / "system-font-preferences.png"))
            assert app.poll() is None, "Font updates must also work while Preferences is open"
            app.terminate()
            app.wait(timeout=10)
            stop_daemon(env["GMUX_SOCKET"])
            # An explicit custom font must remain stable when GNOME changes.
            app = start(dict(use_system_font=False, font="Monospace", font_size=12))
            pane = rpc("list")["tabs"][0]["active_pane"]
            custom = grid(pane)
            set_font(20)
            time.sleep(0.6)
            assert grid(pane) == custom
            print("PASS: default settings use GNOME font, live updates resize both PTYs, zoom reset follows GNOME, Preferences updates live, and custom font overrides stay fixed")
        finally:
            if app and app.poll() is None:
                app.terminate()
                app.wait(timeout=10)
                stop_daemon(env["GMUX_SOCKET"])
