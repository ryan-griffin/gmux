#!/usr/bin/env python3
"""Exercise terminal context menus with native Wayland input and AT-SPI."""
import json
from functools import partial
from test_support import rpc as shared_rpc, descendants, wait_for as shared_wait_for
import os
from pathlib import Path
from test_support import AccessibilityFixture
from cargo_test_support import cargo_binary
import re
import shlex
import subprocess
import tempfile
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Atspi, Gdk, GdkPixbuf, GLib

from daemon_test_support import stop_daemon
from wayland_test_support import ensure_wayland_session, WaylandInput, REMOTE

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
menu_names = ("Copy", "Paste", "Find…", "Clear Screen", "Split Right", "Split Down", "Close Pane")
appearance = os.environ.get("GMUX_REVIEW_APPEARANCE", "dark")
assert appearance in ("light", "dark"), appearance
variant = appearance + ("-hc" if os.environ.get("ADW_DEBUG_HIGH_CONTRAST") == "1" else "")


wait_for = partial(shared_wait_for, context=GLib.MainContext.default(), exceptions=(OSError, GLib.Error))


with tempfile.TemporaryDirectory(prefix="gmux-context-menu-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(
        use_system_font=False, font="Monospace", font_size=12, appearance=appearance,
        show_sidebar=False, desktop_notifications=False)))
    env = dict(os.environ, GTK_A11Y="atspi", SHELL=str(shell),
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
    app = pane = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def find(name=None, role=None):
        return next((obj for obj in descendants(Atspi.get_desktop(0))
            if obj.get_process_id() == app.pid
            and (name is None or obj.get_name() == name)
            and (role is None or obj.get_role_name() == role)
            and obj.get_state_set().contains(Atspi.StateType.SHOWING)), None)

    def menu_visible():
        return find(role="menu") is not None

    def menu_item(name):
        return find(name, "menu item")

    def focused_item(name):
        return (item := menu_item(name)) and item.get_state_set().contains(Atspi.StateType.FOCUSED)

    def terminal_ready():
        viewport = rpc("renderer-info", pane=pane)["viewport"]
        return (viewport["focused"] and viewport["height"] == initial_height
                and viewport["rows"] == viewport["scrollback"]["len"])

    def key(symbol):
        native.key(symbol, True)
        time.sleep(0.05)
        native.key(symbol, False)

    def right_click():
        for pressed in (True, False):
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerButton",
                        "(ib)", (273, pressed))
            if pressed:
                time.sleep(0.05)

    def dismiss():
        key(Gdk.KEY_Escape)
        wait_for(lambda: not menu_visible())
        wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])

    def activate(name):
        item = wait_for(lambda: menu_item(name))
        assert item.get_state_set().contains(Atspi.StateType.SENSITIVE), name
        assert item.get_action_iface().do_action(0), name
        wait_for(lambda: not menu_visible())

    def tab():
        return next(tab for tab in rpc("list")["tabs"] if any(p["id"] == pane for p in tab["panes"]))

    def capture(name):
        frame = native.frame(timeout=0.2, fresh=False)
        pixels = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(frame.data),
            GdkPixbuf.Colorspace.RGB, True, 8, frame.width, frame.height, frame.stride)
        pixels.savev(str(artifacts / f"terminal-context-menu-{name}.png"), "png", [], [])

    with (artifacts / f"terminal-context-menu-{variant}.log").open("w") as log, WaylandInput() as native, AccessibilityFixture():
        try:
            # The check runner shares a compositor across suites. Replace any
            # earlier clipboard offer with a non-text format before checking Paste.
            native.call(REMOTE, native.remote, REMOTE + ".Session", "SetSelection",
                        "(a{sv})", ({"mime-types": GLib.Variant("as", ["application/octet-stream"])},))
            app = subprocess.Popen([str(cargo_binary("gmux"))], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            pane = rpc("new", name="Terminal context menus")["pane"]
            marker = "printf CONTEXT_PASTE_OK"
            command = "printf '\\033[2J\\033[H\\033[48;2;230;30;45m%s\\033[0m\\n' " + shlex.quote(marker)
            rpc("send", pane=pane, text=command, paste=True, enter=True)
            wait_for(lambda: marker in rpc("read", pane=pane)["text"])
            bounds = native.click_color((230, 30, 45))
            wait_for(lambda: rpc("renderer-info")["window_active"])
            initial_height = rpc("renderer-info", pane=pane)["viewport"]["height"]
            left, top, right, bottom = bounds
            cw, cy = (right - left) / len(marker), (top + bottom) / 2
            native.move(left + cw * 2, cy)
            right_click()
            wait_for(menu_visible)
            for name in menu_names:
                wait_for(lambda: menu_item(name))
            assert find("Terminal Actions", "menu")
            for name in ("Copy", "Paste"):
                assert not menu_item(name).get_state_set().contains(Atspi.StateType.SENSITIVE), name
            capture(variant)
            dismiss()

            # Keyboard invocation must open the same native menu and restore focus.
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            wait_for(lambda: focused_item("Find…"))
            for name in ("Clear Screen", "Split Right", "Split Down", "Close Pane"):
                key(Gdk.KEY_Down)
                wait_for(lambda: focused_item(name))
            for name in ("Split Down", "Split Right", "Clear Screen", "Find…"):
                key(Gdk.KEY_Up)
                wait_for(lambda: focused_item(name))
            key(Gdk.KEY_Return)
            wait_for(lambda: not menu_visible())
            wait_for(lambda: (entry := find("Find...")) and entry.get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Escape)
            wait_for(terminal_ready)
            native.frame(timeout=2.0)
            native.key(Gdk.KEY_Shift_L, True)
            native.key(Gdk.KEY_F10, True)
            try:
                wait_for(menu_visible)
            finally:
                native.key(Gdk.KEY_F10, False)
                native.key(Gdk.KEY_Shift_L, False)
            dismiss()

            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            # GtkPopoverMenu uses unmodified mnemonic keys while it is open.
            key(Gdk.KEY_f)
            wait_for(lambda: not menu_visible())
            wait_for(lambda: (entry := find("Find...")) and entry.get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Escape)
            wait_for(terminal_ready)
            native.frame(timeout=2.0)

            # Right-click must preserve the selection used by Copy.
            native.move(left + cw / 2, cy)
            native.button(True)
            time.sleep(0.05)
            native.move(right - cw / 2, cy)
            time.sleep(0.05)
            native.button(False)
            time.sleep(0.05)
            right_click()
            wait_for(menu_visible)
            activate("Copy")
            wait_for(lambda: native.clipboard_text() == marker)
            right_click()
            wait_for(menu_visible)
            activate("Paste")
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])
            key(Gdk.KEY_Return)
            wait_for(lambda: "CONTEXT_PASTE_OK" in rpc("read", pane=pane)["text"].splitlines())

            right_click()
            wait_for(menu_visible)
            activate("Find…")
            wait_for(lambda: (entry := find("Find...")) and entry.get_state_set().contains(Atspi.StateType.FOCUSED))
            key(Gdk.KEY_Escape)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])

            # Clicking a different pane must direct split actions to that pane.
            other = rpc("split", pane=pane)["pane"]
            wait_for(lambda: tab()["active_pane"] == other)
            bounds = wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((230, 30, 45)))
            left, top, right, bottom = bounds
            native.move(left + cw * 2, (top + bottom) / 2)
            right_click()
            wait_for(menu_visible)
            assert tab()["active_pane"] == pane
            old_cols = rpc("renderer-info", pane=pane)["viewport"]["cols"]
            activate("Split Right")
            wait_for(lambda: len(tab()["panes"]) == 3)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["cols"] < old_cols)
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            activate("Close Pane")
            wait_for(lambda: len(tab()["panes"]) == 2)

            rpc("focus", pane=pane)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])
            old_rows = rpc("renderer-info", pane=pane)["viewport"]["rows"]
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            activate("Split Down")
            wait_for(lambda: len(tab()["panes"]) == 3)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["rows"] < old_rows)
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            activate("Close Pane")
            wait_for(lambda: len(tab()["panes"]) == 2)

            # Clear the clicked pane while another pane is active.
            rpc("focus", pane=pane)
            wait_for(lambda: tab()["active_pane"] == pane)
            bounds = wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((230, 30, 45)))
            rpc("focus", pane=other)
            wait_for(lambda: tab()["active_pane"] == other)
            other_text = rpc("read", pane=other)["text"]
            left, top, right, bottom = bounds
            native.move(left + cw * 2, (top + bottom) / 2)
            right_click()
            wait_for(menu_visible)
            assert tab()["active_pane"] == pane
            activate("Clear Screen")
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["scrollback"]["total"]
                     == rpc("renderer-info", pane=pane)["viewport"]["rows"])
            wait_for(lambda: rpc("read", pane=pane)["text"] == "")
            assert len(tab()["panes"]) == 2
            assert rpc("read", pane=other)["text"] == other_text
            rpc("send", pane=pane, text="printf 'AFTER_CLEAR\\n'", paste=True, enter=True)
            wait_for(lambda: "AFTER_CLEAR" in rpc("read", pane=pane)["text"])

            # Terminal mouse reporting keeps normal right-click; Shift overrides it.
            ready, finished, mouse = (temp / name for name in ("ready", "finished", "mouse"))
            program = temp / "mouse.py"
            program.write_text(
                "import os, select, tty\nfrom pathlib import Path\n"
                "tty.setraw(0)\nos.write(1, b'\\x1b[?1049h\\x1b[?1003h\\x1b[?1006h"
                "\\x1b[48;2;30;160;230mMOUSE_READY\\x1b[0m')\n"
                f"Path({str(ready)!r}).touch()\n"
                f"while not Path({str(finished)!r}).exists():\n"
                "    if select.select([0], [], [], 0.05)[0]:\n"
                f"        with open({str(mouse)!r}, 'ab') as stream: stream.write(os.read(0, 4096))\n"
                "os.write(1, b'\\x1b[?1003l\\x1b[?1006l\\x1b[?1049l')\n")
            rpc("focus", pane=pane)
            rpc("send", pane=pane, text=f"python3 {shlex.quote(str(program))}; stty sane -echo", paste=True, enter=True)
            wait_for(ready.exists)
            wait_for(lambda: "MOUSE_READY" in rpc("read", pane=pane)["text"])
            # The daemon's text can arrive before the frontend's mode update.
            # Wait for its alternate screen to reach the composited frame.
            bounds = wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((30, 160, 230)))
            left, top, right, bottom = bounds
            native.move(left + cw / 2, (top + bottom) / 2)
            native.button(True)
            time.sleep(.05)
            native.move(left + cw * 3.5, (top + bottom) / 2)
            time.sleep(.05)
            native.button(False)
            wait_for(lambda: mouse.exists() and b"\x1b[<0;" in mouse.read_bytes()
                     and b"\x1b[<32;" in mouse.read_bytes())
            # Shift-drag selects text instead of sending terminal mouse reports.
            time.sleep(.1)
            data = mouse.read_bytes()
            native.key(Gdk.KEY_Shift_L, True)
            native.move(left + cw / 2, (top + bottom) / 2)
            native.button(True)
            time.sleep(.05)
            native.move(left + cw * 3.5, (top + bottom) / 2)
            time.sleep(.05)
            native.button(False)
            native.key(Gdk.KEY_Shift_L, False)
            rpc("action", name="copy")
            wait_for(lambda: native.clipboard_text() == "MOUS")
            # Releasing Shift can resynchronize the pointer with an unpressed
            # motion report. The drag itself must send no button reports.
            extra = mouse.read_bytes()[len(data):]
            assert re.sub(rb"\x1b\[<35;\d+;\d+M", b"", extra) == b"", extra
            native.move(left + cw * 2, (top + bottom) / 2)
            right_click()
            wait_for(lambda: mouse.exists() and b"\x1b[<2;" in mouse.read_bytes()
                and b"m" in mouse.read_bytes())
            assert not menu_visible(), "Mouse-tracking apps must receive right-click"
            time.sleep(0.1)
            data = mouse.read_bytes()
            assert b"M" in data and b"m" in data, data
            native.key(Gdk.KEY_Shift_L, True)
            right_click()
            native.key(Gdk.KEY_Shift_L, False)
            wait_for(menu_visible)
            assert mouse.read_bytes() == data, "Shift-right-click must not send mouse reports"
            # Any-event tracking also reports motion. The popup must own both
            # pointer motion and wheel input while it is open.
            native.move(left + cw * 3, (top + bottom) / 2 + 24)
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                        "(ddu)", (0.0, 15.0, 2))
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                        "(ddu)", (0.0, 0.0, 1))
            time.sleep(0.1)
            assert mouse.read_bytes() == data, ("Menu pointer and wheel input must not reach the terminal",
                                               mouse.read_bytes()[len(data):])
            dismiss()
            # Closing a popup can synthesize a motion event at the pointer's new
            # terminal position. It is valid once normal input resumes.
            time.sleep(0.1)
            data = mouse.read_bytes()
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            assert mouse.read_bytes() == data, "Menu invocation must not send terminal keys"
            dismiss()
            # Any-event mode can report pointer resynchronization after the
            # popup closes. Menu and Escape must still send no terminal keys.
            time.sleep(0.1)
            extra = mouse.read_bytes()[len(data):]
            assert re.sub(rb"\x1b\[<35;\d+;\d+M", b"", extra) == b"", extra
            key(Gdk.KEY_Menu)
            wait_for(menu_visible)
            key(Gdk.KEY_l)
            wait_for(lambda: not menu_visible())
            wait_for(lambda: find("Close terminal?"))
            assert len(tab()["panes"]) == 2, "Closing a busy pane must require confirmation"
            key(Gdk.KEY_Escape)
            wait_for(lambda: find("Close terminal?") is None)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])
            assert len(tab()["panes"]) == 2, "Cancel must preserve the busy pane"
            finished.touch()
            wait_for(lambda: "MOUSE_READY" not in rpc("read", pane=pane)["text"])
            print(f"PASS ({variant}): native terminal context menu, accessible names, action sensitivity, "
                  "selection and clipboard, Find focus, keyboard navigation and activation, mnemonics, "
                  "Clear Screen clicked-pane targeting and continued shell input, "
                  "pane-specific splitting and closing, busy-process confirmation, mouse reporting, and Shift override")
        except Exception:
            if "mouse" in locals() and mouse.exists():
                print("Recorded mouse input:", mouse.read_bytes(), flush=True)
            capture("failed")
            print("Visible accessible items:", [(obj.get_role_name(), obj.get_name())
                  for obj in descendants(Atspi.get_desktop(0))
                  if app and obj.get_process_id() == app.pid
                  and obj.get_state_set().contains(Atspi.StateType.SHOWING)], flush=True)
            raise
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
