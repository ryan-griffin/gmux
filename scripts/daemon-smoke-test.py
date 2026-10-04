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
import select
import shlex
import socket
import struct
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--native", action="store_true")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(shared_wait_for, exceptions=(OSError, ValueError, KeyError))


with tempfile.TemporaryDirectory(prefix="gmux-daemon-") as directory:
    temp = Path(directory)
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, GMUX_SOCKET=str(temp / "socket"),
               XDG_CONFIG_HOME=str(temp / "config"), SHELL=str(shell))
    log = (artifacts / "daemon-smoke.log").open("w")
    daemon = None
    app = None
    stream = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def start_daemon():
        # Prove the backend has no display requirement even during native checks.
        process = subprocess.Popen([str(root / "target/debug/gmux-daemon")],
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

    try:
        daemon = start_daemon()
        duplicate = subprocess.Popen([str(root / "target/debug/gmux-daemon")], env=env, stdout=log, stderr=log)
        assert duplicate.wait(timeout=5) == 0, "Concurrent launch must leave the existing daemon alive"
        assert rpc("daemon-info")["pid"] == daemon.pid
        first = rpc("new", directory=str(root))
        pane = first["pane"]
        workspace = rpc("workspace-new", name="Background project", directory=str(root))["workspace"]
        empty_workspace = rpc("workspace-new", name="Empty project", directory=str(root))["workspace"]
        rpc("move", tab=first["tab"], workspace=workspace)
        rpc("focus", workspace=empty_workspace)
        assert rpc("list")["active_tab"] is None, "An empty workspace must have no active pane"
        rpc("focus", workspace=workspace)
        assert rpc("list")["active_tab"] == first["tab"]
        wait_for(lambda: rpc("list")["tabs"][0]["panes"][0]["title"])
        time.sleep(0.1)
        send(pane, "printf '\\033[2J\\033[HHEADLESS_READY'")
        wait_for(lambda: "HEADLESS_READY" in rpc("read", pane=pane)["text"])
        assert rpc("daemon-info")["snapshots"] == 0, "Detached output must not extract viewports"
        pid = rpc("daemon-info")["pid"]
        before = ticks(pid)
        time.sleep(1.1)
        idle_ticks = ticks(pid) - before
        assert idle_ticks <= 2, f"Idle daemon used {idle_ticks} scheduler ticks"

        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(5)
        stream.connect(str(temp / "socket.stream"))
        command("attach", version=6)
        command("subscribe", panes=[pane])
        header, cells = screen(pane)
        assert header["rows"] == list(range(header["frame"]["rows"])), header
        assert b"H" in cells
        while select.select([stream], [], [], 0.1)[0]:
            packet()
        command("input", pane=pane, bytes=list(b"printf '\\033[3;1HROW_DELTA'\r"))
        header, cells = screen(pane, lambda header, cells: b"R" in cells)
        assert header["rows"] == [2], f"Expected one changed row, received {header['rows']}"

        hidden = rpc("split", pane=pane)["pane"]
        count = rpc("daemon-info")["snapshots"]
        send(hidden, "for i in $(seq 1 300); do printf 'hidden %s\\n' \"$i\"; done")
        wait_for(lambda: "hidden 300" in rpc("read", pane=hidden)["text"])
        time.sleep(0.1)
        assert rpc("daemon-info")["snapshots"] == count, "Hidden output must not extract a render snapshot"
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
        wait_for(lambda: not rpc("daemon-info")["attached"])

        # Raw terminal status query must receive a reply without a frontend.
        reply_file = temp / "reply"
        program = temp / "query.py"
        program.write_text("import os, tty, select\ntty.setraw(0)\nos.write(1,b'\\x1b[6n')\nassert select.select([0],[],[],3)[0]\nreply=os.read(0,128)\nopen(" + repr(str(reply_file)) + ", 'wb').write(reply)\n")
        send(pane, "python3 " + shlex.quote(str(program)) + "; stty sane -echo")
        wait_for(reply_file.exists)
        assert reply_file.read_bytes().startswith(b"\x1b[") and reply_file.read_bytes().endswith(b"R")

        if args.native:
            def start_app():
                process = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
                wait_for(lambda: (temp / "socket.ui").exists() and rpc("daemon-info")["attached"])
                wait_for(lambda: rpc("renderer-info")["gtk"])
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
            wait_for(lambda: not rpc("daemon-info")["attached"])
            wait_for(progress.exists)
            assert rpc("daemon-info")["pid"] == pid
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
            wait_for(lambda: not rpc("daemon-info")["attached"])
            send(pane, "printf 'AFTER_FRONTEND_CRASH\\n'")
            wait_for(lambda: "AFTER_FRONTEND_CRASH" in rpc("read", pane=pane)["text"])
            app = start_app()
            running_pid = temp / "running.pid"
            send(hidden, f"sleep 30 & echo $! > {shlex.quote(str(running_pid))}; fg")
            wait_for(running_pid.exists)
            command_pid = running_pid.read_text().strip()
            rpc("quit")
            assert daemon.wait(timeout=8) == 0
            daemon = None
            assert app.wait(timeout=8) == 0
            app = None
            wait_for(lambda: not Path(f"/proc/{command_pid}").exists())
            wait_for(lambda: not Path(f"/proc/{original_pid.strip()}").exists())
        else:
            rpc("quit")
            assert daemon.wait(timeout=8) == 0
            daemon = None
        assert not (temp / "socket").exists() and not (temp / "socket.stream").exists()

        # Stopping the backend preserves the layout, then starts fresh shells.
        daemon = start_daemon()
        assert [p["id"] for p in rpc("list")["tabs"][0]["panes"]] == [pane, hidden]
        assert "HEADLESS_READY" not in rpc("read", pane=pane)["text"]
        daemon.terminate()
        assert daemon.wait(timeout=8) == 0
        daemon = None
        assert not (temp / "socket").exists(), "SIGTERM must clean up the socket"
        print(f"PASS: headless PTYs, background workspaces and focus, {idle_ticks} idle CPU ticks, one-row packets, hidden-pane parsing without snapshots, subscription resync, retained scrollback, terminal replies, clean Quit/SIGTERM, and layout restore" + (", native close/reopen with identical shell PID, continued commands, frontend crash recovery, and daemon Quit closing the frontend" if args.native else ""))
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
