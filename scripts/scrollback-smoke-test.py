#!/usr/bin/env python3
"""Check durable scrollback, cache recovery, and ordered checkpoint writes."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
from pathlib import Path
from cargo_test_support import cargo_binary
from stream_test_support import PROTOCOL_VERSION
import runpy
import socket
import struct

from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
ghostty_revision = runpy.run_path(str(root / "scripts/bootstrap-ghostty.py"))["REVISION"]
unicode_line = "你好 e\u0301 🦀 wrapped history " * 6
colored_output = ("\\033[31;44mINDEX\\033[38;5;201;48;5;22mEXTENDED\\033[0m\\n"
                  "\\033[1;3;4;9;38;2;12;34;56;48;2;78;90;123m" + unicode_line + "\\033[0m\\n"
                  "\\033[0mPLAIN_OUTPUT\\n")


class History:
    def __init__(self, fixture, pane):
        self.fixture, self.pane = fixture, pane
        self.frame, self.cells = None, {}
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(8)
        self.socket.connect(str(fixture.path / "socket.stream"))
        self.send("attach", version=PROTOCOL_VERSION, daemon=fixture.rpc("_hello")["daemon"])
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
            if header["meta"]["id"] == self.pane and header["frame"] is not None:
                frame = header["frame"]
                cells = body[4 + header_size:]
                assert len(cells) == len(header["rows"]) * frame["cols"] * 71
                if self.frame is None or (frame["cols"], frame["rows"]) != (self.frame["cols"], self.frame["rows"]):
                    self.cells = {}
                self.frame = frame
                for index, row in enumerate(header["rows"]):
                    for col in range(frame["cols"]):
                        start = (index * frame["cols"] + col) * 71
                        cell = cells[start:start + 71]
                        text = cell[:64].split(b"\0", 1)[0].decode()
                        fg, bg, flags = bytes(cell[64:67]), bytes(cell[67:70]), cell[70]
                        # The formatter emits background-only blanks as spaces.
                        if not text and bg != bytes(frame["background"]) and flags & 64 == 0:
                            text = " "
                        self.cells[row, col] = (text, fg, bg, flags)
            if header["meta"]["id"] == self.pane and predicate(header):
                return header

    def rendition(self):
        self.packet(lambda header: header["frame"] is not None)
        result = {}
        for serial, row in enumerate(range(0, self.frame["scrollback"]["total"], self.frame["rows"]), 1):
            self.send("scroll-to", row=row, serial=serial)
            self.packet(lambda header: header["scroll_serial"] == serial)
            for (y, x), cell in self.cells.items():
                if cell[0] and cell[3] & 64 == 0:
                    result[self.frame["scrollback"]["offset"] + y, x] = cell
        return [result[position] for position in sorted(result)]

    def text(self):
        frame = self.packet(lambda header: header["frame"] is not None)["frame"]
        self.send("select", start=[0, 0], end=[frame["cols"] - 1, frame["scrollback"]["total"] - 1])
        self.packet(lambda header: header["selection"] is not None)
        return self.fixture.rpc("_copy", pane=self.pane)["text"]

    def close(self):
        self.socket.close()


with AppFixture({"appearance": "dark"}, prefix="gmux-scrollback-") as fixture, (artifacts / "scrollback-smoke.log").open("w") as log:
    fixture.env.update(DISPLAY="", WAYLAND_DISPLAY="", GMUX_TEST_DIR=str(fixture.path))
    (fixture.path / "shell").write_text("#!/bin/sh\nexport PS1=''\nstty -echo\necho $$ > \"$GMUX_TEST_DIR/$GMUX_PANE.pid\"\nprintf 'SHELL_READY\\n'\nexec /bin/bash --noprofile --norc -i\n")

    def start():
        daemon = fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("status"))
        assert daemon.poll() is None
        return daemon

    def send(pane, text):
        return fixture.rpc("send", pane=pane, text=text, paste=True, enter=True)

    def stop(daemon):
        result = fixture.rpc("stop")
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

    def rendition(pane):
        probe = History(fixture, pane)
        try:
            return probe.rendition()
        finally:
            probe.close()

    def assert_colors(cells):
        text = [cell[0] for cell in cells]
        for marker, fg, bg, flags in [
            ("INDEX", bytes([192, 28, 40]), bytes([30, 120, 228]), 0),
            ("EXTENDED", bytes([255, 0, 255]), bytes([0, 95, 0]), 0),
            ("你好", bytes([12, 34, 56]), bytes([78, 90, 123]), 27),
            ("PLAIN_OUTPUT", bytes([255, 255, 255]), bytes([28, 28, 31]), 0),
            ("SHELL_READY", bytes([255, 255, 255]), bytes([28, 28, 31]), 0),
        ]:
            starts = [index for index in range(len(cells) - len(marker) + 1)
                      if text[index:index + len(marker)] == list(marker)]
            assert starts, marker
            for start in starts:
                assert all(cell[1:] == (fg, bg, flags) for cell in cells[start:start + len(marker)]), marker

    with scenario("Stop restores tabs and history into new shells"):
        daemon = start()
        pane = fixture.rpc("new", directory=str(root))["pane"]
        ready(pane)
        first_pid = (fixture.path / f"{pane}.pid").read_text()
        send(pane, "printf 'SNAPSHOT_BEGIN\\n" + colored_output + "'; for i in $(seq 1 80); do printf 'history %s\\n' \"$i\"; done; printf 'CHECKPOINT_READY\\n\\033[38;2;111;112;113;48;2;21;22;23mUNRESET'")
        wait_for(lambda: "CHECKPOINT_READY" in fixture.rpc("read", pane=pane)["text"])
        before = rendition(pane)
        assert_colors(before)
        survivor = fixture.rpc("new", directory=str(root))["pane"]
        ready(survivor)
        send(survivor, "printf 'SURVIVING_HISTORY\\n'")
        wait_for(lambda: "SURVIVING_HISTORY" in fixture.rpc("read", pane=survivor)["text"])
        stop(daemon)
        saved = json.loads((fixture.config / "execution.json").read_text())
        snapshot_path = fixture.config / "scrollback" / saved["scrollback"][str(pane)]
        assert snapshot_path.read_bytes()[12:52] == ghostty_revision.encode(), "Snapshot must declare the actual build pin"
        assert snapshot_path.stat().st_mode & 0o777 == 0o600
        assert snapshot_path.parent.stat().st_mode & 0o777 == 0o700
        daemon = start()
        ready(pane)
        assert (fixture.path / f"{pane}.pid").read_text() != first_pid
        text = history(pane)
        assert "SNAPSHOT_BEGIN" in text and unicode_line.rstrip() in text and "history 80" in text, text
        after = rendition(pane)
        assert after[:len(before)] == before, "Restored cell rendition differs from captured history"
        assert_colors(after)
        for cell in after[len(before):]:
            assert cell[1:] == (bytes([255, 255, 255]), bytes([28, 28, 31]), 0), "Saved rendition bled into the fresh shell"
        send(pane, "printf 'NEW_PROCESS_INPUT\\n'")
        wait_for(lambda: "NEW_PROCESS_INPUT" in fixture.rpc("read", pane=pane)["text"])
        stop(daemon)

    with scenario("checkpoint disk locks leave PTY processing active"):
        daemon = start()
        ready(pane)
        lock = (fixture.config / ".execution.json.lock").open("a")
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            with ThreadPoolExecutor() as pool:
                pending = pool.submit(fixture.rpc, "stop")
                wait_for(lambda: fixture.rpc("_hello")["quitting"])
                send(pane, "printf 'OUTPUT_DURING_CHECKPOINT\\n'")
                wait_for(lambda: "OUTPUT_DURING_CHECKPOINT" in fixture.rpc("read", pane=pane)["text"])
                assert not pending.done(), "Stop replied before the durable layout commit"
                fcntl.flock(lock, fcntl.LOCK_UN)
                result = pending.result(timeout=8)
                assert result["ok"] and not result.get("warning"), result
        finally:
            lock.close()
        assert daemon.wait(timeout=8) == 0

    session_path = fixture.config / "execution.json"
    baseline = session_path.read_bytes()
    saved = json.loads(baseline)
    snapshot_path = fixture.config / "scrollback" / saved["scrollback"][str(pane)]
    valid = snapshot_path.read_bytes()
    survivor_path = fixture.config / "scrollback" / saved["scrollback"][str(survivor)]
    survivor_bytes = survivor_path.read_bytes()

    with scenario("startup cleanup preserves committed history after shell spawn failure"):
        session_path.write_bytes(baseline)
        snapshot_path.write_bytes(valid)
        survivor_path.write_bytes(survivor_bytes)
        orphan = snapshot_path.parent / ("99-" + "f" * 32 + ".snapshot")
        orphan.write_bytes(b"interrupted publication")
        shell = fixture.env["SHELL"]
        fixture.env["SHELL"] = str(fixture.path / "missing-shell")
        daemon = start()
        assert not fixture.rpc("list")["tabs"]
        wait_for(lambda: not orphan.exists())
        assert session_path.read_bytes() == baseline
        assert snapshot_path.read_bytes() == valid
        daemon.kill()  # Do not commit an empty layout over the saved one.
        assert daemon.wait(timeout=8) != 0
        fixture.env["SHELL"] = shell
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
        ("oversized payload", valid[:60] + struct.pack("<Q", 8 * 1024 * 1024 + 1) + valid[68:]),
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
        send(pane, "printf 'SIGTERM_HISTORY\\n" + colored_output + "\\033[38;2;111;112;113;48;2;21;22;23mUNRESET'")
        wait_for(lambda: "SIGTERM_HISTORY" in fixture.rpc("read", pane=pane)["text"])
        before = rendition(pane)
        assert_colors(before)
        daemon.terminate()
        assert daemon.wait(timeout=8) == 0
        daemon = start()
        ready(pane)
        assert "SIGTERM_HISTORY" in history(pane)
        after = rendition(pane)
        assert after[:len(before)] == before
        assert_colors(after)
        assert all(cell[1:] == (bytes([255, 255, 255]), bytes([28, 28, 31]), 0) for cell in after[len(before):])
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
        # The prior Stop left one referenced snapshot; replace the cache with
        # a file to simulate an unavailable snapshot destination.
        for path in cache.iterdir():
            path.unlink()
        cache.rmdir()
        cache.write_text("unavailable")
        daemon = start()
        ready(new_pane)
        result = fixture.rpc("stop")
        assert result["ok"] and result.get("warning"), result
        assert daemon.wait(timeout=8) == 0
        saved = json.loads(session_path.read_text())
        assert saved["tabs"] and not saved.get("scrollback")
        cache.unlink()
        daemon = start()
        ready(new_pane)
        stop(daemon)

print("PASS: durable Stop/SIGTERM colored history, exact indexed/RGB cells and attributes, resets without prompt bleed, new shell PIDs and input, Unicode wraps, snapshot permissions, disk backpressure, version/CRC/truncation/oversize recovery, startup recovery, and deleted-pane cleanup")
