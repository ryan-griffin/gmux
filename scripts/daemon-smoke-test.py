#!/usr/bin/env python3
"""Check the headless daemon, incremental transport, and optional native reconnect.

Run python3 scripts/daemon-smoke-test.py for headless checks, or
GTK_A11Y=none dbus-run-session -- python3 scripts/daemon-smoke-test.py --native.
"""
import argparse
import json
from functools import partial
from test_support import rpc as shared_rpc, wait_for as shared_wait_for
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import select
import shlex
import socket
import struct
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--native", action="store_true")
parser.add_argument("--headless", action="store_true")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(shared_wait_for, exceptions=(OSError, ValueError, KeyError))


with tempfile.TemporaryDirectory(prefix="gmux-execution-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, GMUX_SOCKET=str(temp / "socket"),
               XDG_CONFIG_HOME=str(temp / "config"), SHELL=str(shell), HISTFILE=str(temp / "bash_history"),
               GTK_A11Y="none")
    if args.native:
        settings = temp / "config/gmux/clients/gtk/settings.json"
        settings.parent.mkdir(parents=True)
        settings.write_text(json.dumps(dict(local_daemon_enabled=True)))
    log = (artifacts / "daemon-smoke.log").open("w")
    daemon = None
    app = None
    stream = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def cli(*arguments, text=False):
        result = subprocess.check_output([str(cargo_binary("gmux")), *map(str, arguments)],
                                         env=dict(env, DISPLAY="", WAYLAND_DISPLAY=""), text=True)
        return result if text else json.loads(result)

    def start_daemon():
        # Prove the backend has no display requirement even during native checks.
        process = subprocess.Popen([str(cargo_binary("gmux")), "serve"],
                                   env=dict(env, DISPLAY="", WAYLAND_DISPLAY=""),
                                   stdout=log, stderr=log)
        wait_for(lambda: (temp / "socket.stream").exists())
        assert process.poll() is None
        return process

    def send(pane, text):
        return rpc("send", pane=pane, text=text, paste=True, enter=True)

    def ticks(pid):
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(fields[11]) + int(fields[12])

    def command(method, **params):
        stream.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())

    def exact(size):
        data = bytearray()
        while len(data) < size:
            part = stream.recv(size - len(data))
            assert part, "Unexpected stream disconnect"
            data.extend(part)
        return data

    def packet():
        size, = struct.unpack("<I", exact(4))
        assert size <= 16 * 1024 * 1024
        body = exact(size)
        header_size, = struct.unpack("<I", body[:4])
        header = json.loads(body[4:4 + header_size])
        cells = body[4 + header_size:]
        if header["frame"]:
            assert len(cells) == len(header["rows"]) * header["frame"]["cols"] * 71
        else:
            assert not cells
        return header, cells

    def screen(pane, predicate=lambda header, cells: True):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            header, cells = packet()
            if header["meta"]["id"] == pane and header["frame"] and predicate(header, cells):
                return header, cells
        raise AssertionError("No matching viewport packet")

    def copy_reply(predicate):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            header, _ = packet()
            if header["meta"]["id"] == pane and predicate(header):
                return header
        raise AssertionError("No matching copy reply")

    try:
        if args.headless:
            result = subprocess.run([str(cargo_binary("gmux"))],
                                    env=dict(env, DISPLAY="", WAYLAND_DISPLAY=""),
                                    capture_output=True, text=True, timeout=5)
            assert result.returncode == 0 and "Usage: gmux" in result.stdout, result
            assert not (temp / "socket").exists(), "Launching the headless CLI must not start a daemon"
        daemon = start_daemon()
        duplicate = subprocess.run([str(cargo_binary("gmux")), "serve"], env=env,
                                   capture_output=True, text=True, timeout=5)
        assert duplicate.returncode != 0 and "already running or starting" in duplicate.stderr, duplicate
        assert rpc("status")["pid"] == daemon.pid
        first = cli("new", "--directory", root)
        pane = first["pane"]
        assert cli("status")["pid"] == daemon.pid
        # A daemon command must keep its meaning even while a client socket exists.
        with socket.socket(socket.AF_UNIX) as client_socket:
            client_socket.bind(str(temp / "socket.ui"))
            client_socket.listen()
            client_socket.setblocking(False)
            cli("rename", "--tab", first["tab"], "Shared terminal")
            assert cli("list")["tabs"][0]["name"] == "Shared terminal"
            cli("reset-name", "--tab", first["tab"])
            disposable = cli("new", "--name", "Disposable")
            cli("close-tab", "--tab", disposable["tab"])
            assert len(cli("list")["tabs"]) == 1
            try:
                client_socket.accept()
            except BlockingIOError:
                pass
            else:
                raise AssertionError("The daemon CLI connected to a frontend socket")
        (temp / "socket.ui").unlink()
        wait_for(lambda: rpc("list")["tabs"][0]["panes"][0]["title"])
        time.sleep(0.1)
        cli("send", "--pane", pane, "printf '\\033[2J\\033[HHEADLESS_READY'", "--enter")
        wait_for(lambda: "HEADLESS_READY" in cli("read", "--pane", pane, text=True))
        assert rpc("status")["snapshots"] == 0, "Detached output must not extract viewports"
        pid = rpc("status")["pid"]
        before = ticks(pid)
        time.sleep(1.1)
        idle_ticks = ticks(pid) - before
        assert idle_ticks <= 2, f"Idle daemon used {idle_ticks} scheduler ticks"

        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(5)
        stream.connect(str(temp / "socket.stream"))
        command("attach", version=22, daemon=rpc("_hello")["daemon"])
        command("subscribe", panes=[pane])
        header, cells = screen(pane)
        assert header["rows"] == list(range(header["frame"]["rows"])), header
        assert b"H" in cells
        # Selection commands and Copy share the frontend's ordered stream.
        # A missing selection must produce an error, never an empty copy packet.
        for selected in (False, True, False):
            if selected:
                command("select", pane=pane, start=[0, 0], end=[4, 0])
            else:
                command("clear-selection", pane=pane)
            command("copy", pane=pane)
            reply = copy_reply(lambda header: header["copy"] is not None or header["error"] is not None)
            if selected:
                assert reply["copy"] == "HEADL" and reply["error"] is None, reply
            else:
                assert reply["error"] == "Select terminal text to copy it" and reply["copy"] is None, reply
        # Selected whitespace is still a selection and may copy an empty string.
        command("select", pane=pane, start=[0, 1], end=[0, 1])
        command("copy", pane=pane)
        reply = copy_reply(lambda header: header["copy"] is not None or header["error"] is not None)
        assert reply["copy"] == "" and reply["error"] is None, reply
        command("clear-selection", pane=pane)
        while select.select([stream], [], [], 0.1)[0]:
            packet()
        command("input", pane=pane, bytes=list(b"printf '\\033[3;1HROW_DELTA'\r"))
        header, cells = screen(pane, lambda header, cells: b"R" in cells)
        assert header["rows"] == [2], f"Expected one changed row, received {header['rows']}"

        hidden = rpc("split", pane=pane)["pane"]
        count = rpc("status")["snapshots"]
        send(hidden, "for i in $(seq 1 300); do printf 'hidden %s\\n' \"$i\"; done")
        wait_for(lambda: "hidden 300" in rpc("read", pane=hidden)["text"])
        time.sleep(0.1)
        assert rpc("status")["snapshots"] == count, "Hidden output must not extract a render snapshot"
        command("subscribe", panes=[hidden])
        header, cells = screen(hidden)
        assert len(header["rows"]) == header["frame"]["rows"], "New subscription must resync every row"
        command("scroll", pane=hidden, delta=-10000, bottom=False)
        header, cells = screen(hidden, lambda header, cells: len(header["rows"]) > 0)
        text = b"".join(cells[i:i+64].split(b"\0", 1)[0] for i in range(0, len(cells), 71))
        assert b"hidden 1" in text, "Daemon must retain scrollback while hidden"
        assert header["frame"]["scrollback"]["offset"] == 0
        assert header["frame"]["scrollback"]["total"] > header["frame"]["rows"]
        command("scroll-to", pane=hidden, row=12, serial=42)
        header, cells = screen(hidden, lambda header, cells: header["scroll_serial"] == 42)
        assert header["frame"]["scrollback"]["offset"] == 12
        stream.close()
        stream = None
        wait_for(lambda: not rpc("status")["local_clients"])

        # Raw terminal status query must receive a reply without a frontend.
        reply_file = temp / "reply"
        program = temp / "query.py"
        program.write_text("import os, tty, select\ntty.setraw(0)\nos.write(1,b'\\x1b[6n')\nassert select.select([0],[],[],3)[0]\nreply=os.read(0,128)\nopen(" + repr(str(reply_file)) + ", 'wb').write(reply)\n")
        send(pane, "python3 " + shlex.quote(str(program)) + "; stty sane -echo")
        wait_for(reply_file.exists)
        assert reply_file.read_bytes().startswith(b"\x1b[") and reply_file.read_bytes().endswith(b"R")

        if args.native:
            def start_app():
                process = subprocess.Popen([str(cargo_binary("gmux-gtk"))], env=env, stdout=log, stderr=log)
                wait_for(lambda: (temp / "socket.ui").exists() and rpc("status")["local_clients"])
                wait_for(lambda: rpc("renderer-info")["gtk"])
                listed = rpc("list")
                identity = rpc("_hello")["daemon"]
                assert listed["daemon"] == identity
                assert listed["tabs"][0]["reference"] == dict(daemon=identity, id=first["tab"])
                assert listed["tabs"][0]["panes"][0]["reference"] == dict(daemon=identity, id=pane)
                return process

            app = start_app()
            shell_pid = temp / "shell.pid"
            progress = temp / "background-progress"
            send(pane, f"echo $$ > {shlex.quote(str(shell_pid))}; sleep 0.6; printf 'BACKGROUND_CONTINUED\\n'; echo done > {shlex.quote(str(progress))}")
            wait_for(shell_pid.exists)
            original_pid = shell_pid.read_text()
            # This calls the same close-request path as the titlebar button.
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            wait_for(lambda: not rpc("status")["local_clients"])
            wait_for(progress.exists)
            assert rpc("status")["pid"] == pid
            assert "BACKGROUND_CONTINUED" in rpc("read", pane=pane)["text"]
            app = start_app()
            assert [p["id"] for p in rpc("list")["tabs"][0]["panes"]] == [pane, hidden]
            send(pane, f"echo $$ > {shlex.quote(str(shell_pid))}")
            time.sleep(0.1)
            assert shell_pid.read_text() == original_pid, "Reopen must reconnect to the same shell"
            rpc("screenshot", path=str(artifacts / "daemon-reattached.png"))
            # A frontend crash also leaves sessions running.
            app.kill()
            app.wait(timeout=8)
            app = None
            wait_for(lambda: not rpc("status")["local_clients"])
            assert Path(env["GMUX_SOCKET"] + ".ui").exists(), "Exercise a stale frontend socket"
            cli("send", "printf 'AFTER_FRONTEND_CRASH\\n'", "--pane", pane, "--enter")
            wait_for(lambda: "AFTER_FRONTEND_CRASH" in rpc("read", pane=pane)["text"])
            app = start_app()
            running_pid = temp / "running.pid"
            send(hidden, f"sleep 30 & echo $! > {shlex.quote(str(running_pid))}; fg")
            wait_for(running_pid.exists)
            command_pid = running_pid.read_text().strip()
            cli("stop")
            assert daemon.wait(timeout=8) == 0
            daemon = None
            assert app.poll() is None, "Stopping execution must leave the client open"
            wait_for(lambda: rpc("renderer-info")["gtk"])
            wait_for(lambda: rpc("status")["pid"] != pid and rpc("status")["local_clients"])
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            cli("stop")
            wait_for(lambda: not Path(f"/proc/{command_pid}").exists())
            wait_for(lambda: not Path(f"/proc/{original_pid.strip()}").exists())
        else:
            cli("stop")
            assert daemon.wait(timeout=8) == 0
            daemon = None
        assert not (temp / "socket").exists() and not (temp / "socket.stream").exists()

        # Stopping the backend preserves the layout, then starts fresh shells.
        session_path = temp / "config/gmux/execution.json"
        saved = json.loads(session_path.read_text())
        identity = json.loads((temp / "config/gmux/daemon-id.json").read_text())
        saved["tabs"].append(dict(id=999997, created_at=1, name="Restored", custom_name=True,
            layout={"Pane":999998}, directories={"999998":str(root)}))
        saved_bytes = json.dumps(saved).encode()
        session_path.write_bytes(saved_bytes)
        daemon = start_daemon()
        restored = rpc("list")
        assert {t["id"]: t["created_at"] for t in restored["tabs"]} == {
            t["id"]: t["created_at"] for t in saved["tabs"]
        }, "Daemon restart must retain creation times"
        assert [p["id"] for p in restored["tabs"][0]["panes"]] == [pane, hidden]
        assert session_path.read_bytes() == saved_bytes, "Loading must not rewrite execution state"
        assert rpc("_hello")["daemon"] == identity, "Daemon identity must survive restart"
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(5)
        stream.connect(str(temp / "socket.stream"))
        command("attach", version=22, daemon=rpc("_hello")["daemon"])
        command("subscribe", panes=[hidden])
        screen(hidden)
        command("scroll-to", pane=hidden, row=0, serial=99)
        header, cells = screen(hidden, lambda header, cells: header["scroll_serial"] == 99)
        text = b"".join(cells[i:i+64].split(b"\0", 1)[0] for i in range(0, len(cells), 71))
        assert b"hidden 1" in text, "Daemon restart must restore primary-screen history"
        command("scroll", pane=hidden, delta=0, bottom=True)
        screen(hidden, lambda header, cells: header["frame"]["scrollback"]["offset"] > 0)
        stream.close()
        stream = None
        restored_split = rpc("split", pane=pane)["pane"]
        assert restored_split > 999998, "New pane IDs must exceed every restored tab and pane ID"
        rpc("close", pane=restored_split)
        rpc("send", pane=pane, text="printf 'RESTORED_FOCUS_READY\\n'", paste=True, enter=True)
        wait_for(lambda: "RESTORED_FOCUS_READY" in rpc("read", pane=pane)["text"])
        daemon.terminate()
        assert daemon.wait(timeout=8) == 0
        daemon = None
        assert not (temp / "socket").exists(), "SIGTERM must clean up the socket"

        print(f"PASS: headless PTYs, {idle_ticks} idle CPU ticks, one-row packets, ordered copy with absent and blank selections, hidden-pane parsing without snapshots, subscription resync, retained scrollback, terminal replies, clean Stop/SIGTERM, and layout restore" + (", native close/reopen with identical shell PID, continued commands, frontend crash recovery, and an open client after daemon Stop" if args.native else ""))
    finally:
        if stream:
            stream.close()
        if app and app.poll() is None:
            app.terminate()
            app.wait(timeout=8)
        if daemon and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=8)
        log.close()
