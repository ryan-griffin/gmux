#!/usr/bin/env python3
"""Exercise optional Git metadata through local and direct remote streams."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

daemon = cargo_binary("gmux")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


def git(path, *args):
    return subprocess.run(["git", "-C", str(path), *args], check=True, capture_output=True, text=True).stdout


def branch(client, name, *, pane=None, directory=None):
    pane = client.pane if pane is None else pane
    def matches(value):
        return value is not None and value["id"] == pane and value["branch"] == name \
            and (directory is None or value["directory"] == str(directory))
    if matches(client.git.get(pane)):
        return client.git[pane]
    return client.until(lambda header: matches(header.get("git")))["git"]


def stats(fixture):
    return fixture.rpc("status")["git"]


def settled(fixture):
    return wait_for(lambda: (value if (value := stats(fixture))["pending"] == 0 else None))


with (artifacts / "git-smoke.log").open("w") as log, AppFixture() as owner, AppFixture() as receiver:
    repository = owner.path / "repository"
    repository.mkdir()
    git(repository, "init", "--initial-branch=initial")
    nested = repository / "nested"
    nested.mkdir()
    owner.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    owner.start(daemon, "serve", stdout=log, stderr=log)
    receiver.start(daemon, "serve", stdout=log, stderr=log)
    address = wait_for(lambda: owner.rpc("status")["bound_address"])
    receiver.rpc("connection-add", name="owner", address=address, authentication=dict(mode="none"))
    pane = owner.rpc("new", directory=str(repository))["pane"]
    nested_pane = owner.rpc("new", directory=str(nested))["pane"]
    clients = []
    try:
        local = Client(owner, pane)
        clients.append(local)
        remote = Client(receiver, pane, connection="owner")
        clients.append(remote)
        with scenario("headless execution and ordinary stream clients perform no Git work"):
            assert owner.rpc("status")["git_subscribers"] == 0
            assert stats(owner) == dict(panes=0, repositories=0, watches=0, workers=0, lookups=0, pending=0)
            local.send("paste", text="printf 'NO_GIT_WORK\n'", enter=True)
            local.output("NO_GIT_WORK")
            assert stats(owner)["lookups"] == 0

        with scenario("local and remote clients share a repository and receive unborn branches"):
            local.send("git-subscribe", panes=[pane, nested_pane])
            branch(local, "initial", directory=repository)
            branch(local, "initial", pane=nested_pane, directory=nested)
            # Let the initial HEAD watch recheck finish before measuring sharing.
            local.barrier(1)
            before = settled(owner)
            remote.send("git-subscribe", panes=[pane, nested_pane])
            branch(remote, "initial", directory=repository)
            branch(remote, "initial", pane=nested_pane, directory=nested)
            after = settled(owner)
            assert owner.rpc("status")["git_subscribers"] == 2
            assert after["panes"] == 2 and after["repositories"] == 1 and after["workers"] == 2
            assert after["watches"] == before["watches"] and after["lookups"] == before["lookups"]
            assert stats(receiver)["workers"] == 0, "The receiver must not inspect the remote filesystem"
            sharing = dict(one_client=before, two_clients=after)

        with scenario("atomic HEAD replacement updates every subscribed pane and client"):
            began = time.monotonic()
            git(repository, "symbolic-ref", "HEAD", "refs/heads/updated")
            for client in (local, remote):
                branch(client, "updated", directory=repository)
                branch(client, "updated", pane=nested_pane, directory=nested)
            current = settled(owner)
            assert current["lookups"] == after["lookups"] + 1, (after, current)
            sharing["checkout_ms"] = (time.monotonic() - began) * 1000
            (artifacts / "git-subscription-resources.json").write_text(json.dumps(sharing, indent=2) + "\n")

        with scenario("detached HEAD hides the label and linked worktrees keep their own branch"):
            git(repository, "-c", "user.name=Gmux Test", "-c", "user.email=test@example.invalid",
                "commit", "--allow-empty", "-m", "fixture")
            git(repository, "checkout", "--detach")
            branch(local, "", directory=repository)
            branch(remote, "", directory=repository)
            worktree = owner.path / "worktree"
            git(repository, "worktree", "add", "-b", "worktree-branch", str(worktree))
            linked_pane = owner.rpc("new", directory=str(worktree))["pane"]
            local.send("git-subscribe", panes=[pane, nested_pane, linked_pane])
            branch(local, "worktree-branch", pane=linked_pane, directory=worktree)
            git(worktree, "checkout", "-b", "worktree-updated")
            branch(local, "worktree-updated", pane=linked_pane, directory=worktree)
            assert settled(owner)["repositories"] == 2

        with scenario("CWD changes, repository creation and removal clear and restore metadata"):
            directory = owner.path / "later-repository"
            directory.mkdir()
            local.send("paste", text="cd " + shlex.quote(str(directory)), enter=True)
            branch(local, "", directory=directory)
            branch(remote, "", directory=directory)
            before = settled(owner)
            # Git creates .git before HEAD. Let a lookup see that incomplete
            # repository so its later HEAD creation must wake the daemon.
            (directory / ".git").mkdir()
            wait_for(lambda: (value := stats(owner))["lookups"] > before["lookups"]
                     and value["pending"] == 0)
            git(directory, "init", "--initial-branch=later")
            branch(local, "later", directory=directory)
            branch(remote, "later", directory=directory)
            (directory / ".git").rename(directory / "old-git")
            branch(local, "", directory=directory)
            branch(remote, "", directory=directory)
            (directory / "old-git").rename(directory / ".git")
            branch(local, "later", directory=directory)
            branch(remote, "later", directory=directory)

        with scenario("unsubscribe and disconnect release the last repository watches without stopping execution"):
            local.send("git-subscribe", panes=[])
            local.barrier(2)
            wait_for(lambda: owner.rpc("status")["git_subscribers"] == 1)
            assert settled(owner)["panes"] == 2
            remote.close()
            clients.remove(remote)
            wait_for(lambda: owner.rpc("status")["git_subscribers"] == 0)
            wait_for(lambda: stats(owner)["workers"] == 0 and stats(owner)["watches"] == 0)
            assert len(owner.rpc("list")["tabs"]) == 3
            assert "git" not in owner.rpc("_state")["execution"]
            local.send("paste", text="printf 'AFTER_GIT_DETACH\n'", enter=True)
            local.output("AFTER_GIT_DETACH")
    finally:
        for client in clients:
            client.close()

    with scenario("Git unavailable or stalled does not block the daemon or terminal input"), AppFixture() as missing:
        tools = missing.path / "tools"
        tools.mkdir()
        missing.env["PATH"] = str(tools)
        missing.env["SHELL"] = "/bin/sh"
        missing.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: missing.rpc("status"))
        pane = missing.rpc("new", directory=str(missing.path))["pane"]
        client = Client(missing, pane)
        try:
            client.send("git-subscribe", panes=[pane])
            branch(client, "", directory=missing.path)
            wait_for(lambda: stats(missing)["lookups"] > 0 and stats(missing)["pending"] == 0)
            # A fresh subscription starts another lookup after Git becomes available.
            client.send("git-subscribe", panes=[])
            client.barrier(3)
            wait_for(lambda: stats(missing)["workers"] == 0)
            script = tools / "git"
            pid_file = missing.path / "git-pid"
            script.write_text("#!/bin/sh\nprintf '%s\\n' \"$$\" > " + shlex.quote(str(pid_file)) + "\nexec /bin/sleep 30\n")
            script.chmod(0o700)
            client.send("git-subscribe", panes=[pane])
            wait_for(lambda: stats(missing)["pending"] == 1)
            started = time.monotonic()
            client.send("paste", text="printf 'GIT_LOOKUP_IS_ASYNC\n'", enter=True)
            client.output("GIT_LOOKUP_IS_ASYNC")
            assert time.monotonic() - started < 1, "Git lookup blocked terminal execution"
            wait_for(lambda: stats(missing)["pending"] == 0, timeout=5)
            client.send("git-subscribe", panes=[])
            client.barrier(4)
            wait_for(lambda: stats(missing)["workers"] == 0)
            pid_file.unlink()
            client.send("git-subscribe", panes=[pane])
            wait_for(pid_file.exists)
            git_pid = int(pid_file.read_text())
            client.close()
            wait_for(lambda: not Path(f"/proc/{git_pid}").exists(), timeout=1,
                     message="Disconnect did not cancel the running Git process")
        finally:
            client.close()
        wait_for(lambda: stats(missing)["workers"] == 0)

print("PASS: optional Git metadata, shared repository watches, remote routing, checkout/worktree/CWD updates, and cleanup")
