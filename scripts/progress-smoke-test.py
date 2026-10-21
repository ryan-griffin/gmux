#!/usr/bin/env python3
"""Check retained OSC 9;4 progress over local/remote streams and control status."""
import json
from pathlib import Path
import subprocess
import threading
import time

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for
from daemon_test_support import stop_daemon

artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)
daemon = cargo_binary("gmux")


def receive(client, expected):
    header = client.until(lambda h: h["meta"]["id"] == client.pane and h["meta"]["progress"] == expected)
    assert header["frame"] is None and not header["rows"], header
    assert not header["notifications"], header
    assert header["meta"]["unread_sequence"] == 0, header
    return header


with scenario("standalone progress and expiry respect independent frontend intervals"), \
        AppFixture() as paced:
    paced.start(daemon, "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: paced.rpc("status"))
    pane = paced.rpc("new", directory=str(paced.path))["pane"]
    probe = ProgressProbe(paced, pane)
    peers = [Client(paced, pane), Client(paced, pane)]
    try:
        for peer, micros in zip(peers, [8_000, 100_000]):
            peer.send("subscribe", panes=[])
            peer.send("frame-interval", micros=micros)
            peer.barrier("paced-ready")
        before = paced.rpc("status")
        reports = [[], []]
        errors = []

        def collect(index, expected):
            def received(header):
                if header["meta"]["id"] != pane or header.get("error"):
                    return False
                assert header["frame"] is None and not header["rows"], header
                reports[index].append((time.monotonic(), header["meta"]["progress"]))
                return header["meta"]["progress"] == expected
            try:
                peers[index].until(received)
            except Exception as error:
                errors.append(error)

        def readers(expected):
            threads = [threading.Thread(target=collect, args=(i, expected)) for i in range(2)]
            for thread in threads:
                thread.start()
            return threads

        def joined(threads):
            for thread in threads:
                thread.join(8)
            assert not any(thread.is_alive() for thread in threads) and not errors, errors

        final = dict(state="paused", percentage=77)
        threads = readers(final)
        began = time.monotonic()
        probe.burst([f"\x1b]9;4;1;{i % 99}\x07" for i in range(500)]
                    + ["\x1b]9;4;4;77\x07"], .002)
        joined(threads)
        elapsed = time.monotonic() - began
        fast, slow = map(len, reports)
        assert slow >= 5 and fast >= 3 * slow, (fast, slow, elapsed)
        assert fast <= elapsed / .008 + 4 and slow <= elapsed / .1 + 4, (fast, slow, elapsed)
        assert paced.rpc("status")["snapshots"] == before["snapshots"], "Hidden progress must not extract rows"

        # Keep both connections busy on a second hidden pane across expiry, so
        # one connection is still paced when another consumes the global dirty
        # set. The slower connection must retain its own pending remove value.
        time.sleep(14)
        second = paced.rpc("split", pane=pane)["pane"]
        traffic = ProgressProbe(paced, second)
        threads = readers(dict(state="inactive", percentage=0))
        traffic.burst([f"\x1b]9;4;1;{i % 99}\x07" for i in range(300)], .01, wait=False)
        cleared = wait_for(lambda: probe.progress() == dict(state="inactive", percentage=0), timeout=4)
        cleared_at = time.monotonic()
        assert cleared
        joined(threads)
        assert reports[1][-1][0] <= cleared_at + .35, reports[1][-1]
        assert paced.rpc("status")["snapshots"] == before["snapshots"]
        traffic.wait_applied()
    finally:
        for peer in peers:
            peer.close()


with (artifacts / "progress-smoke.log").open("w") as log, AppFixture() as owner, AppFixture() as receiver:
    owner.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    owner.start(daemon, "serve", stdout=log, stderr=log)
    receiver.start(daemon, "serve", stdout=log, stderr=log)
    address = wait_for(lambda: owner.rpc("status")["bound_address"])
    receiver.rpc("connection-add", name="owner", address=address, authentication=dict(mode="none"))
    created = owner.rpc("new", directory=str(owner.path))
    pane = created["pane"]
    probe = ProgressProbe(owner, pane)
    clients = [Client(owner, pane), Client(receiver, pane, connection="owner")]
    try:
        for client in clients:
            client.until(lambda h: h["meta"]["id"] == pane and h["frame"])
            client.barrier(0)
        with scenario("every transition is metadata-only for local and remote frontends"):
            for code, value, state, percentage in [(1, 42, "determinate", 42), (2, None, "error", 42),
                    (4, 81, "paused", 81), (3, 5, "indeterminate", 81), (4, None, "paused", 81),
                    (0, 99, "inactive", 0), (1, None, "determinate", 0), (1, 999, "determinate", 100)]:
                probe.report(code, value)
                expected = probe.expect(state, percentage)
                for client in clients:
                    receive(client, expected)
            status = owner.rpc("list")["tabs"][0]
            assert status["progress"] == dict(pane=pane, progress=expected), status
            assert status["panes"][0]["progress"] == expected, status
            output = subprocess.check_output([str(daemon), "list"], env=owner.env, text=True)
            assert json.loads(output)["tabs"][0]["progress"]["progress"] == expected

        with scenario("changing progress preserves frame pacing for another active pane"):
            second = owner.rpc("split", pane=pane)["pane"]
            active = ProgressProbe(owner, second)
            view = Client(owner, second)
            clients.append(view)
            view.until(lambda h: h["meta"]["id"] == second and h["frame"])
            for client in clients:
                client.barrier("burst-start")
            before = owner.rpc("status")
            began = time.monotonic()
            probe.burst([f"\x1b]9;4;1;{i % 101}\x07" for i in range(700)], .001, wait=False)
            active.burst([f"\rACTIVE-{i:04d}" for i in range(700)], .001, wait=False)
            probe.wait_applied()
            active.wait_applied()
            for client in clients:
                client.barrier("burst-end")
            after = owner.rpc("status")
            elapsed = time.monotonic() - began
            # Two views of the reporter and one active text view. Leave ample
            # scheduler allowance, but reject report-driven unpaced snapshots.
            assert after["snapshots"] - before["snapshots"] <= 3 * (elapsed / .016 + 12), (before, after, elapsed)
            if "ACTIVE-0699" not in view.text():
                view.output("ACTIVE-0699")
            view.close()
            clients.remove(view)
            owner.rpc("close", pane=second)
            probe.report(1, 100)
            expected = probe.expect("determinate", 100)
            for client in clients:
                receive(client, expected)

        with scenario("identical keepalives coalesce and hidden panes still receive progress"):
            clients[0].send("subscribe", panes=[])
            clients[0].barrier(1)
            before = owner.rpc("status")
            for _ in range(8):
                probe.report(1, 100)
            for client in clients:
                seen = []
                method = "progress-keepalive-barrier"
                client.send(method)
                client.until(lambda h: seen.append(h) or (h.get("error") and method in h["error"]))
                assert not [h for h in seen if h["meta"]["id"] == pane and not h.get("error")], seen
            assert owner.rpc("status")["sent_rows"] == before["sent_rows"]
            probe.report(2, 7)
            expected = probe.expect("error", 7)
            for client in clients:
                receive(client, expected)

        with scenario("reconnects read the latest report and full reset clears it"):
            clients[1].close()
            clients[1] = Client(receiver, pane, connection="owner")
            header = clients[1].until(lambda h: h["meta"]["id"] == pane)
            assert header["meta"]["progress"] == expected
            clients[1].barrier(3)
            probe.write("\x1bc")
            expected = probe.expect("inactive", 0)
            # RIS can legitimately change the viewport; its progress must clear too.
            for client in clients:
                client.until(lambda h: h["meta"]["id"] == pane and h["meta"]["progress"] == expected)

        with scenario("detached execution expires without any attached frontends"):
            for client in clients:
                client.close()
            clients.clear()
            wait_for(lambda: owner.rpc("status")["local_clients"] == 0 and owner.rpc("status")["remote_clients"] == 0)
            probe.report(4, 63)
            probe.expect("paused", 63)
            # No control calls while waiting. The daemon's nearest deadline must wake poll.
            time.sleep(16)
            assert probe.progress() == dict(state="inactive", percentage=0)
            fresh = Client(owner, pane)
            clients.append(fresh)
            header = fresh.until(lambda h: h["meta"]["id"] == pane)
            assert header["meta"]["progress"] == dict(state="inactive", percentage=0)

        with scenario("pane removal discards progress and replacement execution starts inactive"):
            probe.report(1, 55)
            probe.expect("determinate", 55)
            owner.rpc("close", pane=pane)
            replacement = owner.rpc("new", directory=str(owner.path))["pane"]
            assert replacement != pane
            assert owner.rpc("_metadata", pane=replacement)["meta"]["progress"] == dict(state="inactive", percentage=0)
            replacement_probe = ProgressProbe(owner, replacement)
            replacement_probe.report(1, 27)
            replacement_probe.expect("determinate", 27)
            for client in clients:
                client.close()
            clients.clear()
            stop_daemon(owner.env["GMUX_SOCKET"])
            owner.start(daemon, "serve", stdout=log, stderr=log)
            wait_for(lambda: owner.rpc("_metadata", pane=replacement)["meta"]["progress"] == dict(state="inactive", percentage=0))
    finally:
        for client in clients:
            client.close()
