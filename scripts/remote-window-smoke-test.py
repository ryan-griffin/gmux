#!/usr/bin/env python3
"""Check GTK tabs across connections, workspace ownership, routing, and detach."""
import fcntl
import json
import os
import signal
import shutil
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import shlex
import subprocess
import time

from cargo_test_support import cargo_binary
from test_support import AppFixture, scenario, wait_for
from wayland_test_support import ensure_wayland_session

ensure_wayland_session()
daemon, cli = cargo_binary("gmux"), cargo_binary("gmux-gtk")
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def start(fixture, log, address="127.0.0.1:0"):
    command(fixture, "connection", "edit", "local", "--remote-access", "true", "--listen-address", address, "--auth", "none")
    process = fixture.start(daemon, "serve", stdout=log, stderr=log)
    wait_for(lambda: fixture.rpc("status")["pid"] == process.pid)
    return process


def command(fixture, *args, check=True, cwd=None, env=None):
    binary = daemon if args[0] == "connection" else cli
    result = subprocess.run([str(binary), *map(str, args)], env=env or fixture.env, cwd=cwd,
                            text=True, capture_output=True, timeout=10)
    assert (result.returncode == 0) == check, result.stderr or result.stdout
    return json.loads(result.stdout) if check else result.stderr


with scenario("a standalone GTK client aggregates remotes and owns workspaces without local execution"), \
        (artifacts / "remote-only-window-smoke.log").open("w") as log, \
        AppFixture(dict(desktop_notifications=False), env={"GTK_A11Y": "none"}) as receiver, \
        AppFixture() as server, AppFixture() as other:
    remote_home = server.path / "home"
    (remote_home / "project").mkdir(parents=True)
    server.env["HOME"] = str(remote_home)
    server_process,other_process=(start(fixture,log) for fixture in (server,other))
    original = server.rpc("new", name="Server")
    second = other.rpc("new", name="Other")
    command(receiver, "connection", "add", server.rpc("status")["bound_address"], "--name", "server", "--auth", "none")
    command(receiver, "connection", "add", other.rpc("status")["bound_address"], "--name", "other", "--auth", "none")
    # Tab management also works directly when no GUI is open.
    command(receiver, "rename", "--tab", original["tab"], "Before GUI", "--connection", "server")
    assert server.rpc("list")["tabs"][0]["name"] == "Before GUI"
    command(receiver, "reset-name", "--tab", original["tab"], "--connection", "server")
    assert not server.rpc("_state")["execution"]["tabs"][0]["custom_name"]
    command(receiver, "rename", "--tab", original["tab"], "Server", "--connection", "server")
    temporary = command(receiver, "new", "--connection", "server")
    command(receiver, "close-tab", "--tab", temporary["tab"], "--connection", "server")
    assert [tab["id"] for tab in server.rpc("list")["tabs"]] == [original["tab"]]
    with scenario("GTK CLI RPC preserves JSON and reports JSON target fields without a running app"):
        request = json.dumps(dict(method="read", pane=original["pane"]))
        assert isinstance(command(server, "rpc", request)["text"], str)
        assert isinstance(command(receiver, "--connection", "server", "rpc", request)["text"], str)
        for method, field in (("read", "pane"), ("move", "tab")):
            error = command(receiver, "--connection", "server", "rpc", json.dumps(dict(method=method)), check=False)
            assert f"explicit {field} field" in error and f"--{field}" not in error, error
    # No sibling daemon executable and no native terminal library are available to this client.
    standalone = receiver.path / "client-only/gmux-gtk"
    standalone.parent.mkdir()
    shutil.copy2(cli, standalone)
    linkage = subprocess.check_output(["ldd", str(standalone)], text=True)
    assert "libghostty" not in linkage, linkage
    receiver.set_settings(dict(local_daemon_enabled=False))
    app = receiver.start(standalone, "--connection", "server", stdout=log, stderr=log)
    wait_for(lambda: receiver.rpc("renderer-info")["gtk"])
    wait_for(lambda: len(receiver.rpc("list")["tabs"]) == 2)
    assert {t["connection"] for t in receiver.rpc("list")["tabs"]} == {"server", "other"}
    assert isinstance(command(receiver, "rpc", json.dumps(dict(method="read", pane=original["pane"])))["text"], str)
    assert "absolute" in command(receiver, "new", "--directory", "relative", check=False)
    created = command(receiver, "new", "--directory", "~/project")
    assert next(tab for tab in server.rpc("list")["tabs"] if tab["id"] == created["tab"])["panes"][0]["directory"] == str(remote_home / "project")
    command(receiver, "close-tab", "--connection", "server", "--tab", created["tab"])
    wait_for(lambda: len(receiver.rpc("list")["tabs"]) == 2)
    with scenario("launch options select existing and new windows without a local daemon"):
        first = receiver.rpc("windows")["windows"][0]["id"]
        launcher = receiver.start(standalone, "--connection", "other", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        assert receiver.rpc("list", window=first)["daemon"] == second["daemon"]
        created = command(receiver, "new")
        assert created["daemon"] == second["daemon"]
        command(receiver, "close-tab", "--connection", "other", "--tab", created["tab"])
        launcher = receiver.start(standalone, "--new-window", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        inherited = next(w["id"] for w in receiver.rpc("windows")["windows"] if w["id"] != first)
        assert receiver.rpc("list", window=inherited)["daemon"] == second["daemon"]
        receiver.rpc("detach", window=inherited)
        wait_for(lambda: len(receiver.rpc("windows")["windows"]) == 1)
        assert "does not exist" in command(receiver, "--new-window", "--connection", "missing", check=False)
        assert [w["id"] for w in receiver.rpc("windows")["windows"]] == [first]
        for via_gio in (False, True):
            control = Path(receiver.env["GMUX_SOCKET"] + ".ui")
            pending = control.with_suffix(".ui-starting")
            # Exercise activation while the process owns its GApplication name but
            # the CLI cannot yet reach its app socket, as during window startup.
            if via_gio:
                control.rename(pending)
            try:
                launcher = receiver.start(standalone, "--new-window", "--connection", "server", "--directory", "~/project", stdout=log, stderr=log)
                assert launcher.wait(timeout=8) == 0
            finally:
                if via_gio:
                    pending.rename(control)
            another = next(w["id"] for w in receiver.rpc("windows")["windows"] if w["id"] != first)
            assert receiver.rpc("list", window=first)["daemon"] == second["daemon"]
            assert receiver.rpc("list", window=another)["daemon"] == original["daemon"]
            created = wait_for(lambda: next((t for t in server.rpc("list")["tabs"] if t["id"] != original["tab"]), None))
            assert created["panes"][0]["directory"] == str(remote_home / "project")
            command(receiver, "close-tab", "--connection", "server", "--tab", created["id"])
            receiver.rpc("detach", window=another)
            wait_for(lambda: len(receiver.rpc("windows")["windows"]) == 1)
        launcher = receiver.start(standalone, "--connection", "server", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
    for connection in ("local", "server"):
        for args in (("quit",), ("screenshot", str(receiver.path / "unexpected.png")), ("workspace", "add", "--name", "Unexpected"), ("focus", "--workspace", "1")):
            assert "omit --connection" in command(receiver, *args, "--connection", connection, check=False)
    assert app.poll() is None and server_process.poll() is None
    workspace = command(receiver, "workspace", "add", "--name", "Remote project", "--connection-directory", "server=~/project")["workspace"]
    assert receiver.rpc("list")["workspaces"][0]["directories"][original["daemon"]] == "~/project"
    command(receiver, "workspace", "edit", workspace, "--name", "Build", "--connection-directory", "server=~/build")
    definition = receiver.rpc("list")["workspaces"][0]
    assert definition["name"] == "Build" and definition["directories"][original["daemon"]] == "~/build"
    temporary = command(receiver, "workspace", "add", "--name", "Temporary")["workspace"]
    receiver.rpc("move", connection="other", tab=second["tab"], workspace=temporary)
    command(receiver, "workspace", "remove", temporary)
    assert next(tab for tab in receiver.rpc("list")["tabs"] if tab["connection"] == "other")["workspace"] is None
    assert other.rpc("list")["tabs"][0]["id"] == second["tab"]
    receiver.rpc("move", connection="server", tab=original["tab"], workspace=workspace)
    command(receiver, "connection", "edit", "server", "--name", "renamed")
    wait_for(lambda: any(t["connection"] == "renamed" and t["workspace"] == workspace for t in receiver.rpc("list")["tabs"]))
    command(receiver,"send","printf 'REMOTE_ONLY_INPUT\\n'","--pane",original["pane"],"--enter","--connection","renamed")
    wait_for(lambda: "REMOTE_ONLY_INPUT" in server.rpc("read", pane=original["pane"])["text"])
    os.kill(server_process.pid,signal.SIGSTOP)
    try:
        before=time.monotonic()
        receiver.rpc("detach")
        assert app.wait(timeout=2)==0
        assert time.monotonic()-before<2,"Remote-only close waited for its default daemon"
    finally:
        os.kill(server_process.pid,signal.SIGCONT)
    app=receiver.start(standalone,"--connection","renamed","--directory","~/project",stdout=log,stderr=log)
    wait_for(lambda:len(receiver.rpc("list")["tabs"])==3)
    created=next(t for t in server.rpc("list")["tabs"] if t["id"]!=original["tab"])
    assert created["panes"][0]["directory"]==str(remote_home/"project")
    command(receiver,"close-tab","--connection","renamed","--tab",created["id"])
    wait_for(lambda:len(receiver.rpc("list")["tabs"])==2)
    receiver.rpc("connection-remove", connection="renamed")
    wait_for(lambda: [t["connection"] for t in receiver.rpc("list")["tabs"]] == ["other"])
    receiver.rpc("connection-remove", connection="other")
    wait_for(lambda: receiver.rpc("list")["tabs"] == [])
    receiver.rpc("workspace-add", name="Offline organization")
    assert len(receiver.rpc("list")["workspaces"]) == 2
    assert app.poll() is None
    receiver.rpc("action", name="quit")
    assert app.wait(timeout=10) == 0
    assert server.rpc("list")["tabs"][0]["id"] == original["tab"]
    assert other.rpc("list")["tabs"][0]["id"] == second["tab"]
    assert not Path(receiver.env["GMUX_SOCKET"]).exists()
    assert not (receiver.config / "daemon-id.json").exists()
    assert not (receiver.config / "execution.json").exists()
    assert not (receiver.config / "daemon.log").exists()
    assert (receiver.config / "clients/gtk/state.json").exists()


with scenario("initial directories wait for the enabled local daemon while remotes connect"), \
        (artifacts / "remote-startup-routing-smoke.log").open("w") as log, \
        AppFixture(dict(desktop_notifications=False), env={"GTK_A11Y": "none"}) as receiver, \
        AppFixture() as server:
    start(server, log)
    original = server.rpc("new", name="Existing remote")
    command(receiver, "connection", "add", server.rpc("status")["bound_address"],
            "--name", "server", "--auth", "none")
    with Path(receiver.env["GMUX_SOCKET"] + ".lock").open("a") as lock:
        # Keep local startup pending until a remote stream and a catalog edit
        # have both been observed by the window.
        fcntl.flock(lock, fcntl.LOCK_EX)
        app = receiver.start(cli, "--directory", "/tmp", stdout=log, stderr=log)
        wait_for(lambda: any(tab["connected"] for tab in receiver.rpc("list")["tabs"]))
        assert receiver.rpc("list")["daemon"] is None, "Local startup selected a remote default"
        command(receiver, "connection", "edit", "server", "--name", "renamed")
        wait_for(lambda: any(tab["connection"] == "renamed" for tab in receiver.rpc("list")["tabs"]))
        assert receiver.rpc("list")["daemon"] is None, "A catalog refresh selected a remote default"
        assert [tab["id"] for tab in server.rpc("list")["tabs"]] == [original["tab"]]
    local = wait_for(lambda: receiver.rpc("status"))
    wait_for(lambda: len(receiver.rpc("list")["tabs"]) == 2)
    listing = receiver.rpc("list")
    assert listing["daemon"] == local["daemon"]
    created = next(tab for tab in listing["tabs"] if tab["reference"]["daemon"] == local["daemon"])
    assert created["panes"][0]["directory"] == "/tmp"
    assert [tab["id"] for tab in server.rpc("list")["tabs"]] == [original["tab"]]
    receiver.rpc("detach")
    assert app.wait(timeout=10) == 0


with (artifacts / "remote-window-smoke.log").open("w") as log, \
        AppFixture(dict(desktop_notifications=False), prefix="gmux-window-a-", env={"GTK_A11Y": "none"}) as a, \
        AppFixture(prefix="gmux-window-b-") as b, AppFixture(prefix="gmux-window-c-") as c:
    pa, pb, pc = (start(f, log) for f in (a, b, c))
    assert "running GTK app" in command(a, "quit", check=False)
    assert pa.poll() is None, "GTK quit without a window stopped the daemon"
    addresses = [f.rpc("status")["bound_address"] for f in (a, b, c)]
    local_name = a.rpc("connection-list")["local"]["name"]
    identities = [f.rpc("_hello")["daemon"] for f in (a, b, c)]
    branches = ["local-branch", "server-branch", "other-branch"]
    for fixture, branch_name in zip((a, b, c), branches):
        subprocess.run(["git", "-C", str(fixture.path), "init", "--initial-branch=" + branch_name],
                       check=True, capture_output=True)
    own = [f.rpc("new", name=name, directory=str(f.path)) for f, name in zip((a, b, c), ("Local", "Server", "Other"))]
    assert len({t["pane"] for t in own}) == 1
    (a.path / "relative").mkdir()
    created = command(a, "new", "--directory", "relative", cwd=a.path)
    assert next(tab for tab in a.rpc("list")["tabs"] if tab["id"] == created["tab"])["panes"][0]["directory"] == str(a.path / "relative")
    command(a, "close-tab", "--tab", created["tab"])
    assert "desktop" not in b.rpc("_state")
    app = a.start_app(cli, stdout=log, stderr=log)
    first = a.rpc("windows")["windows"][0]["id"]

    def view(window=first):
        return a.rpc("list", window=window)

    def tab(daemon_id, tab_id, window=first):
        return next(t for t in view(window)["tabs"] if t["reference"] == dict(daemon=daemon_id, id=tab_id))

    def references(window=first):
        return [t["reference"] for t in view(window)["tabs"]]

    with scenario("runtime remote definitions add all hosts to one sidebar with qualified IDs"):
        for name, address in zip(("server", "other"), addresses[1:]):
            command(a, "connection", "add", address, "--name", name, "--auth", "none")
        wait_for(lambda: len(view()["tabs"]) == 3)
        assert {t["connection"] for t in view()["tabs"]} == {local_name, "server", "other"}
        assert all(t["visible"] and t["connected"] for t in view()["tabs"])
        assert tab(identities[1], own[1]["tab"]).get("workspace") is None
        assert view()["workspaces"] == []
        assert "desktop" not in b.rpc("_state")
        existing = [f.rpc("list", client=False)["tabs"][0] for f in (a, b, c)]
        expected = sorted(existing, key=lambda t: (-t["created_at"], t["reference"]["daemon"], -t["id"]))
        assert references() == [t["reference"] for t in expected], "Imported tabs must use their original creation times"

    with scenario("local commands and inherited shell targets stay local with a remote default"):
        launcher = a.start(cli, "--connection", "server", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        assert view()["daemon"] == identities[1]
        remote_execution = b.rpc("_state")["execution"]
        command(a, "rename", "--connection", "local", "--tab", own[0]["tab"], "Explicit local")
        assert a.rpc("list", client=False)["tabs"][0]["name"] == "Explicit local"
        created_local = command(a, "new", "--connection", "local", "--directory", str(a.path))
        assert created_local["daemon"] == identities[0]
        a.rpc("focus", connection="local", pane=created_local["pane"])
        a.rpc("action", name="split-down")
        assert len(tab(identities[0], created_local["tab"])["panes"]) == 2
        command(a, "close-tab", "--connection", "local", "--tab", created_local["tab"])
        inherited = dict(a.env, GMUX_PANE=str(own[0]["pane"]), GMUX_TAB=str(own[0]["tab"]))
        split = command(a, "split", env=inherited)
        assert split["daemon"] == identities[0]
        command(a, "close", env=dict(inherited, GMUX_PANE=str(split["pane"])))
        workspace = a.rpc("workspace-add", name="Shell target")["workspace"]
        command(a, "move", "--workspace", workspace, env=inherited)
        assert tab(identities[0], own[0]["tab"])["workspace"] == workspace
        a.rpc("workspace-remove", workspace=workspace)
        assert b.rpc("_state")["execution"] == remote_execution
        command(a, "rename", "--connection", "local", "--tab", own[0]["tab"], "Local")
        launcher = a.start(cli, "--connection", "local", stdout=log, stderr=log)
        assert launcher.wait(timeout=8) == 0
        assert view()["daemon"] == identities[0]

    with scenario("GTK CLI renames, resets, and closes tabs on the selected connection"):
        original_names = ["Local", "Server", "Other"]
        for index, connection in ((0, "local"), (1, "server")):
            fixture = (a, b)[index]
            target = own[index]["tab"]
            command(a, "rename", "--tab", target, "CLI renamed", "--connection", connection)
            wait_for(lambda: tab(identities[index], target)["name"] == "CLI renamed")
            assert fixture.rpc("list", client=False)["tabs"][0]["name"] == "CLI renamed"
            for other in range(3):
                if other != index:
                    assert tab(identities[other], own[other]["tab"])["name"] == original_names[other]
            command(a, "reset-name", "--tab", target, "--connection", connection)
            assert not fixture.rpc("_state", client=False)["execution"]["tabs"][0]["custom_name"]
            wait_for(lambda: tab(identities[index], target)["name"] != "CLI renamed")
            command(a, "rename", "--tab", target, original_names[index], "--connection", connection)
            wait_for(lambda: tab(identities[index], target)["name"] == original_names[index])
            temporary = command(a, "new", "--connection", connection)
            command(a, "split", "--pane", temporary["pane"], "--connection", connection)
            command(a, "close-tab", "--tab", temporary["tab"], "--connection", connection)
            wait_for(lambda: len(view()["tabs"]) == 3)
            assert [item["id"] for item in fixture.rpc("list", client=False)["tabs"]] == [target]

    with scenario("local and remote tabs show the owning active pane's branch and share subscriptions between windows"):
        for index in range(3):
            wait_for(lambda: tab(identities[index], own[index]["tab"])["branch"] == branches[index])
        second = a.rpc("new-window")["window"]
        wait_for(lambda: references(second) == references())
        for index, fixture in enumerate((a, b, c)):
            wait_for(lambda: tab(identities[index], own[index]["tab"], second)["branch"] == branches[index])
            info = fixture.rpc("status")
            assert info["git_subscribers"] == 2 and info["git"]["workers"] == 2
        subprocess.run(["git", "-C", str(b.path), "symbolic-ref", "HEAD", "refs/heads/remote-updated"],
                       check=True, capture_output=True)
        for window in (first, second):
            wait_for(lambda: tab(identities[1], own[1]["tab"], window)["branch"] == "remote-updated")
            assert tab(identities[0], own[0]["tab"], window)["branch"] == "local-branch"
        branches[1] = "remote-updated"
        other_directory = b.path / "different-pane"
        other_directory.mkdir()
        subprocess.run(["git", "-C", str(other_directory), "init", "--initial-branch=pane-branch"],
                       check=True, capture_output=True)
        split = a.rpc("split", window=first, connection="server", pane=own[1]["pane"])
        a.rpc("focus", window=first, connection="server", pane=split["pane"])
        b.rpc("send", pane=split["pane"], text="cd " + shlex.quote(str(other_directory)), paste=True, enter=True)
        wait_for(lambda: tab(identities[1], own[1]["tab"])["branch"] == "pane-branch")
        assert tab(identities[1], own[1]["tab"], second)["branch"] == "remote-updated"
        a.rpc("focus", window=first, connection="server", pane=own[1]["pane"])
        wait_for(lambda: tab(identities[1], own[1]["tab"])["branch"] == "remote-updated")
        b.rpc("close", pane=split["pane"])
        a.rpc("detach", window=second)
        for fixture in (a, b, c):
            wait_for(lambda: fixture.rpc("status")["git_subscribers"] == 1)

    with scenario("tabs created by other clients appear first in every receiving window without changing selection"):
        observer = a.rpc("new-window")["window"]
        wait_for(lambda: references(observer) == references())
        selections = [view(window)["active_reference"] for window in (first, observer)]
        original = references()
        prefix = []
        created_elsewhere = []
        for fixture, identity in ((b, identities[1]), (c, identities[2])):
            created_elsewhere.append((fixture, fixture.rpc("new", name="External tab")))
            prefix.insert(0, dict(daemon=identity, id=created_elsewhere[-1][1]["tab"]))
            for window in (first, observer):
                wait_for(lambda: references(window) == prefix + original)
            assert [view(window)["active_reference"] for window in (first, observer)] == selections
        for fixture, created_tab in created_elsewhere:
            fixture.rpc("close-tab", tab=created_tab["tab"])
        for window in (first, observer):
            wait_for(lambda: references(window) == original)
        a.rpc("detach", window=observer)
        wait_for(lambda: len(a.rpc("windows")["windows"]) == 1)

    with scenario("removing a definition detaches its rows and preserves its execution"):
        command(a, "connection", "remove", "other")
        wait_for(lambda: len(view()["tabs"]) == 2)
        wait_for(lambda: c.rpc("status")["remote_clients"] == 0)
        assert c.rpc("list")["tabs"][0]["id"] == own[2]["tab"]
        command(a, "connection", "add", addresses[2], "--name", "other", "--auth", "none")
        wait_for(lambda: len(view()["tabs"]) == 3)

    with scenario("explicit remote commands never infer local targets or ignore a connection selector"):
        a.rpc("focus", pane=own[0]["pane"])
        before = [f.rpc("_state", client=False)["execution"] for f in (a, b, c)]
        for method, params in [
            ("split", {}), ("close", {}), ("move", {"workspace": None}),
            ("read", {"pane": own[1]["pane"]}),
            ("send", {"pane": own[1]["pane"], "text": "DO_NOT_SEND"}),
            ("workspace-add", {"name": "Do not create"}),
        ]:
            assert "error" in a.rpc(method, connection="server", check=False, **params)
        for args in [("split",), ("close",), ("rpc", '{"method":"split"}')]:
            result = subprocess.run([str(cli), "--connection", "server", *args],
                env=dict(a.env, GMUX_PANE=str(own[0]["pane"])),
                text=True, capture_output=True, timeout=10)
            assert result.returncode != 0, result.stdout
        assert [f.rpc("_state", client=False)["execution"] for f in (a, b, c)] == before
        split = command(a, "--connection", "server", "split", "--pane", own[1]["pane"])
        assert split["daemon"] == identities[1]
        command(a, "--connection", "server", "close", "--pane", split["pane"])
        wait_for(lambda: len(tab(identities[1], own[1]["tab"])["panes"]) == 1)

    with scenario("explicit remote creation keeps All Tabs visible and targets the chosen connection"):
        assert "error" in a.rpc("filter", connection="server", check=False)
        assert view()["filter"] == "all"
        assert all(t["visible"] for t in view()["tabs"])
        a.rpc("new", connection="server")
        selected = next(t for t in view()["tabs"] if t["reference"] == view()["active_reference"])
        created = dict(tab=selected["id"], pane=selected["active_pane"])
        assert selected["reference"]["daemon"] == identities[1]
        assert selected["panes"][0]["directory"] == str(Path(b.env["HOME"])), "New Tab inherited the selected pane's directory"
        assert len(a.rpc("_state", client=False)["execution"]["tabs"]) == 1
        assert len(b.rpc("_state")["execution"]["tabs"]) == 2
        a.rpc("send", text="printf '\\nREMOTE_UI_INPUT\\n'", paste=True, enter=True)
        wait_for(lambda: "REMOTE_UI_INPUT" in b.rpc("read", pane=created["pane"])["text"])
        wait_for(lambda: "REMOTE_UI_INPUT" in a.rpc("read")["text"], message="Remote viewport did not reach GTK")
        assert "REMOTE_UI_INPUT" not in a.rpc("read", pane=own[0]["pane"])["text"]
        a.rpc("action", name="split-down")
        assert view()["active_reference"]["daemon"] == identities[1]
        wait_for(lambda: len(tab(identities[1], created["tab"])["panes"]) == 2)
        # An implicit command uses the selected remote pane.
        a.rpc("close")
        wait_for(lambda: len(tab(identities[1], created["tab"])["panes"]) == 1)
        assert "desktop" not in b.rpc("_state")

    with scenario("All Tabs creates locally when a remote tab is selected"):
        remote_counts = [len(f.rpc("list")["tabs"]) for f in (b, c)]
        for method in ("action", "new"):
            a.rpc("filter", value="all")
            a.rpc("focus", connection="server", pane=own[1]["pane"])
            assert view()["active_reference"]["daemon"] == identities[1]
            assert view()["filter"] == "all"
            if method == "action":
                a.rpc(method, name="new")
            else:
                assert a.rpc(method)["daemon"] == identities[0]
            current = view()
            selected = next(t for t in current["tabs"] if t["reference"] == current["active_reference"])
            assert selected["reference"]["daemon"] == identities[0]
            assert selected .get("workspace") is None
            assert selected["panes"][0]["directory"] == str(Path(a.env["HOME"]))
            assert [len(f.rpc("list")["tabs"]) for f in (b, c)] == remote_counts

    with scenario("receiving workspaces span hosts without altering the remote desktop"):
        workspace = a.rpc("workspace-add", name="Mixed", directory=str(a.path))["workspace"]
        a.rpc("focus", connection="server", pane=created["pane"])
        a.rpc("move", workspace=workspace)
        a.rpc("focus", pane=own[0]["pane"])
        a.rpc("move", workspace=workspace)
        a.rpc("filter", workspace=workspace)
        assert {t["connection"] for t in view()["tabs"] if t["visible"]} == {local_name, "server"}
        command(a, "--connection", "server", "focus", "--pane", created["pane"])
        a.rpc("filter", workspace=workspace)
        assert view()["active_reference"]["daemon"] == identities[1]
        local = a.rpc("new", name="Workspace local")
        assert local["daemon"] == identities[0] and local["workspace"] == workspace
        assert tab(identities[0], local["tab"])["panes"][0]["directory"] == str(a.path)
        delayed = a.rpc("_state")
        a.rpc("filter", value="all")
        added = a.rpc("new", connection="server", name="Workspace remote")
        a.rpc("move", workspace=workspace)
        a.rpc("filter", workspace=workspace)
        assert added["daemon"] == identities[1] and added .get("workspace") is None
        assert tab(identities[1], added["tab"])["workspace"] == workspace
        a.rpc("_desktop-save", desktop=delayed["desktop"], revision=0)
        persisted = next(t for t in a.rpc("_state")["desktop"]["tabs"] if t["reference"] == dict(daemon=identities[1], id=added["tab"]))
        assert persisted["workspace"] == workspace, "A delayed window save removed a newer assignment"
        assert next(t for t in b.rpc("_state")["execution"]["tabs"] if t["id"] == added["tab"]).get("workspace") is None
        assert "catalog" not in b.rpc("_state")
        a.rpc("filter", value="all")
        a.rpc("search-tabs", query="other")
        assert [t["connection"] for t in view()["tabs"] if t["visible"]] == ["other"]
        a.rpc("search-tabs", query="")
        a.rpc("screenshot", path=str(artifacts / "remote-window.png"))

    with scenario("CLI remote creation applies receiver directories and workspace membership in an open window"):
        a.rpc("workspace-edit", workspace=workspace, name="Mixed",
              directories={identities[0]: str(a.path), identities[1]: str(b.path)})
        created_cli = command(a, "--connection", "server", "new", "--workspace", workspace)
        assert created_cli["workspace"] == workspace
        routed_tab = wait_for(lambda: next((t for t in view()["tabs"]
            if t["reference"] == dict(daemon=identities[1], id=created_cli["tab"])), None))
        wait_for(lambda: tab(identities[1], created_cli["tab"])["workspace"] == workspace)
        assert routed_tab["panes"][0]["directory"] == str(b.path)
        assert next(t for t in b.rpc("_state")["execution"]["tabs"] if t["id"] == created_cli["tab"]).get("workspace") is None
        b.rpc("close", pane=created_cli["pane"])
        wait_for(lambda: all(t["reference"] != dict(daemon=identities[1], id=created_cli["tab"]) for t in view()["tabs"]))
        a.rpc("workspace-edit", workspace=workspace, name="Mixed", directories={identities[0]: str(a.path)})

    with scenario("notifications distinguish identical pane IDs on different hosts"):
        a.rpc("focus", connection="other", pane=own[2]["pane"])
        before = references()
        b.rpc("notify", pane=own[1]["pane"], message="Remote origin alert")
        wait_for(lambda: tab(identities[1], own[1]["tab"])["unread"])
        assert references() == before, "Notifications must not reorder tabs"
        assert not tab(identities[0], own[0]["tab"])["unread"]
        assert not tab(identities[2], own[2]["tab"])["unread"]
        a.rpc("focus", connection="server", pane=own[1]["pane"])
        wait_for(lambda: not b.rpc("list")["tabs"][-1]["unread"])
        assert references() == before, "Reading notifications must not reorder tabs"
        command(a, "notify", "GUI_ACTIVE_REMOTE", env={key:value for key,value in a.env.items() if key != "GMUX_PANE"})
        assert not tab(identities[0], own[0]["tab"])["unread"]

    with scenario("explicit local IDs still target local execution while a remote pane is selected"):
        extra = a.rpc("_command", command=dict(method="new", name="Later local", directory=str(a.path)))["result"]
        wait_for(lambda: any(t["reference"] == dict(daemon=identities[0], id=extra["tab"]) for t in view()["tabs"]))
        a.rpc("focus", connection="other", pane=own[2]["pane"])
        a.rpc("send", pane=extra["pane"], text="printf '\\nLOCAL_ID_ROUTING\\n'", paste=True, enter=True)
        wait_for(lambda: "LOCAL_ID_ROUTING" in a.rpc("read", pane=extra["pane"])["text"])
        assert "LOCAL_ID_ROUTING" not in c.rpc("read", pane=own[2]["pane"])["text"]
        local_split = a.rpc("split", pane=extra["pane"])
        assert local_split["daemon"] == identities[0]
        a.rpc("close", pane=local_split["pane"])

    with scenario("local and remote tab churn preserves routing and rejects closed references"):
        a.rpc("filter", value="all")
        references = {(t["reference"]["daemon"], t["reference"]["id"]) for t in view()["tabs"]}
        for params, identity in (({}, identities[0]), ({"connection": "server"}, identities[1])):
            for index in range(12):
                created = a.rpc("new", name=f"Temporary {index}", **params)
                assert created["daemon"] == identity
                split = a.rpc("split", pane=created["pane"], **params)
                assert split["daemon"] == identity
                a.rpc("close-tab", tab=created["tab"], **params)
                wait_for(lambda: {(t["reference"]["daemon"], t["reference"]["id"]) for t in view()["tabs"]} == references)
                assert "error" in a.rpc("focus", pane=split["pane"], check=False, **params)
        assert all(t["connected"] for t in view()["tabs"])

    with scenario("a stalled remote leaves local and other connections responsive and cannot delay closing"):
        blocked_window = a.rpc("new-window")["window"]
        wait_for(lambda: len(view(blocked_window)["tabs"]) == len(view()["tabs"]))
        a.rpc("filter", window=blocked_window, value="all")
        os.kill(pb.pid, signal.SIGSTOP)
        try:
            with ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(a.rpc, "new", window=blocked_window, connection="server",
                                      name="Slow remote creation", timeout=10)
                time.sleep(0.1)
                assert not pending.done()
                before = time.monotonic()
                local = a.rpc("new", window=blocked_window, name="Local during remote wait")
                other = a.rpc("new", window=blocked_window, connection="other", name="Other during remote wait")
                assert time.monotonic() - before < 2, "A stalled remote blocked unrelated connections"
                assert local["daemon"] == identities[0] and other["daemon"] == identities[2]
                os.kill(pb.pid, signal.SIGCONT)
                slow = pending.result(timeout=3)
                assert view(blocked_window)["active_reference"] == dict(daemon=identities[2], id=other["tab"]), "An earlier slow creation stole focus"
                a.rpc("close-tab", window=blocked_window, tab=local["tab"])
                a.rpc("close-tab", window=blocked_window, connection="other", tab=other["tab"])
                a.rpc("close-tab", window=blocked_window, connection="server", tab=slow["tab"])
                os.kill(pb.pid, signal.SIGSTOP)
                pending = pool.submit(a.rpc, "rename", window=blocked_window, connection="server",
                                      tab=added["tab"], name="Rename while paused", timeout=10, check=False)
                time.sleep(0.1)
                assert not pending.done()
                before = time.monotonic()
                a.rpc("detach", window=blocked_window)
                wait_for(lambda: all(w["id"] != blocked_window for w in a.rpc("windows")["windows"]), timeout=2)
                assert time.monotonic() - before < 2, "Closing waited for the stalled remote"
                try:
                    result = pending.result(timeout=2)
                    assert "error" in result, result
                except (ValueError, OSError):
                    # Destroying the window may close its public reply socket.
                    pass
        finally:
            os.kill(pb.pid, signal.SIGCONT)
        wait_for(lambda: b.rpc("status")["remote_clients"] == 1)
        wait_for(lambda: c.rpc("status")["remote_clients"] == 1)
        # The daemon may finish a rename it already accepted before frontend cancellation.
        a.rpc("rename", connection="server", tab=added["tab"], name="Remote after resume")

    with scenario("multiple windows retain independent selections and release streams on detach"):
        second = a.rpc("new-window")["window"]
        wait_for(lambda: len(view(second)["tabs"]) == len(view()["tabs"]))
        for f in (b, c):
            wait_for(lambda: f.rpc("status")["remote_clients"] == 2)
        a.rpc("focus", window=first, connection="other", pane=own[2]["pane"])
        a.rpc("focus", window=second, connection="server", pane=added["pane"])
        assert view()["active_reference"]["daemon"] == identities[2]
        assert view(second)["active_reference"] == dict(daemon=identities[1], id=added["tab"])
        selections = [view(window)["active_reference"] for window in (first, second)]
        command(a, "connection", "edit", "local", "--name", "desktop")
        for window in (first, second):
            wait_for(lambda: tab(identities[0], own[0]["tab"], window)["connection"] == "desktop")
        for binary in (daemon, cli):
            listed = subprocess.check_output([str(binary), "list", "--connection", "desktop"],
                                             env=a.env, text=True, timeout=10)
            assert json.loads(listed)["daemon"] == identities[0]
        assert [view(window)["active_reference"] for window in (first, second)] == selections
        command(a, "connection", "edit", "local", "--use-hostname")
        for window in (first, second):
            wait_for(lambda: tab(identities[0], own[0]["tab"], window)["connection"] == local_name)
        command(a, "connection", "edit", "server", "--name", "renamed")
        for window in (first, second):
            wait_for(lambda: tab(identities[1], added["tab"], window)["connection"] == "renamed")
            assert tab(identities[1], added["tab"], window)["workspace"] == workspace
        assert [view(window)["active_reference"] for window in (first, second)] == selections
        assert b.rpc("status")["remote_clients"] == 2
        a.rpc("send", window=second, text="printf '\\nMULTI_WINDOW_REMOTE_EDIT\\n'", paste=True, enter=True)
        wait_for(lambda: "MULTI_WINDOW_REMOTE_EDIT" in b.rpc("read", pane=added["pane"])["text"].splitlines())
        command(a, "connection", "edit", "renamed", "--name", "server")
        for window in (first, second):
            wait_for(lambda: tab(identities[1], added["tab"], window)["connection"] == "server")
        a.rpc("detach", window=first)
        wait_for(lambda: len(a.rpc("windows")["windows"]) == 1)
        for f in (b, c):
            wait_for(lambda: f.rpc("status")["remote_clients"] == 1)
        a.rpc("detach", window=second)
        assert app.wait(timeout=10) == 0
        for f in (b, c):
            wait_for(lambda: f.rpc("status")["remote_clients"] == 0)
        assert "error" in a.rpc("focus", connection="server", pane=added["pane"], check=False)
        saved = json.loads((a.config / "clients/gtk/state.json").read_text())["desktop"]
        assert len({(t["reference"]["daemon"], t["reference"]["id"]) for t in saved["tabs"]}) == len(saved["tabs"])
        assert saved["active"] == dict(daemon=identities[1], id=added["tab"])
        app = a.start_app(cli, stdout=log, stderr=log)
        first = a.rpc("windows")["windows"][0]["id"]
        wait_for(lambda: view()["active_reference"] == saved["active"])
        assert tab(identities[1], added["tab"])["workspace"] == workspace

    with scenario("window creation rejects disconnected streams before starting remote shells"):
        before = b.rpc("_state")["execution"]
        remote_access = b.rpc("connection-list")["remote_access"]
        remote_access["enabled"] = False
        b.rpc("connection-edit", connection="local", remote_access=remote_access)
        wait_for(lambda: not tab(identities[1], added["tab"])["connected"])
        remote_access.update(enabled=True, listen_address=addresses[1])
        b.rpc("connection-edit", connection="local", remote_access=remote_access)
        # Control is reachable again, but this window still needs an explicit reconnect.
        for args in (("new",), ("split", "--pane", str(added["pane"]))):
            result = subprocess.run([str(cli), "--connection", "server", *args], env=a.env,
                                    text=True, capture_output=True, timeout=10)
            assert result.returncode != 0 and "Reconnect in Manage Connections" in result.stderr, result
        assert b.rpc("_state")["execution"] == before, "A rejected creation started a remote shell"
        assert not tab(identities[1], added["tab"])["connected"]

    with scenario("disconnection preserves organization and reopening reconnects the host"):
        original_times = {t["id"]: t["created_at"] for t in b.rpc("list")["tabs"]}
        b.rpc("stop")
        assert pb.wait(timeout=10) == 0
        wait_for(lambda: not tab(identities[1], added["tab"])["connected"])
        assert tab(identities[1], added["tab"])["branch"] == ""
        assert tab(identities[1], added["tab"])["connection"] == "server", "Connectivity changed the routable connection name"
        a.rpc("filter", value="all")
        local = a.rpc("new", name="Local while disconnected")
        assert local["daemon"] == identities[0]
        a.rpc("detach")
        assert app.wait(timeout=10) == 0
        saved = json.loads((a.config / "clients/gtk/state.json").read_text())["desktop"]
        refs = [(t["reference"]["daemon"], t["reference"]["id"]) for t in saved["tabs"]]
        assert len(set(refs)) == len(refs), "Offline tabs were duplicated by presentation save"
        before = time.monotonic()
        app = a.start_app(cli, stdout=log, stderr=log)
        assert time.monotonic() - before < 3, "Offline host delayed local startup"
        first = a.rpc("windows")["windows"][0]["id"]
        # Let the failed attach finish, then retry the same pinned identity.
        pb = start(b, log, addresses[1])
        a.rpc("detach")
        assert app.wait(timeout=10) == 0
        app = a.start_app(cli, stdout=log, stderr=log)
        first = a.rpc("windows")["windows"][0]["id"]
        wait_for(lambda: any(t["reference"] == dict(daemon=identities[1], id=added["tab"]) and t["connected"] for t in view()["tabs"]))
        assert tab(identities[1], added["tab"])["workspace"] == workspace
        assert {t["id"]: t["created_at"] for t in b.rpc("list")["tabs"]} == original_times
        timestamps = [t["created_at"] for t in view()["tabs"]]
        assert timestamps == sorted(timestamps, reverse=True), "Reconnecting must preserve creation order"

    with scenario("an enabled local daemon restarts while GTK and remote jobs keep running"):
        marker = c.path / "job-pid"
        c.rpc("send", pane=own[2]["pane"], text="sleep 120 & echo $! > " + shlex.quote(str(marker)), paste=True, enter=True)
        wait_for(lambda: marker.is_file())
        pid = int(marker.read_text())
        local_pid = a.rpc("status")["pid"]
        a.rpc("stop")
        assert pa.wait(timeout=10) == 0
        assert app.poll() is None
        wait_for(lambda: a.rpc("status")["pid"] != local_pid)
        wait_for(lambda: all(t["connected"] for t in view()["tabs"]
                             if t["reference"]["daemon"] == identities[0]))
        a.rpc("detach")
        assert app.wait(timeout=10) == 0
        assert pc.poll() is None and pb.poll() is None
        assert Path(f"/proc/{pid}").exists()
        for f in (b, c):
            wait_for(lambda: f.rpc("status")["remote_clients"] == 0)

print("PASS: native tabs across connections, filters, ownership, routing, reconnect, and local Quit")
