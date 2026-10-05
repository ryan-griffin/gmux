#!/usr/bin/env python3
"""Check conditional window closure and preservation of background terminals."""
from pathlib import Path
import shlex
import time

import gi
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk

from test_support import AppFixture, scenario, wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def close_shortcut(native):
    keys = (Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_w)
    for key in keys:
        native.key(key, True)
    time.sleep(0.05)
    for key in reversed(keys):
        native.key(key, False)
    # Let Mutter process the releases before a second chord reuses these keys.
    time.sleep(0.05)


with (artifacts / "window-close-smoke.log").open("w") as log, WaylandInput() as native:
    for case in ("empty startup", "last pane", "empty workspace", "Close Window command"):
        with scenario(case), AppFixture(
            dict(restore=False, desktop_notifications=False), env={"GTK_A11Y": "none"},
            prefix="gmux-window-close-",
            shell_script="#!/bin/sh\nexport PS1='GMUX_CLOSE> '\nstty -echo\n"
                         "exec /bin/bash --noprofile --norc -i\n",
        ) as fixture:
            app = fixture.start_app(root / "target/debug/gmux", cwd=root, stdout=log, stderr=log)
            wait_for(lambda: fixture.rpc("renderer-info")["window_active"])
            daemon_pid = fixture.rpc("daemon-info")["pid"]
            if case != "empty startup":
                pane = fixture.rpc("new")["pane"]
                wait_for(lambda: "GMUX_CLOSE>" in fixture.rpc("read", pane=pane)["text"])
                wait_for(lambda: fixture.rpc("renderer-info")["viewport"]["focused"])
                if case == "last pane":
                    close_shortcut(native)
                    wait_for(lambda: not fixture.rpc("list")["tabs"])
                    assert app.poll() is None, "Closing the last pane also closed the window"
                else:
                    ready = fixture.path / "busy-ready"
                    fixture.rpc("send", pane=pane, text="touch " + shlex.quote(str(ready)) + "; sleep 60",
                                paste=True, enter=True)
                    wait_for(ready.exists)
                    if case == "empty workspace":
                        workspace = fixture.rpc("workspace-new", name="Empty", directory=str(root))["workspace"]
                        fixture.rpc("filter", workspace=workspace)
            if case == "Close Window command":
                fixture.rpc("action", name="close-window")
            else:
                assert fixture.rpc("list")["active_tab"] is None
                close_shortcut(native)
            assert app.wait(timeout=8) == 0
            info = fixture.rpc("daemon-info")
            assert info["pid"] == daemon_pid and not info["attached"], info
            if case in ("empty workspace", "Close Window command"):
                tabs = fixture.rpc("list")["tabs"]
                assert len(tabs) == 1 and tabs[0]["active_pane"] == pane
                assert not tabs[0]["panes"][0]["exited"]
    print("PASS: Ctrl+Shift+W closes a pane or empty window, and window closure "
          "preserves the daemon and running terminals")
