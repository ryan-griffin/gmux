#!/usr/bin/env python3
"""Check terminal find controls through native GTK accessibility.

Run with GDK_BACKEND=wayland dbus-run-session -- python3
scripts/find-smoke-test.py. Uses its own app, settings, and accessibility bus.
"""
from test_support import AccessibilityFixture, AppFixture, descendants, wait_accessible as wait_for
from pathlib import Path
from cargo_test_support import cargo_binary
import os
import shlex

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi
from wayland_test_support import ClipboardObserver, WaylandInput

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def find(name, role):
    return next((element for element in descendants(Atspi.get_desktop(0))
                 if element.get_process_id() == app.pid and element.get_name() == name and element.get_role_name() == role), None)


def sensitive(element):
    element.clear_cache()
    return element.get_state_set().contains(Atspi.StateType.SENSITIVE)


def sidebar_directory(directory):
    for row in descendants(Atspi.get_desktop(0)):
        if row.get_process_id() != app.pid or row.get_role_name() != "list item":
            continue
        states = row.get_state_set()
        if not (states.contains(Atspi.StateType.SHOWING) and states.contains(Atspi.StateType.SELECTED)):
            continue
        labels = [label.get_name() for label in descendants(row) if label.get_role_name() == "label"]
        if labels and labels[0] == "Find Test" and str(directory) in labels[1:]:
            return row
    return None


def window_size():
    frame = next((element for element in descendants(Atspi.get_desktop(0))
                  if element.get_process_id() == app.pid and element.get_role_name() == "frame"), None)
    if frame:
        bounds = frame.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
        return bounds.width, bounds.height


with (artifacts / "find-smoke.log").open("w") as log, AccessibilityFixture(), WaylandInput() as native:
    clipboard = ClipboardObserver(native)
    for appearance in ("dark", "light"):
        with AppFixture(dict(desktop_notifications=False, appearance=appearance),
            prefix="gmux-find-", env={"GTK_A11Y": "atspi", "GDK_DEBUG": os.environ.get("GDK_DEBUG", "") + ":no-vsync"},
            shell_script="#!/bin/sh\nexport PS1='GMUX_FIND> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n",
        ) as fixture:
            rpc = fixture.rpc
            app = fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
            first_directory = fixture.path / "first-directory"
            first_directory.mkdir()
            first = rpc("new", name="Find Test", directory=str(first_directory))
            wait_for(lambda: "GMUX_FIND>" in rpc("read", pane=first["pane"])["text"])
            rpc("send", pane=first["pane"], text="printf '\\033[2J\\033[HAlpha beta ALPHA\\nalpha\\n'", paste=True, enter=True)
            wait_for(lambda: "Alpha beta ALPHA" in rpc("read", pane=first["pane"])["text"])
            rpc("action", name="find")
            entry = wait_for(lambda: (find("Find...", "entry") or find("Find...", "text")))
            previous = wait_for(lambda: find("Previous match (Ctrl+Shift+G)", "button"))
            following = wait_for(lambda: find("Next match (Ctrl+G)", "button"))
            assert not sensitive(previous) and not sensitive(following)
            assert entry.get_editable_text_iface().set_text_contents("alpha")
            wait_for(lambda: find("1 of 3", "label"))
            assert sensitive(previous) and sensitive(following)
            assert previous.get_action_iface().do_action(0)
            wait_for(lambda: find("3 of 3", "label"))
            assert following.get_action_iface().do_action(0)
            wait_for(lambda: find("1 of 3", "label"))
            assert following.get_action_iface().do_action(0)
            wait_for(lambda: find("2 of 3", "label"))
            rpc("screenshot", path=str(artifacts / f"find-{appearance}.png"))
            field = entry.get_parent()
            field_bounds = field.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
            count_bounds = find("2 of 3", "label").get_component_iface().get_extents(Atspi.CoordType.WINDOW)
            assert count_bounds.x > field_bounds.x
            assert count_bounds.x + count_bounds.width < field_bounds.x + field_bounds.width
            # The compact controls must also fit a narrow window.
            previous_width = rpc("renderer-info", pane=first["pane"])["viewport"]["width"]
            rpc("resize", width=540, height=480)
            wait_for(lambda: window_size() == (540, 480), message="Find window did not reach its narrow size")
            wait_for(lambda: rpc("renderer-info", pane=first["pane"])["viewport"]["width"] < previous_width)
            rpc("screenshot", path=str(artifacts / f"find-{appearance}-narrow.png"))
            wait_for(lambda: find("2 of 3", "label"))
            assert entry.get_component_iface().get_extents(Atspi.CoordType.WINDOW).width > 80
            narrow_width = rpc("renderer-info", pane=first["pane"])["viewport"]["width"]
            rpc("resize", width=1120, height=740)
            # Terminal width can grow just because the collapsed sidebar hides.
            # Wait for the native window and its sidebar layout before switching tabs.
            wait_for(lambda: window_size() == (1120, 740), message="Find window did not return to its wide size")
            wait_for(lambda: not rpc("list")["sidebar_collapsed"] and rpc("list")["sidebar_visible"],
                     message="Wide Find window did not restore its sidebar")
            wait_for(lambda: rpc("renderer-info", pane=first["pane"])["viewport"]["width"] > narrow_width)

            # New output updates the total without changing the current match.
            rpc("send", pane=first["pane"], text="printf 'ALPHA\\n'", paste=True, enter=True)
            wait_for(lambda: find("2 of 4", "label"))
            # Reopen Find to focus the entry after native window resizing.
            rpc("action", name="find")
            rpc("action", name="find")
            assert entry.get_editable_text_iface().set_text_contents("missing")
            wait_for(lambda: not sensitive(previous) and not sensitive(following))
            assert not sensitive(previous) and not sensitive(following)
            assert not find("0 of 0", "label")
            assert not find("2 of 4", "label")
            entry.clear_cache()
            assert entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)
            rpc("screenshot", path=str(artifacts / f"find-{appearance}-not-found.png"))
            assert entry.get_editable_text_iface().set_text_contents("alpha")
            wait_for(lambda: find("1 of 4", "label"))
            entry.clear_cache()
            assert not entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)

            # Counts follow the active pane and tab, rather than stale output.
            rpc("new", name="Other Tab")
            wait_for(lambda: not sensitive(previous) and not sensitive(following))
            rpc("focus", pane=first["pane"])
            wait_for(lambda: find("1 of 4", "label"))
            other_pane = rpc("split", pane=first["pane"])["pane"]
            wait_for(lambda: not sensitive(previous) and not sensitive(following))
            rpc("focus", pane=first["pane"])
            wait_for(lambda: find("1 of 4", "label"))
            # Sidebar paths must follow focus when split panes use different directories.
            second_directory = fixture.path / "second-directory"
            second_directory.mkdir()
            rpc("send", pane=other_pane, text=f"cd {shlex.quote(str(second_directory))}", paste=True, enter=True)
            wait_for(lambda: any(p["id"] == other_pane and p["directory"] == str(second_directory)
                                 for t in rpc("list")["tabs"] for p in t["panes"]))
            assert next(t for t in rpc("list")["tabs"] if t["id"] == first["tab"])["active_pane"] == first["pane"]
            wait_for(lambda: sidebar_directory(first_directory),
                     message="Selected sidebar row did not show the first pane's directory")
            rpc("focus", pane=other_pane)
            wait_for(lambda: sidebar_directory(second_directory),
                     message="The sidebar kept the previously focused pane's directory")
            rpc("focus", pane=first["pane"])
            wait_for(lambda: sidebar_directory(first_directory))
            wait_for(lambda: find("1 of 4", "label"))
            clear = wait_for(lambda: find("Clear search", "button"))
            assert clear.get_action_iface().do_action(0)
            wait_for(lambda: not find("1 of 4", "label"))
            assert not sensitive(previous) and not sensitive(following)
            entry.clear_cache()
            assert not entry.get_state_set().contains(Atspi.StateType.INVALID_ENTRY)
            rpc("action", name="find")
            rpc("action", name="find")
            entry = wait_for(lambda: (find("Find...", "entry") or find("Find...", "text")))
            assert Atspi.Text.get_text(entry, 0, -1) == ""
            assert rpc("list")["active_tab"] == first["tab"]
            # Find must scan retained history and reveal an offscreen match.
            rpc("send", pane=first["pane"], text="printf '\\033[3J\\033[2J\\033[HSCROLLBACK_FIND_界\\n'; seq 1 200", paste=True, enter=True)
            wait_for(lambda: "200" in rpc("read", pane=first["pane"])["text"])
            assert "SCROLLBACK_FIND_界" not in rpc("read", pane=first["pane"])["text"]
            assert entry.get_editable_text_iface().set_text_contents("scrollback_find_界")
            wait_for(lambda: find("1 of 1", "label"))
            assert following.get_action_iface().do_action(0)
            wait_for(lambda: "SCROLLBACK_FIND_界" in rpc("read", pane=first["pane"])["text"])
            # Place the query across the terminal's current wrap boundary.
            cols = rpc("_metadata", pane=first["pane"])["meta"]["size"][0]
            wrapped = "x" * (cols - 2) + "needle"
            rpc("send", pane=first["pane"], text="printf '\\033[3J\\033[2J\\033[H" + wrapped + "\\n'", paste=True, enter=True)
            wait_for(lambda: wrapped in rpc("read", pane=first["pane"])["text"].replace("\n", ""))
            assert entry.get_editable_text_iface().set_text_contents("needle")
            wait_for(lambda: find("1 of 1", "label"))

            # Quit checkpoints history; reopening starts fresh shells. Find must
            # still reveal Unicode and wrapped text, including after grid reflow.
            restored_line = "RESTORED_FIND_界 e\u0301 " + wrapped
            rpc("send", pane=first["pane"], text="printf '\\033[3J\\033[2J\\033[H\\033[38;2;12;34;56m\\033[48;2;78;90;123m" + restored_line + "\\033[0m\\n'; seq 1 200",
                paste=True, enter=True)
            wait_for(lambda: "200" in rpc("read", pane=first["pane"])["text"])
            assert "RESTORED_FIND_界" not in rpc("read", pane=first["pane"])["text"]
            result = rpc("quit")
            assert result["ok"] and not result.get("warning"), result
            assert app.wait(timeout=8) == 0
            app = fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
            wait_for(lambda: "GMUX_FIND>" in rpc("read", pane=first["pane"])["text"])
            rpc("focus", pane=first["pane"])
            wait_for(lambda: sidebar_directory(first_directory))
            assert "RESTORED_FIND_界" not in rpc("read", pane=first["pane"])["text"]
            closed = rpc("renderer-info", pane=first["pane"])
            closed_top = closed["viewport"]["origin"][1]
            closed_bottom = closed["window_size"][1] - closed_top - closed["viewport"]["height"]
            rpc("action", name="find")
            entry = wait_for(lambda: (find("Find...", "entry") or find("Find...", "text")))
            following = wait_for(lambda: find("Next match (Ctrl+G)", "button"))
            assert entry.get_editable_text_iface().set_text_contents("restored_find_界")
            wait_for(lambda: find("1 of 1", "label"))
            assert following.get_action_iface().do_action(0)
            wait_for(lambda: restored_line in rpc("read", pane=first["pane"])["text"].replace("\n", ""))
            old_cols = rpc("_metadata", pane=first["pane"])["meta"]["size"][0]
            rpc("resize", width=960, height=740)
            wait_for(lambda: window_size() == (960, 740))
            wait_for(lambda: rpc("_metadata", pane=first["pane"])["meta"]["size"][0] != old_cols)
            assert entry.get_editable_text_iface().set_text_contents("needle")
            wait_for(lambda: find("1 of 1", "label"))
            assert following.get_action_iface().do_action(0)
            wait_for(lambda: restored_line in rpc("read", pane=first["pane"])["text"].replace("\n", ""))
            # Remove search highlights to inspect exact restored background
            # pixels in a composited frame, then copy the reflowed Unicode line
            # with native pointer selection and the Wayland clipboard.
            assert entry.get_editable_text_iface().set_text_contents("")
            rpc("action", name="find")
            def search_hidden():
                state = rpc("renderer-info", pane=first["pane"])
                viewport = state["viewport"]
                return (viewport["origin"][1] == closed_top and
                    viewport["height"] == state["window_size"][1] - closed_top - closed_bottom)
            wait_for(search_hidden, message="Find did not return to its closed vertical allocation")

            def grid_current():
                viewport = rpc("renderer-info", pane=first["pane"])["viewport"]
                return (not viewport["scroll_pending"] and
                    rpc("_metadata", pane=first["pane"])["meta"]["size"] == [viewport["cols"], viewport["rows"]])
            wait_for(grid_current, message="Hidden Find layout did not reach the daemon grid")
            # Hiding Find animates the terminal's origin and grid. Old capture
            # coordinates can select a different history row during that motion.
            native.drain_frames()
            previous_frame = native.latest_frame
            def restored_bounds():
                frame = native.frame(timeout=.1, fresh=False)
                return frame.color_bounds((78, 90, 123)) if frame is not previous_frame else None
            left, top, right, bottom = wait_for(restored_bounds)
            state = rpc("renderer-info", pane=first["pane"])["viewport"]
            cols, cw, ch = state["cols"], state["cell_width"], state["cell_height"]
            # The wide CJK character and combining mark cancel in len().
            end_row, end_col = divmod(len(restored_line) - 1, cols)
            native.move(left + cw / 2, top + ch / 2)
            native.button(True)
            native.move(left + (end_col + .5) * cw, top + (end_row + .5) * ch)
            native.button(False)
            wait_for(lambda: rpc("_copy", pane=first["pane"])["text"] == restored_line)
            wait_for(lambda: rpc("renderer-info", pane=first["pane"])["viewport"]["selection"] is not None)
            owner_count = clipboard.checkpoint()
            rpc("action", name="copy")
            clipboard.wait_for_text(restored_line, owner_count)
            text = clipboard.read()
            assert text == restored_line, (text, restored_line)
            rpc("screenshot", path=str(artifacts / f"find-{appearance}-restored-colors.png"))
            print(f"PASS: {appearance} Find, focused sidebar directories, retained and checkpoint-restored Unicode/wrapped history, reflow, restored color pixels and native clipboard copy", flush=True)

print("PASS: dark/light find bars, Unicode history search, offscreen navigation, match counts, wrapping, live output, tab/pane switching, focused sidebar directories, clearing and reopening, checkpoint restore and reflow")
