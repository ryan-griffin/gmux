#!/usr/bin/env python3
"""Exercise Kitty images through a PTY and independent local and remote views."""
from base64 import b64encode
from contextlib import ExitStack
import shlex
import struct
import subprocess
import sys
import time
import zlib

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, assert_stays, scenario, wait_for


def command(options, pixels=b""):
    return b"\x1b_G" + options.encode() + b";" + b64encode(pixels) + b"\x1b\\"


def chunked(options, payload):
    return b"".join(
        command((options + "," if start == 0 else "") + f"m={int(start + 3072 < len(payload))}",
                payload[start:start + 3072])
        for start in range(0, len(payload), 3072)
    )


def png_image(width, height, pixel):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    scanlines = (b"\0" + pixel * width) * height
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))


with AppFixture(prefix="gmux-graphics-") as fixture, ExitStack() as stack:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = fixture.path / "graphics.py"
    probe.write_text(r'''
import os, select, time, tty
from pathlib import Path

tty.setraw(0)
os.write(1, b"\x1b]2;GRAPHICS_READY\x07")
number = 0
while True:
    request = Path(f"request-{number}")
    if not request.exists():
        time.sleep(.005)
        continue
    os.write(1, request.read_bytes())
    reply = bytearray()
    while not reply.endswith(b"c"):
        if not select.select([0], [], [], 3)[0]:
            raise TimeoutError(reply)
        reply.extend(os.read(0, 4096))
    temporary = Path("response.tmp")
    temporary.write_bytes(reply)
    temporary.replace(f"response-{number}")
    number += 1
''')
    fixture.rpc("send", pane=pane, enter=True, text=shlex.join([sys.executable, str(probe)]))
    wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["title"] == "GRAPHICS_READY")
    local = Client(fixture, pane)
    remote = Client(fixture, pane, address=address)
    peers = [local, remote]
    for peer in peers:
        stack.callback(peer.close)
        peer.send("resize", cols=12, rows=4, cw=8, ch=16)
        peer.until(lambda h: h.get("frame") and h["frame"]["cols"] == 12)

    number = 0

    def exchange(data, reply=b""):
        global number
        title = f"GRAPHICS_{number}"
        before = fixture.path / "request.tmp"
        before.write_bytes(data + f"\x1b]2;{title}\x07\x1b[c".encode())
        before.replace(fixture.path / f"request-{number}")
        response = fixture.path / f"response-{number}"
        wait_for(response.exists)
        assert response.read_bytes().startswith(reply + b"\x1b[?"), response.read_bytes()
        number += 1
        return title

    def graphics(peer, condition=lambda _: True):
        return peer.until(lambda h: "graphics" in h and condition(h["graphics"]))["graphics"]

    red = b"\xff\x00\x00"
    blue = b"\0\0\xff"
    green = b"\x00\xff\x00\xff"
    png_pixel = red + b"\x80"
    png = png_image(1, 1, png_pixel)

    with scenario("Kitty query and chunked RGB transmission"):
        exchange(command("a=q,i=42,f=24,s=1,v=1", red), b"\x1b_Gi=42;OK\x1b\\")
        exchange(b"\x1b[H\x1b[2J" + command("a=T,f=24,i=42,s=1,v=2,c=1,r=2,C=1,m=1", red)
                 + command("m=0", blue), b"\x1b_Gi=42;OK\x1b\\")
        for peer in peers:
            frame = graphics(peer, lambda g: bool(g["images"]))
            image, = frame["images"]
            assert image["pixels"] == red + blue, image
            placement, = frame["placements"]
            assert placement["cell"] == [0, 0] and placement["size"] == [8, 32], placement
            assert frame["cell_pixels"] == [8, 16]

    with scenario("Movement and a second placement reuse pixels on both streams"):
        exchange(b"\x1b[1;3H" + command("a=p,i=42,p=9,c=1,r=2,C=1,z=-2,q=2"))
        for peer in peers:
            frame = graphics(peer)
            assert not frame["images"] and len(frame["placements"]) == 2, frame
            assert frame["placements"][0]["layer"] == "below_text", frame
        exchange(b"\x1b[4;1H\r\n")
        for peer in peers:
            frame = graphics(peer)
            assert not frame["images"] and {p["cell"][1] for p in frame["placements"]} == {-1}, frame
        remote.send("scroll-to", row=remote.frame["scrollback"]["offset"] - 1, serial=1)
        frame = graphics(remote)
        assert not frame["images"] and {p["cell"][1] for p in frame["placements"]} == {0}, frame

    with scenario("Synchronized output freezes each viewport until a PNG replaces the placements"):
        title = exchange(b"\x1b[?2026h" + command("a=d,d=I,i=42,q=2")
                         + b"\x1b[2;1H" + command("a=T,f=100,i=43,c=1,r=1,C=1,q=2", png))
        for peer in peers:
            header = peer.until(lambda h: h.get("meta", {}).get("title") == title)
            assert "graphics" not in header, header
        exchange(b"\x1b[?2026l")
        for peer in peers:
            frame = graphics(peer)
            assert frame["images"][0]["pixels"] == png_pixel, frame
            assert len(frame["placements"]) == 1

    with scenario("Unicode and relative placements follow alternate-screen transitions"):
        exchange(b"\x1b[?1049h" + command("a=T,U=1,f=32,i=3,p=1,s=1,v=1,c=1,r=1,q=2", b"\x00\xff\x00\xff")
                 + "\x1b[2;3H\x1b[38;5;3m\U0010EEEE\u0305\u0305\x1b[39m".encode()
                 + command("a=T,i=8,p=2,P=3,Q=1,H=1,f=24,o=z,s=1,v=1,c=1,r=1,C=1,q=2", zlib.compress(blue)))
        for peer in peers:
            frame = graphics(peer, lambda g: bool(g["images"]))
            assert frame["images"][0]["pixels"] == b"\x00\xff\x00\xff"
            assert frame["placements"][0]["cell"] == [2, 1]
            assert frame["placements"][0]["layer"] == "below_text"
            relative = frame["placements"][1]
            assert relative["cell"] == [3, 1] and relative["layer"] == "above_text", relative
            assert frame["images"][1]["pixels"] == blue
        exchange(b"\x1b[?1049l")
        for peer in peers:
            assert graphics(peer)["images"][0]["pixels"] == png_pixel

    with scenario("A reconnect receives its own pixels, and deletion clears the viewport"):
        reconnect = Client(fixture, pane, address=address)
        stack.callback(reconnect.close)
        initial = graphics(reconnect, lambda g: bool(g["images"]))
        assert initial["replace"] and initial["images"][0]["pixels"] == png_pixel
        exchange(command("a=d,d=I,i=43,q=2"))
        for peer in [*peers, reconnect]:
            assert graphics(peer) == {"replace": False, "cell_pixels": [0, 0], "placements": [], "images": []}
        reconnect.close()

    with scenario("PNG exceeding the decoded image limit is rejected"):
        oversized = png_image(2000, 1500, png_pixel)
        title = exchange(chunked("a=T,i=45,f=100", oversized), b"\x1b_Gi=45;EINVAL: invalid data\x1b\\")
        for peer in peers:
            header = peer.until(lambda h: h.get("meta", {}).get("title") == title)
            assert "graphics" not in header, header

    with scenario("Animation frame edits respect the screen image quota"):
        for peer in peers:
            peer.send("scroll", delta=0, bottom=True)
            peer.barrier("quota-bottom")
        payload = zlib.compress(red * (1300 * 1000))
        options = "a=T,f=24,o=z,s=1300,v=1000,c=1,r=1,C=1,q=2"
        exchange(b"\x1b[H" + chunked(options + ",i=51", payload)
                 + b"\x1b[1;2H" + chunked(options + ",i=52", payload))
        for peer in peers:
            graphics(peer, lambda g: len(g["placements"]) == 2)
        exchange(command("a=f,i=51,r=1,f=32,s=1,v=1", green), b"\x1b_Gi=51,r=1;OK\x1b\\")
        exchange(command("a=f,i=52,r=1,f=32,s=1,v=1", green), b"\x1b_Gi=52;ENOSPC: image storage full\x1b\\")
        reconnect = Client(fixture, pane, address=address)
        stack.callback(reconnect.close)
        frame = graphics(reconnect, lambda g: len(g["images"]) == 2)
        assert sorted(im["format"] for im in frame["images"]) == ["rgb", "rgba"]
        reconnect.close()
        exchange(command("a=d,d=I,i=51,q=2") + command("a=d,d=I,i=52,q=2"))
        for peer in peers:
            graphics(peer, lambda g: not g["placements"])

    with scenario("Kitty animation advances while the PTY is idle"):
        exchange(b"\x1b[1;1H" +
                 command("a=T,f=32,i=44,s=1,v=1,c=1,r=1,C=1,q=2", red + b"\xff") +
                 command("a=a,i=44,r=1,z=80,q=2") +
                 command("a=f,i=44,f=32,s=1,v=1,z=80,q=2", green) +
                 command("a=a,i=44,s=3,q=2"))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == green for im in g["images"]))
            assert len(frame["placements"]) == 1

    with scenario("Kitty animation pauses outside every subscribed viewport and resumes on scroll"):
        animation_row = local.frame["scrollback"]["offset"]
        exchange(b"\x1b[4;1H" + b"\r\n" * 8
                 + command("a=T,f=24,i=46,s=1,v=1,c=1,r=1,C=1,q=2", blue))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            assert len(frame["placements"]) == 1
        time.sleep(0.25)  # Allow the post-output scrollback prefetch to settle.
        snapshots = fixture.rpc("status")["snapshots"]
        assert_stays(lambda: fixture.rpc("status")["snapshots"] == snapshots, 0.35,
                     message="An offscreen animation is still triggering snapshots")
        exchange(command("a=a,i=46,r=1,z=20000,q=2")
                 + command("a=f,i=46,f=32,s=1,v=1,z=20000,q=2", red + b"\xff")
                 + command("a=a,i=46,s=3,q=2"))
        graphics(remote, lambda g: any(im["pixels"] == blue + b"\xff" for im in g["images"]))
        local.send("scroll-to", row=animation_row, serial=2)
        frame = graphics(local, lambda g: any(im["pixels"] == green for im in g["images"]))
        assert len(frame["placements"]) == 1
        graphics(local, lambda g: any(im["pixels"] == red + b"\xff" for im in g["images"]))

    with scenario("A running animation leaves scrollback Find scans intact"):
        local.send("scroll", delta=0, bottom=True)
        local.barrier("find-bottom")
        exchange(b"\x1b[4;1H\r\n" + b"needle\r\n" * 512
                 + command("a=T,f=32,i=48,s=1,v=1,c=1,r=1,C=1,q=2", red + b"\xff")
                 + command("a=a,i=48,r=1,z=15,q=2")
                 + command("a=f,i=48,f=32,s=1,v=1,z=15,q=2", green)
                 + command("a=a,i=48,s=3,q=2"))
        graphics(local, lambda g: any(im["pixels"] == green for im in g["images"]))
        local.send("find", query="needle")
        local.until(lambda h: h.get("find") and h["find"]["total"] == 512 and not h["find"]["pending"])
