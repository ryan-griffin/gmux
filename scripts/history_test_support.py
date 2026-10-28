"""Shared terminal histories and Linux process measurements for benchmarks."""
import hashlib
import math
import os
from pathlib import Path
import statistics
import subprocess
import time


HISTORY_SHELL = '#!/bin/sh\nexec python3 "$GMUX_HISTORY_WORKER"\n'


WORKER = r'''
import os, tty
tty.setraw(0)
os.write(1, b"BOOT")
sequence = 0
generation = 0
while True:
    key = os.read(0, 1)
    if not key or key == b"q":
        break
    if key == b"g":
        generation += 1
        lines = int(os.environ["GMUX_HISTORY_LINES"])
        for start in range(0, lines, 256):
            data = "".join(
                f"\x1b[38;5;{row % 16}m{row:06d} build src/module_{row % 97:02d}.rs "
                f"compiled successfully 界é \x1b[0m\x1b]8;;https://example.com/log/{row % 32}\x07details\x1b]8;;\x07\r\n"
                for row in range(start, min(start + 256, lines)))
            os.write(1, data.encode())
        os.write(1, f"READY{generation:06d}".encode())
    else:
        sequence += 1
        os.write(1, f"\r\x1b[2KACK{sequence:06d}".encode())
'''


def process_sample(pid):
    fields = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines()
                  if ":" in line)
    main = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/task/{pid}/status").read_text().splitlines()
                if ":" in line)
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return dict(rss_bytes=int(fields["VmRSS"].split()[0]) * 1024,
                cpu_seconds=(int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK"),
                main_waits=int(main["voluntary_ctxt_switches"]),
                wait_channel=Path(f"/proc/{pid}/wchan").read_text().strip())


def distribution(values):
    return dict(samples=len(values), median_ms=statistics.median(values),
                p95_ms=sorted(values)[math.ceil(len(values) * .95) - 1], max_ms=max(values))


def binary_metadata(target, baseline=False):
    linkage = subprocess.check_output(["ldd", str(target)], text=True)
    library = next(Path(line.split()[2]).resolve() for line in linkage.splitlines()
                   if line.strip().startswith("libghostty-vt.so") and "=>" in line)
    return dict(path=str(target), baseline=baseline,
                sha256=hashlib.sha256(target.read_bytes()).hexdigest(),
                ghostty_library=str(library), ghostty_sha256=hashlib.sha256(library.read_bytes()).hexdigest())


def prepare_history(fixture, lines):
    worker = fixture.path / "worker.py"
    worker.write_text(WORKER)
    fixture.env.update(GMUX_HISTORY_WORKER=str(worker), GMUX_HISTORY_LINES=str(lines))


def idle_sample(pid, seconds=5.5):
    # No RPCs during this interval. Inspect only /proc from the observer.
    time.sleep(.15)
    before = process_sample(pid)
    time.sleep(seconds)
    after = process_sample(pid)
    return dict(seconds=seconds, main_wakeups=after["main_waits"] - before["main_waits"],
                cpu_seconds=after["cpu_seconds"] - before["cpu_seconds"],
                wait_channel=after["wait_channel"])

