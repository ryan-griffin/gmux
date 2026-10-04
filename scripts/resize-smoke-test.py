#!/usr/bin/env python3
"""Check the native resize overlay, PTY sizing, and dismissal deadline."""
from pathlib import Path
import re
import time

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


def central_text(image):
    # The shell's output stays at the top. Only the centered overlay adds
    # bright pixels to this otherwise blank portion of the dark terminal.
    width, height = image.get_width(), image.get_height()
    pixels = image.get_pixels()
    channels, stride = image.get_n_channels(), image.get_rowstride()
    return sum(
        max(pixels[y * stride + x * channels:y * stride + x * channels + 3]) > 130
        for y in range(int(height * 0.4), int(height * 0.72))
        for x in range(int(width * 0.44), int(width * 0.86))
    )


with (artifacts / "resize-smoke.log").open("w") as log, AppFixture(
    dict(appearance="dark", restore=False), prefix="gmux-resize-",
    env={"GTK_A11Y": "none", "GDK_DEBUG": "no-vsync"},
    shell_script="#!/bin/sh\nexport PS1='GMUX_RESIZE> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n",
) as fixture:
    rpc = fixture.rpc
    fixture.start_app(root / "target/debug/gmux", "--directory", root, stdout=log, stderr=log)
    pane = rpc("list")["tabs"][0]["active_pane"]
    wait_for(lambda: "GMUX_RESIZE>" in rpc("read", pane=pane)["text"])

    def capture(name):
        path = artifacts / f"{name}.png"
        rpc("screenshot", path=str(path))
        return GdkPixbuf.Pixbuf.new_from_file(str(path))

    def grid(marker):
        rpc("send", pane=pane, text=f"printf '\\n{marker} '; stty size", paste=True, enter=True)
        match = wait_for(lambda: re.search(rf"^{marker} (\d+) (\d+)$",
                                          rpc("read", pane=pane)["text"], re.MULTILINE))
        return tuple(map(int, match.groups()))

    with scenario("resize shows the overlay and dismissal preserves PTY dimensions"):
        wait_for(lambda: central_text(capture("resize-initial")) == 0,
                 message="Initial resize overlay did not settle")
        old = grid("GRID_BEFORE")
        rpc("resize", width=960, height=680)
        wait_for(lambda: central_text(capture("resize-during")) > 30, timeout=2,
                 message="Resizing did not show the centered overlay")
        resized = grid("GRID_DURING")
        assert old != resized, (old, resized)
        wait_for(lambda: central_text(capture("resize-after")) == 0, timeout=3,
                 message="The resize overlay did not dismiss")
        assert grid("GRID_AFTER") == resized, "Overlay dismissal resized the PTY"

    with scenario("a later resize restarts the overlay's dismissal deadline"):
        rpc("resize", width=1000, height=680)
        wait_for(lambda: central_text(capture("resize-again")) > 30, timeout=2)
        first_visible = time.monotonic()
        # These intervals test the timer itself. The observation falls after
        # the first resize's dismissal and before the second one's dismissal.
        time.sleep(0.5)
        rpc("resize", width=1040, height=680)
        capture("resize-repeat-layout")
        remaining = first_visible + 1.15 - time.monotonic()
        if remaining > 0:
            time.sleep(remaining)
        assert central_text(capture("resize-repeat")) > 30, "The later resize did not restart dismissal"
        wait_for(lambda: central_text(capture("resize-repeat-after")) == 0, timeout=3,
                 message="The restarted resize overlay did not dismiss")
