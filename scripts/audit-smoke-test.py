#!/usr/bin/env python3
"""Check focus, directory, screenshot, activation, and restoration regressions."""
import json
from pathlib import Path
import resource
import shlex
import subprocess

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, scenario, wait_for

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
    result = subprocess.run([str(root / "target/debug/gmux"), "--directory", str(opened)],
                            env=fixture.env, cwd=root, stdout=log, stderr=log, timeout=10)
    assert result.returncode == 0
    wait_for(lambda: len(fixture.rpc("list")["tabs"]) == count + 1, timeout=3)
    assert fixture.rpc("list")["tabs"][0]["panes"][0]["directory"] == str(opened)


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
    app = fixture.start_app(root / "target/debug/gmux", cwd=root, stdout=log, stderr=log)
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
            app = fixture.start_app(root / "target/debug/gmux", "--directory", root,
                                    cwd=root, stdout=log, stderr=log)
            test(fixture, app, log)
