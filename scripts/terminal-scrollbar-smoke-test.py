#!/usr/bin/env python3
"""Exercise native GTK terminal scrollbars in the isolated Wayland fixture."""
import json
import math
import os
from pathlib import Path
import shlex
import socket
import subprocess
import tempfile
import time

import gi
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Gdk, GdkPixbuf, GLib

from daemon_test_support import stop_daemon
from wayland_test_support import ensure_wayland_session, WaylandInput, REMOTE

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if result := predicate():
                return result
        except (OSError, ValueError, KeyError, GLib.Error):
            pass
        time.sleep(0.025)
    raise AssertionError("Timed out waiting for terminal scrollbar state")


with tempfile.TemporaryDirectory(prefix="gmux-scrollbar-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    settings = dict(use_system_font=False, font="Monospace", font_size=12,
                    appearance="dark", restore=False, show_sidebar=False,
                    desktop_notifications=False)
    (config / "settings.json").write_text(json.dumps(settings))
    env = dict(os.environ, GTK_A11Y="none", SHELL=str(shell),
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
    app = None
    pane = None

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
            return result

    def geometry(target=None):
        return rpc("renderer-info", pane=target or pane)["viewport"]

    def settled(predicate=lambda state: True):
        state = geometry()
        bar, history, height = state["scrollbar"], state["scrollback"], state["cell_height"]
        # Font metrics change during drawing; GTK receives the new range afterwards.
        ready = (not state["scroll_pending"]
                 and bar["upper"] == history["total"] * height
                 and bar["page_size"] == history["len"] * height)
        return state if ready and predicate(state) else None

    def send(text):
        rpc("send", pane=pane, text=text, paste=True, enter=True)

    def chord(*keys):
        for key in keys:
            native.key(key, True)
        for key in reversed(keys):
            native.key(key, False)

    def wheel(dy):
        native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                    "(ddu)", (0.0, dy, 2))
        native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                    "(ddu)", (0.0, 0.0, 1))

    def finger(dy, finish=False):
        native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                    "(ddu)", (0.0, dy, 1 if finish else 4))

    def assert_adjustment(state):
        bar, history, height = state["scrollbar"], state["scrollback"], state["cell_height"]
        assert bar["upper"] == history["total"] * height, state
        assert bar["page_size"] == history["len"] * height, state
        assert math.floor(bar["value"] / height + 0.5) == history["offset"], state

    def touchpad_scroll():
        before = geometry()
        initial = before["scrollbar"]["value"]
        # Small deltas must accumulate even after the daemon acknowledges a row.
        for _ in range(8):
            previous = geometry()["scrollbar"]["value"]
            finger(2)
            wait_for(lambda: settled(lambda state: state["scrollbar"]["value"] > previous))
            time.sleep(0.04)
        state = geometry()
        distance = state["scrollbar"]["value"] - initial
        assert 16 <= distance <= 48, ("Touchpad deltas must move pixels, not rows", distance, state)
        assert_adjustment(state)
        # Avoid adding release velocity to the measured gesture distance.
        time.sleep(0.2)
        finger(0, finish=True)
        wait_for(lambda: settled())
        return distance

    def wait_still():
        previous = geometry()["scrollbar"]["value"]
        last_change = time.monotonic()

        def stopped():
            nonlocal previous, last_change
            state = geometry()
            value = state["scrollbar"]["value"]
            if value != previous:
                previous, last_change = value, time.monotonic()
            if not state["scroll_pending"] and time.monotonic() - last_change > 0.2:
                return state

        return wait_for(stopped, timeout=6)

    def flick(dy):
        for _ in range(6):
            finger(dy)
            time.sleep(0.012)
        value = geometry()["scrollbar"]["value"]
        finger(0, finish=True)
        return value

    def kinetic_scroll():
        released = flick(8)
        height = geometry()["cell_height"]
        state = wait_for(lambda: settled(lambda state: state["scrollbar"]["value"] > released + height))
        reverse_from = state["scrollbar"]["value"]
        # A new gesture must cancel forward momentum and respond to reversal.
        for dy in (-12, -12, 8, -12, -12):
            finger(dy)
            time.sleep(0.012)
        released = geometry()["scrollbar"]["value"]
        assert released < reverse_from - height, "Rapid reversal must cancel forward momentum"
        finger(0, finish=True)
        wait_for(lambda: settled(lambda state: state["scrollbar"]["value"] < released - height))
        state = wait_still()
        assert_adjustment(state)
        assert 0 < state["scrollback"]["offset"] < state["scrollback"]["total"] - state["scrollback"]["len"]

    def window_offset(bounds):
        path = temp / "placement.png"
        rpc("screenshot", path=str(path))
        image = GdkPixbuf.Pixbuf.new_from_file(str(path))
        marker = bytes((230, 30, 45, 255) if image.get_has_alpha() else (230, 30, 45))
        pixel = image.get_pixels().index(marker)
        x = pixel % image.get_rowstride() // image.get_n_channels()
        y = pixel // image.get_rowstride()
        return bounds[0] - x, bounds[1] - y

    with (artifacts / "terminal-scrollbar-smoke.log").open("w") as log, WaylandInput() as native:
        try:
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            pane = rpc("new", name="Native scrollbars")["pane"]
            wait_for(lambda: geometry()["rows"] > 3)
            send("for i in $(seq 1 300); do printf '\\033[48;2;230;30;45m \\033[0mrow %03d\\n' \"$i\"; done")
            wait_for(lambda: "row 300" in rpc("read", pane=pane)["text"])
            state = wait_for(lambda: settled(lambda state: state["scrollbar"]["visible"]))
            assert_adjustment(state)
            bounds = native.click_color((230, 30, 45))
            wait_for(lambda: rpc("renderer-info")["window_active"])
            state = geometry()
            # Match the marker in window and compositor captures, including blank rows.
            offset = window_offset(bounds)
            bar = state["scrollbar"]
            x = offset[0] + bar["origin"][0] + bar["width"] / 2
            y = offset[1] + bar["origin"][1]
            thumb_height = max(32, bar["height"] * bar["page_size"] / bar["upper"])
            native.move(x, y + bar["height"] - thumb_height / 2 - 4)
            time.sleep(0.2)
            native.button(True)
            for fraction in (0.7, 0.4, 0.1, 0.0):
                native.move(x, y + bar["height"] * fraction + 1)
                time.sleep(0.04)
            native.button(False)
            state = wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] == 0))
            assert state["focused"], "Dragging the scrollbar must preserve terminal keyboard focus"
            assert "row 001" in rpc("read", pane=pane)["text"]
            rpc("screenshot", path=str(artifacts / "terminal-scrollbar-top.png"))

            # Output must extend the history without moving a scrolled viewport.
            total = state["scrollback"]["total"]
            send("printf 'EXTRA_OUTPUT\\n'")
            wait_for(lambda: settled(lambda state: state["scrollback"]["total"] > total))
            assert geometry()["scrollback"]["offset"] == 0

            native.move(offset[0] + state["origin"][0] + 80, offset[1] + state["origin"][1] + 80)
            touchpad_distance = touchpad_scroll()
            # A reversed gesture must return toward the start at the same speed.
            previous = geometry()["scrollback"]["offset"]
            finger(-8)
            wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] < previous))
            time.sleep(0.2)
            finger(0, finish=True)
            wheel(15)
            state = wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] > 0))
            old_offset = state["scrollback"]["offset"]
            native.key(Gdk.KEY_Shift_L, True)
            wheel(15)
            native.key(Gdk.KEY_Shift_L, False)
            state = wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] > old_offset))
            old_offset = state["scrollback"]["offset"]
            chord(Gdk.KEY_Shift_L, Gdk.KEY_Page_Down)
            state = wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] > old_offset))
            assert_adjustment(state)

            # Zoom changes the viewport row count and scrollbar page size together.
            old_rows = state["rows"]
            chord(Gdk.KEY_Control_L, Gdk.KEY_plus)
            state = wait_for(lambda: settled(lambda state: state["rows"] < old_rows))
            assert state["scrollback"]["len"] == state["rows"]
            assert_adjustment(state)
            assert abs(touchpad_scroll() - touchpad_distance) < 0.1, "Zoom must preserve touchpad speed in pixels"
            kinetic_scroll()

            # Alternate-screen mouse tracking owns wheel input, without scrollback.
            ready = temp / "alternate-ready"
            finished = temp / "alternate-finished"
            mouse = temp / "mouse-bytes"
            program = temp / "alternate.py"
            program.write_text(
                "import os, select, tty\nfrom pathlib import Path\n"
                "tty.setraw(0)\nos.write(1, b'\\x1b[?1049h\\x1b[?1000h\\x1b[?1006hALT_SCREEN')\n"
                f"Path({str(ready)!r}).touch()\n"
                f"while not Path({str(finished)!r}).exists():\n"
                "    if select.select([0], [], [], 0.05)[0]:\n"
                f"        with open({str(mouse)!r}, 'ab') as stream: stream.write(os.read(0, 4096))\n"
                "os.write(1, b'\\x1b[?1000l\\x1b[?1006l\\x1b[?1049l')\n")
            send(f"python3 {shlex.quote(str(program))}; stty sane -echo")
            wait_for(ready.exists)
            wait_for(lambda: settled(lambda state: not state["scrollbar"]["visible"]))
            wheel(-15)
            wait_for(lambda: mouse.exists() and b"\x1b[<64;" in mouse.read_bytes())
            # Touchpad input in a TUI must accumulate a row before reporting a wheel button.
            before = len(mouse.read_bytes())
            finger(-1)
            time.sleep(0.1)
            assert len(mouse.read_bytes()) == before, "Sub-row touchpad motion must not report a wheel button"
            finger(-geometry()["cell_height"])
            wait_for(lambda: len(mouse.read_bytes()) > before)
            time.sleep(0.2)
            finger(0, finish=True)
            assert geometry()["scrollback"]["offset"] == 0
            finished.touch()
            state = wait_for(lambda: settled(lambda state: state["scrollbar"]["visible"]))

            # A fresh frontend must receive the daemon's retained scroll position.
            saved = state["scrollback"]["offset"]
            rpc("detach")
            assert app.wait(timeout=8) == 0
            settings["appearance"] = "light"
            (config / "settings.json").write_text(json.dumps(settings))
            app = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            state = wait_for(lambda: settled(lambda state: state["scrollbar"]["visible"]))
            # Zoom is frontend state, so reopening also reflows the viewport.
            assert_adjustment(state)
            assert saved > 0 and state["scrollback"]["offset"] > 0
            wait_for(lambda: native.frame(timeout=0.1, fresh=False).data.count(b"\xff\xff\xff\xff") > 100_000)
            bounds = native.click_color((230, 30, 45))
            wait_for(lambda: rpc("renderer-info")["window_active"])
            offset = window_offset(bounds)
            other = rpc("split", pane=pane)["pane"]
            wait_for(lambda: geometry()["cols"] < state["cols"])
            rpc("send", pane=other, text="for i in $(seq 1 150); do printf 'other %03d\\n' \"$i\"; done", paste=True, enter=True)
            wait_for(lambda: geometry(other)["scrollbar"]["visible"])
            bar = geometry()["scrollbar"]
            native.move(offset[0] + bar["origin"][0] + bar["width"] / 2,
                        offset[1] + bar["origin"][1] + bar["height"] / 2)
            time.sleep(0.1)
            native.button(True)
            time.sleep(0.05)
            native.button(False)
            wait_for(lambda: settled(lambda state: state["focused"]))
            other_offset = geometry(other)["scrollback"]["offset"]
            old_offset = geometry()["scrollback"]["offset"]
            chord(Gdk.KEY_Shift_L, Gdk.KEY_Page_Down)
            wait_for(lambda: settled(lambda state: state["scrollback"]["offset"] > old_offset))
            assert geometry(other)["scrollback"]["offset"] == other_offset
            # The divider must remain draggable inside its own gutter.
            before = geometry()
            bar = before["scrollbar"]
            divider_x = offset[0] + bar["origin"][0] + bar["width"] + 3
            divider_y = offset[1] + bar["origin"][1] + bar["height"] / 2
            native.move(divider_x, divider_y)
            time.sleep(0.05)
            native.button(True)
            native.move(divider_x + 40, divider_y)
            time.sleep(0.1)
            native.button(False)
            wait_for(lambda: geometry()["cols"] > before["cols"])
            rpc("screenshot", path=str(artifacts / "terminal-scrollbar-light.png"))
            print("PASS: native scrollbar dragging, synchronized thumb and history, new output while scrolled, "
                  "wheel, touchpad sensitivity, fractional motion, momentum and rapid reversal, keyboard scrolling, zoom, "
                  "alternate-screen mouse reporting, reconnect, light appearance, independent split panes, and divider resizing")
        except Exception:
            frame = native.frame(timeout=0.1, fresh=False)
            pixels = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(frame.data),
                GdkPixbuf.Colorspace.RGB, True, 8, frame.width, frame.height, frame.stride)
            pixels.savev(str(artifacts / "terminal-scrollbar-compositor.png"), "png", [], [])
            if app is not None and app.poll() is None and pane is not None:
                print("Last scrollbar state:", geometry(), flush=True)
                rpc("screenshot", path=str(artifacts / "terminal-scrollbar-failed.png"))
            raise
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
