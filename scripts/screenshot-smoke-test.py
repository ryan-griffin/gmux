#!/usr/bin/env python3
"""Check fresh diagnostic screenshots with a paused, occluded Wayland window."""
import json
import math
import os
from pathlib import Path
from cargo_test_support import cargo_binary
import sys
import time

import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

from test_support import AppFixture, assert_stays, scenario, stop_process, wait_for
from wayland_test_support import WaylandInput, monitor_scale

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)
appearance = os.environ.get("GMUX_REVIEW_APPEARANCE", "dark")
window_size = (560, 400)
mapped_color = (52, 86, 120)
occluded_color = (161, 67, 101)

# This process connects only to the fixture's private Wayland display. A solid
# fullscreen window lets the compositor capture prove that Gmux is fully hidden.
cover_script = """
import gi
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, GLib
Gtk.init()
css = Gtk.CssProvider()
css.load_from_string('window { background: rgb(12, 200, 50); }')
window = Gtk.Window(decorated=False)
Gtk.StyleContext.add_provider_for_display(window.get_display(), css, 800)
window.fullscreen()
window.present()
GLib.MainLoop().run()
"""


def color_count(pixbuf, color):
    pixels = pixbuf.get_pixels()
    channels, stride = pixbuf.get_n_channels(), pixbuf.get_rowstride()
    target = bytes(color)
    return sum(pixels[y * stride + x * channels:y * stride + x * channels + 3] == target
               for y in range(pixbuf.get_height()) for x in range(pixbuf.get_width()))


def decoration(pixbuf):
    # The root window's CSS shadow extends outside the child content. Require
    # translucent shadow pixels and opaque header pixels in the same image.
    width, height = pixbuf.get_width(), pixbuf.get_height()
    assert pixbuf.get_has_alpha(), "Screenshot lost the window shadow's alpha channel"
    pixels = pixbuf.get_pixels()
    channels, stride = pixbuf.get_n_channels(), pixbuf.get_rowstride()

    def alpha(y):
        return pixels[y * stride + (width // 2) * channels + 3]

    assert any(0 < alpha(y) < 255 for y in range(20)), \
        "Screenshot lost the window's top shadow"
    assert alpha(35) == 255, "Screenshot lost the window header"
    assert alpha(height // 2) == 255, "Screenshot lost its background"


def body_dimensions(pixbuf):
    # Adwaita changes the shadow's extent when the window loses focus. Measure
    # the opaque window body separately from that state-dependent PNG padding.
    width, height = pixbuf.get_width(), pixbuf.get_height()
    pixels = pixbuf.get_pixels()
    channels, stride = pixbuf.get_n_channels(), pixbuf.get_rowstride()
    assert channels == 4, "Screenshot lost its alpha channel"
    return (sum(pixels[(height // 2) * stride + x * channels + 3] == 255 for x in range(width)),
            sum(pixels[y * stride + (width // 2) * channels + 3] == 255 for y in range(height)))


with (artifacts / f"screenshot-{appearance}-smoke.log").open("w") as log:
    for scale in (1, 1.25):
        with (
            scenario(f"fresh {appearance} mapped and occluded screenshots at scale {scale}"),
            monitor_scale(scale),
            WaylandInput() as desktop,
            AppFixture(
                dict(appearance=appearance, restore=True, desktop_notifications=False, show_sidebar=False,
                     use_system_font=False, font="Monospace", font_size=12),
                prefix="gmux-screenshot-", env={"GTK_A11Y": "none"},
            ) as fixture,
        ):
            # Start small enough to avoid Mutter automatically maximizing the
            # default 1120x740 window on the fractional-scale logical monitor.
            (fixture.config / "session.json").write_text(json.dumps(dict(
                tabs=[], filter="all", active=0,
                width=window_size[0], height=window_size[1], maximized=False,
            )))
            fixture.start_app(cargo_binary("gmux"), "--directory", root,
                              stdout=log, stderr=log)
            pane = fixture.rpc("list")["tabs"][0]["active_pane"]

            def capture(name):
                path = artifacts / f"screenshot-{appearance}-scale-{scale}-{name}.png"
                result = fixture.rpc("screenshot", path=str(path), check=False)
                if result.get("error") == "Window has not rendered yet":
                    return None
                assert "error" not in result, result
                return GdkPixbuf.Pixbuf.new_from_file(str(path))

            def updated_capture(name, color):
                shot = capture(name)
                if (shot is not None and body_dimensions(shot) == window_size
                        and color_count(shot, color) > 10_000):
                    return shot
                return None

            def update(color, marker):
                fixture.rpc("send", pane=pane, paste=True, enter=True,
                            text=f"printf '\\033]11;#{bytes(color).hex()}\\007\\033[2J\\033[H{marker}\\n'")
                wait_for(lambda: fixture.rpc("read", pane=pane)["text"].startswith(marker))

            update(mapped_color, "MAPPED_CAPTURE")

            mapped = wait_for(lambda: updated_capture("mapped", mapped_color),
                              message="Mapped screenshot did not update its content and logical size")
            decoration(mapped)
            dimensions = mapped.get_width(), mapped.get_height()
            info = fixture.rpc("renderer-info")
            assert info["window_mapped"] and info["window_scale"] == math.ceil(scale), info
            assert info["viewport"]["scale"] == scale, info
            assert info["display"] == "GdkWaylandDisplay", info
            print(f"Scale {scale}: mapped PNG {dimensions}, renderer {info['gtk']}", flush=True)

            cover = fixture.start(sys.executable, "-c", cover_script, stdout=log, stderr=log)

            def covered():
                assert cover.poll() is None, "The fullscreen cover exited"
                frame = desktop.frame(timeout=0.1, fresh=False)
                return all(frame.pixel(x, y) == (12, 200, 50)
                           for x in (10, frame.width // 2, frame.width - 10)
                           for y in (10, frame.height // 2, frame.height - 10))

            wait_for(covered, message="The private compositor did not fully cover Gmux")
            wait_for(lambda: not fixture.rpc("renderer-info")["window_active"])

            # Submit one hidden update before the measured change. GTK can
            # submit that first frame before blocking on a Wayland callback.
            update(mapped_color, "OCCLUSION_PRIMER")

            # Wait for Mutter to stop sending frame callbacks, then verify that
            # new terminal content does not advance the window's frame clock.
            observed = [None, time.monotonic()]

            def paused():
                current = fixture.rpc("renderer-info")["frame_counter"]
                if current != observed[0]:
                    observed[:] = current, time.monotonic()
                return time.monotonic() - observed[1] > 1.5

            wait_for(paused, message="The occluded window's frame clock did not pause")
            counter = observed[0]
            assert counter is not None
            update(occluded_color, "OCCLUDED_CAPTURE")
            assert_stays(lambda: fixture.rpc("renderer-info")["frame_counter"] == counter, 0.3,
                         message="The occluded window rendered an on-screen frame")
            assert fixture.rpc("renderer-info")["window_mapped"]
            occluded = wait_for(lambda: updated_capture("occluded", occluded_color),
                                message="Occluded screenshot did not update its content and logical size")
            decoration(occluded)
            assert fixture.rpc("renderer-info")["frame_counter"] == counter, \
                "Capture depended on resuming the window's frame clock"
            assert covered(), "Capture raised Gmux above the fullscreen cover"
            print(f"Scale {scale}: fresh occluded PNG "
                  f"{(occluded.get_width(), occluded.get_height())} at paused frame {counter}", flush=True)

            stop_process(cover)
            wait_for(lambda: fixture.rpc("renderer-info")["frame_counter"] > counter,
                     message="The revealed window's frame clock did not resume")
            revealed = wait_for(lambda: updated_capture("revealed", occluded_color),
                                message="Revealed screenshot did not preserve its content and logical size")
            decoration(revealed)

print("PASS: fresh mapped, fully occluded, and revealed screenshots, decoration, and logical scaling")
