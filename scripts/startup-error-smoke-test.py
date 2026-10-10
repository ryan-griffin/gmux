#!/usr/bin/env python3
"""Exercise startup alerts before a main window exists, using private fixtures."""
from contextlib import contextmanager
from collections import Counter
import json
from pathlib import Path
import socket
import subprocess
import threading
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import Atspi, Gdk, GdkPixbuf, GLib

from cargo_test_support import cargo_binary
from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
heading = "Could Not Start Gmux"
incompatible = "Incompatible gmux daemon. Stop it with gmux quit before upgrading."


@contextmanager
def incompatible_daemon(fixture):
    """Reply to the real startup handshake, without launching or stopping a daemon."""
    path = Path(fixture.env["GMUX_SOCKET"])
    requests, errors = [], []
    stopped = threading.Event()
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(path))
        listener.listen()
        listener.settimeout(0.1)

        def serve():
            try:
                while not stopped.is_set():
                    try:
                        connection, _ = listener.accept()
                    except socket.timeout:
                        continue
                    with connection:
                        connection.settimeout(2)
                        request = json.loads(connection.makefile("rb").readline())
                        requests.append(request)
                        connection.sendall(b'{"version":0,"quitting":false}\n')
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            yield requests
        finally:
            stopped.set()
            worker.join(timeout=3)
            assert not worker.is_alive(), "Startup handshake fixture did not stop"
            path.unlink()
            assert not errors, errors
            assert requests == [{"method": "_hello"}], requests


def elements(app):
    return [obj for obj in descendants(Atspi.get_desktop(0))
            if obj.get_process_id() == app.pid]


def showing(obj):
    return obj.get_state_set().contains(Atspi.StateType.SHOWING)


def find(app, name=None, role=None):
    return next((obj for obj in elements(app) if showing(obj)
                 and (name is None or obj.get_name() == name)
                 and (role is None or obj.get_role() == role)), None)


def focused(obj):
    obj.clear_cache()
    return obj.get_state_set().contains(Atspi.StateType.FOCUSED)


def bounds(obj):
    # GTK on Wayland cannot report global screen positions. Window-relative
    # extents still describe the native layout and each response's allocation.
    rect = obj.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    return rect.x, rect.y, rect.width, rect.height


def chord(native, *keys):
    for key in keys:
        native.key(key, True)
    time.sleep(0.05)
    for key in reversed(keys):
        native.key(key, False)


def capture(native, alert, close, name):
    # Accessibility can expose allocations before the compositor draws them.
    deadline = time.monotonic() + 0.3
    while time.monotonic() < deadline:
        frame = native.frame(timeout=0.1, fresh=False)
    pixels = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(frame.data),
        GdkPixbuf.Colorspace.RGB, True, 8, frame.width, frame.height, frame.stride)
    # This private compositor contains only the alert. Locate its neutral
    # background in the composited frame rather than using unavailable screen
    # coordinates from AT-SPI. The fixture's desktop background is blue.
    colors = Counter(zip(frame.data[0::4], frame.data[1::4], frame.data[2::4]))
    neutral = next(rgb for rgb, count in colors.most_common() if max(rgb) - min(rgb) < 5)
    left, top, right, bottom = frame.color_bounds(neutral)
    _, _, width, height = bounds(alert)
    x, y = (left + right - width) // 2, (top + bottom - height) // 2
    assert 0 <= x < x + width <= frame.width, bounds(alert)
    assert 0 <= y < y + height <= frame.height, bounds(alert)
    pixels.new_subpixbuf(x, y, width, height).savev(
        str(artifacts / f"startup-error-{name}.png"), "png", [], [])
    # The response's interior stays neutral in light, dark, and high contrast.
    # A focus outline may use the accent color; it is separate from the fill.
    bx, by, bw, bh = bounds(close)
    rgb = frame.pixel(x + bx + bw // 4, y + by + bh // 2)
    assert max(rgb) - min(rgb) < 20, (name, "Close has an accent-colored fill", rgb)
    return x, y


def inspect(app, *, long=False):
    alert = wait_accessible(lambda: find(app, heading, Atspi.Role.ALERT))
    close = wait_accessible(lambda: find(app, "Close", Atspi.Role.PUSH_BUTTON))
    wait_accessible(lambda: focused(close), message="Close did not receive initial keyboard focus")
    buttons = [obj for obj in elements(app) if showing(obj) and obj.get_role() == Atspi.Role.PUSH_BUTTON]
    assert len(buttons) == 1, [(obj.get_name(), obj.get_role_name()) for obj in buttons]
    assert not any(showing(obj) and obj.get_role() == Atspi.Role.ICON for obj in elements(app))
    scrollbars = [obj for obj in elements(app) if showing(obj) and obj.get_role() == Atspi.Role.SCROLL_BAR]
    x, y, width, height = bounds(alert)
    bx, by, bw, bh = bounds(close)
    assert x <= bx < bx + bw <= x + width and y <= by < by + bh <= y + height
    if long:
        assert scrollbars, "Long error text did not exercise the alert's scrollable body"
        assert all(bounds(obj)[1] + bounds(obj)[3] <= by for obj in scrollbars), \
            "The Close response scrolls with the message"
    else:
        assert not scrollbars, "The normal startup error unnecessarily scrolls"
        assert width < 560 and height < 340, ("Startup alert kept the oversized allocation", bounds(alert))
    relations = alert.get_relation_set()
    descriptions = [relation.get_target(index) for relation in relations
                    if relation.get_relation_type() == Atspi.RelationType.DESCRIBED_BY
                    for index in range(relation.get_n_targets())]
    assert descriptions, "The alert has no accessible error description"
    return alert, close, descriptions[0]


with (artifacts / "startup-error-smoke.log").open("w") as log, \
        WaylandInput() as native, AccessibilityFixture():
    for appearance, high_contrast, dismissal in (
        ("light", False, (Gdk.KEY_Return,)),
        ("dark", False, (Gdk.KEY_Escape,)),
        ("light", True, (Gdk.KEY_space,)),
        ("dark", True, (Gdk.KEY_Alt_L, Gdk.KEY_c)),
        ("light", False, None),
    ):
        name = appearance + ("-hc" if high_contrast else "")
        if dismissal is None:
            name += "-pointer"
        with scenario(f"incompatible daemon, {name}"), AppFixture(
            dict(appearance=appearance), prefix="gmux-startup-",
            env={"GTK_A11Y": "atspi", "ADW_DEBUG_HIGH_CONTRAST": str(int(high_contrast))},
        ) as fixture, incompatible_daemon(fixture):
            app = fixture.start(cargo_binary("gmux"), cwd=root, stdout=log, stderr=log)
            alert, close, body = inspect(app)
            assert body.get_name() == incompatible, body.get_name()
            assert app.poll() is None, "The application did not retain its standalone alert"
            # Both activation and directory opening must raise this same alert.
            for args in ((), ("--directory", str(root))):
                subprocess.run([str(cargo_binary("gmux")), *args], env=fixture.env,
                               cwd=root, stdout=log, stderr=log, timeout=8, check=True)
                assert len([obj for obj in elements(app) if obj.get_role() == Atspi.Role.ALERT]) == 1
            origin_x, origin_y = capture(native, alert, close, name)
            if dismissal is not None:
                chord(native, Gdk.KEY_Tab)
                if not focused(close):
                    chord(native, Gdk.KEY_Shift_L, Gdk.KEY_Tab)
                wait_accessible(lambda: focused(close))
                capture(native, alert, close, name + "-keyboard")
            if dismissal is None:
                x, y, width, height = bounds(close)
                native.move(origin_x + x + width / 2, origin_y + y + height / 2)
                native.button(True)
                native.button(False)
            else:
                chord(native, *dismissal)
            assert app.wait(timeout=8) == 1, "Dismissal lost the startup failure exit status"
            assert not Path(fixture.env["GMUX_SOCKET"] + ".ui").exists()

    for name, dismissal in (("long", (Gdk.KEY_Return,)), ("long-system-close", (Gdk.KEY_Alt_L, Gdk.KEY_F4))):
        with scenario(name), AppFixture(dict(appearance="dark"), prefix="gmux-startup-long-",
                                       env={"GTK_A11Y": "atspi"}) as fixture:
            # Real storage failures include paths and chained parse errors. Use a
            # path long enough to overflow the monitor, including markup characters.
            config = fixture.path.joinpath(*[f"details-<&>-{index}-" + "x" * 140 for index in range(16)])
            (config / "gmux").mkdir(parents=True)
            (config / "gmux/settings.json").write_bytes(fixture.settings_path.read_bytes())
            malformed = config / "gmux/workspaces.json"
            malformed.write_text("{")
            fixture.env["XDG_CONFIG_HOME"] = str(config)
            app = fixture.start(cargo_binary("gmux"), "--directory", root,
                                cwd=root, stdout=log, stderr=log)
            alert, close, body = inspect(app, long=True)
            description = body.get_name()
            assert str(malformed) in description and "EOF" in description, description
            assert "<&>" in description, "Error details were interpreted as markup"
            capture(native, alert, close, name)
            if name == "long":
                chord(native, Gdk.KEY_Tab)
                wait_accessible(lambda: not focused(close), message="Tab did not reach the scrollable message")
                chord(native, Gdk.KEY_Shift_L, Gdk.KEY_Tab)
                wait_accessible(lambda: focused(close), message="Shift+Tab did not return to Close")
                chord(native, Gdk.KEY_Tab)
                wait_accessible(lambda: not focused(close))
                scrollbar = find(app, role=Atspi.Role.SCROLL_BAR)
                value = scrollbar.get_value_iface()
                before = value.get_current_value()
                chord(native, Gdk.KEY_End)
                wait_accessible(lambda: value.get_current_value() > before,
                                message="Keyboard scrolling did not reach the long error details")
                assert showing(close), "Scrolling the error hid Close"
                # Enter must still activate the default response when the body has focus.
            chord(native, *dismissal)
            assert app.wait(timeout=8) == 1
            assert malformed.read_text() == "{", "Presenting the failure changed the corrupt file"
            assert not Path(fixture.env["GMUX_SOCKET"]).exists(), "Storage failure launched a daemon"

    log.flush()
    output = (artifacts / "startup-error-smoke.log").read_text()
    assert output.count("Could not start Gmux:") == 7, output
    assert not any(message in output for message in ("CRITICAL", "WARNING", "Gtk-WARNING", "Adwaita-CRITICAL")), output
print("PASS: startup alerts fit their content, keep Close visible and focused, expose error details, "
      "and dismiss through the keyboard without touching a running daemon")
