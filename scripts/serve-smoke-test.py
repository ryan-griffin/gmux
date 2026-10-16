#!/usr/bin/env python3
"""Check detached startup, duplicate rejection, saved settings, and startup errors."""
import json
import fcntl
from pathlib import Path
import socket
import subprocess
import time

from cargo_test_support import cargo_binary
from daemon_test_support import stop_daemon
from test_support import AppFixture, scenario

binary = cargo_binary("gmux")
artifacts = Path(__file__).resolve().parent.parent / "artifacts"
artifacts.mkdir(exist_ok=True)


def launch(fixture, *args, success=True):
    process = fixture.start(binary, "serve", "--background", *args,
                            cwd=fixture.path, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    # Waiting for EOF also verifies that the daemon did not inherit these pipes.
    stdout, stderr = process.communicate(timeout=12)
    assert (process.returncode == 0) == success, (process.returncode, stdout, stderr)
    return process.pid, stderr


def incoming(fixture, *args):
    result = subprocess.run([str(binary), "connection", "edit", "local", *map(str, args)],
                            env=fixture.env, cwd=fixture.path, capture_output=True, text=True, timeout=12)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


with AppFixture(env={"DISPLAY": "", "WAYLAND_DISPLAY": ""}) as fixture, AppFixture() as client:
    with scenario("serve accepts launch options and rejects connection configuration flags"):
        for args in (("--listen-address", "127.0.0.1:0"), ("--auth", "none"), ("--token-file", "token")):
            _, error = launch(fixture, *args, success=False)
            assert "unexpected argument" in error, error
        assert not Path(fixture.env["GMUX_SOCKET"]).exists()

    with scenario("background serve returns with a ready daemon detached from its launcher"):
        started = time.monotonic()
        launcher, _ = launch(fixture)
        startup_ms = (time.monotonic() - started) * 1000
        info = fixture.rpc("status")
        pid = info["pid"]
        assert pid != launcher
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        assert int(fields[2]) == int(fields[3]) == pid, "Daemon is not its own process group and session"
        for descriptor in (0, 1):
            assert Path(f"/proc/{pid}/fd/{descriptor}").readlink() == Path("/dev/null")
        log = fixture.config / "daemon.log"
        assert Path(f"/proc/{pid}/fd/2").readlink() == log
        assert log.stat().st_mode & 0o777 == 0o600
        before = sum(map(int, fields[11:13]))
        time.sleep(.5)
        after = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        status = Path(f"/proc/{pid}/status").read_text().splitlines()
        (artifacts / "serve-background-resources.json").write_text(json.dumps(dict(
            startup_ms=startup_ms, idle_cpu_ticks=sum(map(int, after[11:13])) - before,
            resources=[line for line in status if line.startswith(("VmRSS:", "Threads:"))]), indent=2) + "\n")
        tab = subprocess.check_output([str(binary), "new", "--name", "Background job"], env=fixture.env, text=True)
        tab = json.loads(tab)
        assert fixture.rpc("list")["tabs"][0]["id"] == tab["tab"]

    with scenario("duplicate starts fail and concurrent starts produce exactly one successful launcher"):
        _, error = launch(fixture, success=False)
        assert "already running or starting" in error and "connection edit local" in error, error
        assert fixture.rpc("status")["pid"] == pid
        assert fixture.rpc("list")["tabs"][0]["id"] == tab["tab"]
        stop_daemon(fixture.env["GMUX_SOCKET"])
        children = [fixture.start(binary, "serve", "--background", stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True) for _ in range(4)]
        successes = 0
        for child in children:
            stdout, stderr = child.communicate(timeout=12)
            if child.returncode == 0:
                successes += 1
            else:
                assert "already running or starting" in stderr, (stdout, stderr)
        assert successes == 1, successes
        restarted = fixture.rpc("status")
        assert restarted["pid"] != pid
        assert restarted["daemon"] == info["daemon"]
        assert len(fixture.rpc("list")["tabs"]) == 1

    with scenario("stop waits for shutdown so serve can immediately restart execution"):
        stopped = subprocess.run([str(binary), "stop"], env=fixture.env,
                                 capture_output=True, text=True, timeout=12, check=True)
        assert json.loads(stopped.stdout)["ok"]
        assert not Path(fixture.env["GMUX_SOCKET"]).exists()
        launch(fixture)
        assert fixture.rpc("status")["daemon"] == info["daemon"]
        assert len(fixture.rpc("list")["tabs"]) == 1

    with scenario("serve rejects a held startup lock before a control socket exists"):
        stop_daemon(fixture.env["GMUX_SOCKET"])
        with Path(fixture.env["GMUX_SOCKET"] + ".lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for args in ((), ("--background",)):
                child = fixture.start(binary, "serve", *args, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, text=True)
                stdout, stderr = child.communicate(timeout=3)
                assert child.returncode != 0 and "already running or starting" in stderr, (stdout, stderr)
            assert not Path(fixture.env["GMUX_SOCKET"]).exists()
        launch(fixture)
        assert fixture.rpc("status")["daemon"] == info["daemon"]

    with scenario("GTK's launcher waits for startup races and attaches to the same daemon"):
        stop_daemon(fixture.env["GMUX_SOCKET"])
        with Path(fixture.env["GMUX_SOCKET"] + ".lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            children = [fixture.start(binary, "rpc", '{"method":"local-daemon-start"}',
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                        for _ in range(2)]
            time.sleep(.3)
            assert all(child.poll() is None for child in children), "Attach gave up during startup"
        for child in children:
            stdout, stderr = child.communicate(timeout=12)
            assert child.returncode == 0, (stdout, stderr)
        running = fixture.rpc("status")["pid"]
        attached = subprocess.run([str(binary), "rpc", '{"method":"local-daemon-start"}'],
                                  env=fixture.env, capture_output=True, text=True, timeout=12)
        assert attached.returncode == 0, attached.stderr
        assert fixture.rpc("status")["pid"] == running

    with scenario("background serve uses saved incoming settings and token paths"):
        stop_daemon(fixture.env["GMUX_SOCKET"])
        token = fixture.path / "token"
        token.write_text("a" * 64 + "\n")
        incoming(fixture, "--remote-access", "true", "--listen-address", "127.0.0.1:0",
                 "--auth", "token", "--token-file", "token")
        launch(fixture)
        address = fixture.rpc("status")["bound_address"]
        subprocess.run([str(binary), "connection", "add", address, "--name", "server", "--auth", "token",
                        "--token-file", str(token)], env=client.env,
                       capture_output=True, text=True, timeout=12, check=True)
        result = subprocess.run([str(binary), "--connection", "server", "list"], env=client.env,
                                capture_output=True, text=True, timeout=12, check=True)
        assert json.loads(result.stdout)["daemon"] == info["daemon"]
        assert len(json.loads(result.stdout)["tabs"]) == 1
        assert not Path(client.env["GMUX_SOCKET"]).exists()

    with scenario("saved listener failures leave local execution available for live correction"):
        stop_daemon(fixture.env["GMUX_SOCKET"])
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            address = f"127.0.0.1:{occupied.getsockname()[1]}"
            incoming(fixture, "--remote-access", "true", "--listen-address", address, "--auth", "none")
            launch(fixture)
            status = subprocess.run([str(binary), "connection", "list"], env=fixture.env,
                                    capture_output=True, text=True, timeout=12, check=True)
            assert "Address already in use" in json.loads(status.stdout)["remote_access_error"], status.stdout
            assert fixture.rpc("status")["bound_address"] is None
        running = fixture.rpc("status")["pid"]
        incoming(fixture, "--listen-address", "127.0.0.1:0")
        assert fixture.rpc("status")["pid"] == running
        assert fixture.rpc("status")["bound_address"]

    with scenario("the daemon validates saved execution and reports startup failure in its log"):
        stop_daemon(fixture.env["GMUX_SOCKET"])
        saved = (fixture.config / "execution.json").read_bytes()
        (fixture.config / "execution.json").write_text("{invalid")
        _, error = launch(fixture, success=False)
        assert "Daemon exited" in error and "daemon.log" in error, error
        assert "execution.json" in (fixture.config / "daemon.log").read_text()
        assert (fixture.config / "execution.json").read_text() == "{invalid"
        assert not Path(fixture.env["GMUX_SOCKET"]).exists()
        assert not Path(fixture.env["GMUX_SOCKET"] + ".stream").exists()
        (fixture.config / "execution.json").write_bytes(saved)
        launch(fixture)

print("PASS: detached readiness, duplicate rejection, saved settings, live correction, and startup errors")
