#!/usr/bin/env python3
"""Check native Wayland keys, pointer selection, accessibility, and clipboard.

Run python3 scripts/terminal-input-smoke-test.py. The script starts its own
headless Mutter compositor with XWayland disabled and a private PipeWire server.
"""
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
from gi.repository import Atspi, Gdk, Gio, GLib

from daemon_test_support import stop_daemon
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        try:
            if result := predicate():
                return result
        except (FileNotFoundError, ConnectionRefusedError, GLib.Error):
            pass
        time.sleep(0.025)
    raise AssertionError("Native Wayland terminal input did not reach the expected state")


def descendants(element):
    element.clear_cache()
    yield element
    for index in range(element.get_child_count()):
        child = element.get_child_at_index(index)
        if child:
            yield from descendants(child)


with tempfile.TemporaryDirectory(prefix="gmux-terminal-input-") as directory:
    temp = Path(directory)
    rcfile = temp / "bashrc"
    rcfile.write_text("PS1=$'\\e[48;2;30;90;180mGMUX_INPUT> \\e[0m'\nPROMPT_COMMAND=''\nstty -echo\n")
    shell = temp / "shell"
    shell.write_text(f"#!/bin/sh\nexec /bin/bash --noprofile --rcfile {shlex.quote(str(rcfile))} -i\n")
    shell.chmod(0o700)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(
        use_system_font=False, font="Monospace", font_size=12, appearance="dark",
        restore=False, show_sidebar=False, desktop_notifications=False)))
    env = dict(os.environ, GTK_A11Y="atspi", SHELL=str(shell),
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
    app = None
    registry = None
    pane = None

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    with (artifacts / "terminal-input-smoke.log").open("w") as log, WaylandInput() as native:
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            address = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                "GetAddress", None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE,
                5000, None).unpack()[0]
            registry = subprocess.Popen(["/usr/lib/at-spi2-registryd"], stdout=log, stderr=log,
                env=dict(env, AT_SPI_BUS_ADDRESS=address, DBUS_SESSION_BUS_ADDRESS=address))
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            pane = rpc("new", name="Native Wayland input")["pane"]
            terminal = wait_for(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_role_name() == "terminal"), None))
            assert terminal.get_role_name() == "terminal"
            rpc("focus", pane=pane)
            wait_for(lambda: "GMUX_INPUT>" in rpc("read", pane=pane)["text"])
            native.click_color((30, 90, 180))
            wait_for(lambda: rpc("renderer-info", pane=pane)["window_active"])
            assert rpc("renderer-info")["display"] == "GdkWaylandDisplay"
            rpc("screenshot", path=str(artifacts / "terminal-input-initial.png"))

            command = r"printf '\033[2J\033[H\033[48;2;230;30;45mKEYBOARD_INPUT_OK\033[0m\n'"
            for char in command + "\n":
                symbol = Gdk.KEY_Return if char == "\n" else Gdk.unicode_to_keyval(ord(char))
                native.key(symbol, True)
                native.key(symbol, False)
                time.sleep(0.005)
            wait_for(lambda: "KEYBOARD_INPUT_OK" in rpc("read", pane=pane)["text"].splitlines())
            bounds = wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((230, 30, 45)))
            left, top, right, bottom = bounds
            cw = (right - left) / len("KEYBOARD_INPUT_OK")
            assert cw >= 5 and bottom - top >= 10, bounds
            native.move(left + cw / 2, (top + bottom) / 2)
            native.button(True)
            time.sleep(0.05)
            native.move(left + cw * 16.5, (top + bottom) / 2)
            time.sleep(0.05)
            native.button(False)
            time.sleep(0.05)
            rpc("screenshot", path=str(artifacts / "terminal-input-drag.png"))
            keys = [Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_c]
            for symbol in keys:
                native.key(symbol, True)
            for symbol in reversed(keys):
                native.key(symbol, False)
            wait_for(lambda: native.clipboard_text() == "KEYBOARD_INPUT_OK")
            rpc("screenshot", path=str(artifacts / "terminal-input-selection.png"))
            print("PASS: native Wayland keyboard input, terminal accessibility role, composited output, "
                  "pointer selection, Ctrl+Shift+C, and clipboard contents with XWayland disabled")
        except Exception:
            if app is not None and app.poll() is None and pane is not None:
                print("Last terminal output:", rpc("read", pane=pane)["text"], flush=True)
                rpc("screenshot", path=str(artifacts / "terminal-input-failed.png"))
            raise
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
            if registry is not None:
                registry.terminate()
                registry.wait(timeout=8)
