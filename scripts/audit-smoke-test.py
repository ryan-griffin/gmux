#!/usr/bin/env python3
"""Check focus, directory, screenshot, activation, and restoration regressions."""
import json
from pathlib import Path
from cargo_test_support import cargo_binary
import resource
import shlex
import subprocess

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, assert_stays, rpc as direct_rpc, scenario, wait_for

resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def tab(fixture, identifier):
    return next(t for t in fixture.rpc("list")["tabs"] if t["id"] == identifier)


def run(fixture, pane, command):
    wait_for(lambda: "GMUX_AUDIT>" in fixture.rpc("read", pane=pane)["text"])
    fixture.rpc("send", pane=pane, text=command, paste=True, enter=True)


def inactive_pane_focus(fixture, app, log):
    first = fixture.rpc("list")["tabs"][0]
    a = first["active_pane"]
    b = fixture.rpc("split", pane=a)["pane"]
    c = fixture.rpc("split", pane=b)["pane"]
    fixture.rpc("focus", pane=c)
    fixture.rpc("close", pane=b)
    assert tab(fixture, first["id"])["active_pane"] == c


def background_tab_focus(fixture, app, log):
    first = fixture.rpc("list")["tabs"][0]
    a = first["active_pane"]
    b = fixture.rpc("split", pane=a)["pane"]
    selected = fixture.rpc("new", name="Keep selected", directory=str(root))
    fixture.rpc("close", pane=a)
    assert fixture.rpc("list")["active_tab"] == selected["tab"]
    fixture.rpc("close", pane=b)
    assert fixture.rpc("list")["active_tab"] == selected["tab"]


def stale_directory(fixture, app, log):
    first = fixture.rpc("list")["tabs"][0]
    pane = first["active_pane"]
    changed = fixture.path / "new-directory"
    changed.mkdir()
    run(fixture, pane, f"printf '\\033]7;{root.as_uri()}\\007'; cd {shlex.quote(str(changed))}")
    wait_for(lambda: tab(fixture, first["id"])["panes"][0]["directory"] == str(changed), timeout=4)
    new = fixture.rpc("split", pane=pane)["pane"]
    assert next(p for p in tab(fixture, first["id"])["panes"] if p["id"] == new)["directory"] == str(changed)


def fresh_screenshot(fixture, app, log):
    pane = fixture.rpc("list")["tabs"][0]["active_pane"]
    before, after = artifacts / "audit-before.png", artifacts / "audit-after.png"

    def background_pixels(path):
        image = GdkPixbuf.Pixbuf.new_from_file(str(path))
        pixels = image.get_pixels()
        channels, stride = image.get_n_channels(), image.get_rowstride()
        color = bytes((52, 86, 120))
        return sum(pixels[y * stride + x * channels:y * stride + x * channels + 3] == color
                   for y in range(image.get_height()) for x in range(image.get_width()))

    fixture.rpc("screenshot", path=str(before))
    assert background_pixels(before) < 100, "The screenshot fixture already contains the target background"
    run(fixture, pane, "printf '\\033]11;#345678\\007\\033[2J\\033[HCAPTURE_UPDATED\\n'")
    wait_for(lambda: fixture.rpc("read", pane=pane)["text"].startswith("CAPTURE_UPDATED"))

    def updated_pixels():
        fixture.rpc("screenshot", path=str(after))
        return background_pixels(after) > 10_000

    wait_for(updated_pixels, message="Screenshot did not render the terminal's new background color")


def activation(fixture, app, log):
    count = len(fixture.rpc("list")["tabs"])
    opened = fixture.path / "open-directory"
    opened.mkdir()
    result = subprocess.run([str(cargo_binary("gmux")), "--directory", str(opened)],
                            env=fixture.env, cwd=root, stdout=log, stderr=log, timeout=10)
    assert result.returncode == 0
    wait_for(lambda: len(fixture.rpc("list")["tabs"]) == count + 1, timeout=3)
    assert fixture.rpc("list")["tabs"][0]["panes"][0]["directory"] == str(opened)


def malformed_request_targets(fixture, app, log):
    first = fixture.rpc("list")["tabs"][0]
    pane = first["active_pane"]
    wait_for(lambda: "GMUX_AUDIT>" in fixture.rpc("read", pane=pane)["text"])
    for value in ("invalid", -1, 1.5, True, {}, []):
        for method, params in (
            ("send", dict(text="printf 'INVALID_TARGET_SENT\\n'\n", paste=True)),
            ("read", {}),
            ("renderer-info", {}),
            ("focus", dict(tab=first["id"])),
        ):
            result = fixture.rpc(method, pane=value, check=False, **params)
            assert "error" in result, (method, value, result)
        assert "error" in fixture.rpc("filter", workspace=value, value="all", check=False)
        assert "error" in fixture.rpc("focus", pane=pane, tab=value, check=False)
    for field in ("paste", "enter"):
        for value in ("invalid", 0, None, {}, []):
            result = fixture.rpc("send", pane=pane, text="printf 'INVALID_OPTION_SENT\\n'\n",
                                 check=False, **{field: value})
            assert "error" in result, (field, value, result)
    assert fixture.rpc("list")["active_tab"] == first["id"]
    assert "INVALID_TARGET_SENT" not in fixture.rpc("read")["text"]
    assert "INVALID_OPTION_SENT" not in fixture.rpc("read", pane=None)["text"]
    fixture.rpc("send", pane=pane, text="printf 'VALID_REQUEST_SENT\\n'", paste=True, enter=True)
    wait_for(lambda: "VALID_REQUEST_SENT" in fixture.rpc("read", pane=pane)["text"])


def delayed_navigation(fixture, app, log):
    with fixture.delayed_app(cargo_binary("gmux"), "rename", cwd=root, stdout=log, stderr=log) as delay:
        a = fixture.rpc("new", name="Pending rename")
        b = fixture.rpc("new", name="Later selection")
        fixture.rpc("split", pane=b["pane"])
        workspace = fixture.rpc("workspace-new", name="Later filter", directory=str(root))["workspace"]
        fixture.rpc("move", tab=b["tab"], workspace=workspace)
        fixture.rpc("focus", pane=a["pane"])
        with delay.command(tab=a["tab"], name="Renamed tab"):
            fixture.rpc("filter", workspace=workspace)
            fixture.rpc("focus", pane=b["pane"])
            expected = fixture.rpc("list")
            assert expected["active_tab"] == b["tab"]
            assert tab(fixture, b["tab"])["active_pane"] == b["pane"]
            assert expected["filter"] == {"workspace": workspace}
        actual = fixture.rpc("list")
        assert actual["active_tab"] == b["tab"], "A delayed reply replaced the later tab selection"
        assert tab(fixture, b["tab"])["active_pane"] == b["pane"], "A delayed reply replaced the later pane selection"
        assert actual["filter"] == {"workspace": workspace}, "A delayed reply replaced the later workspace filter"
        assert tab(fixture, a["tab"])["name"] == "Renamed tab", "The command change was not applied"
        # Commands that create a pane still reveal their requested result.
        split = fixture.rpc("split", pane=a["pane"])
        assert fixture.rpc("list")["active_tab"] == a["tab"]
        assert tab(fixture, a["tab"])["active_pane"] == split["pane"]
        assert fixture.rpc("list")["filter"] == "all"


def delayed_workspace_moves(fixture, app, log):
    for into_empty, populated in ((True, False), (True, True), (False, False)):
        name = ("delayed move into a workspace with no search results" if populated else
                "delayed move into a selected empty workspace" if into_empty else
                "delayed move out of the later selected workspace")
        with scenario(name):
            with fixture.delayed_app(cargo_binary("gmux"), "move", cwd=root, stdout=log, stderr=log) as delay:
                suffix = "search" if populated else "incoming" if into_empty else "outgoing"
                source = fixture.rpc("workspace-new", name=f"Source {suffix}", directory=str(root))["workspace"]
                destination = fixture.rpc("workspace-new", name=f"Destination {suffix}", directory=str(root))["workspace"]
                moving = fixture.rpc("new", name="Moving tab", workspace=source)
                staying = fixture.rpc("new", name="Staying tab", workspace=source)
                if populated:
                    fixture.rpc("new", name="Existing destination tab", workspace=destination)
                if into_empty:
                    fixture.rpc("focus", pane=moving["pane"])
                else:
                    fixture.rpc("new", name="Other tab", workspace=None)
                selected = destination if into_empty else source
                expected = moving if into_empty else staying
                with delay.command(tab=moving["tab"], workspace=destination):
                    if populated:
                        fixture.rpc("search-tabs", query="Moving tab")
                    fixture.rpc("filter", workspace=selected)
                    if into_empty:
                        assert fixture.rpc("list")["active_tab"] is None
                    else:
                        fixture.rpc("focus", pane=moving["pane"])
                        assert fixture.rpc("list")["active_tab"] == moving["tab"]
                actual = fixture.rpc("list")
                assert actual["filter"] == {"workspace": selected}, "A move replaced the later workspace filter"
                assert actual["active_tab"] == expected["tab"], "Focus did not follow the updated workspace contents"
                assert tab(fixture, expected["tab"])["visible"]
                if populated:
                    assert actual["tab_search"] == "Moving tab", "A move cleared the sidebar search"
                assert fixture.rpc("renderer-info")["viewport"] is not None, "A populated workspace kept the empty screen"
                assert_stays(lambda: fixture.rpc("list")["active_tab"] == expected["tab"], .3,
                             message="A move reverted focus after reconciling workspace membership")


def delayed_closed_selection(fixture, app, log):
    with fixture.delayed_app(cargo_binary("gmux"), "close", cwd=root, stdout=log, stderr=log) as delay:
        closing = fixture.rpc("new", name="Closing tab")
        remaining = fixture.rpc("new", name="Remaining tab")
        with delay.command(pane=closing["pane"]):
            fixture.rpc("focus", pane=closing["pane"])
            assert fixture.rpc("list")["active_tab"] == closing["tab"]
        assert fixture.rpc("list")["active_tab"] == remaining["tab"], "A closed tab retained focus"
        assert all(t["id"] != closing["tab"] for t in fixture.rpc("list")["tabs"])


def delayed_deleted_workspace_filter(fixture, app, log):
    for populated in (False, True):
        with scenario("delayed deletion of a populated workspace" if populated else "delayed deletion of an empty workspace"):
            with fixture.delayed_app(cargo_binary("gmux"), "workspace-delete", cwd=root, stdout=log, stderr=log) as delay:
                survivor = fixture.rpc("new", name="Visible survivor")
                workspace = fixture.rpc("workspace-new", name="Deleted workspace", directory=str(root))["workspace"]
                if populated:
                    member = fixture.rpc("new", name="Surviving former member", workspace=workspace)
                    fixture.rpc("focus", pane=survivor["pane"])
                with delay.command(workspace=workspace):
                    fixture.rpc("filter", workspace=workspace)
                    assert fixture.rpc("list")["active_tab"] == (member["tab"] if populated else None)
                actual = fixture.rpc("list")
                expected = member["tab"] if populated else survivor["tab"]
                assert actual["filter"] == "all"
                assert actual["active_tab"] == expected, "A deleted filter discarded a valid tab selection"
                assert any(t["id"] == expected and t["visible"] for t in actual["tabs"])


def delayed_shell_exits(fixture, app, log):
    for method in ("rename", "new", "split"):
        with scenario(f"shell exit before a delayed {method} reply"):
            armed = False
            with fixture.delayed_app(cargo_binary("gmux"), method, match=lambda _: armed, cwd=root, stdout=log, stderr=log) as delay:
                survivor = fixture.rpc("new", name="Surviving tab")
                original = str(fixture.path / "socket")
                if method == "new":
                    params = dict(name="Exit before new reply")
                else:
                    source = fixture.rpc("new", name="Exit before reply")
                    if method == "rename":
                        params = dict(tab=source["tab"], name="Renamed before exit")
                        fixture.rpc("focus", pane=survivor["pane"])
                    else:
                        # Two removals before the selected new pane must focus
                        # the next surviving pane in the original layout.
                        next_pane = fixture.rpc("split", pane=source["pane"])["pane"]
                        fixture.rpc("split", pane=next_pane)
                        fixture.rpc("split", pane=next_pane)
                        params = dict(pane=source["pane"])
                known = {p["id"] for t in fixture.rpc("list")["tabs"] for p in t["panes"]}
                armed = True
                with delay.command(**params):
                    snapshot = direct_rpc(original, "_state")
                    target = (source["pane"] if method == "rename" else
                              next(m["id"] for m in snapshot["metadata"] if m["id"] not in known))
                    exiting = [target] + ([source["pane"]] if method == "split" else [])
                    # Bypass UI forwarding for the new pane, which has not
                    # been attached by the frontend until its reply arrives.
                    ui_path = Path(original + ".ui")
                    ui_path.unlink()
                    try:
                        for pane in exiting:
                            direct_rpc(original, "send", pane=pane, text="exit", paste=True, enter=True)
                    finally:
                        ui_path.symlink_to(fixture.env["GMUX_SOCKET"] + ".ui")
                    wait_for(lambda: all(m["id"] not in exiting for m in direct_rpc(original, "_state")["metadata"]))
                    # The stream orders this title after the exit packets.
                    # Observing it confirms that preattach exits reached Ui.
                    fixture.rpc("send", pane=survivor["pane"],
                                text="printf '\\033]2;EXIT_PACKETS_OBSERVED\\007'", paste=True, enter=True)
                    wait_for(lambda: any(p["id"] == survivor["pane"] and p["title"] == "EXIT_PACKETS_OBSERVED"
                                         for t in fixture.rpc("list")["tabs"] for p in t["panes"]))

                def no_exited_panes():
                    return all(p["id"] not in exiting for t in fixture.rpc("list")["tabs"] for p in t["panes"])

                assert_stays(no_exited_panes, 1.2, message=f"A stale {method} snapshot recreated an exited pane")
                if method == "split":
                    assert tab(fixture, source["tab"])["active_pane"] == next_pane
                else:
                    assert fixture.rpc("list")["active_tab"] == survivor["tab"]
                armed = False
                later = fixture.rpc("new", name="Created after delayed reply")
                assert later["pane"] > max(exiting), "A later pane reused an exited ID"
                run(fixture, later["pane"], "printf 'LATER_PANE_ALIVE\\n'")
                wait_for(lambda: "LATER_PANE_ALIVE" in fixture.rpc("read", pane=later["pane"])["text"])


def shell_exit(fixture, app, log):
    selected = fixture.rpc("list")["active_tab"]
    workspace = fixture.rpc("workspace-new", name="Keep workspace", directory=str(root))["workspace"]
    exiting = fixture.rpc("new", name="Exit checks", workspace=workspace)
    left = exiting["pane"]
    middle = fixture.rpc("split", pane=left)["pane"]
    right = fixture.rpc("split", pane=middle, down=True)["pane"]
    run(fixture, left, "printf '\\nSTILL_HERE\\n'; (exit 7)")
    wait_for(lambda: "STILL_HERE" in fixture.rpc("read", pane=left)["text"].splitlines())
    assert len(tab(fixture, exiting["tab"])["panes"]) == 3, "A subshell exit closed its parent pane"
    run(fixture, left, "exit")
    wait_for(lambda: len(tab(fixture, exiting["tab"])["panes"]) == 2)
    assert tab(fixture, exiting["tab"])["active_pane"] == right
    assert tab(fixture, exiting["tab"])["layout"]["Split"]["first"] == {"Pane": middle}
    run(fixture, right, "exit 7")
    wait_for(lambda: tab(fixture, exiting["tab"])["layout"] == {"Pane": middle})
    assert tab(fixture, exiting["tab"])["active_pane"] == middle
    fixture.rpc("send", pane=middle, text="\x04")
    wait_for(lambda: all(t["id"] != exiting["tab"] for t in fixture.rpc("list")["tabs"]))
    assert fixture.rpc("list")["active_tab"] == selected
    assert any(w["id"] == workspace for w in fixture.rpc("list")["workspaces"])


def invalid_saved_ids(fixture, app, log):
    workspace = fixture.rpc("workspace-new", name="Saved workspace", directory=str(root))["workspace"]
    first = fixture.rpc("new", name="Saved split", workspace=workspace)
    second = fixture.rpc("split", pane=first["pane"])["pane"]
    fixture.rpc("split", pane=second, down=True)
    fixture.stop()
    session_path = fixture.config / "session.json"
    saved = json.loads(session_path.read_text())
    valid = saved["tabs"]
    invalid = dict(id=2**64 - 1, name="Invalid tab", custom_name=True, workspace=None,
                   layout={"Pane": 42}, directories={"42": str(root)}, active=42)
    duplicate = dict(valid[0], name="Duplicate tab")
    duplicate_pane = dict(duplicate, id=10_000, name="Duplicate pane")
    saved["tabs"] = [invalid, valid[0], duplicate, duplicate_pane] + valid[1:]
    session_path.write_text(json.dumps(saved))
    app = fixture.start_app(cargo_binary("gmux"), cwd=root, stdout=log, stderr=log)
    restored = wait_for(lambda: fixture.rpc("list")["tabs"])
    for key in ("name", "id", "layout", "workspace"):
        assert [t[key] for t in restored] == [t.get(key) for t in valid], key
    for old, current in zip(valid, restored):
        assert {str(p["id"]): p["directory"] for p in current["panes"]} == old["directories"]
    for current in restored:
        for pane in current["panes"]:
            run(fixture, pane["id"], "exit")
    wait_for(lambda: fixture.rpc("list")["tabs"] == [])
    assert fixture.rpc("list")["active_tab"] is None
    assert app.poll() is None, "Exiting the last pane closed the empty window"
    assert fixture.rpc("list")["workspaces"], "Exiting shells deleted workspace definitions"


cases = [
    ("closing an inactive pane preserves focus", inactive_pane_focus),
    ("closing background panes and the last background pane preserves focus", background_tab_focus),
    ("stale OSC directories do not override the shell's directory", stale_directory),
    ("screenshots render the current terminal background", fresh_screenshot),
    ("opening a directory activates the existing application", activation),
    ("malformed UI request targets and send options are rejected", malformed_request_targets),
    ("delayed command replies preserve later tab, pane, and workspace choices", delayed_navigation),
    ("delayed moves reconcile focus with updated workspace contents", delayed_workspace_moves),
    ("delayed close replies select a surviving tab", delayed_closed_selection),
    ("a deleted workspace filter cannot retain an empty selection", delayed_deleted_workspace_filter),
    ("delayed command snapshots cannot undo observed shell exits", delayed_shell_exits),
    ("shell exit preserves remaining splits, focus, and workspaces", shell_exit),
    ("invalid saved IDs are skipped and valid tabs restore", invalid_saved_ids),
]
with (artifacts / "audit-smoke.log").open("w") as log:
    for name, test in cases:
        with scenario(name), AppFixture(
            dict(appearance="dark", desktop_notifications=False), prefix="gmux-audit-",
            env={"GTK_A11Y": "none"},
            shell_script="#!/bin/sh\nexport PS1='GMUX_AUDIT> '\nexport PROMPT_COMMAND=''\nexec /bin/bash --noprofile --norc -i\n",
        ) as fixture:
            app = fixture.start_app(cargo_binary("gmux"), "--directory", root,
                                    cwd=root, stdout=log, stderr=log)
            test(fixture, app, log)
