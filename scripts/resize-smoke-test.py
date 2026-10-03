#!/usr/bin/env python3
"""Check the resize overlay in the native GTK app, including its PTY size.

Run with GTK_A11Y=none GDK_BACKEND=wayland dbus-run-session -- python3
scripts/resize-smoke-test.py. Requires PyGObject and a native display.
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

import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            if result := predicate():
                return result
        except (FileNotFoundError, ConnectionRefusedError):
            pass
        time.sleep(0.05)
    raise AssertionError("Timed out waiting for application state")


def central_text(image):
    # The shell's output stays at the top. Only the centered overlay adds
    # bright pixels to this otherwise blank portion of the dark terminal.
    width, height = image.get_width(), image.get_height()
    pixels = image.get_pixels()
    channels, stride = image.get_n_channels(), image.get_rowstride()
    return sum(
        max(pixels[y * stride + x * channels:y * stride + x * channels + 3]) > 130
        for y in range(int(height * 0.4), int(height * 0.72))
        for x in range(int(width * 0.44), int(width * 0.86))
    )


with tempfile.TemporaryDirectory(prefix="gmux-resize-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(appearance="dark", restore=False)))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
               GMUX_SOCKET=str(temp / "socket"), SHELL=str(shell), GTK_A11Y="none")

    # Keep occluded test windows accepting resize configures from the compositor.
    env["GDK_DEBUG"] = env.get("GDK_DEBUG", "") + ":no-vsync"

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    def capture(name):
        path = artifacts / f"{name}.png"
        rpc("screenshot", path=str(path))
        return GdkPixbuf.Pixbuf.new_from_file(str(path))

    def grid(marker):
        rpc("send", pane=pane, text=f"printf '{marker} '; stty size", paste=True, enter=True)
        match = wait_for(lambda: re.search(rf"^{marker} (\d+) (\d+)$",
                                          rpc("read", pane=pane)["text"], re.MULTILINE))
        return tuple(map(int, match.groups()))

    with (artifacts / "resize-smoke.log").open("w") as log:
        app = subprocess.Popen([str(root / "target/debug/gmux"), "--directory", str(root)],
                               env=env, stdout=log, stderr=log)
        try:
            pane = wait_for(lambda: rpc("list")["tabs"])[0]["active_pane"]
            capture("resize-initial-layout")
            time.sleep(1.1)
            assert central_text(capture("resize-initial")) == 0, "Size overlay must start hidden"
            old = grid("GRID_BEFORE")
            rpc("resize", width=960, height=680)
            capture("resize-layout")
            time.sleep(0.22)
            assert central_text(capture("resize-during")) > 30, "Resizing should show the centered overlay"
            resized = grid("GRID_DURING")
            assert old != resized, (old, resized)
            time.sleep(1.15)
            assert central_text(capture("resize-after")) == 0, "The overlay should disappear after resizing stops"
            assert grid("GRID_AFTER") == resized, "Showing and hiding the overlay must not resize the PTY"
            rpc("resize", width=1000, height=680)
            capture("resize-again-layout")
            time.sleep(0.35)
            rpc("resize", width=1040, height=680)
            capture("resize-repeat-layout")
            time.sleep(0.55)
            assert central_text(capture("resize-repeat")) > 30, "Each resize should restart the hide deadline"
            time.sleep(0.7)
            assert central_text(capture("resize-repeat-after")) == 0
            print("PASS: initially hidden, centered on resize, automatic dismissal, stable PTY size after dismissal, and repeated-resize deadline reset")
        finally:
            app.terminate()
            app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
