#!/usr/bin/env python3
"""Check frontend request fairness and measure bounded floods and UTF-8 pastes."""
import argparse
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import shlex
import socket
import struct
import subprocess
import threading
import time

from cargo_test_support import cargo_binary
from test_support import AppFixture, scenario, wait_for


def wire(method, **params):
    return (json.dumps(dict(method=method, **params), ensure_ascii=False,
                       separators=(",", ":")) + "\n").encode()


class Client:
    def __init__(self, fixture, pane):
        self.pane = pane
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(6)
        self.socket.connect(fixture.env["GMUX_SOCKET"] + ".stream")
        self.send("attach", version=7)

    def send(self, method, **params):
        self.socket.sendall(wire(method, pane=self.pane, **params))

    def exact(self, size):
        data = bytearray()
        while len(data) < size:
            part = self.socket.recv(size - len(data))
            assert part, "Frontend disconnected"
            data.extend(part)
        return data

    def until(self, predicate):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            size, = struct.unpack("<I", self.exact(4))
            assert 4 <= size <= 16 * 1024 * 1024
            body = self.exact(size)
            header_size, = struct.unpack("<I", body[:4])
            header = json.loads(body[4:4 + header_size])
            cells = body[4 + header_size:]
            text = b"".join(cells[i:i + 64].split(b"\0", 1)[0]
                            for i in range(0, len(cells), 71)).decode()
            if predicate(header, text):
                return header
        raise AssertionError("Frontend did not receive the expected reply")

    def close(self):
        self.socket.close()


def memory(pid):
    fields = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines()
                  if ":" in line)
    return {name: int(fields[name].split()[0]) for name in ("VmRSS", "VmHWM")}


@contextmanager
def flood(client, payload):
    """Write for at most three seconds or 16 MiB, with bounded cleanup."""
    stop, started = threading.Event(), threading.Event()
    sent, errors = [], []

    def write():
        deadline = time.monotonic() + 3
        total = 0
        started.set()
        try:
            while (not stop.is_set() and time.monotonic() < deadline
                   and total + len(payload) <= 16 * 1024 * 1024):
                client.socket.sendall(payload)
                total += len(payload)
        except OSError as error:
            errors.append(error)
        sent.append(total)

    writer = threading.Thread(target=write)
    writer.start()
    assert started.wait(1)
    try:
        yield writer
    finally:
        stop.set()
        writer.join(7)
        assert not writer.is_alive(), "Flood writer did not stop"
        assert not errors, errors
        assert sent[0] > 0, "Flood sent no commands"


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path)
parser.add_argument("--measure", action="store_true", help="Report baseline timings without latency assertions")
parser.add_argument("--report", type=Path)
args = parser.parse_args()
binary = args.binary or cargo_binary("gmux-daemon")
results = []

for kind in ("valid-input-scroll", "empty-lines-input"):
    with scenario(kind), AppFixture(dict(restore=False), prefix="gmux-frontend-input-") as fixture:
        daemon = fixture.start(binary, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: fixture.rpc("daemon-info"))
        pane = fixture.rpc("new")["pane"]
        other = fixture.rpc("split", pane=pane)["pane"]
        noisy, observer = Client(fixture, pane), Client(fixture, other)
        try:
            observer.send("subscribe", panes=[other])
            observer.until(lambda h, text: h["meta"]["id"] == other and h["frame"])
            # A larger, supported viewport makes repeated snapshot work visible.
            noisy.send("subscribe", panes=[pane])
            noisy.send("resize", cols=160, rows=50)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [160, 50])
            command = wire("input", pane=pane, bytes=[0])
            if kind == "valid-input-scroll":
                payload = (command + wire("scroll", pane=pane)) * 1024
            else:
                payload = b"\n" * 65536 + command
            before = memory(daemon.pid)
            controls, echoes = [], []
            with flood(noisy, payload) as writer:
                for index in range(6):
                    if not writer.is_alive():
                        break
                    start = time.monotonic()
                    fixture.rpc("daemon-info")
                    controls.append(time.monotonic() - start)
                    marker = f"FAIR_INPUT_{index}"
                    start = time.monotonic()
                    observer.send("input", bytes=list(f"printf '{marker}\\n'\n".encode()))
                    observer.until(lambda h, text: h["meta"]["id"] == other and marker in text)
                    echoes.append(time.monotonic() - start)
            after = memory(daemon.pid)
            result = dict(workload=kind, control_ms=[n * 1000 for n in controls],
                          other_pane_ms=[n * 1000 for n in echoes], before_kib=before, after_kib=after)
            results.append(result)
            print(json.dumps(result), flush=True)
            assert controls and echoes, "Flood ended before responsiveness probes"
            if not args.measure:
                assert max(controls + echoes) < .2, result
        finally:
            noisy.close()
            observer.close()

with scenario("buffered attach and fragmented 4 MiB UTF-8 paste"), AppFixture(dict(restore=False)) as fixture:
    fixture.start(binary, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc("daemon-info"))
    pane = fixture.rpc("new")["pane"]
    client = None
    try:
        # A new, unattached connection has no metadata deadline to rescue a lost wake.
        with socket.socket(socket.AF_UNIX) as delayed:
            delayed.settimeout(3)
            delayed.connect(fixture.env["GMUX_SOCKET"] + ".stream")
            delayed.sendall(wire("focused", active=False) * 128 + wire("attach", version=7))
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 1)
            # State output proves Attach ran even when it was left in a read buffer.
            assert delayed.recv(4), "Buffered Attach did not run"

        with socket.socket(socket.AF_UNIX) as incompatible:
            incompatible.settimeout(3)
            incompatible.connect(fixture.env["GMUX_SOCKET"] + ".stream")
            incompatible.sendall(wire("focused", active=False) * 128 + wire("attach", version=0))
            assert incompatible.recv(4) == b"", "Incompatible buffered Attach stayed connected"

        client = Client(fixture, pane)
        text = "é界🙂" * (4 * 1024 * 1024 // 9)
        raw = text.encode()
        digest = hashlib.sha256(raw).hexdigest()
        elapsed = []
        program = fixture.path / "consume.py"
        for index in range(10):
            output = fixture.path / f"digest-{index}"
            program.write_text(
                "import hashlib, os, tty\n"
                "tty.setraw(0)\n"
                f"remaining = {len(raw)}\n"
                "digest = hashlib.sha256()\n"
                f"os.write(1, b'PASTE_READY_{index}')\n"
                "while remaining:\n"
                "    data = os.read(0, min(remaining, 65536))\n"
                "    digest.update(data)\n"
                "    remaining -= len(data)\n"
                f"open({str(output) + '.tmp'!r}, 'w').write(digest.hexdigest())\n"
                f"os.rename({str(output) + '.tmp'!r}, {str(output)!r})\n")
            fixture.rpc("send", pane=pane, text=f"python3 {shlex.quote(str(program))}; stty sane -echo",
                        paste=True, enter=True)
            wait_for(lambda: f"PASTE_READY_{index}" in fixture.rpc("read", pane=pane)["text"])
            start = time.monotonic()
            client.send("paste-begin", size=len(raw))
            for offset in range(0, len(text), 4500):
                packet = wire("paste-data", pane=pane, text=text[offset:offset + 4500])
                split = packet.index("é".encode()) + 1
                client.socket.sendall(packet[:split])
                client.socket.sendall(packet[split:])
            client.send("paste-end")
            deadline = time.monotonic() + 6
            while not output.exists() and time.monotonic() < deadline:
                time.sleep(.001)
            assert output.exists(), "Paste did not reach the PTY"
            assert output.read_text() == digest, "Fragmented UTF-8 paste changed bytes or order"
            elapsed.append((time.monotonic() - start) * 1000)
        results.append(dict(workload="4-mib-utf8-paste", elapsed_ms=elapsed))
        print(json.dumps(results[-1]), flush=True)
    finally:
        if client is not None:
            client.close()

if args.report:
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(results, indent=2) + "\n")
print("PASS: bounded floods, control and independent pane responsiveness, buffered Attach, and ordered UTF-8 paste")
