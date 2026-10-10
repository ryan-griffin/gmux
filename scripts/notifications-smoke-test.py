#!/usr/bin/env python3
"""Check unread alerts across reconnects and bounded CLI/OSC notification queues.

Run GTK_A11Y=none dbus-run-session -- python3 scripts/notifications-smoke-test.py.
Uses private settings, sockets, and shells.
"""
import json
from functools import partial
from test_support import rpc as shared_rpc, wait_for as shared_wait_for
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import socket
import struct
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

from daemon_test_support import stop_daemon

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


wait_for = partial(shared_wait_for, exceptions=(OSError, ValueError, KeyError))


with tempfile.TemporaryDirectory(prefix="gmux-notifications-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(restore=True, desktop_notifications=False)))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"), GMUX_SOCKET=str(temp / "socket"),
               SHELL=str(shell), GTK_A11Y="none")
    app = None
    stream = None
    with (artifacts / "notifications-smoke.log").open("w") as log:
        def rpc(method, **params):
            return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

        def tab(identifier):
            return next(tab for tab in rpc("list")["tabs"] if tab["id"] == identifier)

        def start():
            global app
            app = subprocess.Popen([str(cargo_binary("gmux"))], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            wait_for(lambda: rpc("renderer-info")["gtk"])

        def detach():
            global app
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            wait_for(lambda: not rpc("daemon-info")["attached"])

        def send(pane, text):
            rpc("send", pane=pane, text=text, paste=True, enter=True)

        def attach_stream():
            global stream
            stream = socket.socket(socket.AF_UNIX)
            stream.settimeout(6)
            stream.connect(env["GMUX_SOCKET"] + ".stream")
            command("attach", version=7)

        def command(method, **params):
            stream.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())

        def exact(size):
            result = bytearray()
            while len(result) < size:
                part = stream.recv(size - len(result))
                assert part
                result.extend(part)
            return result

        def packet(pane):
            while True:
                size, = struct.unpack("<I", exact(4))
                body = exact(size)
                header_size, = struct.unpack("<I", body[:4])
                header = json.loads(body[4:4 + header_size])
                if header["meta"]["id"] == pane:
                    return header

        try:
            start()
            background = rpc("new", name="Background", directory=str(temp))
            foreground = rpc("new", name="Foreground", directory=str(temp))
            with ThreadPoolExecutor(max_workers=24) as requests:
                states = list(requests.map(lambda _: rpc("list"), range(48)))
                assert all(len(state["tabs"]) == 2 for state in states), "Queued GUI requests must all receive replies"
            send(background["pane"], "printf '\\033]9;UNREAD_OSC\\007'")
            wait_for(lambda: tab(background["tab"])["unread"])
            daemon_pid = rpc("daemon-info")["pid"]
            detach()
            assert tab(background["tab"])["unread"]
            start()
            assert rpc("daemon-info")["pid"] == daemon_pid
            assert tab(background["tab"])["unread"], "An unread alert must survive window reopening"
            rpc("focus", pane=background["pane"])
            wait_for(lambda: rpc("_metadata", pane=background["pane"])["meta"]["unread"] == 0)
            assert not tab(background["tab"])["unread"]
            rpc("focus", pane=foreground["pane"])
            detach()
            assert not tab(background["tab"])["unread"], "The GUI must acknowledge alerts in the daemon"
            start()
            assert not tab(background["tab"])["unread"]
            rpc("notify", pane=background["pane"], message="DIRECT_ALERT")
            wait_for(lambda: tab(background["tab"])["unread"])
            detach()
            start()
            assert tab(background["tab"])["unread"], "CLI and OSC alerts must share unread state"
            detach()

            queued = background["pane"]
            for index in range(100):
                rpc("notify", pane=queued, message=f"cli-{index}")
            attach_stream()
            header = packet(queued)
            assert header["notifications"] == [f"cli-{i}" for i in range(36, 100)], header
            sequence = header["meta"]["unread"]
            rpc("notify", pane=queued, message="NEWER_ALERT")
            command("mark-read", pane=queued, through=sequence)
            newer = wait_for(lambda: (value := rpc("_metadata", pane=queued)["meta"]["unread"]) > sequence and value)
            time.sleep(0.1)
            assert rpc("_metadata", pane=queued)["meta"]["unread"] == newer, "A stale acknowledgement must preserve a new alert"
            command("mark-read", pane=queued, through=newer)
            wait_for(lambda: rpc("_metadata", pane=queued)["meta"]["unread"] == 0)
            stream.close()
            stream = None
            wait_for(lambda: not rpc("daemon-info")["attached"])

            previous = newer
            send(queued, "for i in $(seq 0 99); do printf '\\033]9;osc-%s\\007' \"$i\"; done")
            wait_for(lambda: rpc("_metadata", pane=queued)["meta"]["unread"] >= previous + 100)
            attach_stream()
            header = packet(queued)
            assert header["notifications"] == [f"osc-{i}" for i in range(36, 100)], header
            stream.close()
            stream = None
            wait_for(lambda: not rpc("daemon-info")["attached"])
            start()
            assert tab(background["tab"])["unread"], "Unread state must survive delivery of queued alerts"
            print("PASS: unread OSC/CLI alerts survive reconnect, GUI reads clear daemon flags, stale acknowledgements preserve newer alerts, both queues retain the latest 64 messages, and queued GUI requests drain without a polling timer")
        finally:
            if stream:
                stream.close()
            if app and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
