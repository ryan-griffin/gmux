#!/usr/bin/env python3
"""Check native progress roles/values, layout, hidden tabs, and GTK reconnects."""
import json
from pathlib import Path
import subprocess

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible, wait_for
from wayland_test_support import ensure_wayland_session

ensure_wayland_session()
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "progress-ui-smoke.log").open("w") as log, AccessibilityFixture(), \
        AppFixture(dict(show_pane_titles=True), env=dict(GTK_A11Y="atspi")) as fixture:
    with scenario("native pane bars preserve layout at normal and enlarged interface fonts"):
        result = subprocess.run([str(cargo_binary("progress-check", example=True))],
            env=dict(fixture.env, GTK_A11Y="test"), capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        (artifacts / "progress-ui-sizes.json").write_text(result.stdout)
        sizes = json.loads(result.stdout)
        assert len(sizes) == 4, sizes
        assert {size["direction"] for size in sizes} == {"Ltr", "Rtl"}, sizes
        assert all(size["pane_bar"]["widget_count"] == 3
                   and size["pane_bar"]["style"] == "Adwaita osd" for size in sizes), sizes

    app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
    created = fixture.rpc("new", name="Progress", directory=str(fixture.path))
    pane = created["pane"]
    probe = ProgressProbe(fixture, pane)

    def indicators(value_text=None):
        return [obj for obj in descendants(Atspi.get_desktop(0)) if obj.get_process_id() == app.pid
            and obj.get_role_name() == "progress bar" and obj.get_name() == "Progress"
            and obj.get_state_set().contains(Atspi.StateType.SHOWING)
            and (value_text is None or Atspi.Value.get_text(obj.get_value_iface()) == value_text)]

    def check_value(state, percentage):
        def values_ready():
            nodes = indicators(state)
            if len(nodes) != 1:
                return None
            node = nodes[0]
            value = node.get_value_iface()
            if value is None or Atspi.Value.get_text(value) != state:
                return None
            if percentage is not None and Atspi.Value.get_current_value(value) != percentage:
                return None
            return node
        node = wait_accessible(values_ready)
        value = node.get_value_iface()
        assert value is not None
        assert Atspi.Value.get_minimum_value(value) == 0
        assert Atspi.Value.get_maximum_value(value) == 100
        assert Atspi.Value.get_text(value) == state
        before = Atspi.Value.get_current_value(value)
        Atspi.Value.set_current_value(value, 99)
        assert Atspi.Value.get_current_value(value) == before
        if percentage is not None:
            assert before == percentage
        return node

    with scenario("pane bars and sidebar icons expose progress without terminal text events"):
        terminal = wait_accessible(lambda: next((obj for obj in descendants(Atspi.get_desktop(0))
            if obj.get_process_id() == app.pid and obj.get_role_name() == "terminal"), None))
        original = Atspi.Text.get_text(terminal.get_text_iface(), 0, -1)
        def geometry():
            viewport = fixture.rpc("renderer-info", pane=pane)["viewport"]
            return viewport["cols"], viewport["rows"]
        grid = geometry()
        events = []
        def text_changed(event, _):
            if event.source.get_process_id() == app.pid and event.source.get_role_name() == "terminal":
                events.append(event.type)
        listener = Atspi.EventListener.new(text_changed, None)
        assert listener.register("object:text-changed")
        try:
            for code, percent, name in [(1, 42, "In progress, 42%"), (4, None, "Paused, 42%"),
                    (2, 7, "Error, 7%"), (3, None, "In progress, percentage unknown")]:
                probe.report(code, percent)
                check_value(name, None if code == 3 else (7 if code == 2 else 42))
                assert Atspi.Text.get_text(terminal.get_text_iface(), 0, -1) == original
                assert geometry() == grid, "Progress must not resize the terminal grid"
            assert not events, events
            fixture.rpc("screenshot", path=str(artifacts / "progress-indeterminate.png"))
        finally:
            listener.deregister("object:text-changed")
        probe.report(0)
        probe.expect("inactive", 0)
        wait_accessible(lambda: not indicators())

    with scenario("hidden panes keep an attributed sidebar icon without an animated bar"):
        second = fixture.rpc("split", pane=pane)["pane"]
        other = ProgressProbe(fixture, second)
        probe.report(1, 91)
        other.report(2, 12)
        other.expect("error", 12)
        tab = wait_for(lambda: next((tab for tab in fixture.rpc("list")["tabs"]
            if tab["id"] == created["tab"] and tab["progress"] and tab["progress"]["pane"] == second), None))
        assert tab["progress"]["progress"] == dict(state="error", percentage=12), tab
        check_value("Error, 12%", 12)
        fixture.rpc("new", name="Foreground", directory=str(fixture.path))
        other.report(4, 63)
        other.expect("paused", 63)
        wait_accessible(lambda: not indicators())
        wait_accessible(lambda: any(obj.get_process_id() == app.pid
            and obj.get_role_name() == "image" and obj.get_name() == "Paused, 63%"
            and obj.get_state_set().contains(Atspi.StateType.SHOWING)
            for obj in descendants(Atspi.get_desktop(0))))
        hidden = next(tab for tab in fixture.rpc("list")["tabs"] if tab["id"] == created["tab"])
        assert hidden["progress"] == dict(pane=second, progress=dict(state="paused", percentage=63)), hidden

    with scenario("GTK detach/reconnect reads latest progress without restarting execution"):
        pid = fixture.rpc("status")["pid"]
        fixture.rpc("detach")
        assert app.wait(timeout=8) == 0
        other.report(2, 23)
        other.expect("error", 23)
        app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
        assert fixture.rpc("status")["pid"] == pid
        wait_for(lambda: next(tab for tab in fixture.rpc("list")["tabs"]
            if tab["id"] == created["tab"])["progress"] ==
            dict(pane=second, progress=dict(state="error", percentage=23)))
        wait_accessible(lambda: not indicators())
        fixture.rpc("focus", pane=second)
        check_value("Error, 23%", 23)
        fixture.rpc("screenshot", path=str(artifacts / "progress-error.png"))
        other.report(0)
        other.expect("inactive", 0)
        wait_accessible(lambda: not indicators("Error, 23%"))

    with scenario("pane progress stays visible below the top edge with pane titles disabled"):
        probe.report(0)
        probe.expect("inactive", 0)
        fixture.rpc("detach")
        assert app.wait(timeout=8) == 0
        fixture.set_settings(dict(show_pane_titles=False))
        other.report(1, 58)
        app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
        check_value("In progress, 58%", 58)
        fixture.rpc("screenshot", path=str(artifacts / "progress-no-titles.png"))
        other.report(0)
        wait_accessible(lambda: not indicators())
