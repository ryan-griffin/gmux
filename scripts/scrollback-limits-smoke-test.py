#!/usr/bin/env python3
"""Check daemon retention controls and optionally measure pruning, memory, and latency."""
import argparse
import fcntl
import json
import platform
from pathlib import Path
import socket
import subprocess
import time

from cargo_test_support import cargo_binary
from history_test_support import (
    HISTORY_SHELL, binary_metadata, distribution, idle_sample, prepare_history, process_sample,
)
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

DEFAULTS = dict(max_bytes=10_000_000, max_lines="unlimited")


def command(binary, fixture, *args, check=True):
    result = subprocess.run([str(binary), *map(str, args)], env=fixture.env,
                            capture_output=True, text=True, timeout=20)
    assert (result.returncode == 0) == check, result.stderr or result.stdout
    return json.loads(result.stdout) if check else result.stderr


def generate(fixture, panes, generation=1):
    for pane in panes:
        if generation == 1:
            wait_for(lambda: "BOOT" in fixture.rpc("read", pane=pane)["text"])
        fixture.rpc("send", pane=pane, text="g")
    for pane in panes:
        wait_for(lambda: f"READY{generation:06d}" in fixture.rpc("read", pane=pane)["text"])


def scroll(client, row, serial):
    client.send("scroll-to", row=row, serial=serial)
    return client.until(lambda h: h["scroll_serial"] == serial and h["frame"])


def correctness(binary, report):
    with AppFixture(prefix="gmux-retention-", shell_script=HISTORY_SHELL) as fixture, \
            AppFixture(prefix="gmux-retention-client-") as remote, \
            report.with_suffix(".log").open("w") as log:
        prepare_history(fixture, 4000)
        daemon = fixture.start(binary, "serve", stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("status"))
        with scenario("defaults, complete validation, and explicit reset/unlimited/zero semantics"):
            assert command(binary, fixture, "scrollback")["limits"] == DEFAULTS
            fixture.rpc("scrollback", max_bytes="unlimited", max_lines="unlimited")
            pane = fixture.rpc("new")["pane"]
            second = fixture.rpc("split", pane=pane)["pane"]
            generate(fixture, [pane, second])
            local = Client(fixture, pane)
            local.output("READY")
            top = scroll(local, 0, 1)
            total = top["frame"]["scrollback"]["total"]
            assert total >= 4000
            saved = (fixture.config / "daemon-settings.json").read_bytes()
            for invalid in (None, -1, 1.25, True, "5", "bogus", [], {}, 2**64 - 1, 2**64):
                rejected = fixture.rpc("scrollback", max_bytes=0, max_lines=invalid, check=False)
                assert "error" in rejected, rejected
            assert "error" in fixture.rpc("scrollback", max_bytes=0, unknown=True, check=False)
            assert (fixture.config / "daemon-settings.json").read_bytes() == saved
            assert fixture.rpc("scrollback")["limits"] == dict(max_bytes="unlimited", max_lines="unlimited")
            assert scroll(local, 0, 2)["frame"]["scrollback"]["total"] == total

        with scenario("ordered disk worker saves before pruning and leaves input/control responsive"):
            # Hold the disk worker in flock. The owner thread must keep serving
            # PTYs and read-only requests, with the old policy still in force.
            lock_path = fixture.config / ".daemon-settings.json.lock"
            with lock_path.open("a") as lock, socket.socket(socket.AF_UNIX) as request, \
                    socket.socket(socket.AF_UNIX) as following:
                fcntl.flock(lock, fcntl.LOCK_EX)
                request.settimeout(8)
                request.connect(fixture.env["GMUX_SOCKET"])
                request.sendall(b'{"method":"scrollback","max_lines":3000}\n')
                wait_for(lambda: any("lock" in path.read_text() for path in
                                     Path(f"/proc/{daemon.pid}/task").glob("*/wchan")))
                assert fixture.rpc("scrollback")["limits"]["max_lines"] == "unlimited"
                # This byte update is unchanged, but it must still wait for the
                # line update and resolve its omitted field against that result.
                threads = set(Path(f"/proc/{daemon.pid}/task").iterdir())
                following.settimeout(8)
                following.connect(fixture.env["GMUX_SOCKET"])
                following.sendall(b'{"method":"scrollback","max_bytes":"unlimited"}\n')
                # A new IPC worker waits on its reply only after enqueueing.
                wait_for(lambda: any(path.parent not in threads and "futex" in path.read_text()
                                     for path in Path(f"/proc/{daemon.pid}/task").glob("*/wchan")))
                local.send("scroll", delta=0, bottom=True)
                local.send("input", bytes=[ord("x")])
                local.output("ACK000001")
                fcntl.flock(lock, fcntl.LOCK_UN)
                reply = json.loads(request.makefile("rb").readline())
                assert reply["saved"] and reply["applied"], reply
                assert reply["limits"]["max_lines"] == 3000
                next_reply = json.loads(following.makefile("rb").readline())
                assert next_reply["saved"] and next_reply["applied"], next_reply
                assert next_reply["limits"] == dict(max_bytes="unlimited", max_lines=3000)
            assert fixture.rpc("scrollback")["limits"]["max_lines"] == 3000
            # A damaged file must be preserved, with no irreversible pruning.
            settings = fixture.config / "daemon-settings.json"
            saved = settings.read_bytes()
            settings.write_text("{broken")
            rejected = fixture.rpc("scrollback", max_bytes=0, check=False)
            assert "error" in rejected and not rejected["applied"] and not rejected["saved"], rejected
            assert settings.read_text() == "{broken"
            assert fixture.rpc("scrollback")["limits"]["max_bytes"] == "unlimited"
            assert scroll(local, 0, 3)["frame"]["scrollback"]["total"] > 2000
            settings.write_bytes(saved)

        with scenario("existing local/remote views prune without output, including queued cached replies"):
            command(binary, fixture, "connection", "edit", "local", "--listen-address", "127.0.0.1:0",
                    "--remote-access", "true", "--auth", "none")
            address = fixture.rpc("status")["bound_address"]
            command(binary, remote, "connection", "add", address, "--name", "server", "--auth", "none")
            other = Client(remote, pane, connection="server")
            other.until(lambda h: h["frame"] is not None)
            before = scroll(other, 1000, 10)
            # Leave a cached viewport reply queued on the remote transport.
            other.send("scroll-to", row=1001, serial=11)
            other.send("find", query="module_")
            current = scroll(local, 0, 4)
            local.send("select", start=[0, 0], end=[8, 0], serial=1)
            local.until(lambda h: h["selection_serial"] == 1)
            start = time.perf_counter()
            updated = command(binary, remote, "--connection", "server", "scrollback", "--max-lines", 100)
            update_ms = (time.perf_counter() - start) * 1000
            assert updated["saved"] and updated["applied"]
            new = local.until(lambda h: h["frame"] and h["frame"]["scrollback"]["total"] < 1024)
            delayed = other.until(lambda h: h["frame"] and h["frame"]["scrollback"]["total"] < 1024)
            assert new["selection"] is None
            assert new["frame"]["scrollback"]["offset"] == 0
            for old, fresh in ((current, new), (before, delayed)):
                removed = old["frame"]["scrollback"]["total"] - fresh["frame"]["scrollback"]["total"]
                assert fresh["generation"] == old["generation"]
                assert fresh["cache_origin"] == old["cache_origin"] + removed
                assert fresh["cache_valid_before"] is not None
            assert "BOOT" not in local.text() and "界é" in local.text()
            assert scroll(other, 0, 12)["frame"]["scrollback"]["offset"] == 0
            other.send("scroll", delta=0, bottom=True)
            other.send("input", bytes=[ord("x")])
            other.output("ACK000002")
            other.close()
            local.close()
            assert not (remote.config / "daemon-settings.json").exists()
            assert not Path(remote.env["GMUX_SOCKET"]).exists()

        with scenario("new panes and restart/restored history use the saved daemon policy"):
            new_pane = fixture.rpc("new")["pane"]
            generate(fixture, [new_pane])
            probe = Client(fixture, new_pane)
            probe.output("READY")
            assert probe.frame["scrollback"]["total"] < 1024
            probe.close()
            fixture.rpc("stop")
            daemon.wait(timeout=10)
            daemon = fixture.start(binary, "serve", stdout=log, stderr=log)
            wait_for(lambda: fixture.rpc("status"))
            assert fixture.rpc("scrollback")["limits"] == dict(max_bytes="unlimited", max_lines=100)
            probe = Client(fixture, pane)
            probe.output("BOOT")
            assert probe.frame["scrollback"]["total"] < 1024
            probe.close()
            assert fixture.rpc("scrollback", max_bytes="default")["limits"] == dict(max_bytes=10_000_000, max_lines=100)
            assert fixture.rpc("scrollback", max_lines="default")["limits"] == DEFAULTS
            fixture.rpc("scrollback", max_bytes=0, max_lines=0)
            zero = fixture.rpc("new")["pane"]
            generate(fixture, [zero])
            probe = Client(fixture, zero)
            probe.output("READY")
            assert probe.frame["scrollback"]["total"] == probe.frame["rows"]
            probe.close()
            fixture.rpc("stop")
            daemon.wait(timeout=10)
            # Invalid saved numbers fail startup before any shell is restored.
            (fixture.config / "daemon-settings.json").write_text('{"max_bytes":0,"max_lines":-1}')
            failed = fixture.start(binary, "serve", stdout=log, stderr=log)
            assert failed.wait(timeout=8) != 0
        return dict(remote_cli_update_ms=update_ms)


def measure(binary, args):
    with AppFixture(prefix="gmux-retention-measure-", shell_script=HISTORY_SHELL) as fixture, \
            args.report.with_suffix(".measure.log").open("w") as log:
        prepare_history(fixture, args.lines)
        daemon = fixture.start(binary, "serve", stdout=log, stderr=log)
        wait_for(lambda: fixture.rpc("status"))
        fixture.rpc("scrollback", max_bytes=64 * 1024 * 1024, max_lines="unlimited")
        panes = [fixture.rpc("new")["pane"] for _ in range(args.panes)]
        generate(fixture, panes)
        client = Client(fixture, panes[0])
        client.output("READY")
        sequence = 0

        def snapshot():
            return dict(process=process_sample(daemon.pid), terminals=fixture.rpc("status")["terminal_memory"])

        def latency():
            nonlocal sequence
            inputs, controls = [], []
            for _ in range(args.samples):
                sequence += 1
                at = time.perf_counter()
                client.send("input", bytes=[ord("x")])
                client.output(f"ACK{sequence:06d}")
                inputs.append((time.perf_counter() - at) * 1000)
                at = time.perf_counter()
                fixture.rpc("scrollback")
                controls.append((time.perf_counter() - at) * 1000)
            return dict(input=distribution(inputs), control=distribution(controls))

        before = snapshot()
        before_latency = latency()
        start = time.perf_counter()
        result = fixture.rpc("scrollback", max_bytes=256 * 1024, max_lines=200)
        update_ms = (time.perf_counter() - start) * 1000
        assert result["saved"] and result["applied"]
        after = snapshot()
        after_latency = latency()
        assert sum(t["memory"]["virtual_bytes"] for t in after["terminals"]) < sum(
            t["memory"]["virtual_bytes"] for t in before["terminals"])
        client.close()
        # Repeat with compressed history. Statistics must not decompress pages.
        fixture.rpc("scrollback", max_bytes=64 * 1024 * 1024, max_lines="unlimited")
        generate(fixture, panes, generation=2)
        wait_for(lambda: all(not t["scheduled"] for t in fixture.rpc("status")["terminal_memory"]), timeout=30)
        cold_before = snapshot()
        if all(t["memory"]["compression_supported"] for t in cold_before["terminals"]):
            assert sum(t["memory"]["compressed_pages"] for t in cold_before["terminals"]) > 0
        start = time.perf_counter()
        fixture.rpc("scrollback", max_bytes=256 * 1024, max_lines=200)
        cold_update_ms = (time.perf_counter() - start) * 1000
        cold_after = snapshot()
        start = time.perf_counter()
        fixture.rpc("scrollback", max_bytes=0)
        zero_ms = (time.perf_counter() - start) * 1000
        zero = snapshot()
        wait_for(lambda: all(not t["scheduled"] for t in fixture.rpc("status")["terminal_memory"]), timeout=30)
        idle = idle_sample(daemon.pid)
        assert idle["main_wakeups"] == 0, idle
        return dict(panes=args.panes, generated_lines_per_pane=args.lines, before=before, after=after,
                    before_latency=before_latency, after_latency=after_latency, update_ms=update_ms,
                    cold_before=cold_before, cold_after=cold_after, cold_update_ms=cold_update_ms,
                    zero=zero, zero_update_ms=zero_ms, idle=idle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--measure", action="store_true")
    parser.add_argument("--panes", type=int, default=4)
    parser.add_argument("--lines", type=int, default=20000)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--report", type=Path, default=Path("artifacts/scrollback-limits.json"))
    args = parser.parse_args()
    if not 1 <= args.panes <= 24 or args.lines < 4000 or args.samples < 1:
        parser.error("Use 1..24 panes, at least 4000 lines, and at least one latency sample")
    args.report.parent.mkdir(parents=True, exist_ok=True)
    binary = (args.binary or cargo_binary("gmux")).resolve()
    cpu = next((line.partition(":")[2].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), platform.machine())
    report = dict(platform=platform.platform(), cpu=cpu,
                  binary=binary_metadata(binary), correctness=correctness(binary, args.report))
    if args.measure:
        with scenario(f"measure retention and latency across {args.panes} panes"):
            report["measurement"] = measure(binary, args)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"PASS: daemon scrollback limits; {args.report}")


if __name__ == "__main__":
    main()
