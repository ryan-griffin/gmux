#!/usr/bin/env python3
"""Check GTK status labels, attribution, hidden tabs, reconnect and shared windows."""
import base64
import shlex
from pathlib import Path
import subprocess
import time

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible, wait_for
from wayland_test_support import ensure_wayland_session

ensure_wayland_session()
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


def osc(state, **fields):
    if "msg" in fields:
        fields["msg"] = base64.b64encode(fields["msg"].encode()).decode()
    return "\x1b]7501;" + ":".join(f"{k}={v}" for k, v in dict(state=state, **fields).items()) + "\x07"


with (artifacts / "program-status-ui.log").open("w") as log, AccessibilityFixture(), \
        AppFixture(dict(show_pane_titles=False), env=dict(GTK_A11Y="atspi")) as fixture:
    with scenario("sidebar tooltips and unread badges preserve title allocation at large fonts and in RTL"):
        result = subprocess.run([str(cargo_binary("program-status-check", example=True))],
            env=dict(fixture.env, GTK_A11Y="test"), capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        (artifacts / "program-status-ui-sizes.json").write_text(result.stdout)
    app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
    created = fixture.rpc("new", name="Status source", directory=str(fixture.path))
    pane = created["pane"]
    probe = ProgressProbe(fixture, pane)
    second = fixture.rpc("split", pane=pane)["pane"]
    other = ProgressProbe(fixture, second)

    def state(window=None):
        return fixture.rpc("list", **(dict(window=window) if window else {}))

    def summary(window=None):
        return next(t for t in state(window)["tabs"] if t["id"] == created["tab"])["program_status"]

    def showing(text):
        return [node for node in descendants(Atspi.get_desktop(0)) if node.get_process_id() == app.pid
            and text in node.get_name() and node.get_state_set().contains(Atspi.StateType.SHOWING)]

    with scenario("blocked child wins idle root and working pane with plain attributed accessible text"):
        geometry = [None, time.monotonic()]
        def settled_viewport():
            viewport = fixture.rpc("renderer-info", pane=second)["viewport"]
            current = tuple(viewport[key] for key in ("width", "height", "cols", "rows"))
            if current != geometry[0]:
                geometry[:] = [current, time.monotonic()]
            # PTY metadata can be ready before GTK's initial split resize reply.
            matches = (viewport["width"] > 0 and viewport["height"] > 0
                       and viewport["cols"] == int(viewport["width"] / viewport["cell_width"])
                       and viewport["rows"] == int(viewport["height"] / viewport["cell_height"]))
            return viewport if matches and time.monotonic() - geometry[1] >= .1 else None
        original = wait_for(settled_viewport)
        probe.write(osc("working", app="build"))
        other.write(osc("idle", app="deploy") + osc("blocked", id="child", kind="permission", msg="<b>Approve?</b>\u202e\u200b"))
        status = wait_for(lambda: (s := summary()) and s["pane"] == second and s["status"]["record"]["state"] == "blocked" and s)
        text = "Approval needed"
        wait_accessible(lambda: len([n for n in showing(text) if n.get_role_name() == "image"]) == 1)
        assert not [n for n in showing(text) if n.get_role_name() == "label"], "Status text must stay out of the terminal viewport"
        assert any("<b>Approve?</b>" in node.get_name() for node in showing(text))
        assert all("\u202e" not in node.get_name() and "\u200b" not in node.get_name() for node in showing(text))
        now = fixture.rpc("renderer-info", pane=second)["viewport"]
        assert all(now[key] == original[key] for key in ("width", "height", "cols", "rows")), (original, now)
        fixture.rpc("screenshot", path=str(artifacts / "program-status-pane.png"))
        fixture.rpc("new", name="Foreground", directory=str(fixture.path))
        wait_for(lambda: summary() == status)
        # The same single sidebar icon represents visible and hidden panes.
        wait_accessible(lambda: showing(text))
        fixture.rpc("screenshot", path=str(artifacts / "program-status-hidden.png"))

    with scenario("one sidebar slot combines unread alerts and status with independent acknowledgement"):
        fixture.rpc("notify", pane=second, message="Approval required")
        wait_accessible(lambda: showing(f"{text} · deploy · child · <b>Approve?</b> · 2 records · Unread terminal alerts"))
        fixture.rpc("screenshot", path=str(artifacts / "program-status-unread.png"))
        fixture.rpc("focus", pane=second)
        wait_for(lambda: not fixture.rpc("metadata", pane=second)["meta"]["unread_sequence"])
        wait_accessible(lambda: showing(text) and not showing("Unread terminal alerts"))
        assert summary()["status"]["record"]["state"] == "blocked"

    with scenario("both protocols drive one bar and icon, with rich state preceding legacy progress"):
        def bars(value_text=None):
            return [node for node in showing("Progress") if node.get_role_name() == "progress bar"
                and (value_text is None or Atspi.Value.get_text(node.get_value_iface()) == value_text)]
        def bar_value(text, percentage=None):
            def ready():
                nodes = bars(text)
                if len(nodes) != 1:
                    return False
                value = nodes[0].get_value_iface()
                return (Atspi.Value.get_text(value) == text
                    and (percentage is None or Atspi.Value.get_current_value(value) == percentage))
            wait_accessible(ready)
        # A conflicting legacy error is retained, but does not replace the rich
        # record, percentage, or sidebar source until that record is cleared.
        other.write(osc("clear") + osc("working", app="deploy", progress=38) + "\x1b]9;4;2;91\x07")
        bar_value("In progress, 38%", 38)
        wait_accessible(lambda: any(n.get_role_name() == "image" for n in showing("Working, 38%")))
        # Changes arriving separately on the legacy channel stay masked too.
        other.report(4, 2)
        other.expect("paused", 2)
        bar_value("In progress, 38%", 38)
        assert not showing("Paused, 2%")
        other.report(2, 91)
        other.expect("error", 91)
        bar_value("In progress, 38%", 38)
        other.write(osc("blocked", kind="permission", msg="Approve?", progress=25))
        bar_value("Paused, 25%", 25)
        other.write(osc("blocked", kind="permission", msg="Approve?"))
        bar_value("Paused, percentage unknown")
        other.write(osc("error", msg="Deployment failed"))
        bar_value("Error, percentage unknown")
        other.write(osc("done"))
        wait_accessible(lambda: not bars("Error, percentage unknown") and len(bars()) == 1)
        other.write(osc("clear"))
        bar_value("Error, 91%", 91)
        wait_accessible(lambda: any(n.get_role_name() == "image" for n in showing("Error, 91%")))
        # Idle in another pane must not hide the legacy error.
        probe.write(osc("idle"))
        wait_accessible(lambda: any(n.get_role_name() == "image" for n in showing("Error, 91%")))
        fixture.rpc("new", name="Foreground again", directory=str(fixture.path))
        wait_accessible(lambda: not bars() and any(n.get_role_name() == "image" for n in showing("Error, 91%")))
        other.report(0)
        other.expect("inactive", 0)
        wait_accessible(lambda: not showing("Error, 91%"))
        probe.write(osc("working", app="build"))

    with scenario("reconnecting GTK and independent windows share retained state without notifications"):
        daemon_pid = fixture.rpc("status")["pid"]
        fixture.rpc("detach")
        assert app.wait(timeout=8) == 0
        other.write(osc("clear") + osc("done", msg="Finished while detached"))
        app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
        assert fixture.rpc("status")["pid"] == daemon_pid
        wait_for(lambda: (s := summary()) and s["status"]["record"]["state"] == "done")
        # Select the reporting pane in both windows. Focus must not acknowledge it.
        first = fixture.rpc("windows")["windows"][0]["id"]
        fixture.rpc("focus", window=first, pane=second)
        fixture.rpc("new-window")
        windows = wait_for(lambda: (ws if len(ws := fixture.rpc("windows")["windows"]) == 2 else None))
        last = next(w["id"] for w in windows if w["id"] != first)
        wait_for(lambda: any(p["id"] == second for t in state(last)["tabs"] for p in t["panes"]))
        fixture.rpc("focus", window=last, pane=second)
        wait_for(lambda: summary(first) == summary(last) and summary(first)["status"]["record"]["state"] == "done")
        client = Client(fixture, second)
        client.until(lambda h: h["meta"]["id"] == second and h["frame"])
        client.send("key", name="a", unshifted=97, mods=0, action="press", text="a")
        wait_for(lambda: summary(first)["pane"] == pane and summary(last)["pane"] == pane)
        client.close()
        wait_accessible(lambda: not showing("Finished"))
    with scenario("an exited GTK pane retains its result and closes after shared acknowledgement"):
        dead = fixture.rpc("new", window=first, name="Exited result", directory=str(fixture.path))["pane"]
        command = "exec python3 -c " + shlex.quote("import os; os.write(1, b'\\x1b]7501;state=done:msg=RG9uZQ==\\x07')")
        fixture.rpc("send", pane=dead, text=command, enter=True)
        wait_for(lambda: (m := fixture.rpc("metadata", pane=dead)["meta"])["exited"] and m["program_status"])
        wait_for(lambda: any(p["id"] == dead and p["exited"] and p["program_status"]
                            for t in state(first)["tabs"] for p in t["panes"]))
        wait_accessible(lambda: showing("Finished"))
        peer = Client(fixture, dead)
        peer.until(lambda h: h["meta"]["id"] == dead and h["meta"]["exited"])
        peer.send("key", name="Return", unshifted=13, mods=0, action="press", text="\r")
        wait_for(lambda: not any(p["id"] == dead for t in state(first)["tabs"] for p in t["panes"]))
        wait_for(lambda: not any(p["id"] == dead for t in state(last)["tabs"] for p in t["panes"]))
        peer.close()
    for window in (first, last):
        fixture.rpc("detach", window=window)
    assert app.wait(timeout=8) == 0
print("Passed native GTK program status checks.")
