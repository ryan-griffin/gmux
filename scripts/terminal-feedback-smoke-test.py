#!/usr/bin/env python3
"""Check transient terminal errors and connection banners in native GTK."""
import socket
import time
from pathlib import Path

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Atspi, Gdk

from cargo_test_support import cargo_binary
from test_support import (AccessibilityFixture, AppFixture, assert_stays, descendants,
                          scenario, wait_accessible, wait_for)
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)
daemon, client = cargo_binary("gmux"), cargo_binary("gmux-gtk")

with (artifacts / "terminal-feedback.log").open("w") as log, AccessibilityFixture(), \
        AppFixture(env={"GTK_A11Y": "atspi"}) as desktop, \
        AppFixture(shell_script="#!/bin/sh\nprintf 'Build server ready\\nWaiting for a build command.\\n'\nexec sleep 600\n") as server, WaylandInput() as native:
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        address = f"127.0.0.1:{reservation.getsockname()[1]}"
    server.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address=address, authentication={"mode": "none"}))
    process = server.start(daemon, "serve", stdout=log, stderr=log)
    wait_for(lambda: server.rpc("status")["pid"] == process.pid)
    remote = server.rpc("new", name="Build <&>", directory=str(server.path))
    server.rpc("split", pane=remote["pane"])
    desktop.rpc("connection-add", name="build-server", address=address, authentication={"mode": "none"})
    app = desktop.start_app(client, stdout=log, stderr=log)
    local = desktop.rpc("new", connection="local", name="Local")
    desktop.rpc("focus", connection="build-server", pane=remote["pane"])
    wait_for(lambda: desktop.rpc("renderer-info")["viewport"]["width"] > 0)

    def showing(text, role=None):
        return [node for node in descendants(Atspi.get_desktop(0))
                if node.get_process_id() == app.pid and text in node.get_name()
                and node.get_state_set().contains(Atspi.StateType.SHOWING)
                and (role is None or node.get_role_name() == role)]

    def click(text):
        button = wait_accessible(lambda: next(iter(showing(text, "button")), None))
        assert Atspi.Action.do_action(button.get_action_iface(), 0)

    def screenshot(name):
        desktop.rpc("screenshot", path=str(artifacts / f"terminal-feedback-{name}.png"))

    def viewport():
        return desktop.rpc("renderer-info", pane=remote["pane"])["viewport"]

    with scenario("plain error toast, duplicate suppression and explicit details without resizing terminals"):
        # This program has no command markers, so selecting output really fails
        # in the daemon. No UI-only injection or test API is involved.
        before = viewport()
        desktop.rpc("action", name="select-output")
        wait_accessible(lambda: len(showing("Terminal operation failed", "label")) == 1)
        assert any("Build <&>" in node.get_name() for node in showing("Terminal operation failed", "label"))
        for _ in range(12):
            desktop.rpc("action", name="select-output")
        assert len(showing("Details", "button")) == 1
        now = viewport()
        assert all(now[key] == before[key] for key in ("width", "height", "cols", "rows")), (before, now)
        screenshot("toast")
        click("Details")
        wait_accessible(lambda: showing("No command output available", "label"))
        screenshot("details")
        click("Close")
        wait_accessible(lambda: not showing("No command output available", "label"))
        # Retry after the cooldown, then verify native AdwToast expiry and that
        # the duplicate burst has not left a queue of stale errors behind it.
        def retry():
            desktop.rpc("action", name="select-output")
            return showing("Details", "button")
        wait_accessible(retry)
        wait_accessible(lambda: not showing("Details", "button"), timeout=9)
        assert_stays(lambda: not showing("Details", "button"), 1,
                     message="Duplicate terminal errors replayed after the toast expired")

    with scenario("one connection banner across split panes, tab switching, input suppression and recovery"):
        process.terminate()
        assert process.wait(timeout=8) == 0
        wait_accessible(lambda: len(showing("Reconnecting to build-server", "label")) == 1)
        desktop.rpc("focus", connection="build-server", pane=remote["pane"])
        for _ in range(8):
            native.key(Gdk.KEY_a, True)
            native.key(Gdk.KEY_a, False)
        assert not showing("Details", "button")
        # Copy used to also enqueue an unthrottled window-level fallback.
        for _ in range(8):
            desktop.rpc("action", name="copy")
        assert not showing("Select terminal text to copy it")
        screenshot("reconnecting")
        desktop.rpc("focus", connection="local", pane=local["pane"])
        wait_accessible(lambda: not showing("Reconnecting to build-server", "label"))
        desktop.rpc("focus", connection="build-server", pane=remote["pane"])
        wait_accessible(lambda: showing("Reconnecting to build-server", "label"))
        click("Manage Connections")
        wait_accessible(lambda: showing("Add Connection"))
        native.key(Gdk.KEY_Escape, True)
        native.key(Gdk.KEY_Escape, False)
        wait_accessible(lambda: not showing("Add Connection"))
        desktop.rpc("action", name="find")

        def find_above_banner():
            fields = showing("Find in terminal")
            banners = showing("Reconnecting to build-server", "label")
            if not fields or len(banners) != 1:
                return False
            field = Atspi.Component.get_extents(fields[0].get_component_iface(), Atspi.CoordType.WINDOW)
            banner = Atspi.Component.get_extents(banners[0].get_component_iface(), Atspi.CoordType.WINDOW)
            # Check both layout and visibility after the reveal animations.
            return field.height > 0 and field.y + field.height <= banner.y

        wait_accessible(find_above_banner)
        screenshot("find-banner")
        desktop.rpc("action", name="find")
        wait_accessible(lambda: not showing("Find in terminal"))
        assert showing("Reconnecting to build-server", "label"), "Closing Find hid the connection banner"
        desktop.rpc("action", name="find")
        wait_accessible(find_above_banner)
        process = server.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: server.rpc("status")["pid"] == process.pid)
        wait_for(lambda: any(t["connection"] == "build-server" and t["connected"]
                            for t in desktop.rpc("list")["tabs"]), timeout=20)
        wait_accessible(lambda: not showing("Reconnecting to build-server", "label"))
        assert showing("Find in terminal"), "Connection recovery closed Find"
        assert not showing("Details", "button")
        # Let the banner and resize overlays finish their native transitions.
        time.sleep(1.2)
        screenshot("recovered")
    desktop.rpc("detach")
    assert app.wait(timeout=8) == 0
print("Passed native terminal feedback checks.")
