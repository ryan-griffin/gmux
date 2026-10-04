#!/usr/bin/env python3
"""Check daemon commands, unchanged frames, and shutdown after a save error."""
import json
from pathlib import Path
import socket
import struct
import subprocess
import time

from test_support import AppFixture, rpc, wait_for

root = Path(__file__).resolve().parent.parent
with AppFixture() as fixture:
    daemon = fixture.start(root / "target/debug/gmux-daemon", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: (fixture.path / "socket.stream").exists())

    def command(method, **params):
        return fixture.rpc("_command", command=dict(method=method, **params))

    workspace = command("workspace-new", name=" Project ", directory=str(root))["result"]["workspace"]
    duplicate = rpc(fixture.env["GMUX_SOCKET"], "workspace-new", name="project ", directory=str(root), check=False)
    assert "already exists" in duplicate["error"], duplicate
    created = command("new", name="Server", directory=str(root), workspace=workspace)
    tab, pane = created["result"]["tab"], created["result"]["pane"]
    split = command("split", pane=pane, down=True)
    second = split["result"]["pane"]
    assert split["session"]["tabs"][0]["layout"]["Split"]["vertical"]
    presentation = split["session"]
    presentation["tabs"][0]["layout"]["Split"]["vertical"] = False
    fixture.rpc("_presentation", session=presentation)
    assert fixture.rpc("list")["tabs"][0]["layout"]["Split"]["vertical"], "Presentation saves must preserve daemon topology"
    command("rename", tab=tab, name="Builds")
    command("workspace-edit", workspace=workspace, name="Renamed", directory=str(root))
    closed = command("close", pane=second)
    assert closed["session"]["tabs"][0]["layout"] == {"Pane": pane}
    assert closed["session"]["tabs"][0]["name"] == "Builds"
    # Typed commands keep missing and explicit-null workspace selection distinct.
    fixture.rpc("focus", workspace=workspace)
    inherited = command("new", directory=str(root))
    assert inherited["result"]["workspace"] == workspace
    unassigned = command("new", directory=str(root), workspace=None)
    assert unassigned["result"]["workspace"] is None
    command("close-tab", tab=inherited["result"]["tab"])
    command("close-tab", tab=unassigned["result"]["tab"])
    invalid = rpc(fixture.env["GMUX_SOCKET"], "move", tab=tab, check=False)
    assert "error" in invalid

    catalog_path = fixture.path / "config/gmux/workspaces.json"
    saved_catalog = catalog_path.read_bytes()
    catalog_path.write_bytes(b'{"workspaces":[')
    failed = rpc(fixture.env["GMUX_SOCKET"], "workspace-new", name="Must not replace", check=False)
    assert "workspaces.json" in failed["error"] and "unchanged" in failed["error"]
    assert catalog_path.read_bytes() == b'{"workspaces":['
    catalog_path.write_bytes(saved_catalog)

    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(str(fixture.path / "socket.stream"))
        def send(method, **params):
            stream.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
        def exact(size):
            data = bytearray()
            while len(data) < size:
                part = stream.recv(size-len(data))
                assert part
                data.extend(part)
            return data
        def packet():
            size, = struct.unpack("<I", exact(4))
            data = exact(size)
            header_size, = struct.unpack("<I", data[:4])
            return json.loads(data[4:4+header_size])
        send("attach", version=5)
        send("subscribe", panes=[pane])
        wait_for(lambda: packet()["frame"])
        time.sleep(.1)
        # Drain shell startup and the initial subscription before testing releases.
        stream.settimeout(.15)
        try:
            while packet():
                pass
        except socket.timeout:
            pass
        for _ in range(5):
            send("key", pane=pane, name="a", cp=97, mods=0, release=True, text="")
            time.sleep(.025)
        try:
            while header := packet():
                assert header["frame"] is None, "A key release with no output repainted the viewport"
        except socket.timeout:
            pass
        # A scroll acknowledgement must still arrive even when no cell changes.
        send("scroll-to", pane=pane, row=0, serial=1)
        stream.settimeout(3)
        header = wait_for(lambda: (h if (h := packet())["scroll_serial"] == 1 else None))
        assert header["frame"] is not None
        # Reject an oversized legacy command, then accept the next command.
        send("paste", pane=pane, text="x" * 1_100_000, clipboard=True)
        header = wait_for(lambda: (h if (h := packet())["error"] else None))
        assert "too large" in header["error"]
        send("scroll-to", pane=pane, row=0, serial=2)
        wait_for(lambda: packet()["scroll_serial"] == 2)
        assert fixture.rpc("daemon-info")["attached"]

        # A rejected chunked operation must not paste a partial prefix.
        send("paste-begin", pane=pane, size=1, clipboard=True)
        send("paste-data", pane=pane, text="xx")
        header = wait_for(lambda: (h if (h := packet())["error"] else None))
        assert "Invalid paste chunk" in header["error"]
        send("paste-end", pane=pane)
        header = wait_for(lambda: (h if (h := packet())["error"] else None))
        assert "No paste is in progress" in header["error"]

        # Ghostty encodes one bracketed paste, even across many wire chunks.
        import hashlib
        import shlex
        payload = b"x" * (4 * 1024 * 1024)
        code = ("import os,tty,hashlib; tty.setraw(0); "
                "os.write(1,b'\\x1b[?2004hPASTE_READY\\r\\n'); "
                f"n={len(payload)+12}; data=bytearray(); "
                "exec('while len(data)<n:\\n data.extend(os.read(0,min(65536,n-len(data))))'); "
                "os.write(1,b'\\x1b[?2004lPASTE_RESULT:'+hashlib.sha256(data).hexdigest().encode()+b'\\r\\n')")
        fixture.rpc("send", pane=pane, text="python3 -c " + shlex.quote(code), paste=True, enter=True)
        wait_for(lambda: "PASTE_READY" in fixture.rpc("read", pane=pane)["text"])
        send("paste-begin", pane=pane, size=len(payload), clipboard=True)
        for start in range(0, len(payload), 16384):
            send("paste-data", pane=pane, text=payload[start:start+16384].decode())
        send("paste-end", pane=pane)
        expected = hashlib.sha256(b"\x1b[200~" + payload + b"\x1b[201~").hexdigest()
        wait_for(lambda: expected in fixture.rpc("read", pane=pane)["text"])
        assert fixture.rpc("daemon-info")["attached"]

    session_path = fixture.path / "config/gmux/session.json"
    saved_session = session_path.read_bytes()
    session_path.write_bytes(b'{"tabs":')
    changed = command("new", name="Protected state", directory=str(root))
    assert "session.json" in changed["result"]["warning"]
    assert session_path.read_bytes() == b'{"tabs":'
    session_path.write_bytes(saved_session)
    command("close-tab", tab=changed["result"]["tab"])
    blocker = fixture.path / "config/gmux/session.tmp"
    blocker.mkdir()
    try:
        changed = command("new", name="Unsaved", directory=str(root))
        assert changed["result"]["warning"] and changed["session"]["tabs"][0]["name"] == "Unsaved"
        closed = command("close-tab", tab=changed["result"]["tab"])
        assert closed["result"]["ok"] and closed["result"]["warning"]
        result = fixture.rpc("quit")
        assert result["ok"] and result["warning"], result
        assert daemon.wait(timeout=8) == 0
        assert not (fixture.path / "socket").exists()
    finally:
        blocker.rmdir()
for name in ["settings.json", "session.json", "workspaces.json"]:
    with AppFixture() as fixture:
        damaged = fixture.path / "config/gmux" / name
        damaged.write_bytes(b'{"broken":')
        with (fixture.path / "error.log").open("w") as log:
            daemon = fixture.start(root / "target/debug/gmux-daemon", stdout=log, stderr=log)
            assert daemon.wait(timeout=8) != 0
        assert damaged.read_bytes() == b'{"broken":'
        assert name in (fixture.path / "error.log").read_text()
print("PASS: typed commands, protected state, bounded 4 MiB bracketed paste, oversized-command recovery, unchanged releases, scroll acknowledgements, and Quit after a save error")
