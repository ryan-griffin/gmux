#!/usr/bin/env python3
"""Exercise retained OSC 7501 through native parsing, daemon ownership and streams."""
import base64
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import time

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
binary = cargo_binary("gmux")


def osc(state, **fields):
    for key in ("msg", "title"):
        if key in fields:
            fields[key] = base64.b64encode(fields[key].encode()).decode()
    return "\x1b]7501;" + ":".join(f"{key}={value}" for key, value in dict(state=state, **fields).items()) + "\x07"


def sample(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return sum(map(int, fields[11:13]))


with (artifacts / "program-status-smoke.log").open("w") as log, AppFixture() as owner, AppFixture() as receiver:
    owner.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    owner.start(binary, "serve", stdout=log, stderr=log)
    receiver.start(binary, "serve", stdout=log, stderr=log)
    address = wait_for(lambda: owner.rpc("status")["bound_address"])
    receiver.rpc("connection-add", name="owner", address=address, authentication=dict(mode="none"))
    pane = owner.rpc("new", directory=str(owner.path))["pane"]
    probe = ProgressProbe(owner, pane)
    clients = [Client(owner, pane), Client(receiver, pane, connection="owner")]

    def metadata():
        return owner.rpc("metadata", pane=pane)

    def expected(state, identifier="", count=1):
        return wait_for(lambda: (s := metadata()["meta"]["program_status"])
                        and s["record"]["state"] == state and s["record"]["id"] == identifier
                        and s["count"] == count and s)

    def receive(status):
        for peer in clients:
            header = peer.until(lambda h: h["meta"]["id"] == pane and h["meta"]["program_status"] == status)
            assert header["frame"] is None and not header["rows"], header
            assert not header["notifications"] and header["meta"]["unread_sequence"] == 0, header

    try:
        for peer in clients:
            peer.until(lambda h: h["meta"]["id"] == pane and h["frame"])
            peer.barrier("ready")
        with scenario("actionable children, replacement, inherited app and local/remote source metadata"):
            snapshots = owner.rpc("status")["snapshots"]
            probe.write(osc("idle", app="deploy") + osc("blocked", id="region/test", kind="auth", msg="Login?", progress=42))
            status = expected("blocked", "region/test", 2)
            receive(status)
            assert status["record"]["app"] == "deploy", status
            tab = owner.rpc("list")["tabs"][0]
            assert tab["program_status"] == dict(pane=pane, status=status)
            full = metadata()["program_status_records"]
            assert full[1]["app"] is None and full[1]["message"] == "Login?", full
            remote = json.loads(subprocess.check_output([str(binary), "--connection", "owner", "metadata", "--pane", str(pane)], env=receiver.env))
            assert remote["program_status_records"] == full, remote
            cli = json.loads(subprocess.check_output([str(binary), "metadata", "--pane", str(pane)], env=owner.env))
            assert cli["program_status_records"] == full
            probe.write(osc("working", id="region/test"))
            status = expected("working", "region/test", 2)
            receive(status)
            assert status["record"]["progress"] is None and not status["record"]["message"]
            probe.write(osc("clear", id="region") + osc("done", id="regional") + osc("clear", id="region"))
            receive(expected("done", "regional", 2))
            assert owner.rpc("status")["snapshots"] == snapshots

        with scenario("prompt cleanup, independent legacy progress and shared interaction acknowledgement"):
            probe.write(osc("blocked", id="worker") + "\x1b]133;A\x07" + "\x1b]9;4;1;67\x07")
            status = expected("done", "regional", 2)
            assert metadata()["meta"]["progress"] == dict(state="determinate", percentage=67)
            for peer in clients:
                peer.barrier("prompt")
                peer.send("mark-read", through=0)
                peer.send("key", name="a", unshifted=97, mods=0, action="release", text="")
                peer.barrier("read")
            assert metadata()["meta"]["program_status"] == status
            clients[1].send("key", name="a", unshifted=97, mods=0, action="press", text="a")
            receive(expected("idle"))
            probe.write(osc("clear"))
            wait_for(lambda: metadata()["meta"]["program_status"] is None)
            receive(None)

        with scenario("bounded cap, hidden/detached reports and reconnect without replay"):
            for peer in clients:
                peer.send("subscribe", panes=[])
                peer.barrier("hidden")
            before = owner.rpc("status")
            probe.write("".join(osc("working", id=f"task{i:02}", msg="x" * 2048, title="t" * 192) for i in range(70)))
            status = expected("working", "task06", 64)
            receive(status)
            full = metadata()["program_status_records"]
            assert len(full) == 64 and full[0]["id"] == "task06"
            memory = next(p for p in owner.rpc("status")["terminal_memory"] if p["pane"] == pane)["program_status_bytes"]
            assert 64 * 2048 < memory < 180_000, memory
            assert len(json.dumps(metadata()).encode()) < 190_000
            assert owner.rpc("status")["snapshots"] == before["snapshots"]
            for peer in clients:
                peer.barrier("stable")
            probe.write(osc("working", id="task06", msg="x" * 2048, title="t" * 192) * 200)
            for peer in clients:
                seen = []
                method = "test-barrier-coalesced"
                peer.send(method)
                peer.until(lambda h: (seen.append(h) or bool(h["error"] and method in h["error"])))
                assert not [h for h in seen if h["meta"]["id"] == pane and not h["error"]], seen
            for peer in clients:
                peer.close()
            clients.clear()
            probe.write(osc("clear") + osc("error", msg="Retained while detached"))
            status = expected("error")
            for remote in [False, True]:
                peer = Client(receiver if remote else owner, pane, **(dict(connection="owner") if remote else {}))
                clients.append(peer)
                header = peer.until(lambda h: h["meta"]["id"] == pane and h["meta"]["program_status"] == status)
                assert not header["notifications"]
                peer.send("subscribe", panes=[])
                peer.barrier("reconnected-hidden")

        with scenario("rapid hidden reports keep metadata paced and input responsive"):
            before = owner.rpc("status")
            ticks = sample(before["pid"])
            began = time.monotonic()
            probe.burst([osc("working", msg=str(i)) for i in range(500)] + [osc("blocked", msg="Burst complete")], .001, wait=False)
            other = owner.rpc("new", directory=str(owner.path))["pane"]
            start = time.monotonic()
            owner.rpc("send", pane=other, text="printf 'RESPONSIVE\\n'", enter=True)
            wait_for(lambda: "RESPONSIVE" in owner.rpc("read", pane=other)["text"])
            latency = time.monotonic() - start
            probe.wait_applied()
            receive(expected("blocked"))
            after = owner.rpc("status")
            assert after["snapshots"] == before["snapshots"], (before, after)
            assert latency < 1, latency
            owner.rpc("close", pane=other)
            ticks_used = sample(before["pid"]) - ticks
            measurements = dict(cap=64, retained_bytes=memory, input_roundtrip_ms=latency * 1000,
                burst_reports=501, burst_elapsed=time.monotonic() - began,
                daemon_cpu_seconds=ticks_used / os.sysconf("SC_CLK_TCK"), snapshots_added=0)
            # No requests during the idle sample. Status has no heartbeat/expiry.
            ticks = sample(before["pid"])
            time.sleep(1.2)
            measurements["hidden_idle_cpu_seconds"] = (sample(before["pid"]) - ticks) / os.sysconf("SC_CLK_TCK")
            assert measurements["hidden_idle_cpu_seconds"] < .15, measurements
            (artifacts / "program-status-measurements.json").write_text(json.dumps(measurements, indent=2) + "\n")

        with scenario("detached exit drains after reaping even when a background child holds the PTY"):
            for peer in clients:
                peer.close()
            clients.clear()
            wait_for(lambda: not owner.rpc("status")["local_clients"] and not owner.rpc("status")["remote_clients"])
            daemon_pid = owner.rpc("status")["pid"]
            dead = owner.rpc("new", directory=str(owner.path))["pane"]
            ids = owner.path / "exit-processes.json"
            release = owner.path / "exit-release"
            script = '''import json, os, pathlib, signal, sys, time
ids, release = map(pathlib.Path, sys.argv[1:])
r, w = os.pipe()
child = os.fork()
if child == 0:
    os.close(r)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    os.write(w, b'x')
    os.close(w)
    time.sleep(30)
    os._exit(0)
os.close(w)
os.read(r, 1)
os.close(r)
ids.write_text(json.dumps([os.getpid(), child]))
while not release.exists(): time.sleep(.01)
os.write(1, bytes([27]) + b']7501;state=done:msg=RG9uZQ==' + bytes([7]))
os._exit(0)
'''
            owner.rpc("send", pane=dead, text="exec " + shlex.join(["python3", "-c", script, str(ids), str(release)]), enter=True)
            parent, child = wait_for(lambda: json.loads(ids.read_text()), exceptions=(OSError, json.JSONDecodeError))
            def process_state(pid):
                return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
            try:
                # Queue output and SIGCHLD together so the only wake must drain
                # output after known exit. There is no frontend metadata timer.
                os.kill(daemon_pid, signal.SIGSTOP)
                wait_for(lambda: process_state(daemon_pid) == "T")
                release.touch()
                wait_for(lambda: process_state(parent) == "Z")
                os.kill(daemon_pid, signal.SIGCONT)
                wait_for(lambda: (m := owner.rpc("metadata", pane=dead)["meta"])["exited"] and m["program_status"], timeout=2)
            finally:
                os.kill(daemon_pid, signal.SIGCONT)
                try:
                    os.kill(child, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            owner.rpc("close", pane=dead)

        with scenario("attached process exit retains results until a viewing client interacts"):
            dead = owner.rpc("new", directory=str(owner.path))["pane"]
            command = "exec python3 -c " + shlex.quote("import os; os.write(1, b'\\x1b]7501;state=done:msg=RG9uZQ==\\x07')")
            owner.rpc("send", pane=dead, text=command, enter=True)
            wait_for(lambda: (m := owner.rpc("metadata", pane=dead)["meta"])["exited"] and m["program_status"])
            peer = Client(owner, dead)
            peer.until(lambda h: h["meta"]["id"] == dead and h["meta"]["exited"])
            peer.send("key", name="Return", unshifted=13, mods=0, action="press", text="\r")
            wait_for(lambda: "error" in owner.rpc("metadata", pane=dead, check=False))
            peer.close()
    finally:
        for peer in clients:
            peer.close()
print("Passed OSC 7501 owner, stream, lifecycle, bounds and responsiveness checks.")
