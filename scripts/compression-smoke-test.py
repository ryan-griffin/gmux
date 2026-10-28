#!/usr/bin/env python3
"""Check idle compression and measure daemon memory, work, and input latency."""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

from cargo_test_support import cargo_binary
from history_test_support import (
    HISTORY_SHELL, binary_metadata, distribution, idle_sample, prepare_history, process_sample,
)
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for


@contextmanager
def history_environment(mib):
    if mib is None:
        yield {}
        return
    root = Path(__file__).resolve().parent.parent
    prefix = Path(os.environ.get("GHOSTTY_PREFIX", root / ".deps/ghostty")).resolve()
    with tempfile.TemporaryDirectory(prefix="gmux-compression-build-") as directory:
        interposer = Path(directory) / "history-limit.so"
        subprocess.run(["cc", "-O2", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror",
                        "-I" + str(prefix / "include"), str(root / "scripts/compression-history-limit.c"),
                        "-L" + str(prefix / "lib"), "-Wl,-rpath," + str(prefix / "lib"),
                        "-lghostty-vt", "-ldl", "-o", str(interposer)], check=True)
        preload = str(interposer) + (":" + os.environ["LD_PRELOAD"] if os.environ.get("LD_PRELOAD") else "")
        yield dict(LD_PRELOAD=preload, GMUX_BENCH_SCROLLBACK_BYTES=str(mib * 1024 * 1024))


def terminals(fixture):
    return fixture.rpc("status").get("terminal_memory", [])


def completed(rows):
    return rows and all(row["compression"]["steps"] and not row["scheduled"] for row in rows)


def run(binary, mode, args, baseline=False, reference=None, daemon_env=None):
    label = "baseline" if baseline else "compression"
    log_path = args.report.with_name(f"{args.report.stem}-{label}-{mode}.log")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with AppFixture(prefix="gmux-compression-", env=daemon_env, shell_script=HISTORY_SHELL) as fixture:
        prepare_history(fixture, args.lines)
        with log_path.open("w") as log:
            daemon = fixture.start(binary, "serve", stdout=log, stderr=log)
            wait_for(lambda: fixture.rpc("status"))
            if args.history_mib is not None and not baseline:
                fixture.rpc("scrollback", max_bytes=args.history_mib * 1024 * 1024)
            panes = [fixture.rpc("new")["pane"]]
            for _ in range(args.panes - 1):
                panes.append(fixture.rpc("split", pane=panes[0])["pane"])
            for pane in panes:
                wait_for(lambda: "BOOT" in fixture.rpc("read", pane=pane)["text"])
            client = Client(fixture, panes[0]) if mode == "attached" else None
            if client:
                client.output("BOOT")
            load_started = time.monotonic()
            for pane in panes:
                fixture.rpc("send", pane=pane, text="g", paste=False, enter=False)
            for pane in panes:
                wait_for(lambda: "READY" in fixture.rpc("read", pane=pane)["text"])
            before = dict(process=process_sample(daemon.pid), terminals=terminals(fixture))
            start = time.monotonic()
            load_seconds = start - load_started
            latency = []
            probe = client
            if mode == "attached":
                probe.output("READY")
            first_work_at = None
            reference_seconds = reference["pass_wall_seconds"] if reference else 7
            reference_input_at = reference["first_work_seconds"] if reference else 5
            # Sample input after work starts. This rewrites the live row and
            # does not restore historical pages.
            while time.monotonic() - start < 90:
                elapsed = time.monotonic() - start
                rows = terminals(fixture)
                if any(row["compression"]["steps"] for row in rows) and first_work_at is None:
                    first_work_at = elapsed
                sample_input = elapsed >= reference_input_at if baseline else first_work_at is not None
                if probe and sample_input:
                    sample = len(latency) + 1
                    at = time.perf_counter()
                    probe.send("input", bytes=[ord("x")])
                    probe.output(f"ACK{sample:06d}")
                    latency.append((time.perf_counter() - at) * 1000)
                if (baseline and elapsed >= reference_seconds) or (not baseline and completed(rows)):
                    break
                time.sleep(.02)
            else:
                raise AssertionError("Idle compression never completed")
            pass_wall_seconds = time.monotonic() - start
            after = dict(process=process_sample(daemon.pid), terminals=terminals(fixture))
            if not baseline:
                assert first_work_at is not None
                assert load_seconds + first_work_at >= 4.5, "Compression skipped its idle delay"
                supported = all(row["memory"]["compression_supported"] for row in after["terminals"])
                if supported:
                    assert sum(row["memory"]["compressed_pages"] for row in after["terminals"]) > 0
                    assert sum(row["memory"]["resident_bytes"] for row in after["terminals"]) < sum(
                        row["memory"]["resident_bytes"] for row in before["terminals"])
                # Observing statistics must neither restore history nor rearm work.
                time.sleep(.2)
                assert sorted(terminals(fixture), key=lambda row: row["pane"]) == sorted(
                    after["terminals"], key=lambda row: row["pane"])
            if client:
                client.close()
                client = None
                wait_for(lambda: fixture.rpc("status")["local_clients"] == 0)
            idle = idle_sample(daemon.pid)
            assert idle["main_wakeups"] == 0, idle
            assert idle["cpu_seconds"] <= .02, idle

            # Measure input on the same visible pane after compression as well.
            probe = Client(fixture, panes[0])
            probe.until(lambda h: h["meta"]["id"] == panes[0] and h["frame"])
            post_latency = []
            sequence = len(latency)
            for index in range(args.samples):
                at = time.perf_counter()
                probe.send("input", bytes=[ord("x")])
                probe.output(f"ACK{sequence + index + 1:06d}")
                post_latency.append((time.perf_counter() - at) * 1000)
                time.sleep(.003)
            probe.close()
            result = dict(mode=mode, baseline=baseline, panes=args.panes, generated_lines_per_pane=args.lines,
                          history_limit_mib=args.history_mib,
                          load_seconds=load_seconds,
                          before=before, after=after, pass_wall_seconds=pass_wall_seconds,
                          first_work_seconds=first_work_at, idle=idle,
                          post_compression_input=distribution(post_latency))
            if latency:
                result["during_compression_input"] = distribution(latency)

            if not baseline:
                # Restoring cold history must schedule another pass on its own.
                reader = Client(fixture, panes[0])
                reader.send("scroll-to", row=0, serial=1)
                reader.until(lambda h: h["scroll_serial"] == 1)
                reader.close()
                wait_for(lambda: any(row["pane"] == panes[0] and row["scheduled"] for row in terminals(fixture)))
            # Close with scheduled work still outstanding. No stale timer may
            # survive removal of the last terminal.
            for pane in panes:
                fixture.rpc("close", pane=pane)
            wait_for(lambda: fixture.rpc("status")["panes"] == 0)
            time.sleep(.2)
            result["after_teardown_idle"] = idle_sample(daemon.pid)
            assert result["after_teardown_idle"]["main_wakeups"] == 0, result
            return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--baseline", type=Path, help="Comparison build, normally without idle compression")
    parser.add_argument("--panes", type=int, default=4)
    parser.add_argument("--lines", type=int, default=16000)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--history-mib", type=int, help="Benchmark only: override retention with a native interposer")
    parser.add_argument("--report", type=Path, default=Path("artifacts/compression.json"))
    args = parser.parse_args()
    if not 1 <= args.panes <= 24 or args.lines < 2048 or args.samples < 1:
        parser.error("Use 1..24 panes, at least 2048 lines, and at least one input sample")
    if args.history_mib is not None and not 1 <= args.history_mib <= 1024:
        parser.error("Use 1..1024 MiB of history per pane")
    binary = (args.binary or cargo_binary("gmux")).resolve()
    cpu = next((line.partition(":")[2].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), platform.machine())
    report = dict(platform=platform.platform(), cpu=cpu, results=[], binaries=[])
    with history_environment(args.history_mib) as daemon_env:
        for target, baseline in [(binary, False)] + ([(args.baseline.resolve(), True)] if args.baseline else []):
            report["binaries"].append(binary_metadata(target, baseline))
            for mode in ("detached", "attached"):
                with scenario(f"{'baseline' if baseline else 'compression'} {mode}, {args.panes} panes"):
                    reference = next((row for row in report["results"]
                                      if row["mode"] == mode and not row["baseline"]), None)
                    report["results"].append(run(target, mode, args, baseline, reference, daemon_env))
                    args.report.parent.mkdir(parents=True, exist_ok=True)
                    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"PASS: idle compression, transparent history access, input, and teardown; {args.report}")


if __name__ == "__main__":
    main()
