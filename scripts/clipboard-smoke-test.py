#!/usr/bin/env python3
"""Check terminal clipboard events through local and remote frontend streams."""
import base64
import os
from pathlib import Path
import subprocess
import socket
import time

from clipboard_test_support import check_html
from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty"))
check = artifacts / "clipboard-check"
subprocess.run(["cc", "-O2", "-std=c11", '-DGMUX_VERSION="test"', f"-I{prefix}/include",
                str(root / "scripts/clipboard-check.c"), f"-L{prefix}/lib", f"-Wl,-rpath,{prefix}/lib",
                "-lghostty-vt", "-o", str(check)], check=True)
subprocess.run([str(check)], check=True)


def osc(text, destination="c"):
    return f"\x1b]52;{destination};" + base64.b64encode(text.encode()).decode() + "\x1b\\"


with (artifacts / "clipboard-smoke.log").open("w") as log, AppFixture() as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    process = fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = ProgressProbe(fixture, pane)
    clients = []
    received = {}
    serial = 0

    def client(*, remote=False, capable=True):
        peer = Client(fixture, pane, address=address if remote else None)
        clients.append(peer)
        received[peer] = []
        if capable:
            peer.send("clipboard-writes", enabled=True)
        barrier(peer)
        return peer

    def collect(peer, predicate):
        def inspect(header):
            if header.get("copy") is not None:
                assert header["frame"] is None and not header["rows"], header
                received[peer].append(header["copy"])
            return predicate(header)
        return peer.until(inspect)

    def barrier(peer):
        marker = f"clipboard-barrier-{time.monotonic_ns()}"
        peer.send(marker)
        collect(peer, lambda h: marker in (h.get("error") or ""))

    def emit(data):
        global serial
        serial += 1
        title = f"CLIPBOARD_{serial}"
        probe.write(data + f"\x1b]2;{title}\x07")
        wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["title"] == title)

    def take(peer):
        barrier(peer)
        writes, received[peer] = received[peer], []
        return writes

    def expect(peer, text, destination="clipboard"):
        collect(peer, lambda h: h.get("copy") == dict(destination=destination, content=None if text is None else dict(text=text)))
        assert take(peer) == [dict(destination=destination, content=None if text is None else dict(text=text))]

    try:
        with scenario("detached writes are discarded before a frontend attaches"):
            emit(osc("detached"))
            observer = client(capable=False)
            first = client()
            second = client(remote=True)
            assert not take(first) and not take(second) and not take(observer)
        with scenario("one eligible recipient follows focus then connection order, including remote frontends"):
            observer.send("focused", active=True)
            barrier(observer)
            emit(osc("stable 界🙂e\u0301\n"))
            expect(first, "stable 界🙂e\u0301\n")
            assert not take(second) and not take(observer)
            second.send("focused", active=True)
            barrier(second)
            probe.burst(["\x1b]5", "2;p;", base64.b64encode("primary 界🙂".encode()).decode(), "\x1b", "\\"], .01)
            expect(second, "primary 界🙂", "primary")
            assert not take(first) and not take(observer)
            emit(osc("", "c"))
            expect(second, None)
            emit(osc("unsupported", "s") + "\x1b]52;c;/w==\x07\x1b]52;c;YQBi\x07")
            assert not take(first) and not take(second)
            # A focused window viewing a different pane is ineligible.
            second.send("subscribe", panes=[])
            barrier(second)
            emit(osc("visible owner"))
            expect(first, "visible owner")
            assert not take(second)
        with scenario("explicit copies and terminal writes retain request order"):
            emit("\x1b[H\x1b[2Jselection-text")
            first.output("selection-text")
            for end in [8, 2]:
                first.send("select", start=[0, 0], end=[end, 0], serial=end)
                first.send("copy")
            barrier(first)
            assert [w["content"]["text"] for w in received[first]] == ["selection", "sel"], received[first]
            received[first].clear()
            emit(osc("after copy"))
            expect(first, "after copy")
        with scenario("rich local and remote copies retain per-view bounds, escapes, styles and physical padding"):
            second.send("subscribe", panes=[pane])
            for peer in [first, second]:
                peer.send("resize", cols=10, rows=400)
                barrier(peer)
            cases = [
                ("01界e\u0301XYZ\r\nabcdefgh\r\nq\r\n\r\nlast", [2, 0], [4, 1], "rectangle", "界e\u0301\ncde"),
                ("01界e\u0301XYZ\r\nabcdefgh", [3, 0], [3, 1], "rectangle", "界\nd"),
                ("q\r\n\r\nlast", [1, 0], [3, 2], "rectangle", "   \n   \nast"),
                ("abcdefghijklmnopqrst12", [1, 0], [3, 2], "rectangle", "bcd\nlmn\n2  "),
                ("abcdefghijklmnopqrst12", [1, 0], [1, 2], "cell", "bcdefghijklmnopqrst12"),
                ("<>&\"'界🙂e\u0301", [0, 0], [9, 1], "cell", "<>&\"'界🙂e\u0301"),
                ("\x1b[31mred   \x1b[0m\r\n\r\n\x1b[1;3;4:3;9;48;2;11;22;33mlast  \x1b[0m", [0, 0], [9, 2], "cell", "red\n\nlast"),
                ("123456789界Z", [9, 0], [9, 1], "rectangle", " \n "),
                ("123456789界Z", [0, 0], [9, 0], "cell", "123456789界"),
                ("e" + "\u0301" * 80 + "界", [0, 0], [2, 0], "cell", "e" + "\u0301" * 64 + "界"),
                ("\x1b[48;2;11;22;33mA\x1b[0m\x1b[5GB", [0, 0], [9, 0], "cell", "A   B"),
                ("\x1b]8;;https://example.test/a?x=1&y=2\x07\x1b[31mA\x1b[32mB\x1b]8;;\x07\x1b[0m", [0, 0], [1, 0], "cell", "AB"),
                # Cross many native pages while unwrapping, with styled spaces.
                (("\x1b[31mabcd  \x1b[0mXYZ " * 300), [0, 0], [9, 299], "cell", "abcd  XYZ " * 299 + "abcd  XYZ"),
            ]
            for data, start, end, mode, expected in cases:
                emit("\x1b[3J\x1b[2J\x1b[H\x1b[0m" + data)
                for peer in [first, second]:
                    peer.send("clear-selection", serial=1)
                    peer.send("select", start=start, end=end, mode=mode, serial=2)
                    peer.send("copy", formats="text-html")
                    collect(peer, lambda h: h.get("copy") is not None)
                    writes = take(peer)
                    assert len(writes) == 1, writes
                    parsed = check_html(writes[0]["content"], expected)
                    if "48;2;11;22;33" in data:
                        assert "rgb(11, 22, 33)" in writes[0]["content"]["html"]
                    if expected == "A   B":
                        assert all("rgb(11, 22, 33)" not in "".join(styles) for text, styles in parsed.runs if "   " in text), parsed.runs
                assert not take(observer)
            # Each stream owns its tracked selection, even on the same pane.
            emit("\x1b[3J\x1b[2J\x1b[H\x1b[0m\x1b]10;#123456\x07\x1b]11;#654321\x07\x1b]4;1;#abcdef\x07\x1b[31m0123456789")
            for peer, start, end, expected in [(first, 0, 2, "012"), (second, 5, 8, "5678")]:
                peer.send("clear-selection", serial=3)
                peer.send("select", start=[start, 0], end=[end, 0], serial=4)
                peer.send("copy", formats="text-html")
                collect(peer, lambda h: h.get("copy") is not None)
                content = take(peer)[0]["content"]
                check_html(content, expected)
                assert "color: #abcdef" in content["html"] and "color:#123456" in content["html"]
                assert "background-color:#654321" in content["html"]
            # Rich selection data is transient and does not change OSC 52 policy.
            emit(osc("after rich copy"))
            expect(second, "after rich copy")
            assert not take(first)
        with scenario("styled expansion falls back to plain text on both transports"):
            for peer in [first, second]:
                peer.send("resize", cols=1000, rows=100)
                barrier(peer)
            emit("\x1b[3J\x1b[2J\x1b[H\x1b[0m" + "\x1b[1;3;4:3;9;38;2;11;22;33;48;2;44;55;66mx\x1b[38;2;77;88;99mx" * 50_000)
            for peer in [first, second]:
                peer.send("clear-selection", serial=5)
                peer.send("select", start=[0, 0], end=[999, 99], serial=6)
                peer.send("copy", formats="text-html")
                collect(peer, lambda h: h.get("copy") is not None)
                content = take(peer)[0]["content"]
                assert content == dict(text="x" * 100_000), (content.keys(), len(content["text"]))
            for peer in [first, second]:
                peer.send("resize", cols=80, rows=24)
                barrier(peer)
            second.send("subscribe", panes=[])
            barrier(second)
        with scenario("decoded payload limit accepts the boundary and rejects the whole oversized write"):
            text = "x" * (4 * 1024 * 1024)
            began = time.monotonic()
            emit(osc(text))
            expect(first, text)
            print(f"4 MiB OSC 52 delivery: {time.monotonic() - began:.3f}s including fixture I/O", flush=True)
            emit(osc(text + "x"))
            assert not take(first)
            emit(osc("recovered"))
            expect(first, "recovered")
        with scenario("disconnect, detach, and reconnect never replay old writes"):
            first.close()
            clients.remove(first)
            emit(osc("nobody visible"))
            assert not take(second) and not take(observer)
            second.send("subscribe", panes=[pane])
            barrier(second)
            assert not take(second)
            emit(osc("remote reconnect"))
            expect(second, "remote reconnect")
            second.close()
            clients.remove(second)
            emit(osc("no remote"))
            second = client(remote=True)
            assert not take(second)
            emit(osc("new remote write"))
            expect(second, "new remote write")
        with scenario("a frontend that stops reading cannot stall its PTY or retain an unbounded queue"):
            for peer in list(clients):
                peer.close()
            clients.clear()
            slow = client()
            slow.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            before = int(Path(f"/proc/{process.pid}/status").read_text().split("VmRSS:")[1].split()[0])
            chunk = osc("x" * (1024 * 1024))
            emit(chunk * 48)
            wait_for(lambda: fixture.rpc("status")["local_clients"] == 0)
            after = int(Path(f"/proc/{process.pid}/status").read_text().split("VmRSS:")[1].split()[0])
            peak = int(Path(f"/proc/{process.pid}/status").read_text().split("VmHWM:")[1].split()[0])
            print(f"Stopped reader RSS: before={before} KiB, after={after} KiB, peak={peak} KiB", flush=True)
            assert peak - before < 96 * 1024, (before, after, peak)
            slow.close()
            clients.remove(slow)
            replacement = client()
            assert not take(replacement)
            emit(osc("after slow reader"))
            expect(replacement, "after slow reader")
    finally:
        for peer in clients:
            peer.close()
print("PASS: local/remote OSC 52 routing, Unicode, clear, limits, ordered copy, transient disconnect and bounded backpressure")
