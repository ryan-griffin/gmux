#!/usr/bin/env python3
"""Check live revisions, ordered mutation saves, exits, and shutdown under disk backpressure."""
from contextlib import contextmanager
import fcntl
import json
from pathlib import Path
import select
import socket

from cargo_test_support import cargo_binary
from history_test_support import HISTORY_SHELL, prepare_history
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for


@contextmanager
def pending(fixture, method, **params):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(8)
        connection.connect(fixture.env["GMUX_SOCKET"])
        connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
        yield connection


def reply(connection):
    return json.loads(connection.makefile("rb").readline())


def blocked(daemon):
    return any("lock" in path.read_text() for path in Path(f"/proc/{daemon.pid}/task").glob("*/wchan"))


def name(state, tab):
    return next(t["name"] for t in state["execution"]["tabs"] if t["id"] == tab)


artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)
with AppFixture(prefix="gmux-mutations-", shell_script=HISTORY_SHELL) as fixture, \
        (artifacts / "mutation-smoke.log").open("w") as log:
    prepare_history(fixture, 4000)

    def start():
        daemon = fixture.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("status"))
        return daemon

    def new():
        created = fixture.rpc("new")
        wait_for(lambda: "BOOT" in fixture.rpc("read", pane=created["pane"])["text"])
        return created["tab"], created["pane"]

    daemon = start()
    tab, pane = new()
    other_tab, other = new()
    third_tab, third = new()
    client = Client(fixture, pane)
    client.output("BOOT")
    execution = fixture.config / "execution.json"
    before = fixture.rpc("_state")
    with scenario("live publication, ordered commands, responsive PTYs, and exits during a pending save"):
        with (fixture.config / ".execution.json.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with pending(fixture, "_command", command=dict(method="rename", tab=tab, name="First")) as first:
                wait_for(lambda: blocked(daemon))
                live = fixture.rpc("_state")
                assert live["revision"] == before["revision"] + 1, live
                assert name(live, tab) == "First"
                assert client.state(live["revision"])["revision"] == live["revision"]
                assert not select.select([first], [], [], 0)[0], "Mutation replied before saving"
                with pending(fixture, "_command", command=dict(method="rename", tab=tab, name="Second")) as second:
                    # These read-only/send requests bypass the ordered mutation queue.
                    client.send("input", bytes=[ord("x")])
                    client.output("ACK000001")
                    assert name(fixture.rpc("_state"), tab) == "First"
                    # Exit two processes in separate owner passes. Their automatic
                    # saves follow the explicit write and may coalesce with each other.
                    fixture.rpc("send", pane=other, text="q")
                    wait_for(lambda: len(fixture.rpc("list")["tabs"]) == 2)
                    fixture.rpc("send", pane=third, text="q")
                    wait_for(lambda: len(fixture.rpc("list")["tabs"]) == 1)
                    exited = fixture.rpc("_state")
                    assert exited["revision"] == live["revision"] + 2
                    assert client.state(exited["revision"])["execution"] == exited["execution"]
                    assert not select.select([first, second], [], [], 0)[0]
                    fcntl.flock(lock, fcntl.LOCK_UN)
                    first_reply, second_reply = reply(first), reply(second)
                    assert first_reply["revision"] == exited["revision"], first_reply
                    assert name(first_reply, tab) == "First"
                    assert second_reply["revision"] == exited["revision"] + 1
                    assert name(second_reply, tab) == "Second"
                    assert not first_reply["result"].get("warning")
                    assert not second_reply["result"].get("warning")
        saved = json.loads(execution.read_text())
        assert [t["id"] for t in saved["tabs"]] == [tab]
        assert saved["tabs"][0]["name"] == "Second"
        assert saved["last_id"] >= third

    with scenario("execution save failures warn while live topology and revisions remain authoritative"):
        saved = execution.read_bytes()
        execution.write_text("{broken")
        failed = fixture.rpc("_command", command=dict(method="rename", tab=tab, name="Unsaved"))
        assert failed["result"]["ok"] and failed["result"].get("warning"), failed
        assert failed["revision"] == second_reply["revision"] + 1
        assert name(fixture.rpc("_state"), tab) == "Unsaved"
        assert execution.read_text() == "{broken"
        execution.write_bytes(saved)
        # An abandoned caller cannot suppress publication or persistence.
        with pending(fixture, "rename", tab=tab, name="Abandoned"):
            pass
        wait_for(lambda: name(fixture.rpc("_state"), tab) == "Abandoned")
        wait_for(lambda: json.loads(execution.read_text())["tabs"][0]["name"] == "Abandoned")

    with scenario("process exits remain live while settings wait for durability"):
        _, temporary = new()
        before = fixture.rpc("_state")
        with (fixture.config / ".daemon-settings.json.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with pending(fixture, "scrollback", max_lines=20) as settings:
                wait_for(lambda: blocked(daemon))
                old_limits = fixture.rpc("scrollback")["limits"]
                fixture.rpc("send", pane=temporary, text="q")
                wait_for(lambda: len(fixture.rpc("list")["tabs"]) == 1)
                assert fixture.rpc("_state")["revision"] == before["revision"] + 1
                assert fixture.rpc("scrollback")["limits"] == old_limits
                assert not select.select([settings], [], [], 0)[0]
                fcntl.flock(lock, fcntl.LOCK_UN)
                result = reply(settings)
                assert result["saved"] and result["applied"], result
                assert result["limits"]["max_lines"] == 20

    with scenario("exits during Stop retain restart topology and do not manufacture revisions"):
        before = fixture.rpc("_state")
        with (fixture.config / ".execution.json.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with pending(fixture, "stop") as stop:
                wait_for(lambda: fixture.rpc("_hello")["quitting"])
                wait_for(lambda: blocked(daemon))
                fixture.rpc("send", pane=pane, text="q")
                wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["exited"])
                during = fixture.rpc("_state")
                assert during["revision"] == before["revision"]
                assert during["execution"] == before["execution"]
                rejected = fixture.rpc("rename", tab=tab, name="Too late", check=False)
                assert "shutting down" in rejected["error"]
                assert not select.select([stop], [], [], 0)[0]
                fcntl.flock(lock, fcntl.LOCK_UN)
                stopped = reply(stop)
                assert stopped["ok"] and not stopped.get("warning"), stopped
        assert daemon.wait(timeout=8) == 0
        assert [t["id"] for t in json.loads(execution.read_text())["tabs"]] == [tab]
        client.close()
        daemon = start()
        wait_for(lambda: "BOOT" in fixture.rpc("read", pane=pane)["text"])

    with scenario("SIGTERM during a pending command drains writes before the shutdown checkpoint"):
        with (fixture.config / ".execution.json.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with pending(fixture, "rename", tab=tab, name="Signal shutdown") as rename:
                wait_for(lambda: blocked(daemon))
                daemon.terminate()
                wait_for(lambda: fixture.rpc("_hello")["quitting"])
                fixture.rpc("send", pane=pane, text="x")
                wait_for(lambda: "ACK000001" in fixture.rpc("read", pane=pane)["text"])
                assert daemon.poll() is None
                fcntl.flock(lock, fcntl.LOCK_UN)
                assert reply(rename)["ok"]
        assert daemon.wait(timeout=8) == 0
        saved = json.loads(execution.read_text())
        assert saved["tabs"][0]["name"] == "Signal shutdown"
        assert str(pane) in saved["scrollback"]

print("PASS: explicit mutation effects, revisions, persistence failures, process exits, and shutdown ordering")
