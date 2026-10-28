#!/usr/bin/env python3
"""Measure release text-blink CPU, resident memory and main-thread wakeups.

Uses the private native Wayland fixture. No RPCs or PTY writes occur inside
each measurement interval. Context switches approximate wakeups; they also
include compositor and other event delivery, rather than just blink callbacks.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from test_support import AppFixture, wait_for
from wayland_test_support import ensure_wayland_session, WaylandInput

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path)
parser.add_argument("--seconds", type=float, default=5)
parser.add_argument("--output", type=Path, default=Path("artifacts/text-blink-cpu.json"))
args = parser.parse_args()
assert args.seconds >= 1
ensure_wayland_session()
binary = args.binary.resolve() if args.binary else cargo_binary("gmux-gtk", profile="release")
args.output.parent.mkdir(parents=True, exist_ok=True)
ticks = os.sysconf("SC_CLK_TCK")


def sample(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines())
    return dict(ticks=int(fields[11]) + int(fields[12]),
                rss_mib=int(status["VmRSS"].split()[0]) / 1024,
                switches=int(status["voluntary_ctxt_switches"]) + int(status["nonvoluntary_ctxt_switches"]))


with args.output.with_suffix(".log").open("w") as log, AppFixture(
        settings=dict(appearance="dark", use_system_font=False, font="Monospace", font_size=12),
        env=dict(GTK_A11Y="none")) as fixture, WaylandInput() as native:
    gtk_config = fixture.config.parent / "gtk-4.0"
    gtk_config.mkdir()
    (gtk_config / "settings.ini").write_text(
        "[Settings]\ngtk-enable-animations=true\ngtk-cursor-blink=true\ngtk-cursor-blink-time=1200\n")
    app = fixture.start_app(binary, stdout=log, stderr=log)
    pane = fixture.rpc("new", directory=str(fixture.path))["pane"]
    probe = ProgressProbe(fixture, pane)
    daemon = fixture.rpc("status")["pid"]

    def view():
        return fixture.rpc("renderer-info", pane=pane)["viewport"]

    def draw(mode):
        grid = view()
        cols, rows = grid["cols"], grid["rows"]
        text = "\x1b[0m\x1b[?25l\x1b[H\x1b[2J"
        for y in range(rows):
            text += f"\x1b[{y + 1};1H\x1b[0;4:{1 + y % 5};58;2;180;80;60m"
            if mode == "dense":
                text += "\x1b[5m" + "A" * cols
            elif mode == "alternating":
                text += "".join(
                    ("\x1b[5m" if x % 2 == 0 else "\x1b[25m") + "A" for x in range(cols))
            elif mode == "sparse" and y == rows // 2:
                text += "\x1b[5mA\x1b[25m" + "A" * (cols - 1)
            else:
                text += "A" * cols
        text += "\x1b[0m\x1b[H\x1b[48;2;230;30;45m \x1b[0m"
        probe.write(text)

    draw("steady")
    fixture.rpc("focus", pane=pane)
    native.click_color((230, 30, 45))
    wait_for(lambda: fixture.rpc("renderer-info")["window_active"])
    geometry = view()
    report = dict(binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  gtk=subprocess.check_output(["pkg-config", "--modversion", "gtk4"], text=True).strip(),
                  renderer=fixture.rpc("renderer-info")["gtk"], cols=geometry["cols"], rows=geometry["rows"],
                  cycle_ms=1200, cpu_model=next(line.split(":", 1)[1].strip() for line in
                      Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
                  method="Process CPU percent of one core, sampled RSS, main-thread context switches. No RPCs or PTY writes in intervals. Excludes writer, compositor and observer.",
                  scenarios=[])

    def measure(name, attached=True):
        time.sleep(1)
        status = fixture.rpc("status")
        pids = dict(daemon=daemon, **({"frontend": app.pid} if attached else {}))
        before = {role: sample(pid) for role, pid in pids.items()}
        started = time.monotonic()
        time.sleep(args.seconds)
        elapsed = time.monotonic() - started
        after = {role: sample(pid) for role, pid in pids.items()}
        end_status = fixture.rpc("status")
        traffic = {key: end_status[key] - status[key] for key in ("snapshots", "sent_rows")}
        assert traffic == dict(snapshots=0, sent_rows=0), traffic
        result = dict(name=name, seconds=elapsed, daemon_activity=traffic, resources={role: dict(
            cpu_percent=(after[role]["ticks"] - before[role]["ticks"]) / ticks / elapsed * 100,
            rss_mib=after[role]["rss_mib"],
            main_context_switches_per_second=(after[role]["switches"] - before[role]["switches"]) / elapsed,
        ) for role in pids})
        report["scenarios"].append(result)
        print(json.dumps(result), flush=True)

    for mode in ("steady", "sparse", "dense", "alternating"):
        draw(mode)
        wait_for(lambda: view()["text_blink_running"] == (mode != "steady"))
        if mode != "steady":
            wait_for(lambda: not view()["text_blink_phase_visible"])
        measure(mode)
    hidden = fixture.rpc("new", directory=str(fixture.path))["pane"]
    hidden_probe = ProgressProbe(fixture, hidden)
    hidden_probe.write("\x1b[?25l")
    wait_for(lambda: not view()["text_blink_running"])
    assert view()["text_blink_phase_visible"]
    measure("hidden-tab")
    fixture.rpc("focus", pane=pane)
    wait_for(lambda: view()["text_blink_running"])
    draw("steady")
    wait_for(lambda: not view()["text_blink_running"])
    measure("blink-removed")
    fixture.rpc("detach")
    wait_for(lambda: app.poll() is not None)
    measure("detached", attached=False)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
