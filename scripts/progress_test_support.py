"""A terminal writer controlled without typing further commands into its PTY."""
import json
from pathlib import Path
import shlex
import sys
from test_support import wait_for


class ProgressProbe:
    def __init__(self, fixture, pane):
        self.fixture, self.pane = fixture, pane
        self.control = fixture.path / f"progress-{pane}.json"
        self.applied = fixture.path / f"progress-{pane}.applied"
        self.serial = 0
        self.control.write_text(json.dumps([0, ""]))
        script = fixture.path / "progress-probe.py"
        script.write_text('''import json, os, sys, time
from pathlib import Path
control, applied = map(Path, sys.argv[1:])
os.write(1, b'\\x1b]2;PROGRESS_PROBE_READY\\x07')
last = 0
while True:
    serial, data = json.loads(control.read_text())
    if serial != last:
        if isinstance(data, dict):
            for chunk in data["chunks"]:
                os.write(1, chunk.encode())
                time.sleep(data["interval"])
        else:
            os.write(1, data.encode())
        last = serial
        applied.write_text(str(serial))
    time.sleep(.01)
''')
        command = " ".join(map(shlex.quote, [sys.executable, str(script), str(self.control), str(self.applied)]))
        fixture.rpc("send", pane=pane, text=command, enter=True)
        wait_for(lambda: fixture.rpc("_metadata", pane=pane)["meta"]["title"] == "PROGRESS_PROBE_READY")

    def write(self, data, *, wait=True):
        self.serial += 1
        temporary = self.control.with_suffix(".tmp")
        temporary.write_text(json.dumps([self.serial, data]))
        temporary.replace(self.control)
        if wait:
            self.wait_applied()

    def wait_applied(self):
        wait_for(lambda: self.applied.exists() and self.applied.read_text() == str(self.serial))

    def burst(self, chunks, interval, *, wait=True):
        self.write(dict(chunks=chunks, interval=interval), wait=wait)

    def report(self, state, percentage=None):
        self.write(f"\x1b]9;4;{state}" + ("" if percentage is None else f";{percentage}") + "\x07")

    def progress(self):
        return self.fixture.rpc("_metadata", pane=self.pane)["meta"]["progress"]

    def expect(self, state, percentage):
        expected = dict(state=state, percentage=percentage)
        wait_for(lambda: self.progress() == expected)
        return expected
