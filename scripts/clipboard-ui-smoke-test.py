#!/usr/bin/env python3
"""Read exact application MIME offers from GTK over native Wayland."""
from pathlib import Path
from functools import partial

from cargo_test_support import cargo_binary
from clipboard_test_support import PNG, check_html, kitty_write
from progress_test_support import ProgressProbe
from test_support import AppFixture, assert_stays, scenario, wait_for as shared_wait_for
from wayland_test_support import WaylandInput, ensure_wayland_session
from gi.repository import GLib

wait_for = partial(shared_wait_for, context=GLib.MainContext.default())

ensure_wayland_session()
root = Path(__file__).resolve().parent.parent
artifacts = root / "artifacts"
artifacts.mkdir(exist_ok=True)

with (artifacts / "clipboard-ui-smoke.log").open("w") as log, \
        AppFixture(env={"GTK_A11Y": "none"}) as desktop, AppFixture() as server, WaylandInput() as native:
    server.rpc("connection-edit", connection="local", remote_access=dict(
        enabled=True, listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    server.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    address = wait_for(lambda: server.rpc("status")["bound_address"])
    desktop.rpc("connection-add", name="server", address=address, authentication=dict(mode="none"))
    desktop.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)

    for connection, owner in [("local", desktop), ("server", server)]:
        created = desktop.rpc("new", connection=connection, name="MIME " + connection)
        pane = created["pane"]
        probe = ProgressProbe(owner, pane)
        serial = 0

        def emit(sequence):
            global serial
            serial += 1
            title = f"MIME_{connection}_{serial}"
            probe.write(sequence + f"\x1b]2;{title}\x07")
            wait_for(lambda: owner.rpc("_metadata", pane=pane)["meta"]["title"] == title)

        def formats(expected):
            wait_for(lambda: native.clipboard_formats is not None
                     and set(native.clipboard_formats) >= set(expected),
                     message=f"Missing MIME offer {expected}; got {native.clipboard_formats}")

        with scenario(f"{connection} application publishes PNG, HTML and text in one GTK offer"):
            plain = (connection + " 界🙂e\u0301\n").encode()
            html = b"<b>application</b>\0tail"
            emit(kitty_write([("image/png", PNG), ("text/html", html), ("text/plain", plain)]))
            formats(["image/png", "text/html", "text/plain", "text/plain;charset=utf-8"])
            assert native.clipboard_bytes("image/png") == PNG
            assert native.clipboard_bytes("text/html") == html
            assert native.clipboard_bytes("text/plain") == plain
            assert native.clipboard_text() == plain.decode()
            # Reads happen after the daemon packet and its source transaction died.
            assert native.clipboard_bytes("image/png") == PNG

        with scenario(f"{connection} rejects the whole invalid offer without changing GTK contents"):
            for contents in [
                [("text/plain", b"partial"), ("image/jpeg", PNG)],
                [("image/png", PNG), ("text/plain", b"invalid\0text")],
                [("text/html", b"x" * (4 * 1024 * 1024)), ("image/png", PNG)],
            ]:
                emit(kitty_write(contents))
                assert_stays(lambda: native.clipboard_bytes("image/png") == PNG
                             and native.clipboard_bytes("text/plain") == plain, .2,
                             message="Rejected request changed the clipboard")

        with scenario(f"{connection} image and HTML offers have no invented plain text"):
            emit(kitty_write([("image/png", PNG)]))
            wait_for(lambda: native.clipboard_formats is not None
                     and "image/png" in native.clipboard_formats
                     and "text/html" not in native.clipboard_formats)
            assert not any(mime.startswith("text/plain") for mime in native.clipboard_formats)
            assert native.clipboard_bytes("image/png") == PNG
            emit(kitty_write([("text/html", b"<b>HTML only</b>")]))
            wait_for(lambda: native.clipboard_formats is not None
                     and "text/html" in native.clipboard_formats
                     and "image/png" not in native.clipboard_formats)
            assert not any(mime.startswith("text/plain") for mime in native.clipboard_formats)
            assert native.clipboard_bytes("text/html") == b"<b>HTML only</b>"

        with scenario(f"{connection} distinguishes an empty MIME value from clearing the offer"):
            emit(kitty_write([("text/plain;charset=utf-8", b"")]))
            formats(["text/plain;charset=utf-8"])
            assert native.clipboard_bytes("text/plain;charset=utf-8") == b""
            emit(kitty_write([]))
            wait_for(lambda: native.clipboard_formats == [])

        with scenario(f"{connection} explicit command-output copy still publishes styled text"):
            emit("\x1b[3J\x1b[2J\x1b[H\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07"
                 "\x1b[1;31mstyled <&>界\x1b[0m\r\n\x1b]133;D;0\x07")
            desktop.rpc("action", name="copy-output")
            wait_for(lambda: native.clipboard_text() == "styled <&>界")
            check_html(dict(text="styled <&>界", html=native.clipboard_text("text/html")))

        desktop.rpc("close-tab", connection=connection, tab=created["tab"])

print("PASS: actual local/remote GTK MIME alternatives, binary NUL bytes, atomic rejection, empty/clear and explicit styled copy")
