#!/usr/bin/env python3
"""Check prompt Git branch updates in the native sidebar, including worktrees."""
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon
from test_support import rpc as shared_rpc, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)

with tempfile.TemporaryDirectory(prefix="gmux-branch-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"),
               GMUX_SOCKET=str(temp / "socket"), SHELL=str(shell), GTK_A11Y="none")
    project = temp / "project with spaces\nand a newline"
    project.mkdir()
    subdir = project / "src"
    subdir.mkdir()
    worktree = temp / "worktree"
    outside = temp / "outside"
    outside.mkdir()

    def git(directory, *args):
        subprocess.run(["git", "-C", str(directory), *args], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10)

    git(project, "init", "--quiet", "--initial-branch=main-branch")
    git(project, "-c", "user.name=Gmux test", "-c", "user.email=test@example.com",
        "-c", "commit.gpgsign=false", "commit", "--quiet", "--allow-empty", "-m", "Fixture")
    for branch in ("feature-one", "feature-two"):
        git(project, "branch", branch)
    git(project, "worktree", "add", "--quiet", "-b", "worktree-one", str(worktree))

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, **params)

    def visible():
        return {tab["id"] for tab in rpc("list")["tabs"] if tab["visible"]}

    def expect_branch(branch, tabs, timeout=0.75):
        rpc("search-tabs", query=branch)
        wait_for(lambda: visible() == set(tabs), timeout=timeout,
                 message=f"Sidebar did not update {branch!r} within {timeout}s")

    def pane_directory(pane):
        return next(p["directory"] for tab in rpc("list")["tabs"]
                    for p in tab["panes"] if p["id"] == pane)

    with (artifacts / "branch-smoke.log").open("w") as log:
        app = subprocess.Popen([str(root / "target/debug/gmux")], env=env,
                               stdout=log, stderr=log)
        try:
            wait_for(lambda: rpc("daemon-info")["attached"])
            first = rpc("new", name="Repository", directory=str(subdir))
            second = rpc("new", name="Sibling", directory=str(subdir))
            linked = rpc("new", name="Linked checkout", directory=str(worktree))
            untracked = rpc("new", name="Outside", directory=str(outside))
            shared = {first["tab"], second["tab"]}
            expect_branch("main-branch", shared, timeout=8)
            expect_branch("worktree-one", {linked["tab"]}, timeout=8)

            # These external commands produce no terminal events. The UI must
            # detect each atomic HEAD replacement through its filesystem watch.
            latencies = []
            for branch in ("feature-one", "feature-two", "main-branch") * 3:
                git(project, "switch", "--quiet", branch)
                started = time.monotonic()
                expect_branch(branch, shared)
                latencies.append(time.monotonic() - started)
                expect_branch("worktree-one", {linked["tab"]})

            git(worktree, "branch", "--move", "worktree-renamed")
            expect_branch("worktree-renamed", {linked["tab"]})
            git(worktree, "switch", "--quiet", "--detach")
            expect_branch("worktree-renamed", set())
            git(worktree, "switch", "--quiet", "--create", "worktree-two")
            expect_branch("worktree-two", {linked["tab"]})

            # Repository discovery still has a periodic fallback. Once found,
            # even a repository with no commits must react promptly to HEAD.
            git(outside, "init", "--quiet", "--initial-branch=unborn-one")
            expect_branch("unborn-one", {untracked["tab"]}, timeout=8)
            git(outside, "symbolic-ref", "HEAD", "refs/heads/unborn-two")
            expect_branch("unborn-two", {untracked["tab"]})

            rpc("search-tabs", query="")
            pane = rpc("split", pane=first["pane"])["pane"]
            rpc("send", pane=pane, text=f"cd {shlex.quote(str(worktree))}",
                paste=True, enter=True)
            wait_for(lambda: pane_directory(pane) == str(worktree))
            expect_branch("worktree-two", {first["tab"], linked["tab"]})
            rpc("focus", pane=first["pane"])
            expect_branch("main-branch", shared)
            rpc("focus", pane=pane)
            expect_branch("worktree-two", {first["tab"], linked["tab"]})
            rpc("send", pane=pane, text="exit", paste=True, enter=True)
            wait_for(lambda: next(tab for tab in rpc("list")["tabs"]
                                  if tab["id"] == first["tab"])["active_pane"] == first["pane"])
            expect_branch("main-branch", shared)

            print(f"PASS: external branch switches update in at most {max(latencies)*1000:.0f}ms; "
                  "shared tabs, worktree HEADs, branch rename, detached HEAD, unborn branches, "
                  "directory changes, selected split panes, and shell exit")
        finally:
            app.terminate()
            app.wait(timeout=10)
            stop_daemon(env["GMUX_SOCKET"])
