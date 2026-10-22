#!/usr/bin/env python3
"""Check terminal clipboard events through local and remote frontend streams."""
import base64
import os
from pathlib import Path
import subprocess
import socket
import time

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty"))
check = artifacts / "clipboard-check"
subprocess.run(["cc", "-O2", "-std=c11", '-DGMUX_VERSION="test"', f"-I{prefix}/include",
                str(root / "scripts/clipboard-check.c"), f"-L{prefix}/lib", f"-Wl,-rpath,{prefix}/lib",
                "-lghostty-vt", "-o", str(check)], check=True)
subprocess.run([str(check)], check=True)


def osc(text, destination="c"):
    return f"\x1b]52;{destination};" + base64.b64encode(text.encode()).decode() + "\x1b\\"


with (artifacts / "clipboard-smoke.log").open("w") as log, AppFixture() as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    process = fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = ProgressProbe(fixture, pane)
    clients = []
    received = {}
    serial = 0

    def client(*, remote=False, capable=True):
        peer = Client(fixture, pane, address=address if remote else None)
        clients.append(peer)
        received[peer] = []
        if capable:
            peer.send("clipboard-writes", enabled=True)
        barrier(peer)
        return peer

    def collect(peer, predicate):
        def inspect(header):
            if header.get("copy") is not None:
                assert header["frame"] is None and not header["rows"], header
                received[peer].append(header["copy"])
            return predicate(header)
        return peer.until(inspect)

    def barrier(peer):
        marker = f"clipboard-barrier-{time.monotonic_ns()}"
        peer.send(marker)
        collect(peer, lambda h: marker in (h.get("error") or ""))

    def emit(data):
        global serial
        serial += 1
        title = f"CLIPBOARD_{serial}"
        probe.write(data + f"\x1b]2;{title}\x07")
        wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["title"] == title)

    def take(peer):
        barrier(peer)
        writes, received[peer] = received[peer], []
        return writes

    def expect(peer, text, destination="clipboard"):
        collect(peer, lambda h: h.get("copy") == dict(destination=destination, text=text))
        assert take(peer) == [dict(destination=destination, text=text)]

    try:
        with scenario("detached writes are discarded before a frontend attaches"):
            emit(osc("detached"))
            observer = client(capable=False)
            first = client()
            second = client(remote=True)
            assert not take(first) and not take(second) and not take(observer)
        with scenario("one eligible recipient follows focus then connection order, including remote frontends"):
            observer.send("focused", active=True)
            barrier(observer)
            emit(osc("stable 界🙂e\u0301\n"))
            expect(first, "stable 界🙂e\u0301\n")
            assert not take(second) and not take(observer)
            second.send("focused", active=True)
            barrier(second)
            probe.burst(["\x1b]5", "2;p;", base64.b64encode("primary 界🙂".encode()).decode(), "\x1b", "\\"], .01)
            expect(second, "primary 界🙂", "primary")
            assert not take(first) and not take(observer)
            emit(osc("", "c"))
            expect(second, None)
            emit(osc("unsupported", "s") + "\x1b]52;c;/w==\x07\x1b]52;c;YQBi\x07")
            assert not take(first) and not take(second)
            # A focused window viewing a different pane is ineligible.
            second.send("subscribe", panes=[])
            barrier(second)
            emit(osc("visible owner"))
            expect(first, "visible owner")
            assert not take(second)
        with scenario("explicit copies and terminal writes retain request order"):
            emit("\x1b[H\x1b[2Jselection-text")
            first.output("selection-text")
            for end in [8, 2]:
                first.send("select", start=[0, 0], end=[end, 0], serial=end)
                first.send("copy")
            barrier(first)
            assert [w["text"] for w in received[first]] == ["selection", "sel"], received[first]
            received[first].clear()
            emit(osc("after copy"))
            expect(first, "after copy")
        with scenario("decoded payload limit accepts the boundary and rejects the whole oversized write"):
            text = "x" * (4 * 1024 * 1024)
            began = time.monotonic()
            emit(osc(text))
            expect(first, text)
            print(f"4 MiB OSC 52 delivery: {time.monotonic() - began:.3f}s including fixture I/O", flush=True)
            emit(osc(text + "x"))
            assert not take(first)
            emit(osc("recovered"))
            expect(first, "recovered")
        with scenario("disconnect, detach, and reconnect never replay old writes"):
            first.close()
            clients.remove(first)
            emit(osc("nobody visible"))
            assert not take(second) and not take(observer)
            second.send("subscribe", panes=[pane])
            barrier(second)
            assert not take(second)
            emit(osc("remote reconnect"))
            expect(second, "remote reconnect")
            second.close()
            clients.remove(second)
            emit(osc("no remote"))
            second = client(remote=True)
            assert not take(second)
            emit(osc("new remote write"))
            expect(second, "new remote write")
        with scenario("a frontend that stops reading cannot stall its PTY or retain an unbounded queue"):
            for peer in list(clients):
                peer.close()
            clients.clear()
            slow = client()
            slow.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            before = int(Path(f"/proc/{process.pid}/status").read_text().split("VmRSS:")[1].split()[0])
            chunk = osc("x" * (1024 * 1024))
            emit(chunk * 48)
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 0)
            after = int(Path(f"/proc/{process.pid}/status").read_text().split("VmRSS:")[1].split()[0])
            peak = int(Path(f"/proc/{process.pid}/status").read_text().split("VmHWM:")[1].split()[0])
            print(f"Stopped reader RSS: before={before} KiB, after={after} KiB, peak={peak} KiB", flush=True)
            assert peak - before < 96 * 1024, (before, after, peak)
            slow.close()
            clients.remove(slow)
            replacement = client()
            assert not take(replacement)
            emit(osc("after slow reader"))
            expect(replacement, "after slow reader")
    finally:
        for peer in clients:
            peer.close()
print("PASS: local/remote OSC 52 routing, Unicode, clear, limits, ordered copy, transient disconnect and bounded backpressure")
