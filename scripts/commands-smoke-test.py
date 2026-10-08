#!/usr/bin/env python3
"""Check command search, native rows, and screen-reader announcements.

Run with GDK_BACKEND=wayland dbus-run-session -- python3
scripts/commands-smoke-test.py. Uses a private accessibility bus and settings.
"""
from functools import partial
from test_support import AccessibilityFixture, AppFixture, descendants, wait_accessible
import os
from pathlib import Path
from cargo_test_support import cargo_binary

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(wait_accessible, timeout=10)


def focused(obj):
    # State can be cached before GTK transfers focus to the search entry.
    obj.clear_cache()
    return obj.get_state_set().contains(Atspi.StateType.FOCUSED)


appearance = os.environ.get("GMUX_REVIEW_APPEARANCE", "dark")
with (artifacts / f"commands-{appearance}.log").open("w") as log, AccessibilityFixture(), AppFixture(
    dict(restore=False, appearance=appearance, desktop_notifications=False),
    prefix="gmux-commands-", env={"GTK_A11Y": "atspi"},
) as fixture:
    rpc = fixture.rpc
    app = fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
    listener = None
    try:
        wait_for(lambda: rpc("renderer-info")["window_active"])
        desktop = Atspi.get_desktop(0)

        def find(name, role=None):
            return next((obj for obj in descendants(desktop)
                         if obj.get_process_id() == app.pid and obj.get_name() == name
                         and (role is None or obj.get_role_name() == role)), None)

        announcements = []

        def announced(event, _data):
            announcements.append((event.source.get_name(), event.any_data))

        listener = Atspi.EventListener.new(announced, None)
        assert listener.register("object:announcement")
        rpc("action", name="palette")
        entry = wait_for(lambda: find("Search commands", "entry")
                        or find("Search commands", "text"))
        wait_for(lambda: focused(entry))
        first = wait_for(lambda: find("New Tab", "list item"))
        command_list = first.get_parent()
        selection = command_list.get_selection_iface()
        assert selection
        controls = [relation for relation in entry.get_relation_set()
                    if relation.get_relation_type() == Atspi.RelationType.CONTROLLER_FOR]
        assert controls and controls[0].get_target(0) == command_list
        assert "Up and Down" in entry.get_description()

        def shortcut_labels(row):
            return {obj.get_name() for obj in descendants(row)
                    if obj.get_role_name() == "label" and obj.get_name() != row.get_name()
                    and obj.get_name() and obj.get_state_set().contains(Atspi.StateType.SHOWING)}

        close_pane = wait_for(lambda: find("Close Pane", "list item"))
        close_window = wait_for(lambda: find("Close Window", "list item"))
        pane_shortcut = wait_for(lambda: shortcut_labels(close_pane))
        wait_for(lambda: shortcut_labels(close_window) == pane_shortcut)
        assert not close_pane.get_state_set().contains(Atspi.StateType.SENSITIVE)
        assert close_window.get_state_set().contains(Atspi.StateType.SENSITIVE)

        def selected(name):
            return selection.get_n_selected_children() == 1 and (
                selection.get_selected_child(0).get_name() == name)

        def search(query, name):
            announcements.clear()
            assert entry.get_editable_text_iface().set_text_contents(query)
            wait_for(lambda: selected(name))
            wait_for(lambda: ("Search commands", name) in announcements)
            assert focused(entry)

        # Disabled matches remain visible but cannot be selected or run.
        announcements.clear()
        assert entry.get_editable_text_iface().set_text_contents("splti")
        wait_for(lambda: ("Search commands", "No Available Commands") in announcements)
        assert selection.get_n_selected_children() == 0
        split = wait_for(lambda: find("Split Right", "list item"))
        assert not split.get_state_set().contains(Atspi.StateType.SENSITIVE)
        created = rpc("new", name="Command Test")
        search("", "New Tab")
        wait_for(lambda: close_window.get_state_set().contains(Atspi.StateType.SHOWING))
        wait_for(lambda: not shortcut_labels(close_window))
        wait_for(lambda: close_pane.get_state_set().contains(Atspi.StateType.SENSITIVE))
        assert shortcut_labels(close_pane) == pane_shortcut
        assert close_window.get_state_set().contains(Atspi.StateType.SENSITIVE)
        for query, name in [
            ("settings", "Preferences"), ("settigns", "Preferences"),
            ("font", "Preferences"), ("keybindings", "Keyboard Shortcuts"),
            ("new directory", "New Tab in Directory…"),
            ("splti riht", "Split Right"), ("name", "Rename Tab…"),
            ("toggle sidebar", "Show or Hide Tabs"), ("manage", "Manage Workspaces…"),
        ]:
            search(query, name)

        search("", "New Tab")
        rows = [command_list.get_child_at_index(index)
                for index in range(command_list.get_child_count())]
        rows = [row for row in rows if row.get_role_name() == "list item"]
        def uniform_heights():
            heights = {row.get_component_iface().get_extents(Atspi.CoordType.WINDOW).height
                       for row in rows}
            return bool(rows) and len(heights) == 1 and next(iter(heights)) > 0

        wait_for(uniform_heights)
        for name in ["Add Workspace…", "Split Down", "Toggle Fullscreen", "New Tab"]:
            announcements.clear()
            row = find(name, "list item")
            assert selection.select_child(row.get_index_in_parent())
            wait_for(lambda: ("Search commands", name) in announcements)
            assert selected(name)
            assert focused(entry)
        rpc("screenshot", path=str(artifacts / f"commands-{appearance}.png"))

        announcements.clear()
        assert entry.get_editable_text_iface().set_text_contents("zzzzzz")
        wait_for(lambda: ("Search commands", "No Matching Commands") in announcements)
        assert selection.get_n_selected_children() == 0
        wait_for(lambda: find("No Matching Commands", "label"))
        search("new", "New Tab")
        search("close window", "Close Window")
        rpc("close", pane=created["pane"])
        wait_for(lambda: shortcut_labels(close_window) == pane_shortcut)
        wait_for(lambda: not close_pane.get_state_set().contains(Atspi.StateType.SENSITIVE))
        assert close_window.get_state_set().contains(Atspi.StateType.SENSITIVE)
        rpc("action", name="shortcuts")
        wait_for(lambda: find("Closes the window when the empty state is shown", "label"))
        shortcuts_dialog = wait_for(lambda: find("Main Menu", "label"))
        while shortcuts_dialog and shortcuts_dialog.get_role_name() != "dialog":
            shortcuts_dialog = shortcuts_dialog.get_parent()
        assert shortcuts_dialog, "Main Menu help must belong to the shortcuts dialog"
        for title in ("New Tab", "New Tab in Directory…", "Show or Hide Tabs", "Split Right",
                      "Split Down", "Find in Terminal…", "Zoom In", "Zoom Out", "Reset Zoom",
                      "Main Menu", "Commands…", "Toggle Fullscreen", "Quit Gmux"):
            wait_for(lambda: any(obj.get_role_name() == "label" and obj.get_name() == title
                                for obj in descendants(shortcuts_dialog)))
        print("PASS: command labels, related terms, typo matching, ranking, disabled matches, "
              "uniform rows, retained search focus, selection and no-result announcements, "
              "search-to-list accessibility relation, conditional Close Window shortcut labels, "
              "and the Close Pane shortcut explanation")
    finally:
        if listener:
            listener.deregister("object:announcement")
