"""Shared socket calls, waits, accessibility traversal, and private app fixtures."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from daemon_test_support import stop_daemon


def rpc(socket_path, method, /, *, timeout=6, check=True, **params):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(timeout)
        connection.connect(str(socket_path))
        connection.sendall((json.dumps(dict(method=method, **params)) + "\n").encode())
        result = json.loads(connection.makefile("rb").readline())
    if check:
        assert "error" not in result, result
    return result


def wait_for(predicate, timeout=12, *, context=None, exceptions=(OSError,), message="Test did not reach the expected state"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if context is not None:
            while context.pending():
                context.iteration(False)
        try:
            if result := predicate():
                return result
        except exceptions:
            pass
        time.sleep(0.025)
    raise AssertionError(message)


def descendants(element):
    element.clear_cache()
    yield element
    for index in range(element.get_child_count()):
        child = element.get_child_at_index(index)
        if child:
            yield from descendants(child)


class DelayedViewport:
    """Relay a private daemon stream, with controllable reply latency."""
    def __init__(self, socket_path, proxy_path):
        self.socket_path = str(socket_path)
        self.proxy_path = str(proxy_path)
        self.delay = 0
        self.closed = threading.Event()
        self.sockets = []
        self.threads = []

    def __enter__(self):
        import socket
        Path(self.proxy_path).symlink_to(self.socket_path)
        Path(self.socket_path + ".ui").symlink_to(self.proxy_path + ".ui")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(self.proxy_path + ".stream")
        listener.listen(1)
        self.sockets.append(listener)

        def relay(source, destination, delayed=False):
            try:
                while data := source.recv(65536):
                    if self.closed.wait(self.delay if delayed else 0):
                        return
                    destination.sendall(data)
            except OSError:
                pass  # Closing the fixture interrupts blocked stream operations.

        def accept():
            try:
                client, _ = listener.accept()
                backend = socket.socket(socket.AF_UNIX)
                self.sockets.extend((client, backend))
                backend.connect(self.socket_path + ".stream")
                writer = threading.Thread(target=relay, args=(client, backend), daemon=True)
                self.threads.append(writer)
                writer.start()
                relay(backend, client, delayed=True)
            except OSError:
                pass

        worker = threading.Thread(target=accept, daemon=True)
        self.threads.append(worker)
        worker.start()
        return self

    def __exit__(self, *_):
        import socket
        self.closed.set()
        for connection in self.sockets:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()
        for worker in self.threads:
            worker.join(timeout=1)


class AppFixture:
    def __init__(self, settings=None):
        self.directory = tempfile.TemporaryDirectory(prefix="gmux-test-")
        self.path = Path(self.directory.name)
        config = self.path / "config/gmux"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps(settings or {}))
        shell = self.path / "shell"
        shell.write_text("#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
        shell.chmod(0o700)
        self.env = dict(os.environ, GMUX_SOCKET=str(self.path / "socket"),
                        XDG_CONFIG_HOME=str(self.path / "config"), SHELL=str(shell))
        self.processes = []

    def start(self, binary, **kwargs):
        process = subprocess.Popen([str(binary)], env=self.env, **kwargs)
        self.processes.append(process)
        return process

    def rpc(self, method, **params):
        return rpc(self.env["GMUX_SOCKET"], method, **params)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        try:
            stop_daemon(self.env["GMUX_SOCKET"])
        finally:
            for process in reversed(self.processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=8)
            self.directory.cleanup()
