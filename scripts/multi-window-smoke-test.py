#!/usr/bin/env python3
"""Check native window creation, shared layouts, local navigation, and detach."""
import os
import fcntl
import json
import time
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

with AppFixture(dict(desktop_notifications=False), prefix="gmux-multi-window-", env={"GTK_A11Y": "none"}) as fixture, (artifacts / "multi-window-smoke.log").open("w") as log:
    app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
    first = fixture.rpc("windows")["windows"][0]["id"]
    created = fixture.rpc("new", window=first, name="Original", directory=str(root))
    pane, tab = created["pane"], created["tab"]

    def state(window):
        return fixture.rpc("list", window=window)

    def resources():
        lines = Path(f"/proc/{app.pid}/status").read_text().splitlines()
        result = {key.rstrip(":"): int(next(line.split()[1] for line in lines if line.startswith(key)))
                  for key in ("VmRSS:", "Threads:")}
        result["fds"] = len(list(Path(f"/proc/{app.pid}/fd").iterdir()))
        return result

    with scenario("CLI and window action create additional windows over existing sessions"):
        launcher = fixture.start(cargo_binary("gmux-gtk"), "--new-window", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 2 else None))
        second = next(w["id"] for w in windows if w["id"] != first)
        wait_for(lambda: fixture.rpc("status")["local_clients"] == 2)
        assert [t["id"] for t in state(first)["tabs"]] == [t["id"] for t in state(second)["tabs"]] == [tab]
        fixture.rpc("action", window=second, name="new-window")
        windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 3 else None))
        third = next(w["id"] for w in windows if w["id"] not in (first, second))
        fixture.rpc("detach", window=third)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 2)
        assert app.poll() is None

    with scenario("desktop New Window action opens another window over existing sessions"):
        daemon_pid = fixture.rpc("status")["pid"]
        # Use the built executable and private fixture state for the same GIO
        # action launch GNOME uses from its icon menu and on middle-click.
        launch_env = dict(fixture.env, PATH=str(cargo_binary("gmux-gtk").parent) + os.pathsep + fixture.env["PATH"])
        with patch.dict(os.environ, launch_env):
            desktop = GioUnix.DesktopAppInfo.new_from_filename(str(root / "data/dev.gmux.Gmux.desktop"))
            assert desktop is not None
            assert "new-window" in desktop.list_actions()
            assert desktop.get_action_name("new-window") == "New Window"
            desktop.launch_action("new-window", Gio.AppLaunchContext())
            windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 3 else None))
        third = next(w["id"] for w in windows if w["id"] not in (first, second))
        wait_for(lambda: fixture.rpc("status")["local_clients"] == 3)
        assert fixture.rpc("status")["pid"] == daemon_pid
        for window in (first, second, third):
            view = state(window)
            assert [t["id"] for t in view["tabs"]] == [tab]
            assert [p["id"] for p in view["tabs"][0]["panes"]] == [pane]
        fixture.rpc("detach", window=third)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 2)
        wait_for(lambda: fixture.rpc("status")["local_clients"] == 2)
        assert app.poll() is None

    with scenario("shared tabs and workspaces preserve each window's UI choices"):
        workspace = fixture.rpc("workspace-add", window=first, name="Shared workspace", directory=str(root))["workspace"]
        other = fixture.rpc("new", window=first, name="Project", workspace=workspace)
        wait_for(lambda: len(state(second)["tabs"]) == 2 and len(state(second)["workspaces"]) == 1)
        assert state(first)["active_tab"] == other["tab"] and state(second)["active_tab"] == tab
        fixture.rpc("filter", window=first, workspace=workspace)
        fixture.rpc("filter", window=second, value="all")
        fixture.rpc("action", window=second, name="sidebar")
        fixture.rpc("search-tabs", window=second, query="Original")
        assert state(first)["sidebar_visible"] and not state(second)["sidebar_visible"]
        assert state(first)["tab_search"] == "" and state(second)["tab_search"] == "Original"
        fixture.rpc("rename", window=first, tab=tab, name="Original renamed")
        wait_for(lambda: next(t for t in state(second)["tabs"] if t["id"] == tab)["name"] == "Original renamed")
        assert state(first)["filter"] == {"workspace": workspace}
        assert state(second)["filter"] == "all"
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

    with scenario("deleting a selected workspace returns every affected window to All Tabs"):
        kept = fixture.rpc("workspace-add", window=first, name="Remaining workspace", directory=str(root))["workspace"]
        fixture.rpc("move", window=first, tab=tab, workspace=kept)
        for window in (first, second):
            fixture.rpc("filter", window=window, workspace=workspace)
        fixture.rpc("workspace-remove", window=first, workspace=workspace)
        for window in (first, second):
            wait_for(lambda: state(window)["filter"] == "all")
            view = state(window)
            assert len(view["tabs"]) == 2 and all(t["visible"] for t in view["tabs"])
            assert view["active_tab"] == other["tab"]
            assert [w["id"] for w in view["workspaces"]] == [kept]
            assert next(t for t in view["tabs"] if t["id"] == tab)["workspace"] == kept
            assert next(t for t in view["tabs"] if t["id"] == other["tab"])["workspace"] is None
        assert "error" in fixture.rpc("filter", window=first, value="unassigned", check=False)
        assert state(first)["filter"] == "all"

    with scenario("clearing an inactive custom name uses each window's active pane title"):
        split = fixture.rpc("split", window=first, pane=other["pane"])
        titles = {other["pane"]: "Background first", split["pane"]: "Background second"}
        for identifier, title in titles.items():
            fixture.rpc("send", pane=identifier, text=f"printf '\\033]2;{title}\\007'", paste=True, enter=True)
            wait_for(lambda: fixture.rpc("_metadata", pane=identifier)["meta"]["title"] == title)
        for window, identifier in ((first, other["pane"]), (second, split["pane"])):
            fixture.rpc("focus", window=window, pane=identifier)
            fixture.rpc("focus", window=window, pane=pane)
        fixture.rpc("rename", window=first, tab=other["tab"], name="Background custom")
        for window in (first, second):
            wait_for(lambda: next(t for t in state(window)["tabs"] if t["id"] == other["tab"])["name"] == "Background custom")
        fixture.rpc("reset-name", window=first, tab=other["tab"])
        for window, identifier in ((first, other["pane"]), (second, split["pane"])):
            wait_for(lambda: next(t for t in state(window)["tabs"] if t["id"] == other["tab"])["name"] == titles[identifier])
            assert state(window)["active_tab"] == tab, "Clearing a background name changed selection"
        fixture.rpc("close", window=first, pane=split["pane"])
        fixture.rpc("rename", window=first, tab=other["tab"], name="Project")

    with scenario("client organization saves without changing daemon execution"):
        execution = fixture.config / "execution.json"
        before = execution.read_bytes()
        fixture.rpc("workspace-edit", workspace=kept, name="Renamed workspace", directories={})
        fixture.rpc("move", tab=tab, workspace=None)
        assert execution.read_bytes() == before
        client_state = json.loads((fixture.config / "clients/gtk/state.json").read_text())
        assert client_state["catalog"]["workspaces"][0]["name"] == "Renamed workspace"

    with scenario("batched daemon changes keep tab order consistent in every window"):
        batch = [fixture.rpc("_command", command=dict(method="new", name=name, directory=str(root)))["result"]
                 for name in ("Batch one", "Batch two")]
        order = [t["id"] for t in fixture.rpc("_state")["session"]["tabs"]]
        for window in (first, second):
            wait_for(lambda: [t["id"] for t in state(window)["tabs"]] == order)
        for created in batch:
            fixture.rpc("_command", command=dict(method="close-tab", tab=created["tab"]))
        for window in (first, second):
            wait_for(lambda: len(state(window)["tabs"]) == 2)

    with scenario("repeated tab updates keep retained menu memory bounded across windows"):
        workspaces = [fixture.rpc("workspace-add", name=f"Refresh workspace {index}")["workspace"]
                      for index in range(5)]
        tabs = [fixture.rpc("new", window=first, name=f"Refresh tab {index}")["tab"] for index in range(8)]
        wait_for(lambda: len(state(second)["tabs"]) == 10 and len(state(second)["workspaces"]) == 6)
        for index in range(16):
            fixture.rpc("rename", window=first, tab=tab, name=f"Warmup {index}")
        wait_for(lambda: next(t for t in state(second)["tabs"] if t["id"] == tab)["name"] == "Warmup 15")
        before = resources()
        for index in range(96):
            fixture.rpc("rename", window=first, tab=tab, name=f"Updated tab {index}")
        wait_for(lambda: next(t for t in state(second)["tabs"] if t["id"] == tab)["name"] == "Updated tab 95")
        after = resources()
        assert after["VmRSS"] <= before["VmRSS"] + 8192, (before, after)
        (artifacts / "menu-update-resources.json").write_text(json.dumps(dict(before=before, after=after), indent=2) + "\n")
        for identifier in tabs:
            fixture.rpc("close-tab", window=first, tab=identifier)
        for identifier in workspaces:
            fixture.rpc("workspace-remove", workspace=identifier)
        for window in (first, second):
            wait_for(lambda: len(state(window)["tabs"]) == 2)

    with scenario("closing a window with a blocked state save keeps other windows responsive"):
        closing = fixture.rpc("new-window")["window"]
        wait_for(lambda: fixture.rpc("status")["local_clients"] == 3)
        lock_path = fixture.config / "clients/gtk/.state.json.lock"
        with lock_path.open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            fixture.rpc("detach", window=closing)
            wait_for(lambda: len(fixture.rpc("windows", timeout=1)["windows"]) == 2)
            before = time.monotonic()
            sidebar = state(second)["sidebar_visible"]
            fixture.rpc("action", window=second, name="sidebar", timeout=1)
            assert fixture.rpc("list", window=second, timeout=1)["sidebar_visible"] != sidebar
            (artifacts / "window-close-blocked-save.json").write_text(json.dumps(dict(
                other_window_response_ms=(time.monotonic() - before) * 1000), indent=2) + "\n")
        # Let the queued save finish, including the work held by the closed window.
        workspace = fixture.rpc("workspace-add", name="After blocked save")["workspace"]
        fixture.rpc("workspace-remove", workspace=workspace)

    with scenario("closed windows release client workers and subscriptions"):
        def cycle():
            created = fixture.rpc("new-window")["window"]
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 3)
            fixture.rpc("detach", window=created)
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 2)
        for _ in range(2): cycle()
        time.sleep(0.1)
        before = resources()
        for _ in range(8): cycle()
        time.sleep(0.1)
        after_warmup = resources()
        for _ in range(8): cycle()
        time.sleep(0.1)
        after = resources()
        assert after["fds"] <= before["fds"] + 2, (before,after)
        assert after["Threads"] <= before["Threads"] + 2, (before,after)
        (artifacts / "client-window-resources.json").write_text(json.dumps(dict(before=before, after_eight_cycles=after_warmup, after_sixteen_cycles=after), indent=2) + "\n")

    with scenario("closing one window leaves the other window and all shells alive"):
        daemon_pid = fixture.rpc("status")["pid"]
        fixture.rpc("focus", window=second, pane=pane)
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
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 1)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == survivor_grid,
                     message="Closing the size owner did not apply the unfocused survivor's cached grid")
        finally:
            stop_process(cover)
        wait_for(lambda: len(fixture.rpc("windows")["windows"]) == 1)
        wait_for(lambda: fixture.rpc("status")["local_clients"] == 1)
        assert app.poll() is None and fixture.rpc("status")["pid"] == daemon_pid
        fixture.rpc("send", window=second, pane=pane, text="printf 'SECOND_WINDOW_INPUT\\n'", paste=True, enter=True)
        wait_for(lambda: "SECOND_WINDOW_INPUT" in fixture.rpc("read", window=second, pane=pane)["text"])
        fixture.rpc("detach", window=second)
        assert app.wait(timeout=8) == 0
        wait_for(lambda: not fixture.rpc("status")["local_clients"])
        assert fixture.rpc("status")["pid"] == daemon_pid
        assert len(fixture.rpc("list")["tabs"]) == 2

    with scenario("an explicit startup directory selects a new tab in the saved workspace"):
        # Save a workspace filter and an existing selection before restarting.
        app = fixture.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
        fixture.rpc("move", tab=tab, workspace=kept)
        fixture.rpc("filter", workspace=kept)
        fixture.rpc("detach")
        assert app.wait(timeout=8) == 0
        previous = {t["id"] for t in fixture.rpc("list")["tabs"]}
        started = time.monotonic()
        app = fixture.start_app(cargo_binary("gmux-gtk"), "--directory", str(fixture.path), stdout=log, stderr=log)
        first = fixture.rpc("windows")["windows"][0]["id"]
        created = wait_for(lambda: next((t for t in state(first)["tabs"] if t["id"] not in previous), None))
        wait_for(lambda: state(first)["active_tab"] == created["id"])
        view = state(first)
        created = next(t for t in view["tabs"] if t["id"] == created["id"])
        assert view["filter"] == {"workspace": kept}
        assert created["workspace"] == kept and created["visible"]
        assert created["panes"][0]["directory"] == str(fixture.path), "The workspace default replaced the explicit directory"
        (artifacts / "directory-startup.json").write_text(json.dumps(dict(elapsed_ms=(time.monotonic() - started) * 1000), indent=2) + "\n")

        relative = fixture.path / "relative-launch"
        relative.mkdir()
        control = Path(fixture.env["GMUX_SOCKET"] + ".ui")
        pending = control.with_suffix(".ui-starting")
        control.rename(pending)
        try:
            launcher = fixture.start(cargo_binary("gmux-gtk"), "--new-window", "--directory", relative.name,
                                     cwd=fixture.path, stdout=log, stderr=log)
            assert launcher.wait(timeout=8) == 0
        finally:
            pending.rename(control)
        windows = wait_for(lambda: (windows if len(windows := fixture.rpc("windows")["windows"]) == 2 else None))
        second = next(w["id"] for w in windows if w["id"] != first)
        opened = wait_for(lambda: next((t for t in state(second)["tabs"] if t["id"] not in previous | {created["id"]}), None))
        wait_for(lambda: state(second)["active_tab"] == opened["id"])
        opened = next(t for t in state(second)["tabs"] if t["id"] == opened["id"])
        assert opened["workspace"] == kept and opened["visible"]
        assert opened["panes"][0]["directory"] == str(relative)
        assert state(first)["active_tab"] == created["id"], "Opening a directory in a new window changed the first window's selection"
        for window in (first, second):
            fixture.rpc("detach", window=window)
        assert app.wait(timeout=8) == 0

print("PASS: native windows share terminals and layout while navigation and UI stay independent")
