#!/usr/bin/env python3
"""Copy a tracked selection while row-resize replies are held, using Wayland."""
from pathlib import Path

from cargo_test_support import cargo_binary

from test_support import AppFixture, DelayedViewport, wait_accessible as wait_for
from wayland_test_support import ClipboardObserver, WaylandInput, ensure_wayland_session

ensure_wayland_session()

root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)


with (artifacts / "terminal-copy-smoke.log").open("w") as log, WaylandInput() as native:
    clipboard = ClipboardObserver(native)
    for direction in (1, -1):
        with AppFixture(dict(restore=False, desktop_notifications=False), prefix="gmux-copy-",
                shell_script="#!/bin/sh\nexport PS1='COPY_READY> '\nstty -echo\nexec /bin/bash --noprofile --norc -i\n") as fixture:
            rpc = fixture.rpc
            fixture.start(cargo_binary("gmux-daemon"), stdout=log, stderr=log)
            wait_for(lambda: rpc("daemon-info"))
            original = fixture.env["GMUX_SOCKET"]
            replies = []
            with DelayedViewport(original, fixture.path / "frontend", observe_header=replies.append) as proxy:
                fixture.env["GMUX_SOCKET"] = proxy.proxy_path
                fixture.start_app(cargo_binary("gmux"), stdout=log, stderr=log)
                pane = rpc("new", name="Copy row resize")["pane"]
                wait_for(lambda: "COPY_READY>" in rpc("read", pane=pane)["text"])
                text = "ROW_COPY_界 e\u0301 exact Unicode " + ("grow" if direction == 1 else "shrink")
                rgb = (78, 90, 123) if direction == 1 else (90, 78, 123)
                color = ";".join(map(str, rgb))
                rpc("send", pane=pane, text="printf '\\033[2J\\033[H\\033[48;2;" + color + "m" + text + "\\033[0m\\n'",
                    paste=True, enter=True)
                wait_for(lambda: text in rpc("read", pane=pane)["text"])
                # Distinct exact colors prevent the previous app's queued frame
                # from supplying this app's selection coordinates.
                left, top, right, bottom = wait_for(lambda: native.frame(timeout=.1, fresh=False).color_bounds(rgb))
                state = rpc("renderer-info", pane=pane)["viewport"]
                cw, ch = state["cell_width"], state["cell_height"]
                native.move(left + cw / 2, top + ch / 2)
                native.button(True)
                native.move(left + (len(text) - .5) * cw, top + ch / 2)
                native.button(False)
                wait_for(lambda: rpc("_copy", pane=pane)["text"] == text)

                def selected():
                    state = rpc("renderer-info", pane=pane)
                    return state if state["viewport"]["selection"] is not None else None

                before = wait_for(selected)
                owner_count = clipboard.checkpoint()
                with proxy.hold_replies() as held:
                    width, height = before["window_size"]
                    new_height = height + direction * int(ch) * 2
                    rpc("resize", width=width, height=new_height)

                    def allocated():
                        state = rpc("renderer-info", pane=pane)
                        return state if state["window_size"] == [width, new_height] and state["viewport"]["height"] != before["viewport"]["height"] else None

                    after = wait_for(allocated)
                    assert after["viewport"]["cols"] == before["viewport"]["cols"], (before, after)
                    size = wait_for(lambda: (size if (size := rpc("_metadata", pane=pane)["meta"]["size"])[1] != before["viewport"]["rows"] else None))
                    assert size[0] == before["viewport"]["cols"], (before, size)
                    assert held.wait(3), "No daemon update reached the reply barrier"
                    assert rpc("_copy", pane=pane)["text"] == text
                    assert rpc("renderer-info", pane=pane)["viewport"]["selection"] is None
                    rpc("action", name="copy")

                # A matching old clipboard value cannot satisfy this assertion.
                clipboard.wait_for_text(text, owner_count)
                wait_for(selected)

                # Clear the actual backend selection, then Copy must keep both
                # the existing clipboard text and its owner.
                native.move(left + cw / 2, top + ch / 2)
                native.button(True)
                native.button(False)
                wait_for(lambda: rpc("_copy", pane=pane)["text"] == "")
                owner_count = clipboard.checkpoint()
                reply_count = len(replies)
                rpc("action", name="copy")
                rejection = wait_for(lambda: next((header for header in replies[reply_count:]
                    if header["meta"]["id"] == pane and header["error"] == "Select terminal text to copy it"), None),
                    message="Copy without a selection did not return the daemon rejection")
                assert rejection["selection"] is None and rejection["copy"] is None, rejection
                assert all(header["copy"] is None for header in replies[reply_count:]), replies[reply_count:]
                clipboard.assert_preserved(text, owner_count)
                print(f"PASS: row {'growth' if direction == 1 else 'shrink'}, exact Unicode clipboard publication with replies held, absent selection preserves text and owner", flush=True)
