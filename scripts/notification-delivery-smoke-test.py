#!/usr/bin/env python3
"""Check desktop delivery, click navigation, and unread indicators through splits."""
from contextlib import ExitStack
from pathlib import Path

from notification_server import NotificationServer
from test_support import AppFixture, assert_stays, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def tab(fixture, identifier):
    return next(t for t in fixture.rpc("list")["tabs"] if t["id"] == identifier)


def desktop_delivery(fixture, notifications, app):
    background = fixture.rpc("new", name="Desktop delivery", directory=str(root))
    foreground = fixture.rpc("new", name="Foreground", directory=str(root))
    pane = background["pane"]
    wait_for(lambda: "GMUX_DELIVERY>" in fixture.rpc("read", pane=pane)["text"])
    fixture.rpc("send", pane=pane, text="printf '\\033]777;notify;Test;GMUX_ALERT\\007'",
                paste=True, enter=True)
    alert = wait_for(lambda: next((n for n in notifications.received if n["body"] == "GMUX_ALERT"), None))
    assert alert["summary"] == tab(fixture, background["tab"])["name"]
    assert tab(fixture, background["tab"])["unread"]
    assert not tab(fixture, foreground["tab"])["unread"]
    notifications.click(alert)
    wait_for(lambda: fixture.rpc("list")["active_tab"] == background["tab"]
             and tab(fixture, background["tab"])["active_pane"] == pane)
    wait_for(lambda: not tab(fixture, background["tab"])["unread"])
    assert not notifications.closed, "Focus dismissed a desktop notification"
    fixture.rpc("focus", pane=foreground["pane"])
    # The delay intentionally splits the terminal control sequence across PTY reads.
    fixture.rpc("send", pane=pane, text="printf '\\033]9;GMUX_OSC9'; sleep 0.1; printf '\\007'",
                paste=True, enter=True)
    wait_for(lambda: any(n["body"] == "GMUX_OSC9" for n in notifications.received))
    for message in ("GMUX_DIRECT_ALERT", "GMUX_SECOND_ALERT"):
        fixture.rpc("notify", pane=pane, message=message)
        wait_for(lambda: any(n["body"] == message for n in notifications.received))
    assert all(n["replaces"] == 0 for n in notifications.received), "Alerts replaced each other"
    assert not notifications.closed, "Focus dismissed a desktop notification"


def split_unread(fixture, notifications, app):
    first = fixture.rpc("new", name="Split alerts")
    pane = first["pane"]
    right = fixture.rpc("split", pane=pane)["pane"]
    down = fixture.rpc("split", pane=right, down=True)["pane"]
    fixture.rpc("new", name="Foreground")
    for target, message in ((right, "RIGHT_ALERT"), (down, "DOWN_ALERT")):
        fixture.rpc("notify", pane=target, message=message)
        wait_for(lambda: any(n["body"] == message for n in notifications.received))
    fixture.rpc("focus", pane=pane)
    marked = tab(fixture, first["tab"])
    assert marked["unread"], "Focusing one pane discarded sibling alerts"
    assert not next(p for p in marked["panes"] if p["id"] == pane)["unread"]
    fixture.rpc("close", pane=down)
    assert tab(fixture, first["tab"])["unread"], "Closing one unread pane discarded its sibling's alert"
    fixture.rpc("focus", pane=right)
    wait_for(lambda: not tab(fixture, first["tab"])["unread"])
    assert "error" in fixture.rpc("notify", pane=down, message="Closed pane", check=False)
    assert not tab(fixture, first["tab"])["unread"]
    fixture.rpc("notify", pane=pane, message="CLOSE_ALERT")
    wait_for(lambda: tab(fixture, first["tab"])["unread"])
    fixture.rpc("close", pane=pane)
    assert not tab(fixture, first["tab"])["unread"], "Closing the last unread pane left its tab unread"


def disabled_delivery(fixture, notifications, app):
    disabled = fixture.rpc("new", name="Disabled notifications")
    foreground = fixture.rpc("new", name="Foreground")
    fixture.rpc("notify", pane=disabled["pane"], message="BEFORE_DISABLE")
    wait_for(lambda: any(n["body"] == "BEFORE_DISABLE" for n in notifications.received))
    fixture.rpc("focus", pane=disabled["pane"])
    wait_for(lambda: not tab(fixture, disabled["tab"])["unread"])
    fixture.rpc("focus", pane=foreground["pane"])
    daemon_pid = fixture.rpc("daemon-info")["pid"]
    fixture.rpc("detach")
    assert app.wait(timeout=8) == 0
    fixture.set_settings(dict(desktop_notifications=False))
    fixture.start_app(root / "target/debug/gmux", stdout=log, stderr=log)
    assert fixture.rpc("daemon-info")["pid"] == daemon_pid
    assert not tab(fixture, disabled["tab"])["unread"]
    fixture.rpc("notify", pane=disabled["pane"], message="GMUX_DISABLED_ALERT")
    wait_for(lambda: tab(fixture, disabled["tab"])["unread"])
    assert_stays(lambda: not any(n["body"] == "GMUX_DISABLED_ALERT" for n in notifications.received), 0.3,
                 message="Disabled notifications reached the desktop")
    fixture.rpc("focus", pane=disabled["pane"])
    wait_for(lambda: not tab(fixture, disabled["tab"])["unread"])


cases = [("OSC and CLI desktop delivery and click navigation", desktop_delivery),
         ("split-pane unread indicators survive focus and pane closure", split_unread),
         ("disabling desktop delivery after reconnect preserves unread indicators", disabled_delivery)]
with (artifacts / "notification-delivery-smoke.log").open("w") as log:
    for name, test in cases:
        with scenario(name), ExitStack() as cleanup:
            notifications = NotificationServer()
            cleanup.callback(notifications.stop)
            fixture = cleanup.enter_context(AppFixture(dict(desktop_notifications=True),
                prefix="gmux-delivery-", env={"GTK_A11Y": "none"},
                shell_script="#!/bin/sh\nexport PS1='GMUX_DELIVERY> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n"))
            app = fixture.start_app(root / "target/debug/gmux", stdout=log, stderr=log)
            test(fixture, notifications, app)
