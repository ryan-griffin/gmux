#!/usr/bin/env python3
"""Check atomic OSC 8 maps through daemon streams, independent views and reconnect."""
import os
from pathlib import Path
import shlex
import subprocess

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty"))
check = artifacts / "hyperlink-check"
subprocess.run(["cc", "-O2", "-std=c11", '-DGMUX_VERSION="test"', f"-I{prefix}/include", str(root / "scripts/hyperlink-check.c"),
                f"-L{prefix}/lib", f"-Wl,-rpath,{prefix}/lib", "-lghostty-vt", "-o", str(check)], check=True)
subprocess.run([str(check)], check=True)

with AppFixture(prefix="gmux-hyperlinks-") as fixture:
    control = fixture.path / "control"
    applied = fixture.path / "applied"
    probe = fixture.path / "probe.py"
    probe.write_text("""import os, sys, time, tty
from pathlib import Path
control, applied = map(Path, sys.argv[1:])
tty.setraw(0)
previous = None
while True:
    current = control.read_bytes()
    if current != previous:
        os.write(1, current)
        applied.write_bytes(current)
        previous = current
    time.sleep(.005)
""")
    control.write_bytes(b'\x1b[H\x1b[2Jready')
    (fixture.path / "shell").write_text("#!/bin/sh\nexec python3 " + " ".join(map(shlex.quote, map(str, [probe, control, applied]))) + "\n")
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc("status"))
    pane = fixture.rpc("new", directory=str(root))["pane"]
    first, second = Client(fixture, pane), Client(fixture, pane)
    try:
        first.send("resize", cols=12, rows=3)
        first.output("ready")
        second.output("ready")
        def emit(data):
            control.write_bytes(data)
            wait_for(lambda: applied.read_bytes() == data)
        def link(uri, text):
            return b'\x1b]8;;' + uri.encode() + b'\x1b\\' + text.encode() + b'\x1b]8;;\x1b\\'
        def target(client, cell=0):
            return next((client.links["targets"][index] for start, end, index in client.links["spans"]
                         if start <= cell < end), None)
        with scenario("link-only target changes send atomic maps without rebuilding rows"):
            emit(b'\x1b[H\x1b[2J' + link("test:first", "same 界 text"))
            for client in (first, second):
                client.until(lambda h: target(client) == "test:first")
                assert target(client, 6) == "test:first", "Wide continuation lost its link"
            emit(b'\x1b[H' + link("test:second", "same 界 text"))
            for client in (first, second):
                header = client.until(lambda h: target(client) == "test:second")
                assert header["frame"] and header["rows"] == [], header
            reply = fixture.rpc("links", pane=pane, client=False)
            assert reply["hyperlinks"] == first.links
            cli = subprocess.check_output([str(cargo_binary("gmux")), "links", "--pane", str(pane)], env=fixture.env, text=True)
            assert 'test:second' in cli
        with scenario("separate scrolled viewports retain their own maps across holds and reconnect"):
            emit(b'\x1b[H\x1b[2J' + b''.join(link(f"test:row/{i}", f"row{i}") + b'\r\n' for i in range(8)))
            second.output("row7")
            first.output("row7")
            first.send("scroll-to", row=0, serial=1)
            first.until(lambda h: h["frame"] and h["scroll_serial"] == 1)
            assert target(first) == "test:row/0"
            for row, serial, shift in [(1, 2, 1), (0, 3, -1)]:
                first.send("scroll-to", row=row, serial=serial)
                header = first.until(lambda h: h["frame"] and h["scroll_serial"] == serial)
                assert header["row_shift"] == shift, header
                assert target(first) == f"test:row/{row}"
                assert target(first, 12) == f"test:row/{row + 1}"
            bottom = second.links.copy()
            emit(b'\x1b[?2026h\x1b[H' + link("test:hidden", "row6"))
            first.barrier(1); second.barrier(2)
            assert target(first) == "test:row/0" and second.links == bottom
            emit(b'\x1b[?2026l')
            second.until(lambda h: target(second) == "test:hidden")
            assert target(first) == "test:row/0"
            second.close()
            second = Client(fixture, pane)
            second.until(lambda h: target(second) == "test:hidden")
        with scenario("clear removes targets in every client"):
            first.send("clear-screen")
            for client in (first, second):
                client.until(lambda h: h["frame"] and not client.links["spans"])
                assert not client.links["targets"]
    finally:
        first.close(); second.close()
print("PASS: OSC 8 target-only wire changes, independent viewport holds, reconnect, CLI and clear")
