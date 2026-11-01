#!/usr/bin/env python3
"""Measure native multi-window resource use and RPC-to-view latency.

Run with GMUX_TEST_BINARIES set to the artifact map from cargo_build.py, or let
cargo_test_support build the binaries. Results go to --output. Slow raw stream
consumers exercise daemon buffering independently of the GTK client queues.
"""
import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import shlex
import socket
import statistics
import subprocess
import time

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, wait_for
from wayland_test_support import ensure_wayland_session

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, default=Path("artifacts/resource-budget-native.json"))
args = parser.parse_args()
ensure_wayland_session()
args.output.parent.mkdir(parents=True, exist_ok=True)


def resources(pid):
    status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines())
    return {key: int(status[key].split()[0]) for key in ("VmRSS", "VmHWM", "Threads")}


def summary(values):
    return dict(median_ms=statistics.median(values), max_ms=max(values), samples=len(values))


with ExitStack() as stack:
    log = stack.enter_context(args.output.with_suffix(".log").open("w"))
    fixtures = [stack.enter_context(AppFixture(dict(desktop_notifications=False), env={"GTK_A11Y": "none"})) for _ in range(3)]
    receiver, *servers = fixtures
    daemon = cargo_binary("gmux")
    for index, server in enumerate(servers):
        subprocess.run([str(daemon), "connection", "edit", "local", "--remote-access", "true",
                        "--listen-address", "127.0.0.1:0", "--auth", "none"], env=server.env, check=True, stdout=log)
        process = server.start(daemon, "serve", stdout=log, stderr=log)
        wait_for(lambda: server.rpc("status")["pid"] == process.pid)
        receiver.rpc("connection-add", name=f"remote{index}", address=server.rpc("status")["bound_address"], authentication=dict(mode="none"))
    app = receiver.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)
    before = resources(app.pid)
    first = receiver.rpc("windows")["windows"][0]["id"]
    tabs = []
    for count in (8, 4, 4):
        created = receiver.rpc("new", window=first, connection="local", name="local")
        panes = [created["pane"]]
        for _ in range(count - 1):
            split = receiver.rpc("split", window=first, connection="local", pane=panes[-1])
            panes.append(split["pane"])
        tabs.append(panes)
    for index in range(2):
        receiver.rpc("new", window=first, connection=f"remote{index}", name=f"remote{index}")
    windows = [first] + [receiver.rpc("new-window")["window"] for _ in range(3)]
    for window in windows:
        receiver.rpc("focus", window=window, connection="local", pane=tabs[0][0])
    # Fill history in all 16 local panes, including tabs hidden in every window.
    code = "import os;os.write(1,(''.join(str(i)+' '+('x'*180)+'\\n' for i in range(3000))).encode())"
    for panes in tabs:
        for pane in panes:
            receiver.rpc("send", pane=pane, text="python3 -c " + shlex.quote(code), paste=True, enter=True)
    time.sleep(2)
    slow = [Client(fixture, fixture.rpc("list", client=False)["tabs"][0]["panes"][0]["id"]) for fixture in fixtures]
    for client in slow:
        client.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        stack.callback(client.socket.close)
    # Burst output with subscribed consumers that stop reading after attach.
    for client, fixture in zip(slow, fixtures):
        fixture.rpc("send", client=False, pane=client.pane,
                    text="python3 -c " + shlex.quote("import os;os.write(1,b'pressure '*1000000+b'\\n')"), paste=True, enter=True)
    time.sleep(2)
    pressure = resources(app.pid)
    daemon_resources = [resources(f.rpc("status")["pid"]) for f in fixtures]
    cache_bytes = dict(visible=0, hidden=0)
    for window in windows:
        for tab_index, panes in enumerate(tabs):
            for pane in panes:
                info = receiver.rpc("renderer-info", window=window, pane=pane)["viewport"]
                cache_bytes["visible" if tab_index == 0 else "hidden"] += info["scroll_cache_bytes"]
    control, inputs, scroll = [], [], []
    for index in range(20):
        start = time.monotonic()
        receiver.rpc("list", window=first)
        control.append((time.monotonic() - start) * 1000)
        marker = f"budget_probe_{index}"
        start = time.monotonic()
        receiver.rpc("send", window=first, pane=tabs[0][0],
                     text=f"printf '\\n%s%s\\n' budget_probe_ {index}", paste=True, enter=True)
        wait_for(lambda: marker in receiver.rpc("read", window=first, pane=tabs[0][0])["text"].splitlines())
        inputs.append((time.monotonic() - start) * 1000)
        start = time.monotonic()
        receiver.rpc("action", window=first, name="previous-prompt")
        wait_for(lambda: not receiver.rpc("renderer-info", window=first, pane=tabs[0][0])["viewport"]["scroll_pending"])
        scroll.append((time.monotonic() - start) * 1000)
        receiver.rpc("action", window=first, name="next-prompt")
    for window in windows[1:]:
        receiver.rpc("detach", window=window)
    time.sleep(.5)
    result = dict(windows=4, daemons=3, streams=12, panes=18, views=72, slow_consumers=3,
                  before=before, pressure=pressure, after_close=resources(app.pid), daemon_pressure=daemon_resources,
                  cache_bytes=cache_bytes, control=summary(control), input_to_view=summary(inputs),
                  scroll_ack=summary(scroll), latency_scope="RPC round trip and view observation; 25 ms polling, debug build")
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))
