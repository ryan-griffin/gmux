#!/usr/bin/env python3
"""Measure release frontend/daemon CPU, RSS, and native Wayland input latency.

Run python3 scripts/performance-benchmark.py --binary target/release/gmux-gtk
--output artifacts/performance.json. Starts an isolated headless Mutter compositor
with XWayland disabled, a private PipeWire server, settings, sockets, and shells.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import platform
import shlex
import statistics
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon
from test_support import rpc as socket_rpc
from wayland_test_support import ensure_wayland_session, WaylandInput

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, default=None)
parser.add_argument("--output", type=Path, default=Path("artifacts/performance.json"))
parser.add_argument("--seconds", type=float, default=5)
parser.add_argument("--samples", type=int, default=40)
parser.add_argument("--renderer", choices=["vulkan", "gl", "cairo"], default="vulkan")
args = parser.parse_args()
assert args.seconds >= 1 and args.samples >= 5
ensure_wayland_session()
args.binary = args.binary.resolve() if args.binary else cargo_binary("gmux-gtk", profile="release")
args.output = args.output.resolve()
args.output.parent.mkdir(parents=True, exist_ok=True)
clock_ticks = os.sysconf("SC_CLK_TCK")


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if value := predicate():
                return value
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.025)
    raise AssertionError("Benchmark did not reach the expected state")


def process_sample(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines())
    return dict(ticks=int(fields[11]) + int(fields[12]),
                rss_mib=int(status["VmRSS"].split()[0]) / 1024,
                main_switches=int(status["voluntary_ctxt_switches"]) + int(status["nonvoluntary_ctxt_switches"]))


worker_source = r'''
import os, pathlib, select, sys, time, tty
role, control, ready = sys.argv[1:]
control, ready = pathlib.Path(control), pathlib.Path(ready)
if role == "probe":
    tty.setraw(0)
    os.write(1, b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[48;2;230;30;45m \x1b[0m")
    ready.write_text(str(os.getpid()))
    green = False
    while os.read(0, 1):
        green = not green
        rgb = b"20;220;110" if green else b"230;30;45"
        os.write(1, b"\x1b[H\x1b[48;2;" + rgb + b"m \x1b[0m")
else:
    ready.write_text(str(os.getpid()))
    lines = 0
    while True:
        if control.read_text() == "busy":
            text = "".join(f"\x1b[38;5;{(lines+i)%216+16}mrow {lines+i:08d} " + "abc界🙂 " * 12 + "\x1b[0m\r\n" for i in range(20))
            os.write(1, text.encode())
            lines += 20
        time.sleep(1/60)
'''

app = None
env = None
test_directory = tempfile.TemporaryDirectory(prefix="gmux-performance-", delete=False)
try:
    with test_directory as directory:
        temp = Path(directory)
        config = temp / "config/gmux"
        config.mkdir(parents=True)
        (config / "clients/gtk").mkdir(parents=True, exist_ok=True)
        (config / "clients/gtk/settings.json").write_text(json.dumps(dict(local_execution=True, appearance="dark",
            desktop_notifications=False, use_system_font=False, font="Monospace", font_size=12)))
        rcfile = temp / "bashrc"
        rcfile.write_text("PS1='GMUX_BENCH> '\nPROMPT_COMMAND=''\nstty -echo\n")
        shell = temp / "shell"
        shell.write_text(f"#!/bin/sh\nexec /bin/bash --noprofile --rcfile {shlex.quote(str(rcfile))} -i\n")
        shell.chmod(0o700)
        worker = temp / "worker.py"
        worker.write_text(worker_source)
        control = temp / "control"
        control.write_text("idle")
        env = dict(os.environ, GTK_A11Y="none", GSK_RENDERER=args.renderer,
                   SHELL=str(shell), GMUX_SOCKET=str(temp / "socket"), XDG_CONFIG_HOME=str(temp / "config"))
        with args.output.with_suffix(".log").open("w") as log, WaylandInput() as native:
            def rpc(method, **params):
                return socket_rpc(env["GMUX_SOCKET"], method, **params)

            app = subprocess.Popen([str(args.binary)], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            wait_for(lambda: rpc("renderer-info")["gtk"])
            daemon_pid = rpc("daemon-info")["pid"]
            first = rpc("new", name="Benchmark", directory=str(temp))
            right = rpc("split", pane=first["pane"])["pane"]
            lower_left = rpc("split", pane=first["pane"], down=True)["pane"]
            lower_right = rpc("split", pane=right, down=True)["pane"]
            panes = [first["pane"], right, lower_left, lower_right]

            def run_worker(pane, role):
                wait_for(lambda: "GMUX_BENCH>" in rpc("read", pane=pane)["text"])
                ready = temp / f"ready-{pane}"
                command = shlex.join(["python3", str(worker), role, str(control), str(ready)])
                rpc("send", pane=pane, text=command, paste=True, enter=True)
                wait_for(ready.exists)

            run_worker(first["pane"], "probe")
            output_pids = []
            for pane in panes[1:]:
                run_worker(pane, "output")
                output_pids.append(int((temp / f"ready-{pane}").read_text()))
            rpc("focus", pane=first["pane"])
            native.click_color((230, 30, 45))
            wait_for(lambda: rpc("renderer-info", pane=first["pane"])["viewport"]["focused"])
            time.sleep(1)
            renderer = rpc("renderer-info", pane=first["pane"])
            assert renderer["display"] == "GdkWaylandDisplay", renderer
            wait_for(lambda: rpc("renderer-info")["window_active"])
            report = dict(binary=str(args.binary), date=time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                daemon_sha256=hashlib.sha256(args.binary.with_name("gmux").read_bytes()).hexdigest(),
                cpu_model=next(line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
                kernel=platform.release(), gtk=subprocess.check_output(["pkg-config", "--modversion", "gtk4"], text=True).strip(),
                renderer=renderer, gtk_debug=env.get("GDK_DEBUG", ""),
                compositor=subprocess.check_output(["mutter", "--version"], text=True, stderr=subprocess.STDOUT).strip(),
                monitor=native.parameters, capture="PipeWire / GStreamer RGBA appsink", xwayland=False,
                samples=args.samples, seconds_per_cpu_sample=args.seconds,
                workload="Three output processes, each sending 20 colored Unicode lines at up to 60 batches/s, plus an interactive probe.",
                latency_method="Mutter native keysym q press/release to changed first-cell pixel in a composited PipeWire frame on a 1280x960@60 headless Wayland monitor. Normal GTK frame pacing. Includes input delivery, PTY, daemon transport, GTK rendering, compositor presentation, capture and observer readback; excludes physical display scanout.",
                resource_method="Frontend and daemon process CPU as percent of one core; sampled resident memory and main-thread context switches. Workload child processes, compositor, PipeWire, WirePlumber and benchmark observer excluded.", scenarios=[])

            def measure_resources(name, attached):
                def workload_bytes():
                    total = 0
                    for pid in output_pids:
                        values = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/io").read_text().splitlines())
                        total += int(values["wchar"])
                    return total
                pids = {"daemon": daemon_pid}
                if attached:
                    pids["frontend"] = app.pid
                before = {role: process_sample(pid) for role, pid in pids.items()}
                bytes_before = workload_bytes()
                memory = {role: [] for role in pids}
                start = time.monotonic()
                while time.monotonic() - start < args.seconds:
                    for role, pid in pids.items():
                        memory[role].append(process_sample(pid)["rss_mib"])
                    time.sleep(0.1)
                elapsed = time.monotonic() - start
                after = {role: process_sample(pid) for role, pid in pids.items()}
                return dict(name=name, elapsed_seconds=elapsed, workload_kib_per_second=(workload_bytes() - bytes_before) / 1024 / elapsed, resources={role: dict(
                    cpu_percent=(after[role]["ticks"] - before[role]["ticks"]) / clock_ticks / elapsed * 100,
                    rss_median_mib=statistics.median(memory[role]), rss_peak_mib=max(memory[role]),
                    main_context_switches_per_second=(after[role]["main_switches"] - before[role]["main_switches"]) / elapsed,
                ) for role in pids})

            colors = [(230, 30, 45), (20, 220, 110)]

            def measure_latency(pane):
                rpc("focus", pane=pane)
                wait_for(lambda: rpc("renderer-info", pane=pane)["viewport"]["focused"]
                         and rpc("renderer-info")["window_active"])
                time.sleep(0.2)

                def ready_pixel():
                    frame = native.frame(timeout=0.1, fresh=False)
                    for green, color in enumerate(colors):
                        if bounds := frame.color_bounds(color):
                            left, top, right, bottom = bounds
                            return (int((left + right) / 2), int((top + bottom) / 2), bool(green))
                    return False

                try:
                    px, py, green = wait_for(ready_pixel)
                except AssertionError:
                    rpc("screenshot", path=str(args.output.with_suffix(".failed.png")))
                    print("Probe readiness:", rpc("renderer-info", pane=pane), flush=True)
                    raise
                samples = []
                for index in range(args.samples + 5):
                    assert rpc("renderer-info")["window_active"], "Benchmark window lost input focus"
                    native.drain_frames()
                    expected = not green
                    start = time.perf_counter()
                    native.key(ord("q"), True)
                    native.key(ord("q"), False)
                    while True:
                        remaining = 3 - (time.perf_counter() - start)
                        if remaining <= 0:
                            raise AssertionError("Timed out waiting for composited input")
                        frame = native.frame(timeout=remaining)
                        if frame.pixel(px, py) == colors[int(expected)]:
                            break
                    elapsed_ms = (time.perf_counter() - start) * 1000
                    green = expected
                    if index >= 5:
                        samples.append(elapsed_ms)
                    # Vary phase relative to GTK and compositor frame deadlines.
                    time.sleep(0.013 + index % 7 * 0.001)
                ordered = sorted(samples)
                return dict(median_ms=statistics.median(samples), p95_ms=ordered[math.ceil(len(ordered) * .95) - 1], samples_ms=samples)

            idle = measure_resources("attached_idle_four_panes", True)
            idle["latency"] = measure_latency(first["pane"])
            report["scenarios"].append(idle)
            control.write_text("busy")
            time.sleep(1)
            busy = measure_resources("attached_three_busy_visible_panes", True)
            busy["latency"] = measure_latency(first["pane"])
            report["scenarios"].append(busy)
            hidden_probe = rpc("new", name="Hidden workload probe", directory=str(temp))["pane"]
            run_worker(hidden_probe, "probe")
            time.sleep(1)
            hidden = measure_resources("attached_three_busy_hidden_panes", True)
            hidden["latency"] = measure_latency(hidden_probe)
            report["scenarios"].append(hidden)
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            wait_for(lambda: not rpc("daemon-info")["attached"])
            report["scenarios"].append(measure_resources("detached_three_busy_panes", False))
            control.write_text("idle")
            time.sleep(1)
            report["scenarios"].append(measure_resources("detached_idle", False))
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps({scenario["name"]: dict(resources=scenario["resources"], workload_kib_per_second=scenario["workload_kib_per_second"],
                latency={k: v for k, v in scenario.get("latency", {}).items() if k != "samples_ms"}) for scenario in report["scenarios"]}, indent=2))
finally:
    if app and app.poll() is None:
        app.terminate()
        app.wait(timeout=8)
    if env:
        stop_daemon(env["GMUX_SOCKET"])
    test_directory.cleanup()
