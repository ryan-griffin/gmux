#!/usr/bin/env python3
"""Check native window creation, shared layouts, local navigation, and detach."""
import os
from pathlib import Path
import sys
from unittest.mock import patch

import gi
gi.require_version("GioUnix", "2.0")
from gi.repository import Gio, GioUnix

from cargo_test_support import cargo_binary
from test_support import AppFixture, scenario, stop_process, wait_for
from wayland_test_support import ensure_wayland_session

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)

with AppFixture(dict(restore=False, desktop_notifications=False), prefix="gmux-multi-window-", env={"GTK_A11Y": "none"}) as fixture, (artifacts / "multi-window-smoke.log").open("w") as log:
    app = fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
    first = fixture.rpc("windows")["windows"][0]["id"]
    created = fixture.rpc("new", window=first, name="Original", directory=str(root))
    pane, tab = created["pane"], created["tab"]

    def state(window):
        return fixture.rpc("list", window=window)

    with scenario("CLI and window action create additional windows over existing sessions"):
        launcher = fixture.start(cargo_binary("gmux"), "--new-window", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 2 else None))
        second = next(w["id"] for w in windows if w["id"] != first)
        wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 2)
        assert [t["id"] for t in state(first)["tabs"]] == [t["id"] for t in state(second)["tabs"]] == [tab]
        fixture.rpc("action", window=second, name="new-window")
        windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 3 else None))
        third = next(w["id"] for w in windows if w["id"] not in (first, second))
        fixture.rpc("detach", window=third)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 2)
        assert app.poll() is None

    with scenario("desktop New Window action opens another window over existing sessions"):
        daemon_pid = fixture.rpc("daemon-info")["pid"]
        # Use the built executable and private fixture state for the same GIO
        # action launch GNOME uses from its icon menu and on middle-click.
        launch_env = dict(fixture.env, PATH=str(cargo_binary("gmux").parent) + os.pathsep + fixture.env["PATH"])
        with patch.dict(os.environ, launch_env):
            desktop = GioUnix.DesktopAppInfo.new_from_filename(str(root / "data/dev.gmux.Gmux.desktop"))
            assert desktop is not None
            assert "new-window" in desktop.list_actions()
            assert desktop.get_action_name("new-window") == "New Window"
            desktop.launch_action("new-window", Gio.AppLaunchContext())
            windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 3 else None))
        third = next(w["id"] for w in windows if w["id"] not in (first, second))
        wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 3)
        assert fixture.rpc("daemon-info")["pid"] == daemon_pid
        for window in (first, second, third):
            view = state(window)
            assert [t["id"] for t in view["tabs"]] == [tab]
            assert [p["id"] for p in view["tabs"][0]["panes"]] == [pane]
        fixture.rpc("detach", window=third)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 2)
        wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 2)
        assert app.poll() is None

    with scenario("shared tabs and workspaces preserve each window's UI choices"):
        workspace = fixture.rpc("workspace-new", window=first, name="Shared workspace", directory=str(root))["workspace"]
        other = fixture.rpc("new", window=first, name="Project", workspace=workspace)
        wait_for(lambda: len(state(second)["tabs"]) == 2 and len(state(second)["workspaces"]) == 1)
        assert state(first)["active_tab"] == other["tab"] and state(second)["active_tab"] == tab
        fixture.rpc("filter", window=first, workspace=workspace)
        fixture.rpc("filter", window=second, value="unassigned")
        fixture.rpc("action", window=second, name="sidebar")
        fixture.rpc("search-tabs", window=second, query="Original")
        assert state(first)["sidebar_visible"] and not state(second)["sidebar_visible"]
        assert state(first)["tab_search"] == "" and state(second)["tab_search"] == "Original"
        fixture.rpc("rename", window=first, tab=tab, name="Original renamed")
        wait_for(lambda: next(t for t in state(second)["tabs"] if t["id"] == tab)["name"] == "Original renamed")
        assert state(first)["filter"] == {"workspace": workspace} and state(second)["filter"] == "unassigned"
        assert state(first)["active_tab"] == other["tab"] and state(second)["active_tab"] == tab
        fixture.rpc("search-tabs", window=second, query="")
        fixture.rpc("filter", window=first, value="all")
        fixture.rpc("focus", window=first, pane=pane)

    with scenario("split topology syncs while the selected pane and zoom stay local"):
        split = fixture.rpc("split", window=first, pane=pane)
        wait_for(lambda: len(next(t for t in state(second)["tabs"] if t["id"] == tab)["panes"]) == 2)
        assert next(t for t in state(first)["tabs"] if t["id"] == tab)["active_pane"] == split["pane"]
        assert next(t for t in state(second)["tabs"] if t["id"] == tab)["active_pane"] == pane
        wait_for(lambda: fixture.rpc("renderer-info", window=first, pane=split["pane"])["viewport"]["focused"],
                 message="The newly created split did not receive keyboard focus")
        before = fixture.rpc("renderer-info", window=second, pane=pane)["viewport"]["cell_height"]
        fixture.rpc("action", window=first, name="zoom-in")
        assert fixture.rpc("renderer-info", window=second, pane=pane)["viewport"]["cell_height"] == before
        fixture.rpc("close", window=first, pane=split["pane"])
        wait_for(lambda: next(t for t in state(second)["tabs"] if t["id"] == tab)["layout"] == {"Pane": pane})
        fixture.rpc("workspace-delete", window=first, workspace=workspace)
        wait_for(lambda: not state(second)["workspaces"])
        assert next(t for t in state(second)["tabs"] if t["id"] == other["tab"])["workspace"] is None

    with scenario("batched daemon changes keep tab order consistent in every window"):
        batch = [fixture.rpc("_command", command=dict(method="new", name=name, directory=str(root), workspace=None))["result"]
                 for name in ("Batch one", "Batch two")]
        order = [t["id"] for t in fixture.rpc("_state")["session"]["tabs"]]
        for window in (first, second):
            wait_for(lambda: [t["id"] for t in state(window)["tabs"]] == order)
        for created in batch:
            fixture.rpc("_command", command=dict(method="close-tab", tab=created["tab"]))
        for window in (first, second):
            wait_for(lambda: len(state(window)["tabs"]) == 2)

    with scenario("closing one window leaves the other window and all shells alive"):
        daemon_pid = fixture.rpc("daemon-info")["pid"]
        fixture.rpc("focus", window=first, pane=pane)
        wait_for(lambda: fixture.rpc("renderer-info", window=first, pane=pane)["window_active"])
        for window, dimensions in ((first, [1000, 740]), (second, [760, 540])):
            fixture.rpc("resize", window=window, width=dimensions[0], height=dimensions[1])
            wait_for(lambda: fixture.rpc("renderer-info", window=window, pane=pane)["window_size"] == dimensions)

        def requested_grid(window):
            view = fixture.rpc("renderer-info", window=window, pane=pane)["viewport"]
            return [int(view["width"] // view["cell_width"]), int(view["height"] // view["cell_height"])]

        owner_grid, survivor_grid = requested_grid(first), requested_grid(second)
        assert owner_grid != survivor_grid, "Window fixtures must request different grids"
        wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == owner_grid)
        # Cover the app before closing the owner so the survivor cannot repair
        # its size through a new focus event or a resize request.
        cover = fixture.start(sys.executable, "-c", """
import gi
gi.require_version("Gtk", "4.0")
from gi.repository import Gtk
app = Gtk.Application()
def activate(app):
    window = Gtk.ApplicationWindow(application=app)
    window.fullscreen()
    window.present()
app.connect("activate", activate)
app.run()
""", stdout=log, stderr=log)
        try:
            wait_for(lambda: not any(w["active"] for w in fixture.rpc("windows")["windows"]))
            assert cover.poll() is None, "The focus cover exited"
            fixture.rpc("detach", window=first)
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 1)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == survivor_grid,
                     message="Closing the size owner did not apply the unfocused survivor's cached grid")
        finally:
            stop_process(cover)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 1)
        wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 1)
        assert app.poll() is None and fixture.rpc("daemon-info")["pid"] == daemon_pid
        fixture.rpc("send", window=second, pane=pane, text="printf 'SECOND_WINDOW_INPUT\\n'", paste=True, enter=True)
        wait_for(lambda: "SECOND_WINDOW_INPUT" in fixture.rpc("read", window=second, pane=pane)["text"])
        fixture.rpc("detach", window=second)
        assert app.wait(timeout=8) == 0
        wait_for(lambda: not fixture.rpc("daemon-info")["attached"])
        assert fixture.rpc("daemon-info")["pid"] == daemon_pid
        assert len(fixture.rpc("list")["tabs"]) == 2

print("PASS: native windows share terminals and layout while navigation and UI stay independent")
