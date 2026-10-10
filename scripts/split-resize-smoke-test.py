#!/usr/bin/env python3
"""Check split proportions during incremental native window resizing."""
import json
from pathlib import Path
from cargo_test_support import cargo_binary
import time

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, scenario, wait_for
from wayland_test_support import WaylandInput, ensure_wayland_session, monitor_scale

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "split-resize-smoke.log").open("w") as log:
    for scale in (1, 1.25):
        # Header controls impose the minimum width exercised by the clamping
        # scenario, independently of the application's pane-title default.
        with monitor_scale(scale), WaylandInput() as native, AppFixture(
            dict(appearance="dark", show_sidebar=False,
                 show_pane_titles=True,
                 use_system_font=False, font="Monospace", font_size=12),
            prefix="gmux-split-resize-", env={"GTK_A11Y": "none"},
        ) as fixture:
            rpc = fixture.rpc
            width, height = round(920 / scale), round(620 / scale)
            (fixture.config / "session.json").write_text(json.dumps(dict(
                tabs=[], filter="all", active=0, width=width,
                height=height, maximized=False,
            )))
            fixture.start_app(cargo_binary("gmux"), "--directory", root,
                              stdout=log, stderr=log)
            pane = rpc("list")["tabs"][0]["active_pane"]

            def geometry(target):
                return rpc("renderer-info", pane=target)["viewport"]

            def layout():
                return rpc("list")["tabs"][0]["layout"]

            def capture(name):
                rpc("screenshot", path=str(artifacts / f"split-resize-{name}-scale-{scale}.png"))

            def resize(w, h):
                rpc("resize", width=w, height=h)
                # Window allocation includes its children. Observe native GTK
                # resizing without forcing another layout and PNG on every step.
                wait_for(lambda: rpc("renderer-info", pane=pane)["window_size"] == [w, h],
                         message=f"GTK did not allocate the requested {w} by {h} window")

            def drag_divider(down):
                rpc("focus", pane=pane)
                rpc("send", pane=pane, text="printf '\\033[2J\\033[H\\033[48;2;230;30;45m \\033[0m'",
                    paste=True, enter=True)
                bounds = wait_for(lambda: native.frame(timeout=0.1, fresh=False).color_bounds((230, 30, 45)))
                path = artifacts / f"split-resize-placement-scale-{scale}.png"
                rpc("screenshot", path=str(path))
                image = GdkPixbuf.Pixbuf.new_from_file(str(path))
                marker = bytes((230, 30, 45, 255) if image.get_has_alpha() else (230, 30, 45))
                pixel = image.get_pixels().index(marker)
                x = pixel % image.get_rowstride() // image.get_n_channels()
                y = pixel // image.get_rowstride()
                offset = (bounds[0] - x * scale, bounds[1] - y * scale)
                first = geometry(pane)
                # The canvas has a 3px margin, followed by the 6px gutter.
                x = first["origin"][0] + (first["width"] / 2 if down else first["width"] + 6)
                y = first["origin"][1] + (first["height"] + 6 if down else first["height"] / 2)
                native.move(offset[0] + x * scale, offset[1] + y * scale)
                time.sleep(0.05)
                native.button(True)
                native.move(offset[0] + (x + (0 if down else 80)) * scale,
                            offset[1] + (y + (80 if down else 0)) * scale)
                try:
                    wait_for(lambda: layout()["Split"]["ratio"] > 0.55,
                             message="Dragging the native divider did not update its saved proportion")
                finally:
                    native.button(False)

            for down, axis in ((False, "width"), (True, "height")):
                with scenario(f"scale {scale}: equal {axis} splits survive one-pixel growth and shrinkage"):
                    resize(width, height)
                    other = rpc("split", pane=pane, down=down)["pane"]
                    wait_for(lambda: geometry(pane)[axis] < geometry(other)[axis] + 20)
                    resize(width, height)
                    capture(f"{axis}-initial")
                    samples = []
                    for delta in (*range(81), *range(79, -1, -1)):
                        resize(width + (delta if not down else 0), height + (delta if down else 0))
                        first, second = geometry(pane), geometry(other)
                        samples.append(dict(delta=delta, first=first[axis], second=second[axis],
                                            ratio=layout()["Split"]["ratio"]))
                    report = dict(scale=scale, axis=axis, samples=samples)
                    (artifacts / f"split-resize-{axis}-scale-{scale}.json").write_text(
                        json.dumps(report, indent=2) + "\n")
                    print(json.dumps(dict(scale=scale, axis=axis, first=samples[0],
                                          widest=samples[80], last=samples[-1])), flush=True)
                    for sample in samples:
                        assert abs(sample["first"] - sample["second"]) <= 1, sample
                        assert sample["ratio"] == 0.5, sample
                    capture(f"{axis}-equal")

                with scenario(f"scale {scale}: dragged {axis} proportions survive resizing"):
                    drag_divider(down)
                    resize(width, height)
                    capture(f"{axis}-dragged")
                    ratio = layout()["Split"]["ratio"]
                    first, second = geometry(pane), geometry(other)
                    for delta in (*range(41), *range(39, -1, -1)):
                        resize(width + (delta if not down else 0), height + (delta if down else 0))
                        current, sibling = geometry(pane), geometry(other)
                        growth = current[axis] + sibling[axis] - first[axis] - second[axis]
                        assert abs(current[axis] - first[axis] - growth * ratio) <= 1, (ratio, current, sibling)
                        assert layout()["Split"]["ratio"] == ratio, layout()
                    rpc("close", pane=other)

            with scenario(f"scale {scale}: nested unequal proportions restore and survive resizing"):
                right = rpc("split", pane=pane)["pane"]
                bottom = rpc("split", pane=right, down=True)["pane"]
                fixture.stop()
                session_path = fixture.config / "session.json"
                saved = json.loads(session_path.read_text())
                restored_layout = saved["tabs"][0]["layout"]
                restored_layout["Split"]["ratio"] = 0.9
                restored_layout["Split"]["second"]["Split"]["ratio"] = 0.35
                session_path.write_text(json.dumps(saved))
                fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
                resize(width, height)
                capture("nested-restored")
                first, top, lower = geometry(pane), geometry(right), geometry(bottom)
                assert abs(first["width"] / (first["width"] + top["width"]) - 0.9) < 0.02
                assert abs(top["height"] / (top["height"] + lower["height"]) - 0.35) < 0.05
                for delta in (*range(41), *range(39, -1, -1)):
                    resize(width + delta, height + delta)
                    current, upper, sibling = geometry(pane), geometry(right), geometry(bottom)
                    width_growth = current["width"] + upper["width"] - first["width"] - top["width"]
                    height_growth = upper["height"] + sibling["height"] - top["height"] - lower["height"]
                    assert abs(current["width"] - first["width"] - width_growth * 0.9) <= 1
                    assert abs(upper["height"] - top["height"] - height_growth * 0.35) <= 1
                    assert layout() == restored_layout, layout()

                # Minimum sizes can temporarily prevent the requested ratio.
                # Widening again must recover it without saving the clamped size.
                resize(360, 280)
                assert layout() == restored_layout, layout()
                small, narrow = geometry(pane), geometry(right)
                available = small["width"] + narrow["width"] + 12
                assert small["width"] + 6 < available * 0.9 - 1, (small, narrow)
                resize(width, height)
                for target, before in ((pane, first), (right, top), (bottom, lower)):
                    current = geometry(target)
                    assert (current["width"], current["height"]) == (before["width"], before["height"])
                assert layout() == restored_layout, layout()
                fixture.stop()
                assert json.loads(session_path.read_text())["tabs"][0]["layout"] == restored_layout
