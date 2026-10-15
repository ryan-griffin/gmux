#!/usr/bin/env python3
"""Check direct remote clients, authentication, saved destinations, and daemon isolation."""
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import secrets
import socket
import subprocess
import time

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for, rpc

binary = cargo_binary("gmux")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


def command(fixture, *args, check=True):
    result = subprocess.run([str(binary), *map(str, args)], env=fixture.env,
                            text=True, capture_output=True, timeout=15)
    assert (result.returncode == 0) == check, result.stderr or result.stdout
    if not check:
        return result.stderr
    if "read" in args:
        return {"text": result.stdout}
    return json.loads(result.stdout)


def handshake(address, **params):
    connection = socket.create_connection(("127.0.0.1", int(address.rsplit(":", 1)[1])), 6)
    connection.settimeout(6)
    reader = connection.makefile("rb")
    connection.sendall((json.dumps(dict(version=18, channel="control", **params)) + "\n").encode())
    return connection, reader, json.loads(reader.readline())


def no_local_execution(fixture):
    assert not Path(fixture.env["GMUX_SOCKET"]).exists()
    assert not Path(fixture.env["GMUX_SOCKET"] + ".remote-stream").exists()
    for name in ("daemon-id.json", "execution.json", "hosting.json", "daemon.log"):
        assert not (fixture.config / name).exists(), name


with (artifacts / "remote-smoke.log").open("w") as log, \
        AppFixture(prefix="gmux-server-") as server, \
        AppFixture(prefix="gmux-client-") as client, \
        AppFixture(prefix="gmux-other-") as other, \
        AppFixture(prefix="gmux-settings-") as settings:
    token = secrets.token_hex(32)
    token_file = server.path / "token"
    token_file.write_text(token + "\n")
    token_file.chmod(0o600)
    with scenario("connection and hosting settings work without local execution"):
        listing = command(client, "connection", "list")
        assert listing["local"]["daemon"] is None and listing["running"] is False
        assert listing["remotes"] == []
        hostname = listing["hostname"]
        command(client, "connection", "edit", "local", "--name", "desktop")
        assert command(client, "connection", "list")["local"]["name"] == "desktop"
        assert "cannot be removed" in command(client, "connection", "remove", "desktop", check=False)
        assert "cannot be removed" in command(client, "connection", "remove", "local", check=False)
        assert "--listen" in command(client, "connection", "edit", "local", "--address", "127.0.0.1:1", check=False)
        assert "required" in command(client, "connection", "edit", "local", check=False)
        command(client, "connection", "edit", "desktop", "--use-hostname")
        assert command(client, "connection", "list")["local"]["name"] == hostname
        no_local_execution(client)

        assert command(settings, "connection", "list")["running"] is False
        assert "required" in command(settings, "connection", "edit", "local", "--allow-remote", "true", check=False)
        command(settings, "connection", "edit", "local", "--listen", "127.0.0.1:0")
        assert command(settings, "connection", "list")["hosting"]["enabled"] is False
        # Incoming settings do not depend on the outgoing connection catalog.
        (settings.config / "connections.json").write_text("broken client settings")
        command(settings, "connection", "edit", "local", "--allow-remote", "true", "--listen", "127.0.0.1:0", "--auth", "token", "--token-file", token_file)
        status = command(settings, "rpc", json.dumps({"method":"hosting-get"}))
        assert status["hosting"]["enabled"] and status["running"] is False
        assert status["listen"] is None
        command(settings, "connection", "edit", "local", "--allow-remote", "false")
        status = command(settings, "rpc", json.dumps({"method":"hosting-get"}))
        assert status["hosting"]["enabled"] is False
        assert status["hosting"]["address"] == "127.0.0.1:0"
        assert status["hosting"]["authentication"]["mode"] == "token"
        assert (settings.config / "connections.json").read_text() == "broken client settings"
        assert not Path(settings.env["GMUX_SOCKET"]).exists()
        assert not (settings.config / "daemon-id.json").exists()
        assert not (settings.config / "execution.json").exists()

    with scenario("serving uses saved incoming settings with explicit authentication"):
        command(server, "connection", "edit", "local", "--allow-remote", "true", "--listen", "127.0.0.1:0",
                "--auth", "token", "--token-file", token_file)
        process = server.start(binary, "serve", stdout=log, stderr=log)
        address = wait_for(lambda: server.rpc("daemon-info")["listen"])
        identity = server.rpc("_hello")["daemon"]
        command(other, "connection", "edit", "local", "--allow-remote", "true", "--listen", "127.0.0.1:0", "--auth", "none")
        other_process = other.start(binary, "serve", stdout=log, stderr=log)
        other_address = wait_for(lambda: other.rpc("daemon-info")["listen"])
        other_identity = other.rpc("_hello")["daemon"]

    with scenario("failed local edits preserve both connection names and incoming settings"):
        before = command(server, "connection", "list")
        catalog_path = server.config / "connections.json"
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            error = command(server, "connection", "edit", "local", "--name", "must-not-save",
                            "--listen", f"127.0.0.1:{occupied.getsockname()[1]}", check=False)
            assert "Address already in use" in error
        after = command(server, "connection", "list")
        assert after["local"]["name"] == before["local"]["name"]
        assert after["hosting"] == before["hosting"]
        assert not catalog_path.exists()
        assert "Connection name" in command(server, "connection", "edit", "local", "--name", "bad name",
                                             "--allow-remote", "false", check=False)
        assert command(server, "connection", "list")["hosting"] == before["hosting"]

    with scenario("remote daemons expose only authenticated execution"):
        for supplied in (None, secrets.token_hex(32)):
            connection, reader, reply = handshake(address, token=supplied)
            assert reply == {"error": "Authentication failed"}, reply
            assert reader.read(1) == b""
            reader.close()
            connection.close()
        connection, reader, reply = handshake(address, token=token, daemon=other_identity)
        assert reply == {"error": "Remote connection identity changed"}, reply
        reader.close()
        connection.close()
        connection, reader, reply = handshake(address, token=token)
        assert reply["daemon"] == identity
        for request in (dict(method="workspace-add", name="No"), dict(method="connection-list"),
                        dict(method="hosting-get"), dict(method="_connection-call", connection="No", request=dict(method="state")),
                        dict(method="send", text="No")):
            connection.sendall((json.dumps(request) + "\n").encode())
            assert "error" in json.loads(reader.readline()), request
        connection.sendall(b'{"method":"state"}\n')
        state = json.loads(reader.readline())
        assert state["daemon"] == identity
        assert "remotes" not in state and "catalog" not in state
        reader.close()
        connection.close()
        for method in ("connection-list", "connection-add", "_connection-call", "workspace-add"):
            assert "error" in rpc(server.env["GMUX_SOCKET"], method, client=False, check=False)
        assert not Path(server.env["GMUX_SOCKET"] + ".remote-stream").exists()

    with scenario("saved connections run CLI commands without a local daemon"):
        command(client, "connection", "add", address, "--name", "server", "--auth", "token", "--token-file", token_file)
        own = command(client, "--connection", "server", "new", "--name", "Remote job", "--directory", "/tmp")
        pane = own["pane"]
        assert own["daemon"] == identity
        assert command(client, "--connection", "server", "list")["tabs"][0]["id"] == own["tab"]
        assert command(client, "--connection", "server", "status")["daemon"] == identity
        assert command(client, "status", "--connection", "server")["daemon"] == identity
        assert "Only the local" in command(client, "connection", "edit", "server", "--use-hostname", check=False)
        for flags in (("--allow-remote", "false"), ("--listen", "127.0.0.1:1")):
            assert "only to the local" in command(client, "connection", "edit", "server", *flags, check=False)
        command(client, "--connection", "server", "send", "printf 'DIRECT_CLIENT_OK\\n'", "--pane", pane, "--enter")
        wait_for(lambda: "DIRECT_CLIENT_OK" in command(client, "--connection", "server", "read", "--pane", pane)["text"])
        assert "explicit --pane" in command(client, "--connection", "server", "read", check=False)
        assert "absolute" in command(client, "--connection", "server", "new", "--directory", "relative", check=False)
        no_local_execution(client)
        catalog_path = client.config / "connections.json"
        catalog = json.loads(catalog_path.read_text())
        assert catalog["remotes"][0]["daemon"] == identity
        assert token not in catalog_path.read_text()
        assert catalog_path.stat().st_mode & 0o777 == 0o600
        assert not (server.config / "connections.json").exists()

    with scenario("any client can consume viewport packets and send input over direct TCP"):
        stream = Client(client, pane, connection="server")
        stream.until(lambda header: header["frame"] is not None)
        assert server.rpc("daemon-info")["remote_frontends"] == 1
        assert not server.rpc("daemon-info")["attached"]
        stream.send("paste", text="printf 'VIEWPORT_CLIENT_OK\\n'", clipboard=False, enter=True)
        # The independent protocol client uses the same daemon execution as the CLI.
        wait_for(lambda: "VIEWPORT_CLIENT_OK" in command(client, "--connection", "server", "read", "--pane", pane)["text"])
        stream.close()
        wait_for(lambda: server.rpc("daemon-info")["remote_frontends"] == 0)
        no_local_execution(client)

    with scenario("catalog edits are atomic and cannot silently replace a pinned daemon"):
        command(client, "connection", "edit", "local", "--name", "desktop")
        command(client, "connection", "edit", "server", "--name", hostname)
        assert "already in use" in command(client, "connection", "edit", "local", "--use-hostname", check=False)
        command(client, "connection", "edit", hostname, "--name", "server")
        command(client, "connection", "edit", "local", "--use-hostname")
        saved = catalog_path.read_bytes()
        assert "identity changed" in command(client, "connection", "edit", "server", "--address", other_address, "--auth", "none", check=False)
        assert catalog_path.read_bytes() == saved
        command(client, "connection", "edit", "server", "--name", "renamed")
        assert command(client, "--connection", "renamed", "list")["tabs"][0]["id"] == own["tab"]
        assert "does not exist" in command(client, "--connection", "server", "list", check=False)
        command(client, "connection", "remove", "renamed")
        command(client, "connection", "add", other_address, "--name", "renamed", "--auth", "none")
        stale = dict(method="_connection-call", connection="renamed", daemon=identity, request=dict(method="close-tab", tab=own["tab"]))
        assert "identity changed" in command(client, "rpc", json.dumps(stale), check=False)
        saved = catalog_path.read_bytes()
        for method in ("connection-edit", "connection-remove"):
            stale = dict(method=method, connection="renamed", daemon=identity)
            if method == "connection-edit":
                stale["name"] = "stale-editor"
            assert "identity changed" in command(client, "rpc", json.dumps(stale), check=False)
            assert catalog_path.read_bytes() == saved
        assert server.rpc("list")["tabs"][0]["id"] == own["tab"]
        assert other.rpc("list")["tabs"] == []
        no_local_execution(client)
        # Renaming and removing are local edits and work while the remote is offline.
        other.rpc("quit")
        assert other_process.wait(timeout=10) == 0
        command(client, "connection", "edit", "renamed", "--name", "offline")
        command(client, "connection", "remove", "offline")
        # A stale remote editor cannot change a local alias that reuses its name.
        command(client, "connection", "edit", "local", "--name", "offline")
        stale = dict(method="connection-edit", connection="offline", daemon=other_identity, name="stale-editor")
        assert "identity changed" in command(client, "rpc", json.dumps(stale), check=False)
        assert command(client, "connection", "list")["local"]["name"] == "offline"
        command(client, "connection", "edit", "local", "--use-hostname")
        assert command(client, "connection", "list")["remotes"] == []
        no_local_execution(client)

    with scenario("a daemon starts independently of corrupt outgoing client settings"):
        (other.config / "connections.json").write_text("broken client settings")
        other_process = other.start(binary, "serve", stdout=log, stderr=log)
        wait_for(lambda: other.rpc("daemon-info")["pid"] == other_process.pid)
        assert command(other, "status")["pid"] == other_process.pid
        assert command(other, "status", "--connection", "local")["pid"] == other_process.pid
        assert command(other, "rpc", json.dumps({"method":"hosting-get"}))["running"] is True
        assert (other.config / "connections.json").read_text() == "broken client settings"
        assert not Path(other.env["GMUX_SOCKET"] + ".remote-stream").exists()

    with scenario("local hosting persists separately and shuts down existing remote streams"):
        command(client, "connection", "add", address, "--name", "server", "--auth", "token", "--token-file", token_file)
        stream = Client(client, pane, connection="server")
        stream.until(lambda header: header["frame"] is not None)
        command(server, "connection", "edit", "local", "--allow-remote", "false")
        wait_for(lambda: server.rpc("daemon-info")["remote_frontends"] == 0)
        assert server.rpc("daemon-info")["listen"] is None
        assert json.loads((server.config / "hosting.json").read_text())["enabled"] is False
        assert not (server.config / "connections.json").exists()
        stream.close()
        command(server, "connection", "edit", "local", "--allow-remote", "true", "--listen", "127.0.0.1:0", "--auth", "token", "--token-file", token_file)
        next_address = server.rpc("daemon-info")["listen"]
        command(client, "connection", "edit", "server", "--address", next_address)
        assert command(client, "--connection", "server", "list")["tabs"][0]["id"] == own["tab"]
        server.rpc("quit")
        assert process.wait(timeout=10) == 0
        process = server.start(binary, "serve", stdout=log, stderr=log)
        next_address = wait_for(lambda: server.rpc("daemon-info")["listen"])
        assert server.rpc("_hello")["daemon"] == identity
        command(client, "connection", "edit", "server", "--address", next_address)
        assert command(client, "--connection", "server", "list")["tabs"][0]["id"] == own["tab"]
        no_local_execution(client)

print("Remote smoke test passed")
