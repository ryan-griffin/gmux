#!/usr/bin/env python3
"""Check native SGR blink and rendition through local/remote viewports and history."""
import os
from pathlib import Path
import subprocess

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty"))
check = artifacts / "text-blink-check"
subprocess.run(["cc", "-O2", "-std=c11", f"-I{prefix}/include", str(root / "scripts/text-blink-check.c"),
                f"-L{prefix}/lib", f"-Wl,-rpath,{prefix}/lib", "-lghostty-vt", "-o", str(check)], check=True)
subprocess.run([str(check)], check=True)

with AppFixture(prefix="gmux-text-blink-") as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = ProgressProbe(fixture, pane)
    peers = [Client(fixture, pane), Client(fixture, pane, address=address)]
    serial = 0

    def emit(text):
        global serial
        serial += 1
        title = f"BLINK_{serial}"
        probe.write(text + f"\x1b]2;{title}\x07")
        for peer in peers:
            peer.until(lambda h: h["meta"]["title"] == title)

    def check_flags(peer, row=0):
        flags = [peer.cell_bytes[row, x][70] for x in range(6)]
        assert [bool(flag & 8) for flag in flags] == [True, False, True, True, True, False], flags
        assert peer.cell_bytes[row, 4].split(b"\0", 1)[0] == "e\u0301".encode()

    try:
        for peer in peers:
            peer.send("resize", cols=12, rows=3)
            peer.barrier("resize")
        with scenario("local and TCP streams preserve mixed SGR 5/6/25/0 and wide/combining text"):
            emit("\x1b[H\x1b[2J\x1b[3J\x1b[5mA\x1b[25mB\x1b[6m界e\u0301\x1b[0mC")
            for peer in peers:
                check_flags(peer)
        with scenario("synchronized output freezes blink rendition until release"):
            emit("\x1b[?2026h\x1b[H\x1b[25mZ")
            for peer in peers:
                check_flags(peer)
            emit("\x1b[?2026l")
            for peer in peers:
                assert not peer.cell_bytes[0, 0][70] & 8
        with scenario("styled history prefetch, scrolling and reconnect retain blink flags"):
            emit("\r\x1b[5mA\x1b[25m\r\n" + "".join(
                f"\x1b[{'5' if row % 2 else '25'}mrow{row}\x1b[0m\r\n" for row in range(1, 20)))
            for peer in peers:
                peer.send("scroll-to", row=0, serial=1)
                peer.until(lambda h: h.get("scroll_serial") == 1 and h["frame"])
                check_flags(peer)
                assert peer.cached_cell_bytes, "Expected history prefetch"
                assert any(peer.cached_cell_bytes[index] & 8 for index in range(70, len(peer.cached_cell_bytes), 75))
                peer.send("scroll-to", row=1, serial=2)
                peer.until(lambda h: h.get("scroll_serial") == 2 and h["frame"])
                assert peer.cell_bytes[0, 0][70] & 8
                assert not peer.cell_bytes[1, 0][70] & 8
            peers[1].close()
            peers[1] = Client(fixture, pane, address=address)
            peers[1].send("scroll-to", row=0, serial=3)
            peers[1].until(lambda h: h.get("scroll_serial") == 3 and h["frame"])
            check_flags(peers[1])
        with scenario("screen switches and clear discard obsolete blinking rows"):
            emit("\x1b[?1049h\x1b[H\x1b[5mX\x1b[25mY")
            for peer in peers:
                assert peer.cell_bytes[0, 0][70] & 8
                assert not peer.cell_bytes[0, 1][70] & 8
            emit("\x1b[?1049l")
            for index, peer in enumerate(peers):
                peer.send("scroll-to", row=0, serial=4 + index)
                peer.until(lambda h: h.get("scroll_serial") == 4 + index and h["frame"])
                check_flags(peer)
            peers[0].send("clear-screen")
            for peer in peers:
                peer.barrier("clear")
                assert all(not cell[70] & 8 for cell in peer.cell_bytes.values())
    finally:
        for peer in peers:
            peer.close()
print("PASS: text blink native SGR, local/remote rendition, held frames, cached history, reconnect and screen lifetime")
