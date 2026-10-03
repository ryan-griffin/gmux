#!/usr/bin/env python3
"""Exercise the running GTK app, PTYs, socket API, and persisted session.

Run through dbus-run-session to isolate it from your normal Gmux instance.
Pass --native to test on your Wayland or X11 display and capture screenshots.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

from notification_server import NotificationServer

parser = argparse.ArgumentParser()
parser.add_argument("--native", action="store_true")
parser.add_argument("--binary", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
binary = args.binary.resolve() if args.binary else root / "target/debug/gmux"
assert binary.exists(), "Run cargo build first"
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            pass
        time.sleep(0.05)
    raise AssertionError("Timed out waiting for application state")


with tempfile.TemporaryDirectory(prefix="gmux-smoke-") as temp:
    temp = Path(temp)
    env = os.environ.copy()
    env.update(GMUX_SOCKET=str(temp / "gmux.sock"), XDG_CONFIG_HOME=str(temp / "config"), SHELL="/bin/sh")
    env.pop("G_MESSAGES_DEBUG", None)
    daemon = None
    if not args.native:
        env.update(GDK_BACKEND="broadway", BROADWAY_DISPLAY=":29")
        daemon = subprocess.Popen(["gtk4-broadwayd", "-a", "127.0.0.1", "-p", "8099", ":29"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.3)
    app = None
    logfile = (artifacts / ("smoke-native.log" if args.native else "smoke.log")).open("w")
    notifications = None

    def start(directory=None):
        command = [str(binary)]
        if directory:
            command.extend(["--directory", str(directory)])
        process = subprocess.Popen(command, cwd=root, env=env, stdout=logfile, stderr=logfile)
        wait_for(lambda: ((temp / "gmux.sock.ui").exists() and rpc("daemon-info")["attached"]) or process.poll() is not None)
        assert process.poll() is None, "Application failed to start; see artifacts/smoke.log"
        return process

    def rpc(method, **params):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(env["GMUX_SOCKET"])
            connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def state():
        result = rpc("list")
        assert "error" not in result, result
        return result

    def tab(identifier):
        return next(w for w in state()["tabs"] if w["id"] == identifier)

    def send(pane, text):
        result = rpc("send", pane=pane, text=text, paste=True, enter=True)
        assert result.get("ok"), result

    try:
        notifications = NotificationServer()
        app = start()
        assert state()["tabs"] == [], "First launch should stay empty"
        assert state()["active_tab"] is None
        assert rpc("renderer-info")["terminal"] is None
        assert "error" in rpc("read")
        for name in ["next-pane", "previous-pane", "rename", "split-right", "find"]:
            assert "error" in rpc("action", name=name), f"{name} should be unavailable when empty"
        assert rpc("action", name="new").get("ok")
        created = state()["tabs"][0]
        assert rpc("close", pane=created["active_pane"]).get("ok")
        assert state()["tabs"] == [], "Closing the last pane must not create another shell"
        app.terminate()
        app.wait(timeout=10)
        app = start(root)
        initial = state()
        first = initial["tabs"][0]
        assert len(initial["tabs"]) == 1 and len(first["panes"]) == 1
        pane = first["active_pane"]
        assert rpc("renderer-info", pane=pane)["terminal"] == "gsk"
        time.sleep(0.5)
        send(pane, "printf '\\033[32mGMUX_PTY_OK\\033[0m\\n'")
        wait_for(lambda: "GMUX_PTY_OK" in rpc("read", pane=pane).get("text", ""))
        right = rpc("split", pane=pane)["pane"]
        down = rpc("split", pane=right, down=True)["pane"]
        assert len(tab(first["id"])["panes"]) == 3
        send(right, "printf 'GMUX_SPLIT_OK\\n'")
        wait_for(lambda: "GMUX_SPLIT_OK" in rpc("read", pane=right).get("text", ""))
        project = temp / "project"
        project.mkdir()
        subprocess.run(["git", "init", "--quiet", "--initial-branch=feature/sidebar-layout", str(project)], check=True)
        subdir = project / "src"
        subdir.mkdir()
        new = rpc("new", name="Smoke project", directory=str(project))
        assert new.get("tab"), new
        project_pane = new["pane"]
        time.sleep(0.3)
        send(project_pane, f"cd '{subdir}'")
        wait_for(lambda: tab(new["tab"])["panes"][0]["directory"] == str(subdir))
        # Verify actual desktop delivery and notification click navigation.
        send(pane, "printf '\\033]777;notify;Test;GMUX_ALERT\\007'")
        alert = wait_for(lambda: next((n for n in notifications.received if n["body"] == "GMUX_ALERT"), None))
        assert alert["summary"] == tab(first["id"])["name"], alert
        assert tab(first["id"])["unread"], "A background alert should mark its sidebar item"
        assert not tab(new["tab"])["unread"], "Other sidebar items should stay unmarked"
        if args.native:
            # Allow the background Git lookup to populate the compact metadata line.
            time.sleep(2.2)
            assert "error" not in rpc("screenshot", path=str(artifacts / "sidebar-unread-native.png"))
        notifications.click(alert)
        wait_for(lambda: state()["active_tab"] == first["id"] and tab(first["id"])["active_pane"] == pane)
        assert not tab(first["id"])["unread"], "Clicking an alert should clear its unread indicator"
        # Desktop delivery tests must use an unattended terminal.
        assert rpc("focus", pane=project_pane).get("ok")
        send(pane, "printf '\\033]9;GMUX_OSC9'; sleep 0.1; printf '\\007'")
        wait_for(lambda: any(n["body"] == "GMUX_OSC9" for n in notifications.received))
        assert rpc("notify", pane=pane, message="GMUX_DIRECT_ALERT").get("ok")
        assert rpc("notify", pane=pane, message="GMUX_SECOND_ALERT").get("ok")
        wait_for(lambda: any(n["body"] == "GMUX_SECOND_ALERT" for n in notifications.received))
        assert any(n["body"] == "GMUX_DIRECT_ALERT" for n in notifications.received)
        assert all(n["replaces"] == 0 for n in notifications.received), "Each alert should reach the desktop separately"
        assert not notifications.closed, "Focusing a pane must leave dismissal to the desktop"
        assert "error" in rpc("read", pane=999999)
        assert "error" in rpc("unknown")
        assert "error" in rpc("browser", url="about:blank")
        assert "error" in rpc("action", name="browser")
        assert "error" in rpc("new", directory=str(temp / "missing"))
        assert rpc("notify", pane=right, message="GMUX_RIGHT_ALERT").get("ok")
        assert rpc("notify", pane=down, message="GMUX_DOWN_ALERT").get("ok")
        assert rpc("focus", pane=pane).get("ok")
        marked = tab(first["id"])
        assert marked["unread"], "Focusing one pane must preserve alerts from its siblings"
        assert not next(p for p in marked["panes"] if p["id"] == pane)["unread"]
        assert rpc("close", pane=down).get("ok")
        assert len(tab(first["id"])["panes"]) == 2
        assert tab(first["id"])["unread"], "Closing one unread pane must preserve its sibling's indicator"
        assert rpc("focus", pane=right).get("ok")
        assert not tab(first["id"])["unread"], "Reading the last unread pane should clear the sidebar indicator"
        assert rpc("notify", pane=down, message="Closed pane").get("error")
        assert not tab(first["id"])["unread"]
        assert rpc("notify", pane=pane, message="GMUX_CLOSE_ALERT").get("ok")
        assert rpc("close", pane=pane).get("ok")
        assert not tab(first["id"])["unread"], "Closing the last unread pane should clear its indicator"
        send(right, "exit 7")
        wait_for(lambda: all(t["id"] != first["id"] for t in state()["tabs"]))
        assert state()["active_tab"] == new["tab"], "Exiting the last shell should select a remaining tab"
        assert "error" in rpc("send", pane=right, text="echo should fail")

        if args.native:
            def capture():
                result = rpc("screenshot", path=str(artifacts / "smoke-native.png"))
                return result if "error" not in result else None

            screenshot = wait_for(capture)
            assert "error" not in screenshot, screenshot

        expected = state()
        app.terminate()
        app.wait(timeout=10)
        assert app.returncode == 0
        assert (temp / "gmux.sock").exists(), "Closing the frontend must leave the daemon running"
        app = start()
        restored = state()
        assert len(restored["tabs"]) == 1
        for old, new_tab in zip(expected["tabs"], restored["tabs"]):
            assert old["id"] == new_tab["id"]
            if old["id"] == new["tab"]:
                assert new_tab["name"] == "Smoke project", "Explicit tab names should survive restore"
            assert old["layout"] == new_tab["layout"]
            assert [p["directory"] for p in old["panes"]] == [p["directory"] for p in new_tab["panes"]]
            old_panes = {p["id"]: p for p in old["panes"]}
            for pane in new_tab["panes"]:
                focused = new_tab["id"] == restored["active_tab"] and pane["id"] == new_tab["active_pane"]
                assert pane["unread"] == (old_panes[pane["id"]]["unread"] and not focused), "Reopening must preserve unread alerts in unfocused panes"
            assert not any(p["exited"] for p in new_tab["panes"]), "Reattached panes should keep live shells"
        for w in restored["tabs"]:
            for p in w["panes"]:
                assert rpc("close", pane=p["id"]).get("ok")
        assert state()["tabs"] == []
        assert state()["active_tab"] is None
        app.terminate()
        app.wait(timeout=10)
        # Turning notifications off should prevent desktop delivery after restore.
        settings_path = temp / "config/gmux/settings.json"
        settings_path.parent.mkdir(parents=True, exist_ok=True)
        settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
        settings["desktop_notifications"] = False
        settings_path.write_text(json.dumps(settings))
        app = start()
        assert state()["tabs"] == [], "An empty saved session should stay empty"
        assert state()["active_tab"] is None
        disabled = rpc("new", name="Disabled notifications", directory=str(root))
        assert rpc("new", name="Foreground", directory=str(root)).get("tab")
        count = len(notifications.received)
        assert rpc("notify", pane=disabled["pane"], message="GMUX_DISABLED_ALERT").get("ok")
        time.sleep(0.3)
        assert len(notifications.received) == count, "Disabled notifications must not reach the desktop"
        assert tab(disabled["tab"])["unread"], "Sidebar indicators should work with desktop notifications disabled"
        assert rpc("focus", pane=disabled["pane"]).get("ok")
        assert not tab(disabled["tab"])["unread"]
        print("PASS: empty launch, disabled terminal actions, closing the last tab, PTY output, splits, working directories, OSC and direct desktop notifications, notification click navigation, sidebar unread indicators across split panes, disabling notifications, focus, invalid requests, process exit, pane removal, clean shutdown, and populated and empty session restore" + (", native screenshot" if args.native else ""))
    finally:
        if app and app.poll() is None:
            app.terminate()
            try:
                app.wait(timeout=5)
            except subprocess.TimeoutExpired:
                app.kill()
        stop_daemon(env["GMUX_SOCKET"])
        if daemon:
            daemon.terminate()
            daemon.wait(timeout=5)
        if notifications:
            notifications.stop()
        logfile.close()
