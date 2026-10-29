#!/usr/bin/env python3
"""Preserve native long clusters over Unix/TCP deltas and styled history."""
import subprocess

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

with AppFixture(prefix="gmux-graphemes-") as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = ProgressProbe(fixture, pane)
    peers = [Client(fixture, pane), Client(fixture, pane, address=address)]
    serial = 0

    def until(peer, predicate):
        def observed(header):
            if any(cell[0] == glyphs[-1] for cell in peer.cached_cells):
                peer.saw_long_prefetch = True
            return predicate(header)
        return peer.until(observed)

    def emit(text):
        global serial
        serial += 1
        title = f"GRAPHEME_{serial}"
        probe.write(text + f"\x1b]2;{title}\x07")
        for peer in peers:
            until(peer, lambda h: h["meta"]["title"] == title)

    glyphs = ["a" + "\u0301" * 31, "a" + "\u1ab0" * 21,
              "a" + "\u1ab0" * 64, "🧑" + "\U0001d185" * 64]
    expected = "".join(glyphs) + "X"

    def check(peer):
        assert "".join(peer.cells[0, x] for x in range(6)) == expected
        assert [peer.cells[0, x] for x in range(6)] == glyphs + ["", "X"]
        assert peer.cell_values[0, 4][3] & 64
        assert all(peer.cell_values[0, x][3] & 8 for x in range(5))
        assert all(peer.cell_values[0, x][4] & 7 == 3 for x in range(5))

    try:
        for peer in peers:
            peer.send("resize", cols=12, rows=3)
            peer.barrier("resize")
        with scenario("Unix and TCP preserve complete clusters, native widths, blink and underline"):
            emit("\x1b[H\x1b[2J\x1b[3J\x1b[5;4:3m" + "".join(glyphs) + "\x1b[0mX")
            for peer in peers:
                check(peer)
            assert fixture.rpc("read", pane=pane)["text"] == expected
        with scenario("dirty rows and history prefetch keep long text and rendition"):
            emit("\x1b[3;1Hdirty")
            for peer in peers:
                check(peer)
            emit("\r\nnext\r\nlast\r\n")
            for peer in peers:
                peer.send("scroll-to", row=0, serial=1)
                until(peer, lambda h: h.get("scroll_serial") == 1 and h["frame"])
                check(peer)
                assert getattr(peer, "saw_long_prefetch", False)
        with scenario("new subscribers receive complete historical clusters"):
            peers[1].close()
            peers[1] = Client(fixture, pane, address=address)
            peers[1].send("scroll-to", row=0, serial=2)
            peers[1].until(lambda h: h.get("scroll_serial") == 2 and h["frame"])
            check(peers[1])
    finally:
        for peer in peers:
            peer.close()
print("PASS: complete native graphemes in local/TCP frames, dirty rows, history and reconnect")
