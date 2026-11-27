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
            assert placement["fit_rows"] == [0, 2], placement
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

    with scenario("Synchronized output captures an image first placed in the same PTY write"):
        title = exchange(b"\x1b[2;2H" + command("a=T,f=24,i=44,s=1,v=1,c=1,r=1,C=1,q=2", blue)
                         + b"\x1b[?2026h" + command("a=d,d=I,i=44,q=2") + b"HELD")
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            assert len(frame["placements"]) == 2, frame
            assert "HELD" not in peer.text()
        attached = Client(fixture, pane, address=address)
        stack.callback(attached.close)
        header = attached.until(lambda h: h.get("meta", {}).get("title") == title)
        assert header["frame"] is None, header
        attached.barrier("during-sync")
        assert attached.frame is None
        exchange(b"\x1b[?2026l\x1b[?2026h")
        for peer in peers:
            assert len(graphics(peer)["placements"]) == 1
            assert "HELD" in peer.text()
        assert graphics(attached)["images"][0]["pixels"] == png_pixel
        assert "HELD" in attached.text()
        exchange(b"\x1b[?2026l")
        attached.close()

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

    with scenario("Fractional Unicode crops stay within the source image and draw thin slices"):
        large = (red + b"\xff") * 6
        exchange(b"\x1b[?1049h"
                 + command("a=T,U=1,f=32,i=63,p=1,s=3,v=2,c=2,r=1,q=2", large)
                 + command("a=T,U=1,f=32,i=64,p=1,s=1,v=1,c=4,r=2,q=2", green)
                 + "\x1b[H\x1b[38;5;63m\U0010EEEE\u0305\u030d \x1b[38;5;64m\U0010EEEE\u0305\u030d\x1b[39m".encode())
        for peer in peers:
            frame = graphics(peer, lambda g: len(g["placements"]) == 2)
            sources = {p["image"]: p["source"] for p in frame["placements"]}
            assert {im["pixels"]: sources[im["key"]] for im in frame["images"]} == {
                large: [2, 0, 1, 2],
                green: [0, 0, 1, 1],
            }
            precise = {p["image"]: p["source_subpixels"] for p in frame["placements"]}
            assert {im["pixels"]: precise[im["key"]] for im in frame["images"]} == {
                large: [98304, 0, 98304, 131072],
                green: [16384, 0, 16384, 32768],
            }
        exchange(b"\x1b[?1049l")
        for peer in peers:
            assert graphics(peer)["images"][0]["pixels"] == png_pixel

    with scenario("Margin scrolling clips the letterbox while retaining the original image geometry"):
        exchange(b"\x1b[?1049h\x1b[2;4r\x1b[2;1H"
                 + command("a=T,f=24,i=80,s=8,v=8,c=1,r=3,C=1,q=2", red * 64))
        for peer in peers:
            assert graphics(peer, lambda g: len(g["placements"]) == 1)["placements"][0]["fit_rows"] == [0, 3]
        exchange(b"\x1b[4;1H\n")
        for peer in peers:
            frame = graphics(peer)
            placement, = frame["placements"]
            assert placement["cell"] == [0, 1] and placement["fit_rows"] == [1, 2], placement
            assert placement["size"] == [8, 48] and placement["source"] == [0, 0, 8, 8], placement
            assert not frame["images"], frame
        exchange(b"\x1b[?1049l\x1b[r")
        for peer in peers:
            assert graphics(peer)["images"][0]["pixels"] == png_pixel

    with scenario("Each subscriber sees the part of a trimmed image that fits its own font metrics"):
        for peer, width in zip(peers, (6, 10)):
            peer.send("resize", cols=12, rows=4, cw=width, ch=16)
            peer.barrier("subscriber-cell-width")
        exchange(b"\x1b[?1049h\x1b[2;4r\x1b[2;1H"
                 + command("a=T,f=24,i=80,s=1,v=2,c=1,r=3,C=1,q=2", red * 2))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == red * 2 for im in g["images"]))
        exchange(b"\x1b[4;1H\n\n")
        graphics(local, lambda g: not g["placements"])
        retained = graphics(remote, lambda g: any(p["fit_rows"] == [2, 1] for p in g["placements"]))
        assert not retained["images"], retained
        remote.send("resize", cols=12, rows=4, cw=6, ch=16)
        graphics(remote, lambda g: not g["placements"])
        remote.send("resize", cols=12, rows=4, cw=10, ch=16)
        graphics(remote, lambda g: any(im["pixels"] == red * 2 for im in g["images"]))
        for peer in peers:
            peer.send("resize", cols=12, rows=4, cw=8, ch=16)
            peer.barrier("restore-cell-width")
        exchange(b"\x1b[?1049l\x1b[r")
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == png_pixel for im in g["images"]))

    with scenario("Unicode tiles and thin column placements follow each subscriber's cell aspect"):
        exchange(b"\x1b[?1049h\x1b[H"
                 + command("a=T,U=1,f=24,i=89,s=1,v=2,c=1,r=1,q=2", red * 2)
                 + "\x1b[38;5;89m\U0010EEEE\u0305\u0305\x1b[39m".encode())
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == red * 2 for im in g["images"]))
            assert frame["placements"][0]["size"] == [8, 16], frame
        for width in (24, 16, 24):
            remote.send("resize", cols=12, rows=4, cw=width, ch=16)
            frame = graphics(remote, lambda g: g.get("unicode_cell_pixels") == [width, 16])
            placement, = frame["placements"]
            assert placement["size"] == [8, 16] and placement["offset"] == [(width - 8) // 2, 0], placement
            assert not frame["images"], frame
        thin = blue * 24
        exchange(b"\x1b[2;1H" + command("a=T,f=24,i=90,s=24,v=1,c=1,C=1,q=2", thin))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == thin for im in g["images"]))
            key = next(im["key"] for im in frame["images"] if im["pixels"] == thin)
            placement = next(p for p in frame["placements"] if p["image"] == key)
            assert placement["size"] == [8, 1] and placement["sizing"] == "columns", placement
        remote.send("resize", cols=12, rows=4, cw=8, ch=16)
        remote.barrier("restore-unicode-metrics")
        exchange(b"\x1b[?1049l")
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == png_pixel for im in g["images"]))

    with scenario("A relative image is anchored to the visual left edge of all parent runs"):
        parent = command("a=T,U=1,f=32,i=81,p=2,s=1,v=1,c=1,r=1,q=2", green)
        mark = "\x1b[38;5;81m\U0010EEEE\u0305\u0305\x1b[39m".encode()
        exchange(b"\x1b[?1049h\x1b[H" + parent + "A\u202eس".encode()
                 + b"\x1b[1;3H\x1b[58;5;2m" + mark + b"\x1b[59m"
                 + "مار\u202c".encode() + b"\x1b[2;4H" + mark
                 + command("a=T,f=24,i=82,p=2,P=81,Q=2,V=2,s=1,v=1,q=2", blue))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            child = next(im["key"] for im in frame["images"] if im["pixels"] == blue)
            relative = next(p for p in frame["placements"] if p["image"] == child)
            assert relative["cell"] == [3, 2], (relative, peer.text())
        # The implicit run changes parents even though the text is unchanged.
        exchange(command("a=p,U=1,i=81,p=1,c=1,r=1,q=2"))
        for peer in peers:
            frame = graphics(peer)
            relative = next(p for p in frame["placements"] if p["image"] == child)
            assert relative["cell"] == [4, 2], relative
        exchange(b"\x1b[?1049l")
        for peer in peers:
            assert graphics(peer)["images"][0]["pixels"] == png_pixel

    with scenario("An animated child beyond its viewport edge stays idle until visible"):
        exchange(b"\x1b[?1049h\x1b[1;12H" + parent + mark
                 + command("a=T,f=32,i=82,p=2,P=81,Q=2,H=1,s=1,v=1,q=2", blue + b"\xff")
                 + command("a=a,i=82,r=1,z=80,q=2")
                 + command("a=f,i=82,f=32,s=1,v=1,z=80,q=2", red + b"\xff")
                 + command("a=a,i=82,s=3,q=2"))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == green for im in g["images"]))
            assert len(frame["placements"]) == 1, frame
        time.sleep(0.25)
        snapshots = fixture.rpc("status")["snapshots"]
        assert_stays(lambda: fixture.rpc("status")["snapshots"] == snapshots, 0.35,
                     message="A relative image beyond the viewport is animating")
        exchange(b"\x1b[1;12H \x1b[H" + mark)
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == red + b"\xff" for im in g["images"]))
        exchange(b"\x1b[?1049l")
        for peer in peers:
            assert graphics(peer, lambda g: any(im["pixels"] == png_pixel for im in g["images"]))

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

    with scenario("Compressed transfers cannot inflate beyond the transfer limit"):
        compressed = zlib.compress(bytes(16 * 1024 * 1024 + 1))
        exchange(chunked("a=T,i=45,f=24,o=z,s=1,v=1", compressed),
                 b"\x1b_Gi=45;EINVAL: decompression failed\x1b\\")

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
        snapshots = fixture.rpc("status")["snapshots"]
        for _ in range(6):
            graphics(local, lambda g: bool(g["images"]))
        assert fixture.rpc("status")["snapshots"] - snapshots <= 9, "Animation is redrawing a view that cannot see it"

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

    with scenario("An animation stops when only its empty letterbox remains onscreen"):
        first_frame = (red + b"\xff") * 4
        second_frame = green * 4
        exchange(b"\x1bc" + command("a=T,i=83,f=32,s=4,v=1,c=1,r=4,C=1,q=2", first_frame)
                 + command("a=a,i=83,r=1,z=80,q=2")
                 + command("a=f,i=83,f=32,s=4,v=1,z=80,q=2", second_frame)
                 + command("a=a,i=83,s=3,q=2"))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == second_frame for im in g["images"]))
        exchange(b"\x1b[4;1H" + b"\r\n" * 3
                 + command("a=T,i=84,f=24,s=1,v=1,C=1,q=2", blue))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            assert len(frame["placements"]) == 1, frame
        time.sleep(0.25)
        snapshots = fixture.rpc("status")["snapshots"]
        assert_stays(lambda: fixture.rpc("status")["snapshots"] == snapshots, 0.35,
                     message="An animation is ticking behind its empty letterbox")

    with scenario("Resizing a frozen view preserves pixels deleted by held output"):
        exchange(b"\x1bc\x1b[1;3H" + command("a=T,i=85,f=24,s=1,v=1,c=1,r=1,C=1,q=2", red))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == red for im in g["images"]))
        remote.send("resize", cols=12, rows=4, cw=8, ch=16, width=8, height=64)
        graphics(remote, lambda g: not g["placements"])
        exchange(b"\x1b[?2026h" + command("a=d,d=I,i=85,q=2"))
        for width in (96, 8, 96):
            remote.send("resize", cols=12, rows=4, cw=8, ch=16, width=width, height=64)
            if width == 8:
                graphics(remote, lambda g: not g["placements"])
            else:
                graphics(remote, lambda g: any(im["pixels"] == red for im in g["images"]))
        remote.send("resize", cols=12, rows=4, cw=16, ch=16, width=192, height=64)
        resized = graphics(remote, lambda g: g.get("frozen_cell_pixels") == [8, 16])
        assert len(resized["placements"]) == 1 and not resized["images"], resized
        exchange(b"\x1b[?2026l")
        for peer in peers:
            graphics(peer, lambda g: not g["placements"])

    with scenario("A wider subscriber keeps the visible end of a column-sized image in scrollback"):
        remote.send("resize", cols=12, rows=4, cw=16, ch=16)
        remote.barrier("wider-font")
        exchange(b"\x1bc" + command("a=T,i=86,f=24,s=1,v=3,c=1,C=1,q=2", red * 3))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == red * 3 for im in g["images"]))
        row = remote.frame["scrollback"]["offset"]
        exchange(b"\x1b[4;1H" + b"\r\n" * 6)
        for peer in peers:
            graphics(peer, lambda g: not g["placements"])
        remote.send("scroll-to", row=row + 2, serial=70)
        visible = graphics(remote, lambda g: any(im["pixels"] == red * 3 for im in g["images"]))
        assert visible["placements"][0]["cell"] == [0, -2], visible
        remote.send("resize", cols=12, rows=4, cw=8, ch=16)
        graphics(remote, lambda g: not g["placements"])
        remote.send("scroll", delta=0, bottom=True)
        remote.barrier("restore-viewport")

    with scenario("Unicode animation stops when every subscriber clips its tile"):
        remote.send("resize", cols=1, rows=4, cw=8, ch=16, width=8, height=64)
        remote.barrier("narrow-font")
        exchange(b"\x1bc" + command("a=T,U=1,i=87,f=32,s=1,v=1,c=1,r=1,q=2", green)
                 + "\x1b[1;3H\x1b[38;5;87m\U0010EEEE\u0305\u0305\x1b[39m".encode()
                 + command("a=a,i=87,r=1,z=80,q=2")
                 + command("a=f,i=87,f=32,s=1,v=1,z=80,q=2", red + b"\xff")
                 + command("a=a,i=87,s=3,q=2"))
        graphics(local, lambda g: any(im["pixels"] == red + b"\xff" for im in g["images"]))
        row = local.frame["scrollback"]["offset"]
        exchange(b"\x1b[4;1H" + b"\r\n" * 8)
        graphics(local, lambda g: not g["placements"])
        remote.send("scroll-to", row=row, serial=71)
        packet = remote.until(lambda h: h.get("scroll_serial") == 71)
        assert not packet.get("graphics", {}).get("placements"), packet
        time.sleep(0.25)
        snapshots = fixture.rpc("status")["snapshots"]
        assert_stays(lambda: fixture.rpc("status")["snapshots"] == snapshots, 0.35,
                     message="A clipped Unicode tile is still animating")
        remote.send("resize", cols=12, rows=4, cw=8, ch=16)
        graphics(remote, lambda g: any(im["pixels"] == green for im in g["images"]))
        remote.send("scroll", delta=0, bottom=True)
        graphics(remote, lambda g: not g["placements"])

    with scenario("A new wide subscriber waits for a complete synchronized frame"):
        remote.send("resize", cols=12, rows=4, cw=24, ch=16)
        remote.barrier("wide-view")
        exchange(b"\x1bc" + command("a=T,i=91,f=24,s=1,v=3,c=1,C=1,q=2", blue * 3))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"] == blue * 3 for im in g["images"]))
        exchange(b"\x1b[4;1H" + b"\r\n" * 3)
        graphics(local, lambda g: not g["placements"])
        wide = graphics(remote, lambda g: any(p["cell"] == [0, -3] for p in g["placements"]))
        assert not wide["images"], wide
        title = exchange(b"\x1b[?2026hHELD")
        attached = Client(fixture, pane, address=address)
        stack.callback(attached.close)
        attached.send("resize", cols=12, rows=4, cw=24, ch=16)
        initial = attached.until(lambda h: h.get("meta", {}).get("title") == title)
        assert not initial.get("frame"), initial
        attached.barrier("await-frozen-image")
        assert attached.frame is None
        exchange(b"\x1b[?2026l")
        assert graphics(attached)["placements"][0]["cell"] == [0, -3]
        assert "HELD" in attached.text()
        attached.close()
        remote.send("resize", cols=12, rows=4, cw=8, ch=16)
        remote.barrier("restore-font")

    with scenario("The image count limit evicts the oldest unplaced image before a visible one"):
        exchange(b"\x1bc" + command("a=T,i=1,p=1,f=24,s=1,v=1,C=1,q=2", red)
                 + b"".join(command(f"a=t,i={slot},f=24,s=1,v=1,q=2", red)
                            for slot in range(2, 4097)))
        exchange(command("a=t,i=4097,f=24,s=1,v=1", blue), b"\x1b_Gi=4097;OK\x1b\\")
        exchange(command("a=p,i=2,C=1"), b"\x1b_Gi=2;ENOENT: image not found\x1b\\")
        exchange(command("a=p,i=1,p=1,C=1"), b"\x1b_Gi=1,p=1;OK\x1b\\")
        exchange(command("a=p,i=4097,C=1"), b"\x1b_Gi=4097;OK\x1b\\")
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            assert len(frame["placements"]) == 2

    with scenario("Placement storage allows replacements and rejects additional placements at capacity"):
        exchange(b"\x1bc" + command("a=T,i=57,p=1,f=24,s=1,v=1,c=1,r=1,C=1,q=2", red))
        exchange(b"".join(command(f"a=p,i=57,p={slot},c=1,r=1,C=1,q=2") for slot in range(2, 4097)))
        for peer in peers:
            graphics(peer, lambda g: len(g["placements"]) == 4096)
        exchange(command("a=p,i=57,p=4097,c=1,r=1,C=1"),
                 b"\x1b_Gi=57,p=4097;ENOSPC: placement storage full\x1b\\")
        exchange(b"\x1b[1;2H" + command("a=p,i=57,p=1,c=1,r=1,C=1"), b"\x1b_Gi=57,p=1;OK\x1b\\")
        for peer in peers:
            frame = graphics(peer, lambda g: any(p["cell"] == [1, 0] for p in g["placements"]))
            assert len(frame["placements"]) == 4096 and not frame["images"]

    with scenario("Animation frame slots are shared across images and reclaimed on deletion"):
        exchange(b"\x1bc" + command("a=t,i=58,f=32,s=1,v=1,q=2", green)
                 + command("a=t,i=59,f=32,s=1,v=1,q=2", green))
        exchange(command("a=f,i=58,f=32,s=1,v=1,q=2", green) * 32768)
        exchange(command("a=f,i=59,f=32,s=1,v=1", green),
                 b"\x1b_Gi=59,r=2;ENOSPC: animation frame storage full\x1b\\")
        exchange(command("a=d,d=f,i=58,r=32769,q=2") + command("a=f,i=59,f=32,s=1,v=1", green),
                 b"\x1b_Gi=59,r=2;OK\x1b\\")

    with scenario("Unicode runs leave room for fixed and relative images"):
        for peer in peers:
            peer.send("resize", cols=100, rows=100, cw=8, ch=16)
            peer.until(lambda h: h.get("frame") and h["frame"]["cols"] == 100)
        placeholders = b"".join(
            f"\x1b[{row};1H".encode() + ("\U0010EEEE\u0305\u0305 " * 50).encode()
            for row in range(1, 101)
        )
        exchange(b"\x1bc" + command("a=T,U=1,f=24,i=63,p=1,s=1,v=1,c=1,r=1,q=2", red)
                 + b"\x1b[38;5;63m" + placeholders + b"\x1b[39m\x1b[H"
                 + command("a=T,f=32,i=64,s=1,v=1,c=1,r=1,C=1,q=2", green)
                 + command("a=T,i=65,p=2,P=63,Q=1,H=1,f=24,s=1,v=1,c=1,r=1,C=1,q=2", blue))
        for peer in peers:
            frame = graphics(peer, lambda g: any(im["pixels"] == blue for im in g["images"]))
            assert len(frame["placements"]) == 4096
            assert any(im["pixels"] == green for im in frame["images"])

    with scenario("Only visible placements count toward each subscriber's viewport limit"):
        remote.send("resize", cols=1, rows=100, cw=8, ch=16, width=8, height=1600)
        remote.barrier("narrow-viewport")
        tile = "\U0010EEEE\u0305\u0305"
        offscreen = b"".join(
            f"\x1b[{row};3H".encode() + ((tile + " ") * 49).encode()
            for row in range(1, 100)
        )
        place = "a=T,U=1,f=24,i=66,p=1,s=1,v=1,c=1,r=1,q=2"
        exchange(b"\x1bc" + command(place, red) + b"\x1b[38;5;66m" + offscreen
                 + b"\x1b[100;1H" + tile.encode() + b"\x1b[39m")
        graphics(local, lambda g: len(g["placements"]) == 4096)
        visible = graphics(remote, lambda g: any(im["pixels"] == red for im in g["images"]))
        assert [p["cell"] for p in visible["placements"]] == [[0, 99]], visible
        exchange(command(place, blue) + b"\x1b[?2026h" + command("a=d,d=I,i=66,q=2"))
        frozen = graphics(remote, lambda g: any(im["pixels"] == blue for im in g["images"]))
        assert [p["cell"] for p in frozen["placements"]] == [[0, 99]], frozen
        exchange(b"\x1b[?2026l")
        for peer in peers:
            graphics(peer, lambda g: not g["placements"])
        remote.send("resize", cols=100, rows=100, cw=8, ch=16)
        remote.barrier("restore-width")

    with scenario("Repeated synchronized holds release superseded image generations"):
        payload = zlib.compress(green * (1500 * 1500))
        exchange(b"\x1bc" + chunked("a=T,i=66,f=32,o=z,s=1500,v=1500,c=1,r=1,C=1,q=2", payload))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"][:4] == green for im in g["images"]))
        red_pixel = red + b"\xff"
        blue_pixel = blue + b"\xff"
        edit = "a=f,i=66,r=1,f=32,s=1,v=1,q=2"
        begin = b"\x1b[?2026h"
        end = b"\x1b[?2026l"
        exchange((begin + command(edit, red_pixel) + end) * 16 + begin + command(edit, blue_pixel))
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"][:4] == red_pixel for im in g["images"]))
        assert fixture.rpc("status")["frontend_resources"]["graphics_hold_bytes"] == 9_000_000
        exchange(end)
        for peer in peers:
            graphics(peer, lambda g: any(im["pixels"][:4] == blue_pixel for im in g["images"]))
        assert fixture.rpc("status")["frontend_resources"]["graphics_hold_bytes"] == 0

    with scenario("Clearing the pane releases staged pixels after its viewers unsubscribe"):
        exchange(command(edit, green) + begin)
        for peer in peers:
            peer.send("subscribe", panes=[])
            peer.barrier("unsubscribe-during-sync")
        assert fixture.rpc("status")["frontend_resources"]["graphics_hold_bytes"] == 9_000_000
        local.send("clear-screen")
        local.barrier("clear-during-sync")
        assert fixture.rpc("status")["frontend_resources"]["graphics_hold_bytes"] == 0

    with scenario("A text-only hold gives newly attached viewers its frozen frame"):
        exchange(b"BEFORE\x1b[?2026hAFTER")
        attached = Client(fixture, pane, address=address)
        stack.callback(attached.close)
        attached.until(lambda h: h.get("frame"))
        assert "BEFORE" in attached.text() and "AFTER" not in attached.text()
        exchange(end)
