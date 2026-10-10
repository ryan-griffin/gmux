#!/usr/bin/env python3
"""Check native focus synchronization, directory events, and bounded Git updates."""
from functools import partial
import json
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import shlex
import shutil
import subprocess
import time

import gi
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, GLib

from test_support import AppFixture, wait_for as shared_wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
wait_for = partial(shared_wait_for, context=GLib.MainContext.default())


def chord(native, *keys):
    for key in keys:
        native.key(key, True)
    for key in reversed(keys):
        native.key(key, False)


def pump(seconds):
    until = time.monotonic() + seconds
    context = GLib.MainContext.default()
    while time.monotonic() < until:
        while context.pending():
            context.iteration(False)
        time.sleep(0.005)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


with AppFixture(dict(show_sidebar=False, desktop_notifications=False)) as fixture:
    fixture.env["GTK_A11Y"] = "none"
    real_git = shutil.which("git")
    repository = fixture.path / "repo"
    slow_repository = fixture.path / "slow-repo"

    def git(directory, *args):
        subprocess.run([real_git, "-C", str(directory), *args], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10)

    for directory, branch in ((repository, "fast-main"), (slow_repository, "slow-main")):
        directory.mkdir()
        git(directory, "init", "--quiet", "--initial-branch=" + branch)
        git(directory, "-c", "user.name=Gmux test", "-c", "user.email=test@example.com",
            "-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "Fixture")
    for name in ("a", "b", "c"):
        (repository / name).mkdir()

    wrapper = fixture.path / "bin"
    wrapper.mkdir()
    git_log = fixture.path / "git.log"
    block = fixture.path / "block-git"
    blocked = fixture.path / "blocked-pid"
    (wrapper / "git").write_text(
        "#!/usr/bin/python3\nimport json, os, sys, time\nfrom pathlib import Path\n"
        f"with open({str(git_log)!r}, 'a') as log:\n"
        "    log.write(json.dumps([time.monotonic(), sys.argv[1:]]) + '\\n')\n"
        f"if sys.argv[1:3] == ['-C', {str(slow_repository)!r}] and Path({str(block)!r}).exists():\n"
        f"    Path({str(blocked)!r}).write_text(str(os.getpid()))\n"
        f"    while Path({str(block)!r}).exists(): time.sleep(0.01)\n"
        f"os.execv({real_git!r}, [{real_git!r}] + sys.argv[1:])\n")
    (wrapper / "git").chmod(0o700)
    fixture.env["PATH"] = str(wrapper) + os.pathsep + fixture.env["PATH"]

    def branch_tabs(branch):
        fixture.rpc("search-tabs", query=branch)
        return {tab["id"] for tab in fixture.rpc("list")["tabs"] if tab["visible"]}

    def expect_branch(branch, tabs, timeout=0.75):
        wait_for(lambda: branch_tabs(branch) == set(tabs), timeout=timeout,
                 message=f"Branch {branch} did not update promptly")

    with (artifacts / "scheduling-smoke.log").open("w") as log, WaylandInput() as native:
        fixture.start(cargo_binary("gmux"), cwd=root, stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("daemon-info")["attached"])
        tabs = []
        for name in ("a", "b"):
            tab = fixture.rpc("new", directory=str(repository / name), name="Pane " + name)
            tabs.append(tab)
            fixture.rpc("send", pane=tab["pane"], text="printf '\\033[2J\\033[HMARKER_" + name.upper() + "\\n'",
                        paste=True, enter=True)
            wait_for(lambda: "MARKER_" + name.upper() in fixture.rpc("read", pane=tab["pane"])["text"])
        shared = {tab["tab"] for tab in tabs}
        expect_branch("fast-main", shared)
        fixture.rpc("search-tabs", query="")
        wait_for(lambda: fixture.rpc("renderer-info")["window_active"])

        # Native events bypass the IPC handler's save flush. Check before the
        # two-second persistence delay, and never list tabs before the default read.
        for _ in range(4):
            fixture.rpc("focus", pane=tabs[1]["pane"])
            chord(native, Gdk.KEY_Control_L, Gdk.KEY_Tab)
            pump(0.07)
            assert "MARKER_A" in fixture.rpc("read")["text"], "Default read used the previous tab"
            assert fixture.rpc("_state")["session"]["active"] == tabs[0]["tab"]
        sibling = fixture.rpc("split", pane=tabs[0]["pane"])["pane"]
        fixture.rpc("send", pane=sibling, text="printf 'MARKER_SIBLING\\n'", paste=True, enter=True)
        wait_for(lambda: "MARKER_SIBLING" in fixture.rpc("read", pane=sibling)["text"])
        chord(native, Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_Left)
        pump(0.07)
        assert "MARKER_A" in fixture.rpc("read")["text"], "Default read used the previous split pane"
        state = fixture.rpc("_state")["session"]
        assert next(tab for tab in state["tabs"] if tab["id"] == tabs[0]["tab"])["active"] == tabs[0]["pane"]
        pump(0.3)

        latencies = []
        for number, name in enumerate(("c", "a", "c", "a")):
            target = repository / name
            marker = f"CWD_READY_{number}"
            command = "cd " + shlex.quote(str(target)) + "; printf '\\033]7;file://localhost%s\\007" + marker + "\\n' \"$PWD\""
            native.set_clipboard_text(command)
            chord(native, Gdk.KEY_Control_L, Gdk.KEY_Shift_L, Gdk.KEY_v)
            pump(0.1)
            began = time.monotonic()
            chord(native, Gdk.KEY_Return)
            wait_for(lambda: marker in fixture.rpc("read", pane=tabs[0]["pane"])["text"], timeout=1)
            def updated():
                pane = next(p for tab in fixture.rpc("list")["tabs"] for p in tab["panes"]
                            if p["id"] == tabs[0]["pane"])
                return pane["directory"] == str(target)
            wait_for(updated, timeout=0.3, message="Directory message waited for metadata polling")
            latencies.append(round((time.monotonic() - began) * 1000))

        expect_branch("fast-main", shared)
        fixture.rpc("search-tabs", query="")
        pump(0.3)
        baseline = len(git_log.read_text().splitlines())
        pump(2.5)
        idle = git_log.read_text().splitlines()[baseline:]
        assert not idle, f"Idle tabs launched Git again: {idle}"

        slow = fixture.rpc("new", directory=str(slow_repository), name="Slow repository")
        expect_branch("slow-main", {slow["tab"]})
        pump(0.3)
        block.touch()
        git(slow_repository, "branch", "--move", "slow-stuck")
        wait_for(blocked.exists)
        blocked_pid = int(blocked.read_text())
        began = time.monotonic()
        git(repository, "branch", "--move", "fast-updated")
        expect_branch("fast-updated", shared)
        branch_ms = round((time.monotonic() - began) * 1000)
        wait_for(lambda: not alive(blocked_pid), timeout=3, message="Hung Git was not killed and reaped")
        block.unlink()
        # The working HEAD watch must survive a timeout. A subsequent change
        # should recover immediately, without the failed-lookup fallback.
        git(slow_repository, "branch", "--move", "slow-recovered")
        expect_branch("slow-recovered", {slow["tab"]})
        print(f"PASS: native tab and pane focus sync before saving; directory messages in {max(latencies)}ms; "
              f"zero idle Git launches; unrelated branch update in {branch_ms}ms during a stuck lookup; Git timeout recovery")
