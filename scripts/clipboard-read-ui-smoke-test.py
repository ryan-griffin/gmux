#!/usr/bin/env python3
"""Actual GTK consent, lazy clipboard reads and paste grants on native Wayland."""
import base64
import subprocess
from functools import partial
from pathlib import Path
import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib
from cargo_test_support import cargo_binary
from clipboard_test_support import ReadProbe, kitty_write
from test_support import AccessibilityFixture, AppFixture, descendants, scenario, wait_accessible, wait_for as shared_wait_for
from wayland_test_support import REMOTE, WaylandInput, ensure_wayland_session

ensure_wayland_session()
wait_for = partial(shared_wait_for, context=GLib.MainContext.default())
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)
with (artifacts / "clipboard-read-ui-smoke.log").open("w") as log, AccessibilityFixture(), \
        AppFixture(env=dict(GTK_A11Y="atspi")) as desktop, AppFixture() as server, WaylandInput() as native:
    result = subprocess.run([str(cargo_binary("clipboard-banner-check", example=True))],
                            env=dict(desktop.env, GTK_A11Y="test"), capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    print(result.stdout, end="", flush=True)
    server.rpc("connection-edit", connection="local", remote_access=dict(enabled=True,
                listen_address="127.0.0.1:0", authentication=dict(mode="none")))
    server.start(cargo_binary("gmux"), "serve", stdout=log, stderr=log)
    address = wait_for(lambda: server.rpc("status")["bound_address"])
    desktop.rpc("connection-add", name="server", address=address, authentication=dict(mode="none"))
    app = desktop.start_app(cargo_binary("gmux-gtk"), stdout=log, stderr=log)

    def objects():
        return [obj for obj in descendants(Atspi.get_desktop(0)) if obj.get_process_id() == app.pid
                and obj.get_state_set().contains(Atspi.StateType.SHOWING)]
    def find(name):
        return next((obj for obj in objects() if obj.get_name() == name and obj.get_role_name() == "button"), None)
    def click(name):
        button = wait_accessible(lambda: find(name))
        assert Atspi.Action.do_action(button.get_action_iface(), 0)

    for connection, owner in [("local", desktop), ("server", server)]:
        created = desktop.rpc("new", connection=connection, name="Clipboard " + connection)
        pane = created["pane"]
        probe = ReadProbe(owner, pane)
        native.set_clipboard_text("secret " + connection + " 界🙂")
        transfers = []
        subscription = native.bus.signal_subscribe(REMOTE, REMOTE + ".Session", "SelectionTransfer", native.remote, None, 0,
                                                    lambda *args: transfers.append(args[-1]))
        try:
            with scenario(f"{connection} reads no content until the user approves in its pane"):
                probe.write("\x1b]52;c;?\x07")
                wait_accessible(lambda: find("Allow Once"))
                assert not probe.bytes() and not transfers
                assert not any(find(name).get_state_set().contains(Atspi.StateType.FOCUSED)
                               for name in ("Deny", "Allow Once"))
                assert not any(obj.get_role_name() == "dialog" for obj in objects())
                if connection == "server": assert any("on server?" in obj.get_name() for obj in objects())
                click("Allow Once")
                expected = b"\x1b]52;c;" + base64.b64encode(("secret " + connection + " 界🙂").encode()) + b"\x07"
                wait_for(lambda: probe.bytes() == expected)
                assert transfers
                wait_accessible(lambda: not find("Allow Once"))

            with scenario(f"{connection} MIME reads preserve binary HTML without rendering it"):
                html = b"<b>raw</b>\0tail"
                probe.write(kitty_write([("text/html", html)]) + "\x1b]2;HTML_OFFER\x07")
                wait_for(lambda: owner.rpc("_metadata", pane=pane)["meta"]["title"] == "HTML_OFFER")
                probe.write("\x1b]5522;type=read;dGV4dC9odG1s\x1b\\")
                click("Allow Once")
                wait_for(lambda: base64.b64encode(html) in probe.bytes())

            with scenario(f"{connection} a genuine paste authorizes one follow-up without another prompt"):
                probe.write("\x1b[?5522h\x1b]2;PASTE_MODE\x07")
                wait_for(lambda: owner.rpc("_metadata", pane=pane)["meta"]["title"] == "PASTE_MODE")
                native.set_clipboard_text("PASTEGRANT")
                desktop.rpc("action", name="paste")
                event = wait_for(lambda: probe.bytes() if b":pw=" in probe.bytes() else None)
                password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                probe.write("\x1b]5522;type=read:name=UGFzdGUgZXZlbnQ=:pw=" + password + ";dGV4dC9wbGFpbg==\x1b\\")
                wait_for(lambda: base64.b64encode(b"PASTEGRANT") in probe.bytes())
                wait_accessible(lambda: not find("Allow Once"))

            with scenario(f"{connection} ordinary paste streams a full bounded text value"):
                probe.write("\x1b[?5522l\x1b]2;TEXT_MODE\x07")
                wait_for(lambda: owner.rpc("_metadata", pane=pane)["meta"]["title"] == "TEXT_MODE")
                text = "x" * (4 * 1024 * 1024)
                native.set_clipboard_text(text)
                before = len(probe.bytes())
                desktop.rpc("action", name="paste")
                wait_for(lambda: len(probe.bytes()) - before == len(text), timeout=10)
                assert probe.bytes()[before:] == text.encode()
                wait_accessible(lambda: not find("Allow Once"))

            with scenario(f"{connection} denial completes with no clipboard data"):
                # The first three read requests used the burst allowance. Let one refill.
                GLib.usleep(1_100_000)
                before = len(probe.bytes())
                probe.write("\x1b]52;c;?\x07")
                click("Deny")
                wait_for(lambda: probe.bytes()[before:] == b"\x1b]52;c;\x07")
        finally:
            native.bus.signal_unsubscribe(subscription)
        desktop.rpc("close-tab", connection=connection, tab=created["tab"])

        with scenario(f"{connection} split-pane paste supersedes unanswered consent without reading its clipboard"):
            created = desktop.rpc("new", connection=connection, name="Split paste " + connection)
            other = desktop.rpc("split", connection=connection, pane=created["pane"])["pane"]
            a, b = ReadProbe(owner, created["pane"]), ReadProbe(owner, other)
            desktop.rpc("focus", connection=connection, pane=other)
            text = "SPLIT_PASTE " + connection + " 界🙂"
            native.set_clipboard_text(text)
            transfers = []
            subscription = native.bus.signal_subscribe(REMOTE, REMOTE + ".Session", "SelectionTransfer", native.remote, None, 0,
                                                        lambda *args: transfers.append(args[-1]))
            try:
                for rich in (False, True):
                    b.write(("\x1b[?5522h" if rich else "\x1b[?5522l") + f"\x1b]2;SPLIT_MODE_{rich}\x07")
                    wait_for(lambda: owner.rpc("_metadata", pane=other)["meta"]["title"] == f"SPLIT_MODE_{rich}")
                    transfers.clear()
                    before_a, before_b = len(a.bytes()), len(b.bytes())
                    a.write("\x1b]52;c;?\x07\x1b[5n")
                    wait_accessible(lambda: find("Allow Once"))
                    assert not transfers
                    desktop.rpc("action", name="paste")
                    wait_for(lambda: a.bytes()[before_a:] == b"\x1b]52;c;\x07\x1b[0n", timeout=3)
                    if rich:
                        event = wait_for(lambda: b.bytes()[before_b:] if b"status=DONE" in b.bytes()[before_b:] else None)
                        assert not transfers
                        password = event.split(b":pw=", 1)[1].split(b"\x1b", 1)[0].decode()
                        before_b = len(b.bytes())
                        b.write("\x1b]5522;type=read:name=UGFzdGUgZXZlbnQ=:pw=" + password + ";dGV4dC9wbGFpbg==\x1b\\\x1b[5n")
                        wait_for(lambda: b.bytes()[before_b:].endswith(b"\x1b[0n"))
                        data = b"".join(base64.b64decode(packet.split(b";", 2)[2])
                                        for packet in b.bytes()[before_b:].split(b"\x1b\\") if b"status=DATA" in packet)
                        assert data == text.encode()
                    else:
                        wait_for(lambda: b.bytes()[before_b:] == text.encode(), timeout=3)
                    assert len(transfers) == 1
                    wait_accessible(lambda: not find("Allow Once"))
            finally:
                native.bus.signal_unsubscribe(subscription)
                desktop.rpc("close-tab", connection=connection, tab=created["tab"])
print("PASS: native GTK consent, no prefetch, local/remote MIME reads, genuine paste grants and denial")
