#!/usr/bin/env python3
"""Exercise deferred reads, desktop authority and fairness through real PTYs."""
import base64
import json
import os
from pathlib import Path
import subprocess
import time
from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

from clipboard_test_support import ReadProbe as Probe


def request(peer):
    return peer.until(lambda h: h.get("clipboard_read", {}).get("kind") == "request")["clipboard_read"]["request"]


def reply(peer, req, values=(), *, status="success", epoch=1, available=()):
    peer.send("clipboard-read-result", result=dict(id=req["id"], status=status, epoch=epoch,
              available=list(available), contents=[dict(mime=mime, len=len(data)) for mime, data in values]))
    for _, data in values:
        for start in range(0, len(data), 16384):
            chunk = data[start:start + 16384]
            peer.send("clipboard-read-data", id=req["id"], size=len(chunk))
            peer.socket.sendall(chunk)
    peer.send("clipboard-read-end", id=req["id"])


def kitty(mimes="text/plain", *, extra=""):
    return "\x1b]5522;type=read" + extra + ";" + base64.b64encode(mimes.encode()).decode() + "\x1b\\"


artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)
root = artifacts.parent
prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty"))
check = artifacts / "clipboard-read-check"
subprocess.run(["cc", "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", f"-I{prefix}/include", str(root / "scripts/clipboard-read-check.c"), f"-L{prefix}/lib", f"-Wl,-rpath,{prefix}/lib", "-lghostty-vt", "-o", str(check)], check=True)
subprocess.run([str(check)], check=True)
with (artifacts / "clipboard-read-smoke.log").open("w") as log, AppFixture() as fixture:
    fixture.rpc("connection-edit", connection="local", remote_access=dict(enabled=True,
                listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    address = wait_for(lambda: fixture.rpc("status")["bound_address"])
    measurements = {}
    for remote in (False, True):
        peers = []
        def client(probe, capable=True):
            peer = Client(fixture, probe.pane, address=address if remote else None)
            peers.append(peer)
            if capable:
                peer.send("clipboard-reads", enabled=True)
                peer.send("clipboard-changed", destination="clipboard", epoch=1)
                peer.send("clipboard-changed", destination="primary", epoch=1)
            peer.barrier(time.monotonic_ns())
            return peer
        try:
            with scenario("a deliberate paste supersedes another pane's unanswered read on the same frontend"):
                a = Probe(fixture); b = Probe(fixture); peer = client(a)
                peer.send("subscribe", panes=[a.pane, b.pane]); peer.barrier(1)
                for serial, rich in enumerate((False, True), 1):
                    b.write(("\x1b[?5522h" if rich else "\x1b[?5522l") + f"\x1b]2;MODE_{serial}\x07")
                    wait_for(lambda: fixture.rpc("_metadata", pane=b.pane)["meta"]["title"] == f"MODE_{serial}")
                    peer.pane = a.pane
                    before = len(a.bytes())
                    a.write("\x1b]52;c;?\x07\x1b[5n")
                    old = request(peer)
                    peer.pane = b.pane
                    peer.send("user-paste", serial=serial, destination="clipboard", epoch=1, mimes=["text/plain"])
                    wait_for(lambda: a.bytes()[before:] == b"\x1b]52;c;\x07\x1b[0n", timeout=3)
                    peer.until(lambda h: h["meta"]["id"] == a.pane
                               and h.get("clipboard_read") == dict(kind="cancel", id=old["id"]))
                    if rich:
                        event = wait_for(lambda: b.bytes() if b"status=DONE" in b.bytes() else None)
                        password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                        b.write(kitty(extra=":name=UGFzdGUgZXZlbnQ=:pw=" + password))
                        granted = request(peer); assert granted["granted"]
                        reply(peer, granted, [("text/plain", b"RICH_SPLIT_PASTE")])
                        wait_for(lambda: base64.b64encode(b"RICH_SPLIT_PASTE") in b.bytes())
                    else:
                        peer.until(lambda h: h.get("clipboard_read") == dict(kind="paste_text", serial=serial,
                                   destination="clipboard", epoch=1))
                        peer.send("user-paste-text", serial=serial, epoch=1, text="PLAIN_SPLIT_PASTE")
                        wait_for(lambda: b.bytes() == b"PLAIN_SPLIT_PASTE")

            with scenario(f"{'remote' if remote else 'local'} read pauses only its own PTY and preserves reply order"):
                p = Probe(fixture); peer = client(p)
                other = Probe(fixture)
                p.write("\x1b]52;p;?\x1b\\\x1b[5n\x1b]2;RESUMED\x07")
                req = request(peer)
                assert req["destination"] == "primary" and not req["granted"]
                before = fixture.rpc("_metadata", pane=p.pane)["meta"]["title"]
                assert before != "RESUMED"
                started = time.monotonic()
                other.write("\x1b]2;OTHER_ACTIVE\x07\x1b[5n")
                wait_for(lambda: fixture.rpc("_metadata", pane=other.pane)["meta"]["title"] == "OTHER_ACTIVE")
                wait_for(lambda: b"\x1b[0n" in other.bytes())
                measurements[f"{'remote' if remote else 'local'}_other_pane_ms"] = (time.monotonic() - started) * 1000
                assert not p.bytes()
                reply(peer, req, [("text/plain", "hello 界🙂".encode())])
                expected = b"\x1b]52;p;" + base64.b64encode("hello 界🙂".encode()) + b"\x1b\\\x1b[0n"
                wait_for(lambda: p.bytes() == expected)
                wait_for(lambda: fixture.rpc("_metadata", pane=p.pane)["meta"]["title"] == "RESUMED")

            with scenario("binary MIME reads are bounded, ordered and do not pass through JSON"):
                p = Probe(fixture); peer = client(p)
                payload = bytes(range(256)) * (4 * 1024 * 1024 // 256)
                p.write(kitty("application/octet-stream") + "\x1b[5n")
                req = request(peer)
                reply(peer, req, [("application/octet-stream", payload)])
                data = wait_for(lambda: p.bytes() if p.bytes().endswith(b"\x1b[0n") else None)
                packets = data.split(b"\x1b\\")
                decoded = b"".join(base64.b64decode(packet.split(b";", 2)[2]) for packet in packets if b"status=DATA" in packet)
                assert decoded == payload
                assert b"status=OK" in packets[0] and b"status=DONE" in packets[-2]

            with scenario("expanded duplicate MIME bounds reject the request without detaching its frontend"):
                p = Probe(fixture); peer = client(p)
                p.write(kitty("text/plain text/plain"))
                req = request(peer)
                reply(peer, req, [("text/plain", b"x" * (3 * 1024 * 1024))])
                wait_for(lambda: b"status=EBUSY" in p.bytes())
                p.write(kitty(extra=":id=recovered"))
                req = request(peer)
                reply(peer, req, [("text/plain", b"recovered")])
                wait_for(lambda: base64.b64encode(b"recovered") in p.bytes())

            with scenario("a genuine paste supersedes pending reads and bypasses unsolicited-read cooldown"):
                for deny in (False, True):
                    p = Probe(fixture); peer = client(p)
                    p.write("\x1b[?5522h\x1b]52;c;?\x07")
                    old = request(peer)
                    if deny:
                        reply(peer, old, status="denied")
                        wait_for(lambda: p.bytes() == b"\x1b]52;c;\x07")
                    peer.send("user-paste", serial=1, destination="clipboard", epoch=1, mimes=["text/plain"])
                    event = wait_for(lambda: p.bytes() if b"status=DONE" in p.bytes() else None)
                    assert event.startswith(b"\x1b]52;c;\x07")
                    password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                    p.write(kitty(extra=":name=UGFzdGUgZXZlbnQ=:pw=" + password))
                    req = request(peer); assert req["granted"]
                    reply(peer, req, [("text/plain", b"new paste")])
                    wait_for(lambda: base64.b64encode(b"new paste") in p.bytes())

            with scenario("targets listing preserves a genuine paste grant; scripted paste never creates one"):
                p = Probe(fixture); peer = client(p)
                p.write("\x1b[?5522h\x1b]2;PASTE_MODE\x07")
                wait_for(lambda: fixture.rpc("_metadata", pane=p.pane)["meta"]["title"] == "PASTE_MODE")
                peer.send("user-paste", serial=1, destination="clipboard", epoch=1, mimes=["text/plain"])
                event = wait_for(lambda: p.bytes() if b"status=DONE" in p.bytes() else None)
                password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                auth = ":name=UGFzdGUgZXZlbnQ=:pw=" + password
                p.write(kitty(".", extra=auth))
                listing = request(peer); assert listing["list"] and not listing["mimes"] and not listing["granted"]
                reply(peer, listing, available=["text/plain"])
                wait_for(lambda: p.bytes().count(b"status=DONE") == 2)
                p.write(kitty(extra=auth)); granted = request(peer); assert granted["granted"] and granted["epoch"] == 1
                reply(peer, granted, [("text/plain", b"paste")])
                wait_for(lambda: b"cGFzdGU=" in p.bytes())
                peer.send("paste", text="SCRIPTED", clipboard=True)
                wait_for(lambda: b"SCRIPTED" in p.bytes())
                assert p.bytes().count(b":pw=") == event.count(b":pw=")
                before = len(p.bytes())
                peer.send("user-paste", serial=2, destination="clipboard", epoch=1, mimes=["text/plain"])
                event = wait_for(lambda: p.bytes()[before:] if b"status=DONE" in p.bytes()[before:] else None)
                password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                p.write(kitty(extra=":name=UGFzdGUgZXZlbnQ=:pw=" + password))
                failed = request(peer); assert failed["granted"]
                reply(peer, failed, status="io_error", epoch=0)
                wait_for(lambda: b"status=EIO" in p.bytes())
                peer.barrier(99)

            with scenario("clipboard changes revoke paste authority and detach cancels a pending read"):
                p = Probe(fixture); peer = client(p)
                p.write("\x1b[?5522h\x1b]2;PASTE_MODE\x07")
                wait_for(lambda: fixture.rpc("_metadata", pane=p.pane)["meta"]["title"] == "PASTE_MODE")
                peer.send("user-paste", serial=1, destination="clipboard", epoch=1, mimes=["text/plain"])
                event = wait_for(lambda: p.bytes() if b"status=DONE" in p.bytes() else None)
                password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                p.write(kitty(extra=":loc=primary:name=UGFzdGUgZXZlbnQ=:pw=" + password))
                wait_for(lambda: b"status=EPERM" in p.bytes())
                # A new deliberate paste creates a fresh grant, then a clipboard change revokes it.
                before = len(p.bytes())
                peer.send("user-paste", serial=2, destination="clipboard", epoch=1, mimes=["text/plain"])
                event = wait_for(lambda: p.bytes()[before:] if b"status=DONE" in p.bytes()[before:] else None)
                password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                peer.send("clipboard-changed", destination="clipboard", epoch=2); peer.barrier(1)
                p.write(kitty(extra=":name=UGFzdGUgZXZlbnQ=:pw=" + password))
                req = request(peer); assert not req["granted"]
                peer.close(); peers.remove(peer)
                wait_for(lambda: b"status=EBUSY" in p.bytes() or b"status=EPERM" in p.bytes())
                p.write("\x1b]2;DETACH_RESUMED\x07")
                wait_for(lambda: fixture.rpc("_metadata", pane=p.pane)["meta"]["title"] == "DETACH_RESUMED")
            with scenario("only the chosen frontend can answer a read, without detach failover"):
                p = Probe(fixture); first = client(p); chosen = client(p)
                chosen.send("focused", active=True); chosen.barrier(2)
                p.write(kitty())
                req = request(chosen)
                reply(first, req, [("text/plain", b"FORGED")])
                first.barrier(3)
                assert not p.bytes()
                reply(chosen, req, [("text/plain", b"CHOSEN")])
                wait_for(lambda: base64.b64encode(b"CHOSEN") in p.bytes())
                assert base64.b64encode(b"FORGED") not in p.bytes()
                # The existing reply cannot be repeated to satisfy a new request.
                p.write(kitty(extra=":id=second"))
                newer = request(chosen)
                assert newer["id"] != req["id"]
                reply(chosen, req, [("text/plain", b"STALE")])
                chosen.barrier(4)
                assert base64.b64encode(b"STALE") not in p.bytes()
                chosen.close(); peers.remove(chosen)
                wait_for(lambda: b"status=EPERM:id=second" in p.bytes() or b"status=EBUSY:id=second" in p.bytes())

            with scenario("a stalled partial binary transfer times out without blocking another pane"):
                p = Probe(fixture); peer = client(p)
                other = Probe(fixture)
                p.write(kitty())
                req = request(peer)
                peer.send("clipboard-read-result", result=dict(id=req["id"], status="success", epoch=1, available=[], contents=[dict(mime="text/plain", len=16384)]))
                peer.send("clipboard-read-data", id=req["id"], size=16384)
                peer.socket.sendall(b"x")
                other.write("\x1b]2;TRANSFER_FAIR\x07")
                wait_for(lambda: fixture.rpc("_metadata", pane=other.pane)["meta"]["title"] == "TRANSFER_FAIR")
                wait_for(lambda: b"status=EBUSY" in p.bytes(), timeout=8)
        finally:
            for peer in peers: peer.close()
    (artifacts / "clipboard-read-measurements.json").write_text(json.dumps(measurements, indent=2) + "\n")
    print(json.dumps(measurements))
print("PASS: local/remote deferred reads, binary payloads, per-pane fairness, paste grants, generation revocation and detach")
