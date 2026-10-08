#!/usr/bin/env python3
"""Check durable scrollback, cache recovery, and ordered checkpoint writes."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
from pathlib import Path
import runpy
import socket
import struct

from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
ghostty_revision = runpy.run_path(str(root / "scripts/bootstrap-ghostty.py"))["REVISION"]
unicode_line = "你好 e\u0301 🦀 wrapped history " * 6


class History:
    def __init__(self, fixture, pane):
        self.fixture, self.pane = fixture, pane
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(8)
        self.socket.connect(str(fixture.path / "socket.stream"))
        self.send("attach", version=6)
        self.send("subscribe", panes=[pane])

    def send(self, method, **params):
        self.socket.sendall((json.dumps(dict(method=method, pane=self.pane, **params)) + "\n").encode())

    def exact(self, size):
        result = bytearray()
        while len(result) < size:
            part = self.socket.recv(size - len(result))
            assert part, "Stream disconnected"
            result.extend(part)
        return result

    def packet(self, predicate):
        while True:
            size, = struct.unpack("<I", self.exact(4))
            assert size <= 16 * 1024 * 1024
            body = self.exact(size)
            header_size, = struct.unpack("<I", body[:4])
            header = json.loads(body[4:4 + header_size])
            if header["meta"]["id"] == self.pane and predicate(header):
                return header

    def text(self):
        frame = self.packet(lambda header: header["frame"] is not None)["frame"]
        self.send("select", start=[0, 0], end=[frame["cols"] - 1, frame["scrollback"]["total"] - 1])
        self.packet(lambda header: header["selection"] is not None)
        return self.fixture.rpc("_copy", pane=self.pane)["text"]

    def close(self):
        self.socket.close()


with AppFixture({"restore": True}, prefix="gmux-scrollback-") as fixture, (artifacts / "scrollback-smoke.log").open("w") as log:
    fixture.env.update(DISPLAY="", WAYLAND_DISPLAY="", GMUX_TEST_DIR=str(fixture.path))
    (fixture.path / "shell").write_text("#!/bin/sh\nexport PS1=''\nstty -echo\necho $$ > \"$GMUX_TEST_DIR/$GMUX_PANE.pid\"\nprintf 'SHELL_READY\\n'\nexec /bin/bash --noprofile --norc -i\n")

    def start():
        daemon = fixture.start(root / "target/debug/gmux-daemon", stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("daemon-info"))
        assert daemon.poll() is None
        return daemon

    def send(pane, text):
        return fixture.rpc("send", pane=pane, text=text, paste=True, enter=True)

    def stop(daemon):
        result = fixture.rpc("quit")
        assert result["ok"] and not result.get("warning"), result
        assert daemon.wait(timeout=8) == 0

    def history(pane):
        probe = History(fixture, pane)
        try:
            return probe.text()
        finally:
            probe.close()

    def ready(pane):
        wait_for(lambda: "SHELL_READY" in fixture.rpc("read", pane=pane)["text"])

    with scenario("Quit restores Unicode and wrapped history into new shells"):
        daemon = start()
        pane = fixture.rpc("new", directory=str(root))["pane"]
        ready(pane)
        first_pid = (fixture.path / f"{pane}.pid").read_text()
        send(pane, "printf 'SNAPSHOT_BEGIN\\n%s\\n' '" + unicode_line + "'; for i in $(seq 1 80); do printf 'history %s\\n' \"$i\"; done; printf 'CHECKPOINT_READY\\n'")
        wait_for(lambda: "CHECKPOINT_READY" in fixture.rpc("read", pane=pane)["text"])
        survivor = fixture.rpc("new", directory=str(root))["pane"]
        ready(survivor)
        send(survivor, "printf 'SURVIVING_HISTORY\\n'")
        wait_for(lambda: "SURVIVING_HISTORY" in fixture.rpc("read", pane=survivor)["text"])
        stop(daemon)
        saved = json.loads((fixture.config / "session.json").read_text())
        snapshot_path = fixture.config / "scrollback" / saved["scrollback"][str(pane)]
        assert snapshot_path.read_bytes()[12:52] == ghostty_revision.encode(), "Snapshot must declare the actual build pin"
        assert snapshot_path.stat().st_mode & 0o777 == 0o600
        assert snapshot_path.parent.stat().st_mode & 0o777 == 0o700
        daemon = start()
        ready(pane)
        assert (fixture.path / f"{pane}.pid").read_text() != first_pid
        text = history(pane)
        assert "SNAPSHOT_BEGIN" in text and unicode_line.rstrip() in text and "history 80" in text, text
        send(pane, "printf 'NEW_PROCESS_INPUT\\n'")
        wait_for(lambda: "NEW_PROCESS_INPUT" in fixture.rpc("read", pane=pane)["text"])
        stop(daemon)

    with scenario("checkpoint disk locks leave PTY processing active"):
        daemon = start()
        ready(pane)
        lock = (fixture.config / ".session.json.lock").open("a")
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            with ThreadPoolExecutor() as pool:
                pending = pool.submit(fixture.rpc, "quit")
                wait_for(lambda: fixture.rpc("_hello")["quitting"])
                send(pane, "printf 'OUTPUT_DURING_CHECKPOINT\\n'")
                wait_for(lambda: "OUTPUT_DURING_CHECKPOINT" in fixture.rpc("read", pane=pane)["text"])
                assert not pending.done(), "Quit replied before the durable layout commit"
                fcntl.flock(lock, fcntl.LOCK_UN)
                result = pending.result(timeout=8)
                assert result["ok"] and not result.get("warning"), result
        finally:
            lock.close()
        assert daemon.wait(timeout=8) == 0

    session_path = fixture.config / "session.json"
    baseline = session_path.read_bytes()
    saved = json.loads(baseline)
    snapshot_path = fixture.config / "scrollback" / saved["scrollback"][str(pane)]
    valid = snapshot_path.read_bytes()
    survivor_path = fixture.config / "scrollback" / saved["scrollback"][str(survivor)]
    survivor_bytes = survivor_path.read_bytes()

    for reason in ["shell spawn failure", "Restore tabs disabled"]:
        with scenario(f"startup cleanup preserves committed history after {reason}"):
            session_path.write_bytes(baseline)
            snapshot_path.write_bytes(valid)
            survivor_path.write_bytes(survivor_bytes)
            orphan = snapshot_path.parent / ("99-" + "f" * 32 + ".snapshot")
            orphan.write_bytes(b"interrupted publication")
            shell = fixture.env["SHELL"]
            if reason == "shell spawn failure":
                fixture.env["SHELL"] = str(fixture.path / "missing-shell")
            else:
                fixture.set_settings({"restore": False})
            daemon = start()
            assert not fixture.rpc("list")["tabs"]
            wait_for(lambda: not orphan.exists())
            assert session_path.read_bytes() == baseline
            assert snapshot_path.read_bytes() == valid
            daemon.kill()  # Do not commit an empty layout over the saved one.
            assert daemon.wait(timeout=8) != 0
            fixture.env["SHELL"] = shell
            fixture.set_settings({"restore": True})
            daemon = start()
            ready(pane)
            assert "SNAPSHOT_BEGIN" in history(pane)
            stop(daemon)

    def flip(offset):
        changed = bytearray(valid)
        changed[offset] ^= 0xff
        return changed

    def truncated_payload():
        changed = bytearray(valid[:-1])
        changed[60:68] = struct.pack("<Q", len(changed) - 68)
        return changed

    for name, damaged in [
        ("Gmux format version", flip(8)),
        ("Ghostty format version", flip(68 + 8)),
        ("Ghostty source revision", flip(12)),
        ("pane ownership", flip(52)),
        ("record CRC", flip(len(valid) // 2)),
        ("file truncation", valid[:-1]),
        ("missing FINISH", truncated_payload()),
        ("trailing bytes", valid + b"garbage"),
        ("missing snapshot", None),
    ]:
        with scenario(f"recover from incompatible or damaged {name}"):
            session_path.write_bytes(baseline)
            if damaged is None:
                snapshot_path.unlink(missing_ok=True)
            else:
                snapshot_path.write_bytes(damaged)
            survivor_path.write_bytes(survivor_bytes)
            daemon = start()
            ready(pane)
            assert "SNAPSHOT_BEGIN" not in history(pane)
            ready(survivor)
            assert "SURVIVING_HISTORY" in history(survivor), "One bad pane must not discard another pane's history"
            send(pane, "printf 'RECOVERY_INPUT\\n'")
            wait_for(lambda: "RECOVERY_INPUT" in fixture.rpc("read", pane=pane)["text"])
            stop(daemon)

    with scenario("SIGTERM saves history and deleted panes release their snapshots"):
        daemon = start()
        ready(pane)
        send(pane, "printf 'SIGTERM_HISTORY\\n'")
        wait_for(lambda: "SIGTERM_HISTORY" in fixture.rpc("read", pane=pane)["text"])
        daemon.terminate()
        assert daemon.wait(timeout=8) == 0
        daemon = start()
        ready(pane)
        assert "SIGTERM_HISTORY" in history(pane)
        saved = json.loads(session_path.read_text())
        path = fixture.config / "scrollback" / saved["scrollback"][str(pane)]
        assert path.exists()
        fixture.rpc("close", pane=pane)
        wait_for(lambda: not path.exists())
        assert str(pane) not in json.loads(session_path.read_text()).get("scrollback", {})
        fixture.rpc("close", pane=survivor)
        stop(daemon)
        daemon = start()
        assert not fixture.rpc("list")["tabs"]
        new_pane = fixture.rpc("new", directory=str(root))["pane"]
        ready(new_pane)
        assert "SIGTERM_HISTORY" not in history(new_pane)
        stop(daemon)

    with scenario("snapshot directory failure still saves the layout and quits"):
        cache = fixture.config / "scrollback"
        # The prior Quit left one referenced snapshot; replace the cache with
        # a file to simulate an unavailable snapshot destination.
        for path in cache.iterdir():
            path.unlink()
        cache.rmdir()
        cache.write_text("unavailable")
        daemon = start()
        ready(new_pane)
        result = fixture.rpc("quit")
        assert result["ok"] and result.get("warning"), result
        assert daemon.wait(timeout=8) == 0
        saved = json.loads(session_path.read_text())
        assert saved["tabs"] and not saved.get("scrollback")
        cache.unlink()
        daemon = start()
        ready(new_pane)
        stop(daemon)

print("PASS: durable Quit/SIGTERM history, new shell PIDs and input, Unicode wraps, snapshot permissions, disk backpressure, version/CRC/truncation recovery, startup recovery, and deleted-pane cleanup")
