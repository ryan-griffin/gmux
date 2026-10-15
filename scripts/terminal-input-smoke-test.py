#!/usr/bin/env python3
"""Check native Wayland keys, pointer selection, accessibility, and clipboard.

Run python3 scripts/terminal-input-smoke-test.py. The script starts its own
headless Mutter compositor with XWayland disabled and a private PipeWire server.
"""
import json
from functools import partial
from test_support import rpc as shared_rpc, wait_for as shared_wait_for, descendants, DelayedViewport
import os
from pathlib import Path
from test_support import AccessibilityFixture
from cargo_test_support import cargo_binary
import shlex
import subprocess
import tempfile
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Atspi, Gdk, GLib

from daemon_test_support import stop_daemon
from wayland_test_support import ensure_wayland_session, WaylandInput, REMOTE

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(shared_wait_for, exceptions=(OSError, GLib.Error))


with tempfile.TemporaryDirectory(prefix="gmux-terminal-input-") as directory:
    temp = Path(directory)
    rcfile = temp / "bashrc"
    rcfile.write_text("PS1=$'\\e[48;2;30;90;180mGMUX_INPUT> \\e[0m'\nPROMPT_COMMAND=''\nstty -echo\n")
    shell = temp / "shell"
    shell.write_text(f"#!/bin/sh\nexec /bin/bash --noprofile --rcfile {shlex.quote(str(rcfile))} -i\n")
    shell.chmod(0o700)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "clients/gtk").mkdir(parents=True, exist_ok=True)
    (config / "clients/gtk/settings.json").write_text(json.dumps(dict(
        local_execution=True, use_system_font=False, font="Monospace", font_size=12, appearance="dark",
        show_sidebar=False, desktop_notifications=False)))
    env = dict(os.environ, GTK_A11Y="atspi", SHELL=str(shell),
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
    app = daemon = None
    pane = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    with (artifacts / "terminal-input-smoke.log").open("w") as log, WaylandInput() as native, \
            DelayedViewport(env["GMUX_SOCKET"], temp / "frontend") as proxy, AccessibilityFixture():
        try:
            daemon = subprocess.Popen([str(cargo_binary("gmux")), "serve"], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info"))
            app = subprocess.Popen([str(cargo_binary("gmux-gtk"))],
                env=dict(env, GMUX_SOCKET=proxy.proxy_path), stdout=log, stderr=log)
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

            # Capture actual GTK events in Kitty mode, including a release
            # while the other side of the same modifier is still held.
            keyboard_probe = temp / "keyboard.py"
            keyboard_result = temp / "keyboard.json"
            keyboard_probe.write_text("""import json, os, select, sys, termios, time, tty
from pathlib import Path
old = termios.tcgetattr(0)
data = bytearray()
try:
    tty.setraw(0)
    os.write(1, b'\\x1b[>31u\\x1b]2;KITTY_MODIFIERS_READY\\x07')
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if select.select([0], [], [], max(0, deadline - time.monotonic()))[0]:
            data.extend(os.read(0, 4096))
finally:
    os.write(1, b'\\x1b[<u')
    termios.tcsetattr(0, termios.TCSANOW, old)
Path(sys.argv[1]).write_text(json.dumps(list(data)))
""")
            rpc("send", pane=pane,
                text=f"python3 {shlex.quote(str(keyboard_probe))} {shlex.quote(str(keyboard_result))}",
                paste=True, enter=True)
            wait_for(lambda: any(p["id"] == pane and p["title"] == "KITTY_MODIFIERS_READY"
                                 for t in rpc("list")["tabs"] for p in t["panes"]))
            events = [(Gdk.KEY_Shift_L, True), (Gdk.KEY_Shift_R, True),
                      (Gdk.KEY_Shift_L, False), (Gdk.KEY_Shift_R, False),
                      (Gdk.KEY_Control_R, True), (Gdk.KEY_Control_L, True),
                      (Gdk.KEY_Control_R, False), (Gdk.KEY_Control_L, False),
                      (Gdk.KEY_Alt_L, True), (Gdk.KEY_Alt_L, False),
                      (Gdk.KEY_Caps_Lock, True), (Gdk.KEY_Caps_Lock, False),
                      (Gdk.KEY_Caps_Lock, True), (Gdk.KEY_Caps_Lock, False)]
            for symbol, pressed in events:
                native.key(symbol, pressed)
                time.sleep(.01)
            actual = bytes(wait_for(lambda: json.loads(keyboard_result.read_text()),
                                    exceptions=(OSError, json.JSONDecodeError)))
            expected = (b"\x1b[57441;2u\x1b[57447;2u\x1b[57441;2:3u\x1b[57447;1:3u"
                        b"\x1b[57448;5u\x1b[57442;5u\x1b[57448;5:3u\x1b[57442;1:3u"
                        b"\x1b[57443;3u\x1b[57443;1:3u"
                        b"\x1b[57358;65u\x1b[57358;65:3u\x1b[57358u\x1b[57358;1:3u")
            assert actual == expected, ("GTK modifier events did not include their own state change", actual, expected)

            command = r"printf '\033[2J\033[H\033[48;2;230;30;45mKEYBOARD_INPUT_OK\033[0m\n'"
            for char in command + "\n":
                symbol = Gdk.KEY_Return if char == "\n" else Gdk.unicode_to_keyval(ord(char))
                native.key(symbol, True)
                native.key(symbol, False)
                time.sleep(0.005)
            wait_for(lambda: "KEYBOARD_INPUT_OK" in rpc("read", pane=pane)["text"].splitlines())
            text_iface = terminal.get_text_iface()
            assert text_iface is not None, "Terminal must expose accessible text"
            wait_for(lambda: "KEYBOARD_INPUT_OK" in Atspi.Text.get_text(text_iface, 0, -1))
            assert Atspi.Text.get_caret_offset(text_iface) >= len("KEYBOARD_INPUT_OK")
            for offset, expected in [(0, "KEYBOARD"), (9, "INPUT"), (15, "OK"), (0, "KEYBOARD")]:
                word = Atspi.Text.get_string_at_offset(text_iface, offset, Atspi.TextGranularity.WORD)
                assert word.content == expected, word.content
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
            wait_for(lambda: Atspi.Text.get_n_selections(text_iface) == 1)
            selected = Atspi.Text.get_selection(text_iface, 0)
            assert Atspi.Text.get_text(text_iface, selected.start_offset, selected.end_offset) == "KEYBOARD_INPUT_OK"
            rpc("screenshot", path=str(artifacts / "terminal-input-selection.png"))
            # Updating an existing accessible selection at the last column
            # must exclude the newline separating it from the following row.
            cols = rpc("renderer-info", pane=pane)["viewport"]["cols"]
            native.move(left + cw / 2, (top + bottom) / 2)
            native.button(True)
            native.move(left + cw * (cols - .5), (top + bottom) / 2)
            native.button(False)
            def full_row_selection():
                if Atspi.Text.get_n_selections(text_iface) != 1:
                    return None
                selected = Atspi.Text.get_selection(text_iface, 0)
                return selected if selected.end_offset >= cols else None
            selected = wait_for(full_row_selection)
            assert (selected.start_offset, selected.end_offset) == (0, cols), (selected.start_offset, selected.end_offset, cols)
            assert "\n" not in Atspi.Text.get_text(text_iface, selected.start_offset, selected.end_offset)
            rpc("send", pane=pane, text="printf '\\033[3J\\033[2J\\033[H界éX\\n'", paste=True, enter=True)
            wait_for(lambda: Atspi.Text.get_text(text_iface, 0, -1).startswith("界éX"))
            for offset in (1, 2, 1):
                cluster = Atspi.Text.get_string_at_offset(text_iface, offset, Atspi.TextGranularity.CHAR)
                assert (cluster.start_offset, cluster.end_offset, cluster.content) == (1, 3, "é"), cluster.content
            wide = Atspi.Text.get_character_extents(text_iface, 0, Atspi.CoordType.WINDOW)
            narrow = Atspi.Text.get_character_extents(text_iface, 1, Atspi.CoordType.WINDOW)
            combining = Atspi.Text.get_character_extents(text_iface, 2, Atspi.CoordType.WINDOW)
            assert abs(wide.width - 2 * narrow.width) <= 1, (wide.width, narrow.width)
            assert (narrow.x, narrow.width) == (combining.x, combining.width)
            assert Atspi.Text.get_offset_at_point(text_iface, int(wide.x + wide.width * .75), int(wide.y + wide.height / 2), Atspi.CoordType.WINDOW) == 0
            row_chars = len(Atspi.Text.get_text(text_iface, 0, -1).split("\n", 1)[0])
            row_bounds = Atspi.Text.get_range_extents(text_iface, 0, row_chars, Atspi.CoordType.WINDOW)
            try:
                outside = Atspi.Text.get_offset_at_point(text_iface, row_bounds.x + row_bounds.width + 100, int(wide.y + wide.height / 2), Atspi.CoordType.WINDOW)
            except GLib.Error as error:
                assert "Could not determine offset" in str(error), error
            else:
                assert outside == -1, (outside, row_bounds.x, row_bounds.width)
            native.click_color((30, 90, 180))
            wait_for(lambda: Atspi.Text.get_n_selections(text_iface) == 0)

            # Keep a raw process alive so repainting a row does not also print
            # another shell prompt. Color-only output must preserve the text,
            # cached grapheme boundaries, geometry and notification behavior.
            accessibility_probe = temp / "accessibility.py"
            accessibility_probe.write_text("""import os, termios, tty
old = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, '\\x1b[3J\\x1b[2J\\x1b[H界éX\\r\\nUNCHANGED_ROW\\x1b[4;1H'.encode())
    while True:
        action = os.read(0, 1)
        if action == b'q':
            break
        if action in (b'c', b't', b'u'):
            glyphs = {'c': '界éX', 't': '界éZ', 'u': '🙂öQ'}[action.decode()]
            os.write(1, ('\\x1b7\\x1b[H\\x1b[48;2;50;130;210m' + glyphs + '\\x1b8').encode())
finally:
    termios.tcsetattr(0, termios.TCSANOW, old)
""")
            rpc("send", pane=pane, text=f"python3 {shlex.quote(str(accessibility_probe))}", paste=True, enter=True)
            initial_text = wait_for(lambda: (value if value.startswith("界éX") and
                value.splitlines()[1].startswith("UNCHANGED_ROW") else None)
                if (value := Atspi.Text.get_text(text_iface, 0, -1)) else None)
            initial_caret = Atspi.Text.get_caret_offset(text_iface)
            initial_geometry = Atspi.Text.get_character_extents(text_iface, 0, Atspi.CoordType.WINDOW)
            initial_cluster = Atspi.Text.get_string_at_offset(text_iface, 2, Atspi.TextGranularity.CHAR)
            assert initial_cluster.content == "é"
            text_events = []

            def text_changed(event, _data):
                if event.source.get_process_id() == app.pid and event.source.get_role_name() == "terminal":
                    text_events.append((event.type, event.detail1, event.detail2))

            listener = Atspi.EventListener.new(text_changed, None)
            assert listener.register("object:text-changed")
            try:
                rpc("send", pane=pane, text="c")
                wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((50, 130, 210)))
                # Drain queued AT-SPI signals after observing the rendered row.
                deadline = time.monotonic() + .15
                while time.monotonic() < deadline:
                    while GLib.MainContext.default().pending():
                        GLib.MainContext.default().iteration(False)
                    time.sleep(.005)
                assert Atspi.Text.get_text(text_iface, 0, -1) == initial_text
                assert Atspi.Text.get_caret_offset(text_iface) == initial_caret
                assert not text_events, ("Color-only output replaced accessible text", text_events)
                cluster = Atspi.Text.get_string_at_offset(text_iface, 2, Atspi.TextGranularity.CHAR)
                assert (cluster.start_offset, cluster.end_offset, cluster.content) == (1, 3, "é")
                geometry = Atspi.Text.get_character_extents(text_iface, 0, Atspi.CoordType.WINDOW)
                assert (geometry.x, geometry.y, geometry.width, geometry.height) == (
                    initial_geometry.x, initial_geometry.y, initial_geometry.width, initial_geometry.height)
                for action, expected in [("t", "界éZ"), ("u", "🙂öQ"), ("t", "界éZ")]:
                    text_events.clear()
                    rpc("send", pane=pane, text=action)
                    updated = wait_for(lambda: (value if value.startswith(expected) else None)
                        if (value := Atspi.Text.get_text(text_iface, 0, -1)) else None)
                    wait_for(lambda: any(kind.endswith(":insert") for kind, _, _ in text_events),
                             context=GLib.MainContext.default())
                    assert any(kind.endswith(":delete") for kind, _, _ in text_events), text_events
                    assert updated.splitlines()[1:] == initial_text.splitlines()[1:]
                    assert Atspi.Text.get_caret_offset(text_iface) == initial_caret
                    cluster = Atspi.Text.get_string_at_offset(text_iface, 2, Atspi.TextGranularity.CHAR)
                    assert (cluster.start_offset, cluster.end_offset, cluster.content) == (1, 3, expected[1:3])
            finally:
                listener.deregister("object:text-changed")
                rpc("send", pane=pane, text="q")
            wait_for(lambda: "GMUX_INPUT>" in Atspi.Text.get_text(text_iface, 0, -1))
            native.click_color((30, 90, 180))
            rpc("send", pane=pane, text="printf '\\033[3J\\033[2J\\033[H\\033[48;2;230;30;45mHISTORY_START\\033[0m\\n'; seq 1 120; printf '\\033[48;2;30;200;80mHISTORY_END\\033[0m\\n'", paste=True, enter=True)
            wait_for(lambda: "HISTORY_END" in rpc("read", pane=pane)["text"])

            def page(symbol):
                native.key(Gdk.KEY_Shift_L, True)
                native.key(symbol, True)
                native.key(symbol, False)
                native.key(Gdk.KEY_Shift_L, False)
                time.sleep(.05)

            for _ in range(12):
                page(Gdk.KEY_Page_Up)
            wait_for(lambda: "HISTORY_START" in rpc("read", pane=pane)["text"])
            left, top, right, bottom = wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds((230, 30, 45)))
            viewport_top = top
            state = rpc("renderer-info", pane=pane)["viewport"]
            viewport_bottom = viewport_top + state["height"]
            row_height = state["cell_height"]

            def viewport(predicate=lambda state: True):
                state = rpc("renderer-info", pane=pane)["viewport"]
                return state if not state["scroll_pending"] and predicate(state) else None

            def assert_stopped():
                offset = wait_for(viewport)["scrollback"]["offset"]
                time.sleep(.25)
                assert wait_for(viewport)["scrollback"]["offset"] == offset, "Selection scrolling must stop"

            native.move(left + cw / 2, (top + bottom) / 2)
            native.button(True)
            time.sleep(.05)
            native.move(left + cw / 2, viewport_bottom - 1)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] >= 8),
                     message="Holding a selection at the bottom edge must scroll without more mouse motion")
            native.move(left + cw / 2, (viewport_top + viewport_bottom) / 2)
            assert_stopped()
            native.move(left + cw / 2, viewport_bottom + 2 * row_height)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] ==
                                     state["scrollback"]["total"] - state["scrollback"]["len"]))
            wait_for(lambda: "HISTORY_END" in rpc("read", pane=pane)["text"])
            end_row = rpc("read", pane=pane)["text"].splitlines().index("HISTORY_END")
            native.move(left + cw * (len("HISTORY_END") - .5), viewport_top + (end_row + .5) * row_height)
            time.sleep(.05)
            native.button(False)
            rpc("action", name="copy")
            expected = "HISTORY_START\n" + "\n".join(str(i) for i in range(1, 121)) + "\nHISTORY_END"
            wait_for(lambda: native.clipboard_text() == expected)

            # Reverse the drag and keep the pointer stationary above the viewport.
            native.button(True)
            native.move(left + cw / 2, viewport_top - 2 * row_height)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] == 0))
            native.move(left + cw * 1.5, viewport_top + row_height / 2)
            time.sleep(.05)
            native.button(False)
            rpc("action", name="copy")
            wait_for(lambda: native.clipboard_text() == expected[1:])
            assert_stopped()

            # Release in the edge zone before reaching the end of the history.
            native.button(True)
            native.move(left + cw / 2, viewport_bottom - 1)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] >= 8))
            native.button(False)
            assert_stopped()
            native.button(True)
            time.sleep(.15)
            native.button(False)
            assert_stopped()
            # Delayed daemon replies must not restore an older selection while
            # the pointer rapidly reverses outside the viewport.
            for _ in range(2):
                page(Gdk.KEY_Page_Down)
            wait_for(lambda: viewport(lambda state: 0 < state["scrollback"]["offset"] <
                                     state["scrollback"]["total"] - state["scrollback"]["len"]))
            native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
            native.button(True)
            proxy.delay = .035
            checked = 0
            for index in range(120):
                y = viewport_top - 2 * row_height if index % 2 else viewport_bottom + 2 * row_height
                native.move(left + cw * 6.5, y)
                for _ in range(2):
                    state = rpc("renderer-info", pane=pane)["viewport"]
                    drag = state["selection_drag"]
                    if drag is not None:
                        x, y = drag["position"]
                        col = min(state["cols"] - 1, max(0, int(x // state["cell_width"])))
                        row = min(state["rows"] - 1, max(0, int(y // state["cell_height"])))
                        end = (state["scrollback"]["offset"] + row) * state["cols"] + col
                        expected_selection = sorted((drag["anchor"], end))
                        assert state["selection"] == expected_selection, ("A stale selection reply caused flicker", state)
                        checked += 1
                    time.sleep(.002)
            assert checked >= 100, checked
            proxy.delay = 0
            native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
            time.sleep(.05)
            native.button(False)
            assert_stopped()
            # After rapid motion around an edge, a stationary drag must move
            # the viewport only in that edge's direction, without bouncing back.
            for direction in (-1, 1):
                native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
                native.button(True)
                trace = []
                for index in range(60):
                    distance = (2, 6, 1, 5)[index % 4] * row_height
                    y = viewport_top - distance if direction < 0 else viewport_bottom + distance
                    native.move(left + cw * 6.5, y)
                    time.sleep(.003)
                previous = wait_for(viewport)["scrollback"]["offset"]
                for _ in range(150):
                    state = rpc("renderer-info", pane=pane)["viewport"]
                    offset = state["scrollback"]["offset"]
                    trace.append((offset, state["scrollbar"]["value"] / row_height))
                    assert direction * (offset - previous) >= 0, ("Viewport bounced after fast edge motion", direction, trace)
                    previous = offset
                    time.sleep(.005)
                native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
                time.sleep(.05)
                native.button(False)
                assert_stopped()
            # Prime GTK's overscroll spring at one end, then fling a new
            # selection toward the other while the spring is still running.
            for direction in (-1, 1):
                for _ in range(12):
                    page(Gdk.KEY_Page_Down if direction < 0 else Gdk.KEY_Page_Up)
                wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] ==
                                         (state["scrollback"]["total"] - state["scrollback"]["len"]
                                          if direction < 0 else 0)))
                native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
                for _ in range(6):
                    native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                                "(ddu)", (0.0, -direction * 40.0, 4))
                    time.sleep(.012)
                native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                            "(ddu)", (0.0, 0.0, 1))
                native.button(True)
                y = viewport_top - 4 * row_height if direction < 0 else viewport_bottom + 4 * row_height
                native.move(left + cw * 6.5, y)
                initial = previous = rpc("renderer-info", pane=pane)["viewport"]["scrollback"]["offset"]
                trace = []
                for _ in range(120):
                    state = rpc("renderer-info", pane=pane)["viewport"]
                    offset = state["scrollback"]["offset"]
                    trace.append((offset, state["scrollbar"]["value"] / row_height))
                    assert direction * (offset - previous) >= 0, ("Overscroll reversed the selection scroll", direction, trace)
                    previous = offset
                    time.sleep(.005)
                assert direction * (previous - initial) >= 8, ("Selection did not scroll after overscroll", direction, trace)
                native.button(False)
                assert_stopped()
            for _ in range(12):
                page(Gdk.KEY_Page_Down)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] ==
                                     state["scrollback"]["total"] - state["scrollback"]["len"]))
            # Exercise the GTK clipboard reader and the real frontend writer.
            import hashlib
            payload = "界\tline\n" * 130_000
            encoded = payload.encode()
            code = ("import os,tty,hashlib; tty.setraw(0); "
                    "os.write(1,b'\\x1b[?2004hLARGE_PASTE_READY\\r\\n'); "
                    f"n={len(encoded)+12}; data=bytearray(); "
                    "exec('while len(data)<n:\\n data.extend(os.read(0,min(65536,n-len(data))))'); "
                    "os.write(1,b'\\x1b[?2004lLARGE_PASTE_RESULT:'+hashlib.sha256(data).hexdigest().encode()+b'\\r\\n')")
            rpc("send", pane=pane, text="python3 -c " + shlex.quote(code), paste=True, enter=True)
            wait_for(lambda: "LARGE_PASTE_READY" in rpc("read", pane=pane)["text"])
            native.set_clipboard_text(payload)
            # Keep the new clipboard offer and the paste shortcut ordered on
            # Wayland, rather than racing the offer with an IPC action.
            keys = [Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_v]
            for symbol in keys:
                native.key(symbol, True)
            for symbol in reversed(keys):
                native.key(symbol, False)
            expected_hash = hashlib.sha256(b"\x1b[200~" + encoded + b"\x1b[201~").hexdigest()
            wait_for(lambda: expected_hash in rpc("read", pane=pane)["text"], context=GLib.MainContext.default())
            assert rpc("daemon-info")["attached"] and app.poll() is None
            print("PASS: native Wayland keyboard input, Kitty modifier presses/releases with both sides and Caps Lock, "
                  "accessible terminal text, caret and selection, composited output, "
                  "color-only text reuse without notifications and repeated Unicode row replacement notifications, "
                  "wide-glyph geometry, stationary selection autoscroll in both directions, rapid drag reversals without stale highlights, "
                  "fast edge drags during overscroll without viewport bouncing, "
                  "edge and release cancellation, "
                  "selection across scrollback, Ctrl+Shift+C, and large Unicode clipboard paste with XWayland disabled")
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
            if daemon is not None:
                daemon.wait(timeout=8)
