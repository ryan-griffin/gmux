#!/usr/bin/env python3
"""Check the native resize overlay, PTY sizing, and dismissal deadline."""
from pathlib import Path
from cargo_test_support import cargo_binary
import json
import re
import shlex
import time

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, assert_stays, scenario, wait_for
from wayland_test_support import monitor_scale

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


with (artifacts / "resize-smoke.log").open("w") as log:
    for scale in (1, 1.25):
        with monitor_scale(scale), AppFixture(
            dict(appearance="dark", restore=True), prefix="gmux-resize-",
            env={"GTK_A11Y": "none", "GDK_DEBUG": "no-portals,no-vsync"},
            shell_script="#!/bin/sh\nexport PS1='GMUX_RESIZE> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n",
        ) as fixture:
            rpc = fixture.rpc
            # Avoid Mutter auto-maximizing the default window on the smaller
            # logical monitor at fractional scale. This session has no tabs.
            (fixture.config / "session.json").write_text(json.dumps(dict(
                tabs=[], filter="all", active=0, width=round(1120 / scale),
                height=round(740 / scale), maximized=False,
            )))
            fixture.start_app(cargo_binary("gmux"), "--directory", root, stdout=log, stderr=log)
            pane = rpc("list")["tabs"][0]["active_pane"]
            wait_for(lambda: "GMUX_RESIZE>" in rpc("read", pane=pane)["text"])
            geometry_probe = fixture.path / "geometry.py"
            geometry_result = fixture.path / "geometry.json"
            geometry_probe.write_text(
                "import fcntl, json, struct, sys, termios\n"
                "from pathlib import Path\n"
                "Path(sys.argv[1]).write_text(json.dumps(struct.unpack('HHHH', "
                "fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)))))\n"
            )

            def capture(name):
                path = artifacts / f"{name}-scale-{scale}.png"
                rpc("screenshot", path=str(path))
                return GdkPixbuf.Pixbuf.new_from_file(str(path))

            def grid(marker):
                rpc("send", pane=pane, text=f"printf '\\033[2J\\033[H{marker} '; stty size", paste=True, enter=True)
                match = wait_for(lambda: re.search(rf"^{marker} (\d+) (\d+)$",
                                                  rpc("read", pane=pane)["text"], re.MULTILINE))
                return tuple(map(int, match.groups()))

            def geometry():
                # Keep probe results out of the pixels sampled for the resize overlay.
                geometry_result.unlink(missing_ok=True)
                rpc("send", pane=pane,
                    text=f"printf '\\033[2J\\033[H'; python3 {shlex.quote(str(geometry_probe))} {shlex.quote(str(geometry_result))}",
                    paste=True, enter=True)
                return wait_for(lambda: tuple(json.loads(geometry_result.read_text())),
                                exceptions=(OSError, json.JSONDecodeError))

            def resize(width, height):
                rpc("resize", width=width, height=height)

                def allocated():
                    capture("resize-allocation")
                    info = rpc("renderer-info", pane=pane)
                    return info if info["window_size"] == [width, height] else None

                return wait_for(allocated, message=f"GTK did not allocate the requested {width} by {height} window")

            # Keep the same physical footprint inside the fixture monitor at both scales.
            height = round(680 / scale)
            resize(round(1120 / scale), round(740 / scale))
            assert rpc("renderer-info", pane=pane)["viewport"]["scale"] == scale

            with scenario(f"scale {scale}: resize shows the overlay and dismissal preserves PTY dimensions"):
                wait_for(lambda: central_text(capture("resize-initial")) == 0,
                         message="Initial resize overlay did not settle")
                old = grid("GRID_BEFORE")
                resize(round(960 / scale), height)
                wait_for(lambda: central_text(capture("resize-during")) > 30, timeout=2,
                         message="Resizing did not show the centered overlay")
                resized = grid("GRID_DURING")
                assert old != resized, (old, resized)
                wait_for(lambda: central_text(capture("resize-after")) == 0, timeout=3,
                         message="The resize overlay did not dismiss")
                assert grid("GRID_AFTER") == resized, "Overlay dismissal resized the PTY"

            with scenario(f"scale {scale}: a later resize restarts the overlay's dismissal deadline"):
                resize(round(1000 / scale), height)
                wait_for(lambda: central_text(capture("resize-again")) > 30, timeout=2)
                first_visible = time.monotonic()
                # These intervals test the timer itself. The observation falls after
                # the first resize's dismissal and before the second one's dismissal.
                time.sleep(0.5)
                resize(round(1040 / scale), height)
                capture("resize-repeat-layout")
                remaining = first_visible + 1.15 - time.monotonic()
                if remaining > 0:
                    time.sleep(remaining)
                assert central_text(capture("resize-repeat")) > 30, "The later resize did not restart dismissal"
                wait_for(lambda: central_text(capture("resize-repeat-after")) == 0, timeout=3,
                         message="The restarted resize overlay did not dismiss")

            for axis, cell in (("width", "cell_width"), ("height", "cell_height")):
                viewport = rpc("renderer-info", pane=pane)["viewport"]
                # Move within the current cell, then back, retaining the same grid.
                delta = -1 if viewport[axis] % viewport[cell] >= 1 else 1
                for change in (delta, -delta):
                    with scenario(f"scale {scale}: pixel-only {axis} {change:+} updates PTY geometry without a grid overlay"):
                        info = rpc("renderer-info", pane=pane)
                        viewport = info["viewport"]
                        before = geometry()
                        assert before == (viewport["rows"], viewport["cols"], viewport["width"], viewport["height"])
                        index = 0 if axis == "width" else 1
                        expected_pixels = [viewport["width"], viewport["height"]]
                        expected_pixels[index] += change

                        # The adaptive sidebar may consume one window pixel. Wait for
                        # each actual GTK allocation and require exactly one canvas pixel.
                        for window_delta in (change, 2 * change):
                            requested = list(info["window_size"])
                            requested[index] += window_delta
                            resized = resize(*requested)["viewport"]
                            assert central_text(capture("resize-pixel-only")) == 0, "A pixel-only resize showed a grid overlay"
                            assert (resized["rows"], resized["cols"]) == before[:2], resized
                            if resized[axis] != viewport[axis]:
                                break
                        assert [resized["width"], resized["height"]] == expected_pixels, resized
                        expected = (*before[:2], *expected_pixels)
                        wait_for(lambda: geometry() == expected, message="PTY pixel size stayed stale at the same grid size")

                        # Catch a delayed compositor configure undoing the allocation,
                        # and check overlay absence throughout its dismissal interval.
                        def preserved():
                            shot = capture("resize-pixel-only-overlay")
                            current = rpc("renderer-info", pane=pane)
                            return (central_text(shot) == 0 and current["window_size"] == requested
                                    and current["viewport"]["width"] == expected_pixels[0]
                                    and current["viewport"]["height"] == expected_pixels[1]
                                    and (current["viewport"]["rows"], current["viewport"]["cols"]) == before[:2])

                        assert_stays(preserved, 1.1, message="Pixel-only resize reverted or showed a grid overlay")
                        assert geometry() == expected, "PTY geometry changed after the pixel-only resize settled"
                        report = dict(scale=scale, axis=axis, change=change, window=requested,
                                      before=before, after=expected)
                        (artifacts / f"resize-pixel-{axis}-{change:+}-scale-{scale}.json").write_text(json.dumps(report, indent=2) + "\n")
                        print(json.dumps(report), flush=True)
