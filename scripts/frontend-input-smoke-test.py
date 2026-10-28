#!/usr/bin/env python3
"""Check frontend request fairness and measure bounded floods and UTF-8 pastes."""
import argparse
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import shlex
import socket
import statistics
import struct
import subprocess
import threading
import time

from cargo_test_support import cargo_binary
from stream_test_support import PROTOCOL_VERSION
from test_support import AppFixture, scenario, wait_for


def wire(method, **params):
    return (json.dumps(dict(method=method, **params), ensure_ascii=False,
                       separators=(",", ":")) + "\n").encode()


class Client:
    def __init__(self, fixture, pane):
        self.pane = pane
        self.reply_bytes = self.viewport_rows = self.cache_rows = 0
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(6)
        self.socket.connect(fixture.env["GMUX_SOCKET"] + ".stream")
        self.send("attach", version=PROTOCOL_VERSION, daemon=fixture.rpc("_hello")["daemon"])
        self.send("frame-interval", micros=args.frame_interval_us)

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
            self.reply_bytes += size + 4
            header_size, = struct.unpack("<I", body[:4])
            header = json.loads(body[4:4 + header_size])
            if header["meta"]["id"] == self.pane and header["frame"]:
                self.viewport_rows += len(header["rows"])
                self.cache_rows += len((header.get("cache") or {}).get("row_flags", []))
            cells = body[4 + header_size:]
            text = b"".join(cells[i:i + 64].split(b"\0", 1)[0]
                            for i in range(0, len(cells), 75)).decode()
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
parser.add_argument("--frame-interval-us", type=int, default=16000)
args = parser.parse_args()
if not 1000 <= args.frame_interval_us <= 1_000_000:
    parser.error("--frame-interval-us must be between 1000 and 1000000")
cadence = args.frame_interval_us / 1_000_000
binary = args.binary or cargo_binary("gmux")
results = []

with scenario("empty and metadata-only updates leave the viewport deadline available"), AppFixture(
        prefix="gmux-frame-deadline-") as fixture:
    worker = fixture.path / "probe.py"
    worker.write_text('''
import os, time, tty
tty.setraw(0)
os.write(1, b"\\x1b[?1049h\\x1b[?25l\\x1b[HREADY")
response = metadata = 0
while True:
    key = os.read(0, 1)
    if not key:
        break
    if key == b"m":
        metadata += 1
        os.write(1, f"\\x1b]2;metadata-{metadata}\\x07".encode())
    elif key == b"b":
        for index in range(160):
            os.write(1, f"\\x1b[HSTREAM{index:04d}".encode())
            time.sleep(.001)
        os.write(1, b"\\x1b[HSTREAM_DONE\\x1b]2;stream-done\\x07")
    elif key == b"u":
        for index in range(160):
            os.write(1, b"\\x1b[HUNCHANGED")
            time.sleep(.001)
        os.write(1, b"\\x1b]2;unchanged-done\\x07")
    elif key == b"h":
        os.write(1, b"\\x1b[?1049l\\x1b[2J\\x1b[H" + b"".join(f"history{index:04}\\r\\n".encode() for index in range(2048))
                 + b"\\x1b]2;history-ready\\x07")
    elif key == b"r":
        for index in range(160):
            os.write(1, f"row-output-{index:04}\\r\\n".encode())
            time.sleep(.001)
        os.write(1, b"\\x1b]2;rows-done\\x07")
    elif key == b"w":
        os.write(1, b"\\x1b7\\x1b[24;1Hhistory-edit\\x1b8\\x1b]2;history-edited\\x07")
    else:
        response += 1
        os.write(1, f"\\x1b[HPROBE{response:06d}".encode())
''')
    (fixture.path / "shell").write_text(f"#!/bin/sh\nexec python3 {shlex.quote(str(worker))}\n")
    daemon = fixture.start(binary, "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc("status"))
    pane = fixture.rpc("new")["pane"]
    hidden = fixture.rpc("split", pane=pane)["pane"]
    for target in (pane, hidden):
        wait_for(lambda: "READY" in fixture.rpc("read", pane=target)["text"])
    client = Client(fixture, pane)
    try:
        client.send("subscribe", panes=[pane])
        client.until(lambda h, text: h["meta"]["id"] == pane and "READY" in text)
        response = 0
        metadata = {pane: 0, hidden: 0}
        latencies = {}
        for workload, target in (("idle", None), ("visible-metadata", pane), ("hidden-metadata", hidden)):
            samples = []
            for index in range(23):
                # Finish the preceding real viewport's coalescing interval.
                time.sleep(.025)
                if target is not None:
                    metadata[target] += 1
                    client.socket.sendall(wire("input", pane=target, bytes=[ord("m")]))
                    header = client.until(lambda h, text: h["meta"]["id"] == target
                        and h["meta"]["title"] == f"metadata-{metadata[target]}")
                    assert header["frame"] is None and not header["rows"], header
                response += 1
                start = time.perf_counter_ns()
                # Exercise both raw socket input and the GTK client's key path.
                if index % 2:
                    client.send("input", bytes=[ord("q")])
                else:
                    client.send("key", action="press", name="q", unshifted=ord("q"), text="q")
                client.until(lambda h, text: h["meta"]["id"] == pane and f"PROBE{response:06d}" in text)
                if index >= 3:
                    samples.append((time.perf_counter_ns() - start) / 1e6)
            latencies[workload] = dict(median_ms=statistics.median(samples), samples_ms=samples)
        result = dict(workload="viewport-deadline", latency=latencies,
                      binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                      latency_method="Unix stream key/input send to its matching terminal cell packet; excludes GTK and presentation.")
        results.append(result)
        print(json.dumps(result), flush=True)
        if not args.measure:
            # Medians tolerate isolated scheduling stalls while catching a full
            # 16 ms interval consumed by an empty or metadata-only update.
            assert all(values["median_ms"] < 8 for values in latencies.values()), result

        with scenario("visible streaming output still coalesces viewport packets"):
            time.sleep(.025)
            frames = []
            def streamed(header, text):
                if header["meta"]["id"] == pane and header["frame"]:
                    frames.append(time.perf_counter_ns())
                return header["meta"]["id"] == pane and "STREAM_DONE" in text
            start = time.monotonic()
            client.send("input", bytes=[ord("b")])
            client.until(streamed)
            limit = int((time.monotonic() - start) / cadence) + 3
            assert 2 <= len(frames) <= limit, f"Streaming output produced {len(frames)} viewport packets"
        with scenario("connections independently follow high and low refresh intervals"):
            slow = Client(fixture, pane)
            try:
                slow.send("subscribe", panes=[pane])
                slow.until(lambda h, text: h["meta"]["id"] == pane and h["frame"])
                client.send("frame-interval", micros=4167)
                slow.send("frame-interval", micros=32000)
                samples = {"fast": [], "slow": []}
                errors = []
                def receive(source, name):
                    def update(header, text):
                        if header["meta"]["id"] != pane or not header["frame"]:
                            return False
                        if "STREAM_DONE" in text:
                            return bool(samples[name])
                        if "STREAM" in text:
                            samples[name].append(time.monotonic())
                        return False
                    try:
                        source.until(update)
                    except Exception as error:
                        errors.append(error)
                reader = threading.Thread(target=receive, args=(slow, "slow"))
                reader.start()
                client.send("input", bytes=[ord("b")])
                receive(client, "fast")
                reader.join(8)
                assert not reader.is_alive() and not errors, errors
                assert len(samples["fast"]) >= 2 * len(samples["slow"]) >= 4, samples
                intervals = {name: statistics.median(b - a for a, b in zip(values, values[1:])) * 1000
                             for name, values in samples.items()}
                assert intervals["fast"] < 12, intervals
                assert intervals["slow"] >= 24, intervals
                report = dict(kind="display-pacing", packets={name: len(values) for name, values in samples.items()},
                              median_interval_ms=intervals)
                results.append(report)
                print(json.dumps(report), flush=True)
            finally:
                slow.close()
                client.send("frame-interval", micros=args.frame_interval_us)
        with scenario("repeated identical output still coalesces viewport extraction"):
            time.sleep(.025)
            before = fixture.rpc("status")["snapshots"]
            start = time.monotonic()
            client.send("input", bytes=[ord("u")])
            client.until(lambda h, text: h["meta"]["id"] == pane
                         and h["meta"]["title"] == "unchanged-done")
            snapshots = fixture.rpc("status")["snapshots"] - before
            limit = int((time.monotonic() - start) / cadence) + 4
            assert snapshots <= limit, f"Identical output extracted {snapshots} viewports"
        with scenario("search batches stay paced when no matches change the visible state"):
            client.send("input", bytes=[ord("h")])
            header = client.until(lambda h, text: h["meta"]["id"] == pane
                                  and h["meta"]["title"] == "history-ready")
            history_rows = header["frame"]["scrollback"]["total"]
            assert history_rows > 512, header
            start = time.monotonic()
            client.send("find", query="absent-needle")
            completed = client.until(lambda h, text: h["meta"]["id"] == pane and h["find"]
                                     and not h["find"]["pending"])
            elapsed = time.monotonic() - start
            # Each turn scans at most 64 rows. Allow an immediate first batch
            # and an extra metadata pass, but require pacing for the rest.
            minimum = ((history_rows + 63) // 64 - 2) * cadence
            assert elapsed >= minimum, dict(elapsed=elapsed, rows=history_rows, completed=completed)
        with scenario("scrolling reuses overlapping rows and bounds history prefetch"):
            client.send("find", query="")
            base_row = history_rows // 3
            client.send("scroll-to", row=base_row, serial=1)
            initial = client.until(lambda h, text: h["meta"]["id"] == pane
                                   and h["frame"] and h["scroll_serial"] == 1)
            generation = initial["generation"]
            before = client.reply_bytes, client.viewport_rows, client.cache_rows
            rss = memory(daemon.pid)
            started = time.monotonic()
            for serial in range(2, 66):
                client.send("scroll-to", row=base_row + serial - 1, serial=serial)
                header = client.until(lambda h, text: h["meta"]["id"] == pane
                                      and h["frame"] and h["scroll_serial"] == serial)
                assert header["generation"] == generation, header
                assert header.get("row_shift") == 1, header
                assert header["rows"] == [header["frame"]["rows"] - 1], header
            viewport_rows = client.viewport_rows - before[1]
            cache_rows = client.cache_rows - before[2]
            frame = header["frame"]
            report = dict(kind="scroll-bandwidth", steps=64,
                          frame_interval_us=args.frame_interval_us,
                          elapsed_ms=(time.monotonic() - started) * 1000,
                          reply_bytes=client.reply_bytes - before[0], viewport_rows=viewport_rows,
                          cache_rows=cache_rows, full_grid_cell_bytes=64 * frame["cols"] * frame["rows"] * 75,
                          daemon_rss_before_kib=rss["VmRSS"], daemon_rss_after_kib=memory(daemon.pid)["VmRSS"])
            assert viewport_rows == 64, report
            assert cache_rows <= 2 * frame["rows"] * 5, report
            assert report["reply_bytes"] < report["full_grid_cell_bytes"] / 2, report
            results.append(report)
            print(json.dumps(report), flush=True)
        with scenario("off-screen writes invalidate only mutable rows without sending a viewport"):
            client.send("scroll-to", row=history_rows - frame["rows"] - 20, serial=100)
            cached = client.until(lambda h, text: h["meta"]["id"] == pane
                                  and h["frame"] and h["scroll_serial"] == 100)
            assert cached.get("cache"), cached
            client.send("input", bytes=[ord("w")])
            edited = client.until(lambda h, text: h["meta"]["id"] == pane
                                  and h["meta"]["title"] == "history-edited")
            assert edited["frame"] is None and not edited["rows"], edited
            assert edited["generation"] == cached["generation"], edited
            assert edited["cache_valid_before"] == history_rows - frame["rows"], edited
            generation = edited["generation"]
        with scenario("scrolling preserves cached history during output without resending windows"):
            frames = []
            before = client.cache_rows
            def scrolling_output(header, text):
                if header["meta"]["id"] == pane and header["frame"]:
                    assert header["generation"] == generation, header
                    frames.append(header["frame"]["scrollback"]["offset"])
                    client.send("scroll-to", row=base_row + len(frames), serial=1000 + len(frames))
                return header["meta"]["id"] == pane and header["meta"]["title"] == "rows-done"
            client.send("input", bytes=[ord("r")])
            client.until(scrolling_output)
            assert len(frames) >= 2, frames
            assert client.cache_rows - before <= frame["rows"] * 10, client.cache_rows - before
    finally:
        client.close()

for kind in ("valid-input-scroll", "empty-lines-input"):
    with scenario(kind), AppFixture(prefix="gmux-frontend-input-") as fixture:
        daemon = fixture.start(binary, "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: fixture.rpc("status"))
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
                    fixture.rpc("status")
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

with scenario("buffered attach and fragmented 4 MiB UTF-8 paste"), AppFixture() as fixture:
    fixture.start(binary, "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc("status"))
    pane = fixture.rpc("new")["pane"]
    client = None
    try:
        # A new, unattached connection has no metadata deadline to rescue a lost wake.
        with socket.socket(socket.AF_UNIX) as delayed:
            delayed.settimeout(3)
            delayed.connect(fixture.env["GMUX_SOCKET"] + ".stream")
            delayed.sendall(wire("focused", active=False) * 128 + wire("attach", version=PROTOCOL_VERSION, daemon=fixture.rpc("_hello")["daemon"]))
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 1)
            # State output proves Attach ran even when it was left in a read buffer.
            assert delayed.recv(4), "Buffered Attach did not run"

        with socket.socket(socket.AF_UNIX) as incompatible:
            incompatible.settimeout(3)
            incompatible.connect(fixture.env["GMUX_SOCKET"] + ".stream")
            incompatible.sendall(wire("focused", active=False) * 128 + wire("attach", version=0, daemon=fixture.rpc("_hello")["daemon"]))
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
print("PASS: viewport scheduling, bounded floods, control and independent pane responsiveness, buffered Attach, and ordered UTF-8 paste")
