#!/usr/bin/env python3
"""Check that damaged saved state shows recovery instructions and stays unchanged."""
from pathlib import Path
from cargo_test_support import cargo_binary
import subprocess

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible

root = Path(__file__).resolve().parent.parent
with AccessibilityFixture():
    for name in ("settings.json", "workspaces.json", "session.json"):
        with scenario(f"startup recovery preserves damaged {name}"), AppFixture(
            {"restore": True}, env={"GTK_A11Y": "atspi"},
        ) as fixture:
            path = fixture.config / name
            damaged = b'{"broken":'
            path.write_bytes(damaged)
            app = fixture.start(cargo_binary("gmux"), stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)

            def elements():
                return [node for node in descendants(Atspi.get_desktop(0)) if node.get_process_id() == app.pid]

            wait_accessible(lambda: any(node.get_name() == "Could Not Start Gmux" for node in elements()))
            wait_accessible(lambda: any(str(path) in node.get_name() and "unchanged" in node.get_name()
                                        for node in elements()))
            assert path.read_bytes() == damaged
            close = wait_accessible(lambda: next((node for node in elements()
                if node.get_name() == "Close" and node.get_role_name() == "button"), None))
            assert close.get_action_iface().do_action(0)
            assert app.wait(timeout=8) != 0
