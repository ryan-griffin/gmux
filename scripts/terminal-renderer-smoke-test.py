#!/usr/bin/env python3
"""Exercise interactive terminal rendering and daemon reattachment on a native display.

Run with GTK_A11Y=none dbus-run-session -- python3 scripts/terminal-renderer-smoke-test.py.
Requires Vim and top. Uses isolated settings, shell, and daemon.
"""
import json
from test_support import rpc as shared_rpc, wait_for
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

from daemon_test_support import stop_daemon

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


with tempfile.TemporaryDirectory(prefix="gmux-terminal-renderer-") as directory:
    temp = Path(directory)
    config = temp / "config/gmux"
    config.mkdir(parents=True)
    (config / "settings.json").write_text(json.dumps(dict(
        appearance="dark", restore=False, desktop_notifications=False,
        use_system_font=False, font="Monospace", font_size=12,
    )))
    shell = temp / "shell"
    shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
    shell.chmod(0o700)
    text = temp / "fixture.txt"
    text.write_text("GMUX_VIM_READY\nUnicode: 界 e\u0301 👩‍💻\n" +
                    "\n".join(f"line {i:03d}: continuous terminal output" for i in range(150)) + "\n")
    env = dict(os.environ, XDG_CONFIG_HOME=str(temp / "config"), GMUX_SOCKET=str(temp / "socket"),
               SHELL=str(shell), GTK_A11Y="none", LC_ALL="C")
    env["GDK_DEBUG"] = env.get("GDK_DEBUG", "") + ":no-vsync"
    app = None

    def rpc(method, **params):
        return shared_rpc(env["GMUX_SOCKET"], method, check=True, **params)

    def output(pane):
        return rpc("read", pane=pane)["text"]

    def command(pane, text):
        rpc("send", pane=pane, text=text, paste=True, enter=True)

    def capture(name):
        rpc("screenshot", path=str(artifacts / f"terminal-renderer-{name}.png"))

    with (artifacts / "terminal-renderer-smoke.log").open("w") as log:
        def start():
            process = subprocess.Popen([str(root / "target/debug/gmux")], env=env, stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info")["attached"])
            return process

        try:
            app = start()
            first = rpc("new", name="Interactive renderer")
            pane = first["pane"]
            wait_for(lambda: rpc("renderer-info")["terminal"] == "gsk")
            command(pane, "vim -Nu NONE -n -i NONE " + shlex.quote(str(text)) +
                    " -c 'set number cursorline' -c 'syntax on'")
            wait_for(lambda: "GMUX_VIM_READY" in output(pane))
            capture("vim")
            rpc("send", pane=pane, text="iEDITED_\x1b")
            wait_for(lambda: "EDITED_GMUX_VIM_READY" in output(pane))
            second = rpc("split", pane=pane)["pane"]
            command(second, "top -d 0.1")
            capture("top-start")
            wait_for(lambda: "Tasks:" in output(second))
            rpc("focus", pane=pane)
            capture("split")
            rpc("action", name="zoom-in")
            capture("zoom")
            wait_for(lambda: "EDITED_GMUX_VIM_READY" in output(pane))
            rpc("resize", width=860, height=620)
            time.sleep(0.4)
            capture("resized")
            rpc("send", pane=pane, text="G")
            wait_for(lambda: "line 149" in output(pane))
            capture("scrolled")
            # Both alternate-screen applications keep running with no renderer.
            pid = rpc("daemon-info")["pid"]
            rpc("detach")
            assert app.wait(timeout=8) == 0
            app = None
            time.sleep(0.4)
            assert rpc("daemon-info")["pid"] == pid
            assert not rpc("daemon-info")["attached"]
            app = start()
            wait_for(lambda: "line 149" in output(pane))
            wait_for(lambda: "Tasks:" in output(second))
            capture("reattached")
            rpc("send", pane=pane, text="\x1b:wq\r")
            wait_for(lambda: text.read_text().startswith("EDITED_GMUX_VIM_READY"))
            rpc("send", pane=second, text="q")
            command(pane, "printf 'PRIMARY_SCREEN_RESTORED\\n'")
            wait_for(lambda: "PRIMARY_SCREEN_RESTORED" in output(pane))
            command(second, "printf 'TOP_EXITED\\n'")
            wait_for(lambda: "TOP_EXITED" in output(second))
            # Sustained colored output exercises repeated row replacement.
            command(second, "for i in $(seq 1 3000); do printf '\\033[32mFRAME_%04d\\033[0m\\n' \"$i\"; done")
            wait_for(lambda: "FRAME_3000" in output(second))
            capture("output")
            assert app.poll() is None
            print("PASS: Vim input/edit/save, Unicode, alternate screens, live top output, splits, zoom, "
                  "resize, scrolling, reattachment to running applications, primary-screen restoration, "
                  "and 3000 colored output lines")
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=8)
            stop_daemon(env["GMUX_SOCKET"])
