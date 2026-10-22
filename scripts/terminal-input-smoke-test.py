#!/usr/bin/env python3
"""Check native Wayland keys, pointer selection, accessibility, and clipboard.

Run python3 scripts/terminal-input-smoke-test.py. The script starts its own
headless Mutter compositor with XWayland disabled and a private PipeWire server.
"""
import json
import base64
from contextlib import contextmanager
from functools import partial
from test_support import rpc as shared_rpc, wait_for as shared_wait_for, descendants, DelayedViewport, create_tab_when_connected, assert_stays
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
gi.require_version("Gtk", "4.0")
from gi.repository import Atspi, Gdk, GLib, Gtk

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
        local_daemon_enabled=True, use_system_font=False, font="Monospace", font_size=12, appearance="dark",
        show_sidebar=False, desktop_notifications=False)))
    # A private URI scheme records literal argv. No browser or shell handles targets.
    data_home = temp / "data"
    applications = data_home / "applications"
    applications.mkdir(parents=True)
    launches = temp / "uri-launches"
    handler = temp / "uri-handler"
    handler.write_text("#!/usr/bin/python3\nimport json, sys\nfrom pathlib import Path\np = Path(" + repr(str(launches)) + ")\nwith p.open('a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n")
    handler.chmod(0o700)
    (applications / "gmux-osc8-test.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Gmux OSC 8 test\nNoDisplay=true\n"
        f"Exec={handler} %u\nMimeType=x-scheme-handler/gmux-test-osc8;\n")
    (temp / "config/mimeapps.list").write_text(
        "[Default Applications]\nx-scheme-handler/gmux-test-osc8=gmux-osc8-test.desktop\n")
    compose = temp / "config/gtk-4.0/Compose"
    compose.parent.mkdir(parents=True)
    compose.write_text('<F12> <e> : "é" U00E9\n')
    (compose.parent / "settings.ini").write_text("[Settings]\ngtk-enable-animations=false\n")
    env = dict(os.environ, GTK_A11Y="atspi", SHELL=str(shell),
               GTK_IM_MODULE="simple",
               GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"), XDG_DATA_HOME=str(data_home))
    app = daemon = None
    pane = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    with (artifacts / "terminal-input-smoke.log").open("w") as log, WaylandInput() as native, \
            DelayedViewport(env["GMUX_SOCKET"], temp / "frontend") as proxy, AccessibilityFixture():
        try:
            daemon = subprocess.Popen([str(cargo_binary("gmux")), "serve"], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("status"))
            app = subprocess.Popen([str(cargo_binary("gmux-gtk"))],
                env=dict(env, GMUX_SOCKET=proxy.proxy_path), stdout=log, stderr=log)
            wait_for(lambda: rpc("status")["local_clients"])
            pane = create_tab_when_connected(env["GMUX_SOCKET"], name="Native Wayland input")["pane"]
            terminal = wait_for(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_role_name() == "terminal"), None))
            assert terminal.get_role_name() == "terminal"
            rpc("focus", pane=pane)
            wait_for(lambda: "GMUX_INPUT>" in rpc("read", pane=pane)["text"])
            native.click_color((30, 90, 180))
            wait_for(lambda: rpc("renderer-info", pane=pane)["window_active"])
            assert rpc("renderer-info")["display"] == "GdkWaylandDisplay"
            expected_interval = 1_000_000 / int(os.environ.get("GMUX_TEST_REFRESH_RATE", "60"))
            wait_for(lambda: abs(rpc("renderer-info")["frame_interval_us"] - expected_interval) < 500)
            rpc("screenshot", path=str(artifacts / "terminal-input-initial.png"))

            # The probe consumes input without moving the terminal cursor. This
            # isolates GTK activity resets from cursor-motion-driven resets.
            appearance_probe = temp / "appearance.py"
            appearance_control = temp / "appearance-control"
            appearance_events = temp / "appearance-events"
            appearance_done = temp / "appearance-done"
            appearance_applied = temp / "appearance-applied"
            appearance_control.write_bytes(b'\x1b[1 q\x1b]22;default\x07')
            appearance_probe.write_text("""import os, select, sys, termios, tty
from pathlib import Path
control, events, done, applied = map(Path, sys.argv[1:])
old = termios.tcgetattr(0)
try:
    tty.setraw(0)
    previous = None
    os.write(1, b'\\x1b]2;APPEARANCE_READY\\x07')
    while True:
        command = control.read_bytes()
        if command == b'stop':
            break
        if command != previous:
            os.write(1, command)
            applied.write_bytes(command)
            previous = command
        if select.select([0], [], [], .01)[0]:
            with events.open('ab') as output:
                output.write(os.read(0, 4096))
finally:
    os.write(1, b'\\x1b[0 q\\x1b[?25h\\x1b]22;\\x07')
    termios.tcsetattr(0, termios.TCSANOW, old)
done.touch()
""")
            rpc("send", pane=pane, text="python3 " + " ".join(map(shlex.quote,
                map(str, [appearance_probe, appearance_control, appearance_events, appearance_done, appearance_applied]))), paste=True, enter=True)
            def appearance(predicate=lambda state: True):
                state = rpc("renderer-info", pane=pane)["viewport"]
                return state if predicate(state) else None
            wait_for(lambda: appearance(lambda s: s["mouse_shape"] == 0 and s["pointer_name"] == "default"))
            for sequence, predicate in [
                (b'\x1b[2 q\x1b]22;crosshair\x07', lambda s: s["mouse_shape"] == 7 and s["pointer_name"] == "crosshair" and not s["cursor_blink_running"]),
                (b'\x1b]22;unknown-name\x07', lambda s: s["mouse_shape"] == 7 and s["pointer_name"] == "crosshair"),
                (b'\x1b]22;\x07', lambda s: s["mouse_shape"] == 8 and s["pointer_name"] == "text"),
                (b'\x1b[?2026h\x1b]22;zoom-in\x07', lambda s: s["mouse_shape"] == 32 and s["pointer_name"] == "zoom-in"),
                (b'\x1b[?2026l\x1b]22;\x07', lambda s: s["mouse_shape"] == 8 and s["pointer_name"] == "text"),
                (b'\x1b[1 q\x1b[?25l', lambda s: not s["cursor_blink_running"]),
                (b'\x1b[?25h', lambda s: s["cursor_blink_running"]),
            ]:
                appearance_control.write_bytes(sequence)
                wait_for(lambda: appearance_applied.read_bytes() == sequence)
                wait_for(lambda: appearance(predicate))
            wait_for(lambda: appearance(lambda s: not s["cursor_phase_visible"]))
            native.key(Gdk.KEY_a, True)
            native.key(Gdk.KEY_a, False)
            wait_for(lambda: appearance_events.exists() and b'a' in appearance_events.read_bytes())
            assert appearance()["cursor_phase_visible"], "IME commit must restore cursor without movement"
            # Terminal clipboard writes use the daemon stream and real GDK
            # offers. Primary selection must not overwrite the regular clipboard.
            def program_clipboard(text, destination):
                sequence = b'\x1b]52;' + destination.encode() + b';' + base64.b64encode(text.encode()) + b'\x1b\\'
                appearance_control.write_bytes(sequence)
                wait_for(lambda: appearance_applied.read_bytes() == sequence)
            clipboard_text = "OSC52 clipboard 界🙂e\u0301\n"
            program_clipboard(clipboard_text, "c")
            wait_for(lambda: native.clipboard_text() == clipboard_text)
            primary_text = "OSC52_PRIMARY_界🙂"
            program_clipboard(primary_text, "p")
            clipboard_text += "PRIMARY_APPLIED"
            program_clipboard(clipboard_text, "c")
            wait_for(lambda: native.clipboard_text() == clipboard_text)
            # Paste primary through the terminal's middle-button action while
            # the raw input probe records exactly what GTK delivers.
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerButton", "(ib)", (274, True))
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerButton", "(ib)", (274, False))
            wait_for(lambda: primary_text.encode() in appearance_events.read_bytes(), context=GLib.MainContext.default())
            assert native.clipboard_text() == clipboard_text
            wait_for(lambda: appearance(lambda s: not s["cursor_phase_visible"]))
            native.set_clipboard_text("PASTE_ACTIVITY")
            rpc("action", name="paste")
            wait_for(lambda: b'PASTE_ACTIVITY' in appearance_events.read_bytes(), context=GLib.MainContext.default())
            assert appearance()["cursor_phase_visible"], "Clipboard paste must restore cursor without movement"
            # All hyperlink hit tests use the last painted grid, including delayed replies.
            first_uri = "gmux-test-osc8://actual/first?literal=$(touch%20never);x=1"
            second_uri = "gmux-test-osc8://actual/second"
            def link_frame(uri, prefix=b''):
                return prefix + b'\x1b[H\x1b[2J\x1b[48;2;39;91;173m\x1b[58;2;240;77;151m\x1b]8;;' + uri.encode() + b'\x07label \xe7\x95\x8c wrapped\x1b]8;;\x07\x1b[0m'
            def apply_links(sequence):
                appearance_control.write_bytes(sequence)
                wait_for(lambda: appearance_applied.read_bytes() == sequence)
            def displayed_uri():
                targets = rpc("links", pane=pane)["hyperlinks"]["targets"]
                return targets[0] if targets else None
            def launched():
                return [json.loads(line)[0] for line in launches.read_text().splitlines()] if launches.exists() else []
            def link_click(*modifiers):
                for key in modifiers:
                    native.key(key, True)
                native.button(True)
                native.button(False)
                for key in reversed(modifiers):
                    native.key(key, False)
            apply_links(link_frame(first_uri, b'\x1b]22;crosshair\x07'))
            wait_for(lambda: displayed_uri() == first_uri)
            geometry = appearance()
            lx, ly, _, _ = wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds((39, 91, 173)))
            native.move(lx + 7.5 * geometry["cell_width"], ly + .5 * geometry["cell_height"])
            wait_for(lambda: appearance(lambda state: state["pointer_name"] == "pointer"))
            assert not launched(), "Hover must never launch"
            def underline(visible):
                return bool(native.frame(timeout=.1, fresh=False).color_bounds((240, 77, 151))) == visible
            wait_for(lambda: underline(False))
            # Modifier-only changes repaint underlines without pointer movement.
            native.key(Gdk.KEY_Control_L, True)
            wait_for(lambda: underline(True))
            for extra in [Gdk.KEY_Shift_L, Gdk.KEY_Alt_L]:
                native.key(extra, True)
                wait_for(lambda: underline(False))
                native.key(extra, False)
                wait_for(lambda: underline(True))
            native.key(Gdk.KEY_Control_R, True)
            native.key(Gdk.KEY_Control_L, False)
            wait_for(lambda: underline(True))
            native.key(Gdk.KEY_Control_R, False)
            wait_for(lambda: underline(False))
            # The window tracker also updates a hovered, unfocused terminal.
            # The search bar moves the grid, so reacquire its painted target
            # before testing modifier changes with the pointer stationary.
            rpc("action", name="find")
            wait_for(lambda: not appearance()["focused"])
            wait_for(lambda: appearance()["origin"][1] > geometry["origin"][1])
            lx, ly, _, _ = wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds((39, 91, 173)))
            native.move(lx + 7.5 * geometry["cell_width"], ly + .5 * geometry["cell_height"])
            wait_for(lambda: appearance()["pointer_name"] == "pointer")
            native.key(Gdk.KEY_Control_L, True)
            wait_for(lambda: underline(True))
            native.key(Gdk.KEY_Control_L, False)
            wait_for(lambda: underline(False))
            rpc("action", name="find")
            rpc("focus", pane=pane)
            wait_for(lambda: appearance()["origin"] == geometry["origin"])
            lx, ly, _, _ = wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds((39, 91, 173)))
            native.move(lx + 7.5 * geometry["cell_width"], ly + .5 * geometry["cell_height"])
            wait_for(lambda: appearance()["pointer_name"] == "pointer")
            # Ctrl may already be held when this window receives focus. Its
            # release, or a second modifier, must still clear stationary hover.
            Gtk.init()
            for extra in [None, Gdk.KEY_Shift_L]:
                native.key(Gdk.KEY_Control_L, True)
                wait_for(lambda: underline(True))
                probe_window = Gtk.Window(title="External modifier focus")
                probe_window.set_default_size(180, 100)
                probe_area = Gtk.DrawingArea()
                probe_area.set_draw_func(lambda _, cr, w, h: (cr.set_source_rgb(36/255, 161/255, 99/255), cr.paint()))
                probe_window.set_child(probe_area)
                try:
                    probe_window.present()
                    wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds((36, 161, 99)), context=GLib.MainContext.default())
                    wait_for(lambda: not rpc("renderer-info", pane=pane)["window_active"], context=GLib.MainContext.default())
                    wait_for(lambda: underline(False))
                    native.click_color((36, 161, 99))
                    native.key(Gdk.KEY_Control_L, False)
                    native.key(Gdk.KEY_Control_L, True)
                    # Enter this terminal while the external window still owns
                    # keyboard focus, then return focus without moving again.
                    native.move(lx + 7.5 * geometry["cell_width"], ly + .5 * geometry["cell_height"])
                finally:
                    probe_window.destroy()
                wait_for(lambda: rpc("renderer-info", pane=pane)["window_active"], context=GLib.MainContext.default())
                wait_for(lambda: underline(True))
                if extra is not None:
                    native.key(extra, True)
                else:
                    native.key(Gdk.KEY_Control_L, False)
                wait_for(lambda: underline(False))
                if extra is not None:
                    native.key(extra, False)
                    native.key(Gdk.KEY_Control_L, False)
                    wait_for(lambda: underline(False))
            link_click()
            link_click(Gdk.KEY_Control_L, Gdk.KEY_Shift_L)
            link_click(Gdk.KEY_Control_L, Gdk.KEY_Alt_L)
            assert not launched(), "Only Ctrl alone opens a link"
            # The contextual copy uses the OSC target, including a wide continuation.
            native.key(Gdk.KEY_Control_L, True)
            wait_for(lambda: underline(True))
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerButton", "(ib)", (273, True))
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerButton", "(ib)", (273, False))
            item = wait_for(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_name() == "Copy Link Address"), None))
            wait_for(lambda: underline(False))
            Atspi.Action.do_action(item.get_action_iface(), 0)
            wait_for(lambda: native.clipboard_text() == first_uri)
            wait_for(lambda: underline(True))
            native.key(Gdk.KEY_Control_L, False)
            link_click(Gdk.KEY_Control_L)
            wait_for(lambda: launched() == [first_uri])
            # While replies are paused, a click must still open the displayed old target.
            proxy.pause()
            before = proxy.reply_bytes
            apply_links(link_frame(second_uri))
            wait_for(lambda: proxy.reply_bytes > before)
            assert displayed_uri() == first_uri
            link_click(Gdk.KEY_Control_L)
            wait_for(lambda: launched() == [first_uri, first_uri])
            proxy.resume()
            wait_for(lambda: displayed_uri() == second_uri)
            wait_for(lambda: appearance(lambda state: state["pointer_name"] == "pointer"))
            # A same-text target change inside a hold stays hidden until release.
            apply_links(link_frame(first_uri, b'\x1b[?2026h'))
            assert displayed_uri() == second_uri
            link_click(Gdk.KEY_Control_L)
            wait_for(lambda: launched()[-1] == second_uri)
            apply_links(b'\x1b[?2026l')
            wait_for(lambda: displayed_uri() == first_uri)
            # Mouse reporting keeps ordinary clicks; Ctrl remains an explicit link gesture.
            apply_links(b'\x1b[?1000h\x1b[?1006h')
            before = len(launched())
            link_click()
            wait_for(lambda: b'\x1b[<' in appearance_events.read_bytes())
            assert len(launched()) == before
            link_click(Gdk.KEY_Control_L)
            wait_for(lambda: len(launched()) == before + 1)
            # Removal updates a stationary hover and releases the displayed map.
            native.key(Gdk.KEY_Control_L, True)
            wait_for(lambda: underline(True))
            apply_links(b'\x1b[?1000l\x1b[?1006l\x1b[H\x1b[2Jlabel \xe7\x95\x8c wrapped')
            wait_for(lambda: displayed_uri() is None)
            wait_for(lambda: appearance(lambda state: state["pointer_name"] == "crosshair"))
            wait_for(lambda: underline(False))
            native.key(Gdk.KEY_Control_L, False)
            # Cached previews have no targets, even when their text and target
            # positions match the old authoritative grid. Keep the pointer still.
            linked_row = b'\x1b]8;;' + first_uri.encode() + b'\x07label \xe7\x95\x8c wrapped\x1b]8;;\x07\r\n'
            apply_links(b'\x1b[H\x1b[2J\x1b[3J\x1b[58;2;240;77;151m' + linked_row * (appearance()["rows"] * 3) + b'\x1b[0m')
            wait_for(lambda: displayed_uri() == first_uri)
            wait_for(lambda: appearance(lambda state: state["scroll_cache_bytes"] > 0 and state["pointer_name"] == "pointer"))
            old_offset = appearance()["scrollback"]["offset"]
            proxy.pause()
            native.key(Gdk.KEY_Shift_L, True)
            native.key(Gdk.KEY_Page_Up, True)
            native.key(Gdk.KEY_Page_Up, False)
            native.key(Gdk.KEY_Shift_L, False)
            wait_for(lambda: appearance(lambda state: state["scroll_pending"] and state["scrollback"]["offset"] < old_offset))
            wait_for(lambda: displayed_uri() is None)
            wait_for(lambda: appearance(lambda state: state["pointer_name"] == "crosshair"))
            before = len(launched())
            native.key(Gdk.KEY_Control_L, True)
            wait_for(lambda: underline(False))
            native.button(True)
            native.button(False)
            assert_stays(lambda: len(launched()) == before and underline(False), .2,
                         message="A cached preview must not underline or open an authoritative URI")
            proxy.resume()
            wait_for(lambda: appearance(lambda state: not state["scroll_pending"]))
            wait_for(lambda: displayed_uri() == first_uri)
            wait_for(lambda: appearance(lambda state: state["pointer_name"] == "pointer"))
            wait_for(lambda: underline(True))
            native.key(Gdk.KEY_Control_L, False)
            wait_for(lambda: underline(False))
            native.key(Gdk.KEY_Shift_L, True)
            native.key(Gdk.KEY_Page_Down, True)
            native.key(Gdk.KEY_Page_Down, False)
            native.key(Gdk.KEY_Shift_L, False)
            wait_for(lambda: appearance(lambda state: not state["scroll_pending"] and
                state["scrollback"]["offset"] == state["scrollback"]["total"] - state["scrollback"]["len"]))
            apply_links(b'\x1b[H\x1b[2J\x1b[3Jplain')
            wait_for(lambda: displayed_uri() is None)
            appearance_control.write_bytes(b'stop')
            wait_for(appearance_done.is_file)
            wait_for(lambda: appearance(lambda s: s["pointer_name"] == "text"))

            # Raw and paste requests must both submit Enter through GTK and the daemon.
            for client in (True, False):
                for paste in (False, True):
                    marker = temp / f"send-enter-{client}-{paste}"
                    rpc("send", pane=pane, text=f"touch {shlex.quote(str(marker))}",
                        paste=paste, enter=True, client=client)
                    wait_for(marker.is_file)

            # Record bytes from real GDK events, including compositor-driven
            # auto-repeat. Control files keep the probe alive through focus moves.
            keyboard_probe = temp / "keyboard.py"
            keyboard_probe.write_text("""import os, select, sys, termios, tty
from pathlib import Path
flags, output, stop, done, title = sys.argv[1:]
old = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, f'\\x1b[>{flags}u\\x1b]2;{title}\\x07'.encode())
    with open(output, 'ab', buffering=0) as result:
        while not Path(stop).exists():
            if select.select([0], [], [], .01)[0]:
                result.write(os.read(0, 4096))
finally:
    os.write(1, b'\\x1b[<u')
    termios.tcsetattr(0, termios.TCSANOW, old)
Path(done).touch()
""")

            @contextmanager
            def keyboard_capture(flags):
                # Unique titles and files prevent a previous probe's replies
                # from satisfying readiness or completion checks.
                output = temp / f"keyboard-{time.monotonic_ns()}"
                stop, done = Path(str(output) + ".stop"), Path(str(output) + ".done")
                title = output.name
                output.touch()
                rpc("send", pane=pane, text="python3 " + " ".join(map(shlex.quote,
                    map(str, [keyboard_probe, flags, output, stop, done, title]))), paste=True, enter=True)
                wait_for(lambda: any(p["id"] == pane and p["title"] == title
                                     for t in rpc("list")["tabs"] for p in t["panes"]))
                try:
                    yield output.read_bytes
                except BaseException:
                    print("Keyboard capture:", flags, repr(output.read_bytes()[-2048:]), flush=True)
                    raise
                finally:
                    stop.touch()
                    wait_for(done.exists)

            def tap(symbol):
                native.key(symbol, True)
                native.key(symbol, False)

            def held(read, code, press, repeat, release):
                start = len(read())
                native.keycode(code, True)
                try:
                    wait_for(lambda: read()[start:].count(repeat) >= 2)
                finally:
                    native.keycode(code, False)
                wait_for(lambda: read()[start:].endswith(release))
                actual = read()[start:]
                repeats = actual.count(repeat)
                assert actual == press + repeat * repeats + release, (actual, press, repeat, release)

            # Modifier releases must retain the other side's held state.
            with keyboard_capture(31) as read_keys:
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
                expected = (b"\x1b[57441;2u\x1b[57447;2u\x1b[57441;2:3u\x1b[57447;1:3u"
                            b"\x1b[57448;5u\x1b[57442;5u\x1b[57448;5:3u\x1b[57442;1:3u"
                            b"\x1b[57443;3u\x1b[57443;1:3u"
                            b"\x1b[57358;65u\x1b[57358;65:3u\x1b[57358u\x1b[57358;1:3u")
                wait_for(lambda: read_keys().endswith(b"\x1b[57358;1:3u"))
                assert read_keys() == expected, (read_keys(), expected)
                held(read_keys, 30, b"\x1b[97;;97u", b"\x1b[97;1:2;97u", b"\x1b[97;1:3u")
                held(read_keys, 103, b"\x1b[1;1:1A", b"\x1b[1;1:2A", b"\x1b[1;1:3A")
                # Repeat classification follows hardware identity when Shift
                # changes the keyval and text while the same key stays down.
                native.keycode(30, True)
                wait_for(lambda: read_keys().endswith(b"\x1b[97;;97u"))
                native.key(Gdk.KEY_Shift_R, True)
                try:
                    wait_for(lambda: b"\x1b[97:65;2:2;65u" in read_keys())
                finally:
                    native.keycode(30, False)
                    native.key(Gdk.KEY_Shift_R, False)
                wait_for(lambda: read_keys().endswith(b"\x1b[57447;1:3u"))

                # A shortcut owns its physical key until release, even if its
                # modifiers are released first and native repeat is still active.
                native.key(Gdk.KEY_Control_L, True)
                native.key(Gdk.KEY_Shift_L, True)
                native.keycode(46, True)  # Ctrl+Shift+C
                time.sleep(.04)
                native.key(Gdk.KEY_Shift_L, False)
                native.key(Gdk.KEY_Control_L, False)
                wait_for(lambda: read_keys().endswith(b"\x1b[57442;1:3u"))
                start = len(read_keys())
                time.sleep(.7)
                native.keycode(46, False)
                # The next tap's release is an ordered marker for all preceding
                # input, including the suppressed hold's release.
                tap(Gdk.KEY_c)
                wait_for(lambda: read_keys()[start:].endswith(b"\x1b[99;1:3u"))
                assert read_keys()[start:] == b"\x1b[99;;99u\x1b[99;1:3u"

                # Compose commits once as text, without reusing a held action.
                start = len(read_keys())
                tap(Gdk.KEY_F12)
                tap(Gdk.KEY_e)
                wait_for(lambda: read_keys()[start:].endswith(b"\x1b[101;1:3u"))
                assert read_keys()[start:].count("é".encode()) == 1, read_keys()[start:]
                assert b":2" not in read_keys()[start:], read_keys()[start:]

                # Releases delivered to a search entry, another pane, or an
                # external window cannot leave a held bit in this terminal.
                for destination in ("search", "pane", "menu", "window"):
                    print("Keyboard focus reset:", destination, flush=True)
                    native.keycode(30, True)
                    wait_for(lambda: read_keys().endswith(b"\x1b[97;;97u"))
                    if destination == "search":
                        rpc("action", name="find")
                        wait_for(lambda: not rpc("renderer-info", pane=pane)["viewport"]["focused"])
                    elif destination == "pane":
                        other_pane = rpc("split", pane=pane)["pane"]
                        wait_for(lambda: rpc("renderer-info", pane=other_pane)["viewport"]["focused"])
                    elif destination == "menu":
                        tap(Gdk.KEY_Menu)
                        wait_for(lambda: not rpc("renderer-info", pane=pane)["viewport"]["focused"])
                    else:
                        probe_window = Gtk.Window(title="Keyboard focus reset")
                        probe_window.set_default_size(180, 100)
                        probe_window.present()
                        wait_for(lambda: not rpc("renderer-info", pane=pane)["window_active"],
                                 context=GLib.MainContext.default())
                    native.keycode(30, False)
                    if destination == "search":
                        rpc("action", name="find")
                    elif destination == "pane":
                        rpc("close", pane=other_pane)
                    elif destination == "menu":
                        tap(Gdk.KEY_Escape)
                    else:
                        probe_window.destroy()
                        wait_for(lambda: rpc("renderer-info", pane=pane)["window_active"],
                                 context=GLib.MainContext.default())
                    rpc("focus", pane=pane)
                    wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"])
                    # Popup keyboard grabs return after GTK restores widget
                    # focus. Wait for a fresh compositor frame before typing.
                    native.drain_frames()
                    native.frame()
                    start = len(read_keys())
                    tap(Gdk.KEY_a)
                    wait_for(lambda: read_keys()[start:].endswith(b"\x1b[97;1:3u"))
                    assert read_keys()[start:] == b"\x1b[97;;97u\x1b[97;1:3u", (destination, read_keys()[start:])

                # Two frontend windows over the same session own separate key
                # histories. Closing one while a key is held also resets focus.
                first_window = rpc("windows")["windows"][0]["id"]
                native.keycode(30, True)
                wait_for(lambda: read_keys().endswith(b"\x1b[97;;97u"))
                rpc("action", window=first_window, name="new-window")
                second_window = wait_for(lambda: next((w["id"] for w in rpc("windows")["windows"]
                    if w["id"] != first_window), None))
                wait_for(lambda: any(p["id"] == pane for t in rpc("list", window=second_window)["tabs"]
                                     for p in t["panes"]))
                rpc("focus", window=second_window, pane=pane)
                wait_for(lambda: rpc("renderer-info", window=second_window, pane=pane)["window_active"])
                native.keycode(30, False)
                wait_for(lambda: read_keys().endswith(b"\x1b[97;1:3u"))
                start = len(read_keys())
                tap(Gdk.KEY_a)
                wait_for(lambda: read_keys()[start:].endswith(b"\x1b[97;1:3u"))
                assert read_keys()[start:] == b"\x1b[97;;97u\x1b[97;1:3u", read_keys()[start:]
                native.keycode(30, True)
                wait_for(lambda: read_keys().endswith(b"\x1b[97;;97u"))
                rpc("detach", window=second_window)
                wait_for(lambda: rpc("status")["local_clients"] == 1)
                wait_for(lambda: rpc("renderer-info", window=first_window, pane=pane)["window_active"])
                native.keycode(30, False)
                wait_for(lambda: read_keys().endswith(b"\x1b[97;1:3u"))
                held(read_keys, 30, b"\x1b[97;;97u", b"\x1b[97;1:2;97u", b"\x1b[97;1:3u")

            # With no event reporting, real native holds retain legacy bytes.
            with keyboard_capture(0) as read_keys:
                native.keycode(30, True)
                try:
                    wait_for(lambda: len(read_keys()) >= 3)
                finally:
                    native.keycode(30, False)
                tap(Gdk.KEY_b)
                wait_for(lambda: read_keys().endswith(b"b"))
                actual = read_keys()
                assert actual == b"a" * (len(actual) - 1) + b"b", actual

            command = r"printf '\033[2J\033[H\033[48;2;230;30;45mKEYBOARD_INPUT_OK\033[0m\n'"
            for char in command + "\n":
                symbol = Gdk.KEY_Return if char == "\n" else Gdk.unicode_to_keyval(ord(char))
                native.key(symbol, True)
                native.key(symbol, False)
                time.sleep(0.005)
            wait_for(lambda: "KEYBOARD_INPUT_OK" in rpc("read", pane=pane)["text"].splitlines())
            # Topology changes can replace the terminal widget and its
            # accessible object. Use the current object after the focus tests.
            terminal = wait_for(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_role_name() == "terminal"
                and obj.get_text_iface() is not None), None), context=GLib.MainContext.default())
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

            # No daemon reply is available while recognizing and reversing a drag.
            rpc("send", pane=pane, text="printf '\\033[3J\\033[2J\\033[Hone café_界é two\\n  line two\\n'", paste=True, enter=True)
            wait_for(lambda: Atspi.Text.get_text(text_iface, 0, -1).startswith("one café_界é two"))
            ch = rpc("renderer-info", pane=pane)["viewport"]["cell_height"]

            def move_cell(col, row):
                native.move(left + cw * (col + .5), top + ch * (row + .5))

            def repeat_click(col, row, count, hold=False):
                # End the preceding GTK click sequence before starting another.
                time.sleep(.5)
                move_cell(col, row)
                for index in range(count):
                    native.button(True)
                    time.sleep(.025)
                    if hold and index == count - 1:
                        return
                    native.button(False)
                    time.sleep(.025)

            def copied(expected):
                rpc("action", name="copy")
                wait_for(lambda: native.clipboard_text() == expected,
                         message=f"Selection must copy {expected!r}")

            def resume_selection(expected):
                native.button(False)
                proxy.delay = .035
                proxy.resume()
                deadline = time.monotonic() + .3
                while time.monotonic() < deadline:
                    assert rpc("renderer-info", pane=pane)["viewport"]["selection"] == expected, "A stale selection reply replaced the current highlight"
                    time.sleep(.005)
                proxy.delay = 0

            proxy.pause()
            repeat_click(10, 0, 2, hold=True)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [4, 11],
                     message="Double-click must select the complete word")
            selected = wait_for(lambda: Atspi.Text.get_selection(text_iface, 0)
                                if Atspi.Text.get_n_selections(text_iface) else None)
            assert Atspi.Text.get_text(text_iface, selected.start_offset, selected.end_offset) == "café_界é"
            move_cell(14, 0)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [4, 15])
            move_cell(1, 0)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [0, 11])
            resume_selection([0, 11])
            copied("one café_界é")

            # Output can change word bounds without moving the held pointer.
            repeat_click(10, 0, 2, hold=True)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [4, 11])
            rpc("send", pane=pane, text="printf '\\033[Hone alphabetlong two'", paste=True, enter=True)
            wait_for(lambda: Atspi.Text.get_text(text_iface, 0, -1).startswith("one alphabetlong two"))
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [4, 15])
            native.button(False)
            copied("alphabetlong")

            wrapped_line = "wrapped_" + "x" * cols
            rpc("send", pane=pane, text=f"printf '\\033[3J\\033[2J\\033[H{wrapped_line}\\n  line two\\n'", paste=True, enter=True)
            wait_for(lambda: Atspi.Text.get_text(text_iface, 0, -1).startswith("wrapped_"))
            proxy.pause()
            repeat_click(5, 1, 3, hold=True)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [0, 2 * cols - 1],
                     message="Triple-click must select the complete wrapped line")
            move_cell(3, 2)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [0, 3 * cols - 1])
            resume_selection([0, 3 * cols - 1])
            copied(wrapped_line + "\n  line two")
            repeat_click(5, 1, 1)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] is None)

            # Empty rows must stay unselected in both the immediate preview and
            # delayed engine replies, including after reversing a line drag.
            proxy.pause()
            repeat_click(5, 5, 3, hold=True)
            assert rpc("renderer-info", pane=pane)["viewport"]["selection"] is None
            assert Atspi.Text.get_n_selections(text_iface) == 0
            move_cell(3, 2)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] == [2 * cols, 6 * cols - 1])
            move_cell(5, 5)
            wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["selection"] is None)
            resume_selection(None)
            assert Atspi.Text.get_n_selections(text_iface) == 0

            # Mouse-reporting applications keep repeat clicks unless Shift is held.
            mouse_probe = temp / "mouse.py"
            mouse_result = temp / "mouse-events"
            mouse_probe.write_text("""import os, sys, termios, tty
old = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, b'\\x1b[3J\\x1b[2J\\x1b[Hone two\\r\\n\\x1b[?1000h\\x1b[?1006h\\x1b]2;SELECTION_MOUSE_READY\\x07')
    with open(sys.argv[1], 'wb', buffering=0) as events:
        while True:
            data = os.read(0, 128)
            if b'q' in data:
                break
            events.write(data)
finally:
    os.write(1, b'\\x1b[?1000l\\x1b[?1006l')
    termios.tcsetattr(0, termios.TCSANOW, old)
""")
            rpc("send", pane=pane,
                text=f"python3 {shlex.quote(str(mouse_probe))} {shlex.quote(str(mouse_result))}",
                paste=True, enter=True)
            wait_for(lambda: any(p["id"] == pane and p["title"] == "SELECTION_MOUSE_READY"
                                 for t in rpc("list")["tabs"] for p in t["panes"]))
            repeat_click(1, 0, 3)
            wait_for(lambda: mouse_result.read_bytes().count(b"\x1b[<0;") == 6)
            assert rpc("renderer-info", pane=pane)["viewport"]["selection"] is None
            reported = mouse_result.read_bytes()
            native.key(Gdk.KEY_Shift_L, True)
            repeat_click(1, 0, 2)
            copied("one")
            native.key(Gdk.KEY_Shift_L, False)
            assert mouse_result.read_bytes() == reported
            rpc("send", pane=pane, text="q")
            wait_for(lambda: "GMUX_INPUT>" in Atspi.Text.get_text(text_iface, 0, -1))

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

            # Hold viewport replies while native touchpad input repeatedly
            # reverses. The scrollbar must retain the latest local target until
            # the matching viewport arrives, including through older replies.
            native.move(left + cw * 6.5, (viewport_top + viewport_bottom) / 2)
            assert 0 < state["scroll_cache_bytes"] <= 2 * 1024 * 1024, state
            # Keep changing the live screen while viewing immutable history.
            # Cache prediction must survive updates received before replies pause.
            live_stop = temp / "live-stop"
            live_ready = temp / "live-ready"
            live_probe = temp / "live-output.py"
            live_probe.write_text("import os, time\nfrom pathlib import Path\n"
                f"stop, ready = Path({str(live_stop)!r}), Path({str(live_ready)!r})\n"
                "deadline = time.monotonic() + 5\nready.touch()\n"
                "while not stop.exists() and time.monotonic() < deadline:\n"
                "    os.write(1, b'\\x1b7\\x1b[999;1Hlive output\\x1b8')\n"
                "    time.sleep(.01)\n")
            rpc("send", pane=pane, text=f"python3 {shlex.quote(str(live_probe))} &", paste=True, enter=True)
            wait_for(live_ready.exists)
            time.sleep(.2)
            proxy.pause()
            proxy.delay = .015
            reply_bytes = proxy.reply_bytes
            reply_cell_bytes = proxy.reply_cell_bytes
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                        "(ddu)", (0.0, 20 * row_height, 4))
            time.sleep(.1)
            local_scroll_ms = []
            for index in range(40):
                event_started = time.monotonic()
                native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                            "(ddu)", (0.0, (-1 if index % 2 == 0 else 1) * 3 * row_height, 4))
                observed = []
                def predicted():
                    state = rpc("renderer-info", pane=pane)["viewport"]
                    observed[:] = [state]
                    target = int(state["scrollbar"]["value"] / row_height + .5)
                    return state if state["scrollback"]["offset"] == target else None
                try:
                    wait_for(predicted, timeout=1)
                except AssertionError as error:
                    raise AssertionError(("Cached content must move while replies are held", observed)) from error
                local_scroll_ms.append((time.monotonic() - event_started) * 1000)
                time.sleep(.02)
            native.call(REMOTE, native.remote, REMOTE + ".Session", "NotifyPointerAxis",
                        "(ddu)", (0.0, 0.0, 1))
            # GTK can finish kinetic motion after the final axis event. Let
            # that local motion settle while authoritative replies stay held.
            gesture = dict(value=None, since=time.monotonic())
            def gesture_stopped():
                state = rpc("renderer-info", pane=pane)["viewport"]
                value = state["scrollbar"]["value"]
                if value != gesture["value"]:
                    gesture.update(value=value, since=time.monotonic())
                return state if time.monotonic() - gesture["since"] >= .1 else None
            state = wait_for(gesture_stopped, timeout=3)
            target = int(state["scrollbar"]["value"] / row_height + .5)
            assert state["scroll_pending"] and 0 < target < state["scrollback"]["total"] - state["rows"], state
            started = time.monotonic()
            proxy.resume()

            def scroll_settled():
                state = rpc("renderer-info", pane=pane)["viewport"]
                assert int(state["scrollbar"]["value"] / row_height + .5) == target, ("A stale viewport reset the scrollbar", target, state)
                return state if not state["scroll_pending"] and state["scrollback"]["offset"] == target else None

            wait_for(scroll_settled)
            report = dict(settle_ms=(time.monotonic() - started) * 1000,
                          reply_bytes=proxy.reply_bytes - reply_bytes, target=target,
                          reply_cell_bytes=proxy.reply_cell_bytes - reply_cell_bytes,
                          local_scroll_p95_ms=sorted(local_scroll_ms)[37],
                          cache_bytes=state["scroll_cache_bytes"])
            # Output still delivers cache boundaries and metadata. Bound the
            # cell payload independently of how long live output continues.
            reply_budget = 2 * state["cols"] * state["rows"] * 75
            assert report["reply_cell_bytes"] <= reply_budget, ("Obsolete scroll targets queued viewport data", report, reply_budget)
            (artifacts / "scroll-sync.json").write_text(json.dumps(report, indent=2) + "\n")
            print("Rapid scroll reversals: " + json.dumps(report), flush=True)
            live_stop.touch()
            proxy.delay = 0
            for _ in range(12):
                page(Gdk.KEY_Page_Up)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] == 0))

            # Page shortcuts share the same pending target as pointer input.
            proxy.pause()
            native.key(Gdk.KEY_Shift_L, True)
            for index in range(21):
                symbol = Gdk.KEY_Page_Down if index % 2 == 0 else Gdk.KEY_Page_Up
                native.key(symbol, True)
                native.key(symbol, False)
                time.sleep(.02)
            native.key(Gdk.KEY_Shift_L, False)
            time.sleep(.05)
            state = rpc("renderer-info", pane=pane)["viewport"]
            target = min(state["rows"], state["scrollback"]["total"] - state["rows"])
            assert state["scroll_pending"] and int(state["scrollbar"]["value"] / row_height + .5) == target, state
            proxy.resume()
            wait_for(scroll_settled)
            for _ in range(12):
                page(Gdk.KEY_Page_Up)
            wait_for(lambda: viewport(lambda state: state["scrollback"]["offset"] == 0))

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
            assert rpc("status")["local_clients"] and app.poll() is None
            print("PASS: native Wayland keyboard input, Kitty press/repeat/release and focus resets, composition and shortcut holds, Kitty modifier presses/releases with both sides and Caps Lock, "
                  "OSC 8 hover/copy/Ctrl-click through an isolated URI handler with held and delayed frames, "
                  "accessible terminal text, caret and selection, composited output, "
                  "color-only text reuse without notifications and repeated Unicode row replacement notifications, "
                  "wide-glyph geometry, double/triple-click selection, line dragging, and Shift selection in mouse-reporting applications, "
                  "rapid touchpad and keyboard scroll reversals with delayed replies and a stable scrollbar target, "
                  "stationary selection autoscroll in both directions, rapid drag reversals without stale highlights, "
                  "fast edge drags during overscroll without viewport bouncing, "
                  "edge and release cancellation, "
                  "selection across scrollback, Ctrl+Shift+C, OSC 52 clipboard/primary writes, and large Unicode clipboard paste with XWayland disabled")
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
