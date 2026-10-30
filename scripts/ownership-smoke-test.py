#!/usr/bin/env python3
"""Check execution ownership, client isolation, daemon identities, and durable IDs."""
import json
from pathlib import Path
import socket

from cargo_test_support import cargo_binary
from stream_test_support import Client, PROTOCOL_VERSION
from test_support import AppFixture, scenario, wait_for

binary = cargo_binary("gmux")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


def start(fixture, log):
    process = fixture.start(binary, "serve", stdout=log, stderr=log)
    wait_for(lambda: fixture.rpc("_hello"))
    return process


with (artifacts / "ownership-smoke.log").open("w") as log, \
        AppFixture(prefix="gmux-owner-a-") as a, AppFixture(prefix="gmux-owner-b-") as b:
    invalid_client = a.config / "clients/gtk/state.json"
    invalid_client.write_text("{invalid client state")
    a.settings_path.write_text("invalid preferences")
    first, second = start(a, log), start(b, log)
    with scenario("distinct daemons qualify equal numeric tab and pane IDs"):
        x, y = a.rpc("new"), b.rpc("new")
        assert (x["tab"], x["pane"]) == (y["tab"], y["pane"])
        identity, other = (f.rpc("_hello")["daemon"] for f in (a,b))
        assert identity != other
        listed = a.rpc("list")
        assert listed["tabs"][0]["reference"] == dict(daemon=identity, id=x["tab"])
        assert listed["tabs"][0]["panes"][0]["reference"] == dict(daemon=identity, id=x["pane"])
        with socket.socket(socket.AF_UNIX) as wrong:
            wrong.settimeout(3)
            wrong.connect(a.env["GMUX_SOCKET"] + ".stream")
            wrong.sendall((json.dumps(dict(method="attach", version=PROTOCOL_VERSION, daemon=other)) + "\n").encode())
            assert wrong.recv(4) == b"", "An attach for another daemon must disconnect"

    with scenario("daemon rejects organization and presentation without touching execution"):
        path = a.config / "execution.json"
        before = path.read_bytes()
        for method, params in [("workspace-add",dict(name="Project")),("move",dict(tab=x["tab"],workspace=1)),
                               ("_desktop-save",dict(desktop={},revision=0)),("_presentation",dict(session={})),
                               ("focus",dict(pane=x["pane"])),("new",dict(workspace=1))]:
            assert "error" in a.rpc(method, check=False, **params), method
        assert path.read_bytes() == before
        state = a.rpc("_state")
        assert "desktop" not in state and "catalog" not in state
        assert set(state["execution"]) == {"tabs"}, state
        assert all("workspace" not in tab and "active" not in tab for tab in state["execution"]["tabs"])
        assert invalid_client.read_text() == "{invalid client state"
        assert a.settings_path.read_text() == "invalid preferences"
        assert not (a.config / "desktop.json").exists() and not (a.config / "workspaces.json").exists()

    with scenario("deleted IDs remain reserved and client files are ignored on restart"):
        transient = a.rpc("new")
        greatest = a.rpc("split",pane=transient["pane"])["pane"]
        a.rpc("close-tab",tab=transient["tab"])
        a.rpc("stop")
        assert first.wait(timeout=8) == 0
        saved = json.loads((a.config / "execution.json").read_text())
        assert saved["last_id"] == greatest
        assert saved["scrollback"], "Stop must publish checkpoint references on disk"
        first = start(a,log)
        state = a.rpc("_state")
        assert set(state["execution"]) == {"tabs"}, state
        assert state["daemon"] == identity
        with scenario("control replies and reconnect streams contain only client execution fields"):
            client = Client(a, x["pane"])
            try:
                attached = client.state(state["revision"])
                assert set(attached["execution"]) == {"tabs"}, attached
                assert attached["daemon"] == identity
                reply = a.rpc("_execution", request=dict(method="rename", tab=x["tab"], name="Restored"))
                assert set(reply["execution"]) == {"tabs"}, reply
                assert reply["revision"] > state["revision"]
                updated = client.state(reply["revision"])
                assert updated["revision"] >= reply["revision"]
                assert updated["execution"] == reply["execution"]
                assert updated["daemon"] == identity
            finally:
                client.close()
        assert a.rpc("_hello")["daemon"] == identity
        assert a.rpc("status")["panes"] == 1
        replacement = a.rpc("new")
        assert replacement["tab"] > greatest and replacement["pane"] > greatest
        # Also exercise an empty topology, where no live ID can rebuild the allocator.
        a.rpc("close-tab", tab=x["tab"])
        a.rpc("close-tab", tab=replacement["tab"])
        a.rpc("stop")
        assert first.wait(timeout=8) == 0
        saved = json.loads((a.config / "execution.json").read_text())
        assert not saved["tabs"] and saved["last_id"] == replacement["pane"]
        first = start(a, log)
        assert a.rpc("_hello")["daemon"] == identity
        fresh = a.rpc("new")
        assert fresh["tab"] > replacement["pane"] and fresh["pane"] > fresh["tab"]
        a.rpc("stop")
        assert first.wait(timeout=8) == 0
        assert second.poll() is None and b.rpc("status")["panes"] == 1
        assert invalid_client.read_text() == "{invalid client state"

print("PASS: daemon execution ownership, client file isolation, identities, stale attach rejection, and durable IDs")
