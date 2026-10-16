#!/usr/bin/env python3
"""Exercise the saved local daemon toggle and automatic recovery alongside remotes."""
import fcntl
import json
import os
from pathlib import Path
import signal
import shutil
import shlex
import time

import gi
gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Atspi, Gdk

from cargo_test_support import cargo_binary
from daemon_test_support import stop_daemon
from test_support import AppFixture, AccessibilityFixture, assert_stays, create_tab_when_connected, descendants, scenario, wait_accessible, wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

ensure_wayland_session()
daemon, gtk = cargo_binary("gmux"), cargo_binary("gmux-gtk")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "local-execution-smoke.log").open("w") as log, \
        AppFixture(env={"GTK_A11Y": "atspi"}) as local, AppFixture() as server, \
        AccessibilityFixture(), WaylandInput() as native:
    # Exercise Settings::default, rather than setting the opt-out explicitly.
    local.settings_path.unlink()
    app = local.start(gtk, stdout=log, stderr=log)
    wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))

    def objects():
        return [obj for obj in descendants(Atspi.get_desktop(0))
                if obj.get_process_id() == app.pid and obj.get_state_set().contains(Atspi.StateType.SHOWING)]

    def find(name, role=None):
        return next((obj for obj in objects() if obj.get_name() == name
                     and (role is None or obj.get_role_name() == role)), None)

    def widget(name, role=None):
        return wait_accessible(lambda: find(name, role), message=f"Missing {name} ({role})")

    def key(symbol):
        native.key(symbol, True)
        native.key(symbol, False)

    def click(name, role=None):
        def ready():
            obj = find(name, role)
            return obj if obj and obj.get_state_set().contains(Atspi.StateType.SENSITIVE) else None

        obj = wait_accessible(ready, message=f"{name} did not become sensitive")
        action = obj.get_action_iface()
        if action and Atspi.Action.get_n_actions(action):
            assert Atspi.Action.do_action(action, 0), name
        else:
            for _ in range(24):
                obj.clear_cache()
                if obj.get_state_set().contains(Atspi.StateType.FOCUSED):
                    key(Gdk.KEY_space)
                    return
                key(Gdk.KEY_Tab)
                time.sleep(.04)
            raise AssertionError(f"Could not focus {name}")

    def manager_page():
        wait_accessible(lambda: not find("Local Connection", "dialog"))
        widget("Connections", "dialog")
        widget("Add Connection")

    def manager():
        if not find("Connections", "dialog"):
            local.rpc("action", name="manage-connections")
        manager_page()

    def editor():
        manager()
        click(local.rpc("connection-list")["local"]["name"], "list item")
        widget("Local Daemon", "switch")
        assert not find("Execution", "label")
        assert not find("Start Daemon", "button")
        assert not find("Stop Daemon…", "button")

    def close_manager():
        dialog = widget("Connections", "dialog")
        close = next(obj for obj in descendants(dialog)
                     if obj.get_name() == "Close" and obj.get_role_name() == "button"
                     and obj.get_state_set().contains(Atspi.StateType.SHOWING))
        assert Atspi.Action.do_action(close.get_action_iface(), 0)
        wait_accessible(lambda: not find("Connections", "dialog"))

    def save():
        click("Save", "button")
        manager_page()

    def saved_enabled():
        return local.settings_path.exists() and json.loads(local.settings_path.read_text())["local_daemon_enabled"]

    def no_execution_files():
        assert not Path(local.env["GMUX_SOCKET"]).exists()
        assert not (local.config / "daemon-id.json").exists()
        assert not (local.config / "execution.json").exists()

    def failed_preferences_save(enabled):
        wait_accessible(lambda: next((obj for obj in objects()
                        if obj.get_role_name() == "label"
                        and obj.get_name().startswith("Could not save preferences:")), None))
        widget("Local Connection", "dialog")
        assert widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.CHECKED) == enabled
        assert widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)

    with scenario("fresh GTK starts without a daemon and workspaces work without connections"):
        widget("No Connections", "label")
        no_execution_files()
        workspace = local.rpc("workspace-add", name="Offline project")["workspace"]
        assert local.rpc("list")["workspaces"][0]["id"] == workspace
        no_execution_files()
        before = Path(f"/proc/{app.pid}/stat").read_text().split()
        time.sleep(1)
        after = Path(f"/proc/{app.pid}/stat").read_text().split()
        status = Path(f"/proc/{app.pid}/status").read_text().splitlines()
        (artifacts / "local-execution-idle.json").write_text(json.dumps(dict(
            cpu_ticks=sum(map(int, after[13:15])) - sum(map(int, before[13:15])),
            memory=[line for line in status if line.startswith(("VmRSS:", "Threads:"))]), indent=2) + "\n")

    with scenario("local settings and remote access can be saved without starting execution"):
        editor()
        click("Allow Remote Connections", "switch")
        entry = widget("Listening Address", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "127.0.0.1:0")
        save()
        assert local.rpc("connection-list")["remote_access"]["enabled"]
        assert local.rpc("connection-list")["local"]["daemon"] is None
        no_execution_files()

    with scenario("Cancel discards the local toggle together with the other fields"):
        editor()
        original_name = local.rpc("connection-list")["local"]["name"]
        entry = widget("Name", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "unsaved-name")
        click("Local Daemon", "switch")
        assert widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.SENSITIVE)
        assert widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        no_execution_files()
        assert not saved_enabled()
        click("Cancel", "button")
        manager_page()
        assert local.rpc("connection-list")["local"]["name"] == original_name
        assert not saved_enabled()
        no_execution_files()
        editor()
        assert not widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.CHECKED)
        assert not widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        click("Local Daemon", "switch")
        click("Local Daemon", "switch")
        assert not widget("Save", "button").get_state_set().contains(Atspi.StateType.SENSITIVE)
        click("Cancel", "button")
        manager_page()

    with scenario("invalid connection settings do not enable or start the local daemon"):
        editor()
        entry = widget("Name", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "invalid/name")
        click("Local Daemon", "switch")
        click("Save", "button")
        widget("Connection name must contain 1 to 64 letters, digits, dots, hyphens, or underscores", "label")
        widget("Local Connection", "dialog")
        assert not saved_enabled()
        no_execution_files()
        assert local.rpc("connection-list")["local"]["name"] == original_name
        click("Cancel", "button")
        manager_page()

    settings_lock = local.settings_path.with_name(".settings.json.lock")
    with scenario("a preferences write failure preserves the draft and leaves the daemon disabled"):
        editor()
        click("Local Daemon", "switch")
        assert not saved_enabled()
        no_execution_files()
        before = local.settings_path.read_bytes() if local.settings_path.exists() else None
        settings_lock.unlink(missing_ok=True)
        settings_lock.mkdir()
        try:
            click("Save", "button")
            failed_preferences_save(True)
            assert not saved_enabled()
            assert (local.settings_path.read_bytes() if local.settings_path.exists() else None) == before
            no_execution_files()
            assert not local.rpc("connection-list")["running"]
        finally:
            settings_lock.rmdir()

    with scenario("Save waits for durable preferences while keeping the UI responsive"):
        with settings_lock.open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            click("Save", "button")
            widget("Saving…", "button")
            assert not saved_enabled()
            no_execution_files()
            # Another window can change shared preferences while the write waits.
            other_window = local.rpc("new-window")["window"]
            sidebar = local.rpc("list", window=other_window)["sidebar_visible"]
            local.rpc("action", window=other_window, name="sidebar")
            assert local.rpc("list", window=other_window)["sidebar_visible"] != sidebar
            widget("Saving…", "button")
            widget("Local Connection", "dialog")
            no_execution_files()
            fcntl.flock(lock, fcntl.LOCK_UN)
        manager_page()
        wait_for(lambda: local.rpc("status")["local_clients"])
        wait_for(saved_enabled)
        wait_for(lambda: json.loads(local.settings_path.read_text())["show_sidebar"] != sidebar)
        local.rpc("detach", window=other_window)
        wait_for(lambda: len(local.rpc("windows")["windows"]) == 1)
        local_id = local.rpc("_hello", client=False)["daemon"]
        original = local.rpc("new", name="Local job", workspace=workspace)
        server.rpc("connection-edit", connection="local", remote_access=dict(
            enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
        server.start(daemon, "serve", stdout=log, stderr=log)
        address = wait_for(lambda: server.rpc("status")["bound_address"])
        remote = server.rpc("new", name="Remote job")
        local.rpc("connection-add", name="server", address=address, authentication={"mode": "none"})
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)

    with scenario("stopped and crashed daemons restart automatically across every window"):
        close_manager()
        first_window = local.rpc("windows")["windows"][0]["id"]
        second_window = local.rpc("new-window")["window"]
        wait_for(lambda: local.rpc("status")["local_clients"] == 2)
        editor()
        click("Local Daemon", "switch")
        assert saved_enabled()
        for crash in (False, True):
            pid = local.rpc("status")["pid"]
            if crash:
                os.kill(pid, signal.SIGKILL)
            else:
                stop_daemon(local.env["GMUX_SOCKET"])
            wait_for(lambda: local.rpc("status")["pid"] != pid)
            wait_for(lambda: local.rpc("status")["local_clients"] == 2)
            assert local.rpc("_hello", client=False)["daemon"] == local_id
            for window in (first_window, second_window):
                wait_for(lambda: all(tab["connected"] for tab in local.rpc("list", window=window)["tabs"]))
                assert len(local.rpc("list", window=window)["tabs"]) == 2
            assert app.poll() is None
            assert server.rpc("list")["tabs"][0]["id"] == remote["tab"]
            assert saved_enabled()
            assert not widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.CHECKED)
        click("Cancel", "button")
        manager_page()
        close_manager()

    with scenario("canceling a stop confirmation preserves every draft and the saved daemon state"):
        editor()
        original_name = local.rpc("connection-list")["local"]["name"]
        entry = widget("Name", "text")
        assert Atspi.EditableText.set_text_contents(entry.get_editable_text_iface(), "unsaved-stop-name")
        click("Local Daemon", "switch")
        assert not find("Stop Daemon", "button")
        assert local.rpc("status")["local_clients"] == 2
        click("Save", "button")
        widget("Stop Daemon", "button")
        key(Gdk.KEY_Escape)
        widget("Save", "button")
        widget("Local Connection", "dialog")
        assert not widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.CHECKED)
        assert Atspi.Text.get_text(widget("Name", "text").get_text_iface(), 0, -1) == "unsaved-stop-name"
        assert local.rpc("connection-list")["local"]["name"] == original_name
        assert saved_enabled()
        assert local.rpc("status")["local_clients"] == 2
        assert Atspi.EditableText.set_text_contents(widget("Name", "text").get_editable_text_iface(), original_name)

    with scenario("a preferences write failure leaves the daemon enabled and recovery active"):
        before = local.settings_path.read_bytes()
        pid = local.rpc("status")["pid"]
        settings_lock.unlink()
        settings_lock.mkdir()
        try:
            click("Save", "button")
            click("Stop Daemon", "button")
            failed_preferences_save(False)
            assert saved_enabled()
            assert local.settings_path.read_bytes() == before
            assert local.rpc("status")["pid"] == pid
            assert local.rpc("status")["local_clients"] == 2
            os.kill(pid, signal.SIGKILL)
            wait_for(lambda: local.rpc("status")["pid"] != pid)
            wait_for(lambda: local.rpc("status")["local_clients"] == 2)
            assert saved_enabled()
        finally:
            settings_lock.rmdir()

    with scenario("retrying Save after repairing preferences stops the daemon"):
        click("Save", "button")
        click("Stop Daemon", "button")
        manager_page()
        wait_for(lambda: not Path(local.env["GMUX_SOCKET"]).exists())
        wait_for(lambda: not saved_enabled())
        for window in (first_window, second_window):
            wait_for(lambda: [tab["name"] for tab in local.rpc("list", window=window)["tabs"]] == ["Remote job"])
        assert_stays(lambda: not Path(local.env["GMUX_SOCKET"]).exists(), 2,
                     message="A disabled local daemon restarted")
        editor()
        click("Local Daemon", "switch")
        assert not saved_enabled()
        assert not Path(local.env["GMUX_SOCKET"]).exists()
        click("Cancel", "button")
        manager_page()
        assert not saved_enabled()
        editor()
        assert not widget("Local Daemon", "switch").get_state_set().contains(Atspi.StateType.CHECKED)
        click("Local Daemon", "switch")
        save()
        wait_for(lambda: local.rpc("status")["local_clients"] == 2)
        close_manager()
        local.rpc("detach", window=second_window)
        wait_for(lambda: len(local.rpc("windows")["windows"]) == 1)

    with scenario("the saved toggle restores the local connection on app launch"):
        assert local.rpc("_hello", client=False)["daemon"] == local_id
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)
        assert next(tab for tab in local.rpc("list")["tabs"]
                    if tab["reference"]["daemon"] == local_id)["workspace"] == workspace
        local.rpc("detach")
        app.wait(timeout=8)
        stop_daemon(local.env["GMUX_SOCKET"])
        app = local.start(gtk, stdout=log, stderr=log)
        wait_for(lambda: local.rpc("status")["local_clients"])
        assert local.rpc("_hello", client=False)["daemon"] == local_id
        wait_for(lambda: len(local.rpc("list")["tabs"]) == 2)

    with scenario("a disabled local connection stays off on new windows and later launches"):
        editor()
        click("Local Daemon", "switch")
        click("Save", "button")
        click("Stop Daemon", "button")
        wait_for(lambda: not Path(local.env["GMUX_SOCKET"]).exists())
        manager_page()
        close_manager()
        extra = local.rpc("new-window")["window"]
        wait_for(lambda: len(local.rpc("list", window=extra)["tabs"]) == 1)
        assert_stays(lambda: not Path(local.env["GMUX_SOCKET"]).exists(), 2,
                     message="Opening a window enabled the local daemon")
        local.rpc("detach", window=extra)
        wait_for(lambda: len(local.rpc("windows")["windows"]) == 1)
        local.rpc("detach")
        app.wait(timeout=8)
        local.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: local.rpc("status")["pid"])
        app = local.start(gtk, "--connection", "local", stdout=log, stderr=log)
        wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))
        wait_for(lambda: [tab["name"] for tab in local.rpc("list")["tabs"]] == ["Remote job"])
        assert local.rpc("status")["local_clients"] == 0
        assert local.rpc("_state")["daemon"] is None
        assert "error" in local.rpc("new", check=False)
        close_remote = local.start(gtk, "close-tab", "--connection", "server", "--tab", str(remote["tab"]), stdout=log, stderr=log)
        assert close_remote.wait(timeout=8) == 0
        wait_for(lambda: local.rpc("list")["tabs"] == [])
        server.rpc("stop")
        widget("Not Connected", "label")
        widget("Manage Connections…", "button")
        quit_client = local.start(gtk, "quit", stdout=log, stderr=log)
        assert quit_client.wait(timeout=8) == 0
        app.wait(timeout=8)
        assert local.rpc("status")["local_clients"] == 0

    with scenario("closing GTK cancels a configuration read from a stalled unused local daemon"):
        app = local.start(gtk, stdout=log, stderr=log)
        wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))
        pid = local.rpc("status")["pid"]
        os.kill(pid, signal.SIGSTOP)
        try:
            local.rpc("action", name="manage-connections")
            wait_for(lambda: any(task.joinpath("wchan").read_text().strip() == "unix_stream_read_generic"
                                 for task in Path(f"/proc/{app.pid}/task").iterdir()))
            started = time.monotonic()
            app.terminate()
            assert app.wait(timeout=2) == 0
            assert time.monotonic() - started < 2
        finally:
            os.kill(pid, signal.SIGCONT)

    with scenario("quitting GTK flushes preference changes made while Save is pending"):
        app = local.start(gtk, stdout=log, stderr=log)
        wait_for(lambda: local.rpc("renderer-info", check=False).get("gtk"))
        editor()
        click("Local Daemon", "switch")
        sidebar = local.rpc("list")["sidebar_visible"]
        with settings_lock.open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            click("Save", "button")
            widget("Saving…", "button")
            local.rpc("action", name="sidebar")
            assert local.rpc("list")["sidebar_visible"] != sidebar
            quit_client = local.start(gtk, "quit", stdout=log, stderr=log)
            assert quit_client.wait(timeout=8) == 0
            wait_for(lambda: not Path(local.env["GMUX_SOCKET"] + ".ui").exists())
            fcntl.flock(lock, fcntl.LOCK_UN)
        assert app.wait(timeout=8) == 0
        settings = json.loads(local.settings_path.read_text())
        assert settings["local_daemon_enabled"]
        assert settings["show_sidebar"] != sidebar


with scenario("closing the initiating window leaves recovery available to other windows"), \
        (artifacts / "local-recovery-window-smoke.log").open("w") as log, AppFixture() as retry:
    client = retry.path / "client/gmux-gtk"
    client.parent.mkdir()
    shutil.copy2(gtk, client)
    started = retry.path / "starting-pid"
    executable = client.with_name("gmux")
    executable.write_text("#!/bin/sh\necho $$ > " + shlex.quote(str(started)) + "\nexec sleep 60\n")
    executable.chmod(0o700)
    app = retry.start(client, stdout=log, stderr=log)
    wait_for(started.exists)
    pending_pid = int(started.read_text())
    try:
        first = retry.rpc("windows")["windows"][0]["id"]
        second = retry.rpc("new-window")["window"]
        executable.unlink()
        executable.symlink_to(daemon)
        retry.rpc("detach", window=first)
        wait_for(lambda: len(retry.rpc("windows")["windows"]) == 1)
        wait_for(lambda: retry.rpc("status")["local_clients"])
        create_tab_when_connected(retry.env["GMUX_SOCKET"], window=second, name="Recovered")
        retry.rpc("detach", window=second)
        assert app.wait(timeout=8) == 0
    finally:
        if Path(f"/proc/{pending_pid}").exists():
            os.kill(pending_pid, signal.SIGTERM)

with scenario("failed startup backs off and recovers when the daemon becomes available"), \
        (artifacts / "local-recovery-smoke.log").open("w") as log, AppFixture() as retry:
    client = retry.path / "client/gmux-gtk"
    client.parent.mkdir()
    shutil.copy2(gtk, client)
    attempts = retry.path / "attempts"
    executable = client.with_name("gmux")
    executable.write_text("#!/bin/sh\necho attempt >> " + shlex.quote(str(attempts)) + "\nexit 1\n")
    executable.chmod(0o700)
    app = retry.start(client, stdout=log, stderr=log)
    count = lambda: len(attempts.read_text().splitlines()) if attempts.exists() else 0
    wait_for(lambda: count() >= 3)
    assert count() == 3, "Failed startup retried without backoff"
    before = Path(f"/proc/{app.pid}/stat").read_text().split()
    assert_stays(lambda: count() == 3, 1, message="Failed startup retried too quickly")
    after = Path(f"/proc/{app.pid}/stat").read_text().split()
    (artifacts / "local-recovery-idle.json").write_text(json.dumps(dict(
        attempts=count(), cpu_ticks=sum(map(int, after[13:15])) - sum(map(int, before[13:15]))), indent=2) + "\n")
    executable.unlink()
    executable.symlink_to(daemon)
    wait_for(lambda: retry.rpc("status")["local_clients"])
    assert app.poll() is None
    retry.rpc("detach")
    assert app.wait(timeout=8) == 0

with scenario("a failing local daemon can be disabled without starting another process"), \
        (artifacts / "local-disable-failed-start-smoke.log").open("w") as log, \
        AppFixture(env={"GTK_A11Y": "atspi"}) as local, AccessibilityFixture(), WaylandInput() as native:
    client = local.path / "client/gmux-gtk"
    client.parent.mkdir()
    shutil.copy2(gtk, client)
    attempts = local.path / "attempts"
    executable = client.with_name("gmux")
    executable.write_text("#!/bin/sh\necho attempt >> " + shlex.quote(str(attempts)) + "\nexit 1\n")
    executable.chmod(0o700)
    app = local.start(client, stdout=log, stderr=log)
    count = lambda: len(attempts.read_text().splitlines()) if attempts.exists() else 0
    wait_for(lambda: count() >= 2)
    editor()
    click("Local Daemon", "switch")
    click("Save", "button")
    click("Stop Daemon", "button")
    manager_page()
    assert not saved_enabled()
    stopped_attempts = count()
    assert_stays(lambda: count() == stopped_attempts, 3,
                 message="Disabling the local daemon did not cancel startup retries")
    assert not Path(local.env["GMUX_SOCKET"]).exists()
    close_manager()
    local.rpc("detach")
    assert app.wait(timeout=8) == 0

print("PASS: the saved local daemon toggle, automatic restart, bounded retry, offline settings, and remotes")
