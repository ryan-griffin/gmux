#!/usr/bin/env python3
"""Check system font changes, zoom, and custom fonts on the isolated native display."""
from pathlib import Path
import re
import subprocess

from test_support import AppFixture, assert_stays, scenario, wait_for

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


with (artifacts / "font-smoke.log").open("w") as log, AppFixture(
    prefix="gmux-font-", env={"GSETTINGS_BACKEND": "keyfile", "GTK_A11Y": "none"},
    shell_script="#!/bin/sh\nexport PS1='GMUX_FONT> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n",
) as fixture:
    rpc = fixture.rpc
    serial = 0

    def set_font(size):
        subprocess.run(["gsettings", "set", "org.gnome.desktop.interface",
                        "monospace-font-name", f"Monospace {size}"], env=fixture.env, check=True)

    def metrics(pane):
        # Force layout even when a dialog or another window occludes the pane.
        rpc("screenshot", path=str(artifacts / "font-grid.png"))
        viewport = rpc("renderer-info", pane=pane)["viewport"]
        return viewport["cell_width"], viewport["cell_height"]

    def grid(pane):
        global serial
        wait_for(lambda: "GMUX_FONT>" in rpc("read", pane=pane)["text"])
        rpc("screenshot", path=str(artifacts / "font-grid.png"))
        serial += 1
        marker = f"FONT_SIZE_{serial}"
        rpc("send", pane=pane, text=f"printf '\\n{marker} '; stty size", paste=True, enter=True)
        match = wait_for(lambda: re.search(rf"^{marker} (\d+) (\d+)$",
                                          rpc("read", pane=pane)["text"], re.MULTILINE))
        return tuple(map(int, match.groups()))

    with scenario("system font changes resize both PTYs and zoom reset follows GNOME"):
        set_font(9)
        fixture.set_settings(dict(use_system_font=True, font="Monospace", font_size=12))
        app = fixture.start_app(root / "target/debug/gmux", "--directory", root,
                                cwd=root, stdout=log, stderr=log)
        first = rpc("list")["tabs"][0]["active_pane"]
        second = rpc("split", pane=first)["pane"]
        small = [grid(first), grid(second)]
        before = [metrics(first), metrics(second)]
        set_font(18)
        wait_for(lambda: all(metrics(p)[1] > old[1] for p, old in zip((first, second), before)),
                 message="System font did not update both panes")

        def larger_grid():
            sizes = [grid(first), grid(second)]
            if all(b[0] < a[0] and b[1] < a[1] for a, b in zip(small, sizes)):
                return sizes

        large = wait_for(larger_grid, message="PTY sizes did not follow the larger font")
        baseline = metrics(second)
        rpc("action", name="zoom-in")
        wait_for(lambda: metrics(second)[1] > baseline[1])
        wait_for(lambda: grid(second)[1] < large[1][1])
        rpc("action", name="zoom-reset")
        wait_for(lambda: metrics(second) == baseline)
        wait_for(lambda: grid(second) == large[1], message="Zoom reset did not restore the GNOME size")

    with scenario("system font changes update both panes while Preferences is open"):
        before = [metrics(first), metrics(second)]
        rpc("action", name="preferences")
        set_font(11)
        wait_for(lambda: all(metrics(p)[1] < old[1] for p, old in zip((first, second), before)),
                 message="Preferences prevented the system font update")
        wait_for(lambda: all(b[0] > a[0] and b[1] > a[1]
                 for a, b in zip(large, [grid(first), grid(second)])),
                 message="PTY sizes did not follow the font change with Preferences open")
        rpc("screenshot", path=str(artifacts / "system-font-preferences.png"))
        assert app.poll() is None
    fixture.stop()

    with scenario("custom fonts stay fixed when GNOME's font changes"):
        fixture.set_settings(dict(use_system_font=False, font="Monospace", font_size=12))
        fixture.start_app(root / "target/debug/gmux", "--directory", root,
                          cwd=root, stdout=log, stderr=log)
        pane = rpc("list")["tabs"][0]["active_pane"]
        custom, custom_metrics = grid(pane), metrics(pane)
        set_font(20)
        assert_stays(lambda: metrics(pane) == custom_metrics and grid(pane) == custom, 0.6,
                     message="GNOME font changed a pane using a custom font")
