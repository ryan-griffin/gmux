#!/usr/bin/env python3
"""Exercise independent daemon clients over shared PTYs and layout snapshots."""
import json
from pathlib import Path
import shlex
import socket
import subprocess
import time

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, assert_stays, scenario, wait_for


root = Path(__file__).resolve().parent.parent
with AppFixture(prefix="gmux-multi-client-") as fixture:
    fixture.start(cargo_binary("gmux"), "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc("daemon-info"))
    created = fixture.rpc("_command", command=dict(method="new", directory=str(root)))
    pane, tab = created["result"]["pane"], created["result"]["tab"]
    first, second = Client(fixture, pane), Client(fixture, pane)
    try:
        with scenario("two clients see shared output and can both send input"):
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 2)
            first.send("input", bytes=list(b"printf 'FIRST_INPUT\\n'\n"))
            first.output("FIRST_INPUT")
            second.output("FIRST_INPUT")
            second.send("input", bytes=list(b"printf 'SECOND_INPUT\\n'\n"))
            first.output("SECOND_INPUT")
            second.output("SECOND_INPUT")

        with scenario("interleaved paste transfers remain isolated per connection"):
            text = "printf 'FIRST_PASTE\\n'"
            first.send("paste-begin", size=len(text), enter=True, bottom=True)
            first.send("paste-data", text=text[:8])
            first.send("scroll-to", row=0, serial=5)
            first.until(lambda h: h["meta"]["id"] == pane and h["scroll_serial"] == 5)
            other = "printf 'SECOND_PASTE\\n'"
            second.send("paste-begin", size=len(other), enter=True, bottom=True)
            second.send("paste-data", text=other)
            second.send("paste-end")
            first.output("SECOND_PASTE")
            second.output("SECOND_PASTE")
            first.send("paste-data", text=text[8:])
            first.send("paste-end")
            first.output("FIRST_PASTE")
            second.output("FIRST_PASTE")

        with scenario("scrolling, selection, search, and clipboard replies stay local"):
            first.send("input", bytes=list(b"for i in $(seq 1 80); do printf 'line %02d\\n' \"$i\"; done\n"))
            first.output("line 80")
            second.output("line 80")
            first.send("scroll-to", row=0, serial=10)
            first.until(lambda h: h["meta"]["id"] == pane and h["scroll_serial"] == 10)
            assert first.frame["scrollback"]["offset"] == 0
            row, line = next((row, line) for row, line in enumerate(first.text().splitlines()) if "FIRST_INPUT" in line)
            col = line.index("FIRST_INPUT")
            first.send("select", start=[col, row], end=[col + 10, row])
            first.until(lambda h: h["meta"]["id"] == pane and h["selection"] is not None)
            first.send("copy")
            copied = first.until(lambda h: h["meta"]["id"] == pane and h["copy"] is not None)
            assert "FIRST_INPUT" in copied["copy"], copied
            second.send("copy")
            rejected = second.until(lambda h: h["meta"]["id"] == pane and h["error"] is not None)
            assert rejected["error"] == "Select terminal text to copy it" and rejected["selection"] is None
            first.send("find", query="line")
            first.until(lambda h: h["meta"]["id"] == pane and h["find"] and h["find"]["total"] == 80 and not h["find"]["pending"])
            second.send("find", query="FIRST_INPUT")
            second.until(lambda h: h["meta"]["id"] == pane and h["find"] and h["find"]["total"] == 1 and not h["find"]["pending"])
            second.send("input", bytes=list(b"printf 'SHARED_WHILE_SCROLLED\\n'\n"))
            second.output("SHARED_WHILE_SCROLLED")
            first.until(lambda h: h["meta"]["id"] == pane and h["find"] and not h["find"]["pending"])
            assert first.frame["scrollback"]["offset"] == 0 and "SHARED_WHILE_SCROLLED" not in first.text()

        with scenario("clear screen removes shared history and stale client presentation"):
            first.send("clear-screen")
            for client in (first, second):
                cleared = client.until(lambda h: h["meta"]["id"] == pane and h["frame"]
                    and h["frame"]["scrollback"]["total"] == h["frame"]["rows"]
                    and h["find"] and h["find"]["total"] == 0 and not h["find"]["pending"])
                assert client.text().strip() == "", client.text()
                assert cleared["selection"] is None
                assert client.frame["scrollback"]["offset"] == 0
            first.send("input", bytes=list(b"printf 'line AFTER_CLEAR\\n'\n"))
            first.until(lambda h: h["meta"]["id"] == pane and h["find"]
                        and h["find"]["total"] == 1 and not h["find"]["pending"]
                        and "AFTER_CLEAR" in first.text())
            second.output("AFTER_CLEAR")
            assert not fixture.rpc("_metadata", pane=pane)["meta"]["exited"]

        with scenario("focused client supplies the PTY size without resize contention"):
            first.send("focused", active=True)
            first.send("resize", cols=50, rows=12)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [50, 12])
            second.send("resize", cols=70, rows=20)
            second.send("copy")
            second.until(lambda h: h["meta"]["id"] == pane and h["error"] is not None)
            assert fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [50, 12]
            first.send("focused", active=False)
            second.send("resize", cols=72, rows=22)
            second.barrier(20)
            assert_stays(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [50, 12], 0.1,
                         message="Losing application focus released the last focused client's size")
            second.send("resize", cols=70, rows=20)
            second.send("focused", active=True)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [70, 20])

        with scenario("shared execution topology and name changes reach every client"):
            for command in [dict(method="split", pane=pane), dict(method="rename", tab=tab, name="Shared tab")]:
                result = fixture.rpc("_command", command=command)
                states = []
                for client in (first, second):
                    state = client.state(result["revision"])
                    assert state["session"]["tabs"] == result["session"]["tabs"]
                    states.append(state)
                assert states[0] == states[1], "Clients received different snapshots of the same revision"
            split_pane = result["session"]["tabs"][0]["layout"]["Split"]["second"]["Pane"]
            result = fixture.rpc("_command", command=dict(method="close", pane=split_pane))
            for client in (first, second):
                assert client.state(result["revision"])["session"]["tabs"][0]["layout"] == {"Pane": pane}

        with scenario("a disconnect leaves the other client and shell running"):
            first.send("focused", active=True)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [50, 12])
            first.close()
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 1)
            wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["size"] == [70, 20],
                     message="Disconnecting the size owner did not apply the survivor's cached size")
            second.send("input", bytes=list(b"printf 'SURVIVOR_INPUT\\n'\n"))
            second.output("SURVIVOR_INPUT")
            assert not fixture.rpc("_metadata", pane=pane)["meta"]["exited"]
    finally:
        first.close()
        second.close()

artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
with AppFixture(prefix="gmux-size-owner-") as fixture, (artifacts / "multi-client-ownership.log").open("w") as log:
    fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    wait_for(lambda: fixture.rpc("daemon-info"))
    pane = fixture.rpc("_command", command=dict(method="new", directory=str(root)))["result"]["pane"]
    other = fixture.rpc("_command", command=dict(method="new", directory=str(root)))["result"]["pane"]
    first, second, third = (Client(fixture, pane) for _ in range(3))
    clients = [first, second, third]

    def size():
        return fixture.rpc("_metadata", pane=pane)["meta"]["size"]

    def expect_size(cols, rows):
        wait_for(lambda: size() == [cols, rows], message=f"Expected owner size {[cols, rows]}, got {size()}")

    def expect_frame(client, cols, rows):
        if client.frame is None or (client.frame["cols"], client.frame["rows"]) != (cols, rows):
            client.until(lambda h: h["frame"] and (h["frame"]["cols"], h["frame"]["rows"]) == (cols, rows))

    try:
        first.send("focused", active=True)
        first.send("resize", cols=50, rows=12)
        expect_size(50, 12)
        second.send("resize", cols=70, rows=20, cw=10, ch=20, width=700, height=400)
        second.barrier(1)

        with scenario("switching away hands size to a visible unfocused client"):
            first.send("subscribe", panes=[other])
            expect_size(70, 20)
            expect_frame(second, 70, 20)
            probe = fixture.path / "winsize.json"
            script = "import fcntl, json, struct, termios; print(json.dumps(struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)))))"
            second.send("input", bytes=list(("python3 -c " + shlex.quote(script) + " > " + shlex.quote(str(probe)) + "\n").encode()))
            wait_for(lambda: probe.exists() and probe.read_text().strip())
            assert json.loads(probe.read_text()) == [20, 70, 700, 400], "The PTY did not receive the complete cached geometry"
            first.send("resize", cols=55, rows=13)
            first.barrier(2)
            assert size() == [70, 20], "A hidden pane's resize displaced its visible owner"

        with scenario("unfocused subscribers have a stable owner and hand off on unsubscribe"):
            third.send("resize", cols=90, rows=30)
            third.barrier(3)
            assert size() == [70, 20], "Unfocused clients contended for size"
            second.send("resize", cols=72, rows=22)
            expect_size(72, 22)
            second.send("subscribe", panes=[])
            expect_size(90, 30)
            third.send("subscribe", panes=[])
            third.barrier(4)
            assert size() == [90, 30], "No visible request should preserve the installed size"
            first.send("subscribe", panes=[pane])
            expect_size(55, 13)
            second.send("subscribe", panes=[pane])
            third.send("subscribe", panes=[pane])

        with scenario("clients without cached sizes preserve geometry and do not block valid requests"):
            unsized = Client(fixture, pane)
            clients.append(unsized)
            unsized.send("focused", active=True)
            unsized.barrier(5)
            assert size() == [55, 13], "An unsized focused client blocked the fallback owner"
            for client in (first, second, third):
                client.send("subscribe", panes=[])
                client.barrier(6)
            assert size() == [90, 30], "With no cached visible size, keep the last installed geometry"
            unsized.send("resize", cols=80, rows=24)
            expect_size(80, 24)
            unsized.close()
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 3)
            assert size() == [80, 24], "A disconnect without a sized survivor changed geometry"
            for client in (first, second, third):
                client.send("subscribe", panes=[pane])
                client.barrier(7)
            expect_size(55, 13)

        with scenario("a failed output flush detaches the owner and updates surviving frames"):
            third.send("focused", active=True)
            expect_size(90, 30)
            third.barrier(8)
            first.barrier(8)
            # Keep the request half open so detachment comes from writing a reply.
            third.socket.shutdown(socket.SHUT_RD)
            third.send("scroll-to", row=0, serial=9)
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 2)
            expect_size(55, 13)
            expect_frame(first, 55, 13)

        with scenario("a slow size owner hands off when its pending output exceeds the limit"):
            slow = Client(fixture, pane)
            clients.append(slow)
            slow.send("focused", active=True)
            slow.send("resize", cols=512, rows=128)
            expect_size(512, 128)
            slow.barrier(10)
            # Reattachment requests full frames without changing the surviving views.
            for serial in range(11, 41):
                if fixture.rpc("daemon-info")["frontends"] == 2:
                    break
                slow.send("subscribe", panes=[])
                slow.send("subscribe", panes=[pane])
                first.barrier(serial)
                time.sleep(0.025)
            wait_for(lambda: fixture.rpc("daemon-info")["frontends"] == 2)
            expect_size(55, 13)
            expect_frame(first, 55, 13)
            assert "Frontend is too slow" in (artifacts / "multi-client-ownership.log").read_text(), "The owner did not detach through the pending-output limit"
    finally:
        for client in clients:
            client.close()
print("PASS: shared sessions and layouts with independent daemon client views")
