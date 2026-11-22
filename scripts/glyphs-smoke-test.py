#!/usr/bin/env python3
"""Register, replace and clear a glyph through a PTY and observe both streams."""
from base64 import b64decode, b64encode
import shlex
import struct
import subprocess
import sys

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

CODEPOINT = "\U00100000"
OTHER_CODEPOINT = "\U00100001"
DA1 = "\x1b[c"
TRIANGLE = "AAEAZABkA4QDhAACAAABAQEB9P5wAyADhPzgAAA="


def apc(body):
    return f"\x1b_25a1;{body}\x1b\\"


def outline(glyph):
    return list(struct.iter_unpack("<iiB", b64decode(glyph["outline"])))


def box():
    header = struct.pack(">5hHH", 1, 0, 0, 1000, 1000, 3, 0)
    return b64encode(header + b"\x01" * 4 +
                     struct.pack(">8h", 0, 1000, 0, -1000, 0, 0, 1000, 0)).decode()


with AppFixture(prefix="gmux-glyphs-") as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]

    probe = fixture.path / "probe.py"
    probe.write_text(r'''
import os, re, select, time, tty
from pathlib import Path

tty.setraw(0)
os.write(1, b"\x1b]2;GLYPH_PROBE_READY\x07")
number = 0
while True:
    request = Path(f"request-{number}")
    if not request.exists():
        time.sleep(.005)
        continue
    os.write(1, request.read_bytes())
    response = bytearray()
    while not re.search(rb"\x1b\[\?[0-9;]+c$", response):
        if not select.select([0], [], [], 3)[0]:
            raise TimeoutError(response)
        response.extend(os.read(0, 4096))
    temporary = Path("response.tmp")
    temporary.write_bytes(response)
    temporary.replace(f"response-{number}")
    number += 1
''')
    fixture.rpc("send", pane=pane, enter=True, text=shlex.join([sys.executable, str(probe)]))
    wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["title"] == "GLYPH_PROBE_READY")
    peers = [Client(fixture, pane), Client(fixture, pane, address=address)]
    number = 0

    def exchange(commands, expected, *, mark=False):
        global number
        title = f"GLYPH_{number}"
        if mark:
            commands += f"\x1b]2;{title}\x07"
        tmp = fixture.path / "request.tmp"
        tmp.write_bytes((commands + DA1).encode())
        tmp.replace(fixture.path / f"request-{number}")
        response = fixture.path / f"response-{number}"
        wait_for(response.exists)
        assert response.read_bytes().decode().startswith("".join(map(apc, expected)) + "\x1b[?"), response.read_bytes()
        number += 1
        return title

    def updated(peer):
        return peer.until(lambda h: h.get("meta", {}).get("id") == pane and "glyphs" in h)

    other = None
    try:
        with scenario("Glyph negotiation and PTY registration reach both pane streams"):
            for peer in peers:
                peer.send("resize", cols=12, rows=3)
                peer.barrier("resize")
                if peer.frame is None:
                    assert peer.until(lambda h: h.get("meta", {}).get("id") == pane and h.get("frame"))["glyphs"] == {"replace": []}
            exchange(apc("s"), ["s;fmt=glyf"])
            exchange(apc(f"r;cp=100000;width=2;{TRIANGLE}") + apc(f"r;cp=100001;reply=0;{TRIANGLE}")
                     + "\x1b[H\x1b[2J" + CODEPOINT + " A",
                     ["r;cp=100000;status=0"])
            for peer in peers:
                patch = updated(peer)["glyphs"]["patch"]
                glyph, unmodified = patch["defined"]
                assert patch["removed"] == [] and unmodified["codepoint"] == OTHER_CODEPOINT
                assert glyph["codepoint"] == CODEPOINT and glyph["width"] == 2
                assert outline(glyph) == [(500, 900, 1), (100, 100, 1), (900, 100, 3)], glyph
                assert [peer.cells[0, x] for x in range(3)] == [CODEPOINT, " ", "A"]
            assert fixture.rpc("read", pane=pane)["text"] == CODEPOINT + " A"

        with scenario("Namespace and format checks preserve the glossary"):
            exchange(apc(f"r;cp=e000;{TRIANGLE}") + apc(f"r;cp=100000;fmt=vg;{TRIANGLE}")
                     + apc("q;cp=100000") + apc("q;grapheme=1F468,200d,1f469"),
                     ["r;cp=e000;status=1;reason=out_of_namespace",
                      "r;cp=100000;status=1;reason=unsupported_format",
                      "q;cp=100000;status=glossary",
                      "q;grapheme=1f468,200d,1f469;reason=unsupported"])
            other_pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
            other = Client(fixture, other_pane, address=address)
            assert other.until(lambda h: h.get("meta", {}).get("id") == other_pane and h.get("frame"))["glyphs"] == {"replace": []}

        with scenario("Synchronized output holds the old glyph until the frame commits"):
            title = exchange("\x1b[?2026h" + apc(f"r;cp=100000;fmt=glyf;size=stretch;reply=2;{box()}"),
                             [], mark=True)
            for peer in peers:
                assert "glyphs" not in peer.until(lambda h: h.get("meta", {}).get("title") == title)
            exchange("\x1b[?2026l", [])
            for peer in peers:
                packet = updated(peer)
                patch = packet["glyphs"]["patch"]
                glyph, = patch["defined"]
                assert patch["removed"] == []
                assert glyph["size"] == "stretch" and len(outline(glyph)) == 4, glyph
                assert not packet["rows"] and peer.cells[0, 0] == CODEPOINT
            peers[1].close()
            peers[1] = Client(fixture, pane, address=address)
            glyph, unmodified = updated(peers[1])["glyphs"]["replace"]
            assert len(outline(glyph)) == 4 and unmodified["codepoint"] == OTHER_CODEPOINT

        with scenario("Clear and RIS remove registrations from live panes"):
            exchange(apc("c;cp=100000") + apc("q;cp=100000"),
                     ["c;cp=100000;status=0", "q;cp=100000;reason=unknown"])
            for peer in peers:
                assert updated(peer)["glyphs"]["patch"] == {"defined": [], "removed": [CODEPOINT]}
                assert peer.cells[0, 0] == CODEPOINT
            exchange(apc(f"r;cp=100000;reply=0;{TRIANGLE}"), [])
            for peer in peers:
                glyph, = updated(peer)["glyphs"]["patch"]["defined"]
                assert glyph["codepoint"] == CODEPOINT
            exchange("\x1bc" + apc("q;cp=100000"), ["q;cp=100000;reason=unknown"])
            for peer in peers:
                assert updated(peer)["glyphs"]["patch"] == {
                    "defined": [], "removed": [CODEPOINT, OTHER_CODEPOINT]}
    finally:
        for peer in peers:
            peer.close()
        if other is not None:
            other.close()
print("PASS: Glyph Protocol registration, replacement, clear, reset and pane isolation")
