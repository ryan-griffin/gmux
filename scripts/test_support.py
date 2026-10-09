"""Shared socket calls, waits, accessibility traversal, and private app fixtures."""
import json
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
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


def assert_stays(predicate, duration, *, message):
    """Observe a condition for a bounded interval, including delayed updates."""
    deadline = time.monotonic() + duration
    while True:
        assert predicate(), message
        if time.monotonic() >= deadline:
            return
        time.sleep(0.025)


def descendants(element):
    element.clear_cache()
    yield element
    for index in range(element.get_child_count()):
        child = element.get_child_at_index(index)
        if child:
            yield from descendants(child)


def stop_process(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=8)


@contextmanager
def scenario(name):
    print(f"Running: {name}", flush=True)
    try:
        yield
    except Exception:
        print(f"FAIL: {name}", flush=True)
        raise
    print(f"PASS: {name}", flush=True)


@contextmanager
def delayed_command_reply(fixture, method, *, match=None):
    """Hold a prepared command reply while frontend navigation stays live."""
    original = fixture.env["GMUX_SOCKET"]
    proxy = str(fixture.path / "command-proxy")
    Path(proxy + ".stream").symlink_to(original + ".stream")
    Path(original + ".ui").symlink_to(proxy + ".ui")
    arrived, release = threading.Event(), threading.Event()
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(proxy)
    listener.listen()
    closed = threading.Event()

    def relay(connection):
        with connection, socket.socket(socket.AF_UNIX) as backend:
            request = connection.makefile("rb").readline()
            body = json.loads(request)
            backend.connect(original)
            backend.sendall(request)
            response = backend.makefile("rb").readline()
            command = body.get("command", {})
            if (body.get("method") == "_command" and command.get("method") == method
                    and (match is None or match(command))):
                arrived.set()
                if not release.wait(5):
                    return
            try:
                connection.sendall(response)
            except OSError:
                pass  # Fixture cleanup can close a request that failed its assertion.

    def serve():
        while not closed.is_set():
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            threading.Thread(target=relay, args=(connection,), daemon=True).start()

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    fixture.env["GMUX_SOCKET"] = proxy
    try:
        yield arrived, release
    finally:
        release.set()
        fixture.env["GMUX_SOCKET"] = original
        closed.set()
        listener.shutdown(socket.SHUT_RDWR)
        listener.close()
        server.join(timeout=1)
        Path(proxy).unlink()
        Path(proxy + ".stream").unlink()
        Path(original + ".ui").unlink()


class DelayedCommand:
    def __init__(self, fixture, method, arrived, release):
        self.fixture = fixture
        self.method = method
        self.arrived = arrived
        self.release = release

    @contextmanager
    def command(self, **params):
        """Run navigation assertions while a prepared command reply is held."""
        self.arrived.clear()
        self.release.clear()
        with ThreadPoolExecutor(max_workers=1) as pool:
            command = pool.submit(self.fixture.rpc, self.method, **params)
            try:
                assert self.arrived.wait(3), f"The {self.method} reply did not reach the delay gate"
                yield
            finally:
                self.release.set()
            command.result(timeout=3)


def wait_accessible(predicate, timeout=12, **kwargs):
    from gi.repository import GLib
    return wait_for(predicate, timeout, context=GLib.MainContext.default(),
                    exceptions=(OSError, GLib.Error), **kwargs)


class AccessibilityFixture:
    """Wait for the private accessibility registry and stop it if we started it."""
    def __init__(self, **process_options):
        self.process_options = process_options
        self.process = None
        self.connection = None

    def __enter__(self):
        from gi.repository import Gio, GLib
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        address = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
            None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
        self.connection = Gio.DBusConnection.new_for_address_sync(address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)

        def ready():
            return self.connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                "org.freedesktop.DBus", "NameHasOwner", GLib.Variant("(s)", ("org.a11y.atspi.Registry",)),
                GLib.VariantType.new("(b)"), Gio.DBusCallFlags.NONE, 1000, None).unpack()[0]

        try:
            if not ready():
                self.process = subprocess.Popen(["/usr/lib/at-spi2-registryd"],
                    env=dict(os.environ, AT_SPI_BUS_ADDRESS=address, DBUS_SESSION_BUS_ADDRESS=address),
                    **self.process_options)
            wait_accessible(ready, message="Accessibility registry did not acquire its bus name")
        except Exception:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *_):
        try:
            stop_process(self.process)
        finally:
            if self.connection is not None:
                self.connection.close_sync(None)


class DelayedViewport:
    """Relay a private daemon stream, with controllable reply latency."""
    def __init__(self, socket_path, proxy_path, *, observe_header=None):
        self.socket_path = str(socket_path)
        self.proxy_path = str(proxy_path)
        self.delay = 0
        self.closed = threading.Event()
        self.reply_hold = threading.Event()
        self.reply_held = threading.Event()
        self.reply_release = threading.Event()
        self.reply_release.set()
        self.sockets = []
        self.threads = []
        self.observe_header = observe_header

    def __enter__(self):
        import socket
        Path(self.proxy_path).symlink_to(self.socket_path)
        Path(self.socket_path + ".ui").symlink_to(self.proxy_path + ".ui")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(self.proxy_path + ".stream")
        listener.listen(1)
        self.sockets.append(listener)

        def relay(source, destination, delayed=False):
            pending = b""
            try:
                while data := source.recv(65536):
                    if delayed and self.reply_hold.is_set():
                        self.reply_held.set()
                        assert self.reply_release.wait(12), "Viewport reply barrier was not released"
                    if self.closed.wait(self.delay if delayed else 0):
                        return
                    destination.sendall(data)
                    if delayed and self.observe_header is not None:
                        pending += data
                        while len(pending) >= 4:
                            size = int.from_bytes(pending[:4], "little")
                            assert 4 <= size <= 16 * 1024 * 1024, "Invalid viewport packet size"
                            if len(pending) < 4 + size:
                                break
                            header_size = int.from_bytes(pending[4:8], "little")
                            assert header_size <= size - 4, "Invalid viewport header size"
                            self.observe_header(json.loads(pending[8:8 + header_size]))
                            pending = pending[4 + size:]
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

    @contextmanager
    def hold_replies(self):
        """Keep frontend commands live while daemon updates wait at a barrier."""
        self.reply_held.clear()
        self.reply_release.clear()
        self.reply_hold.set()
        try:
            yield self.reply_held
        finally:
            self.reply_hold.clear()
            self.reply_release.set()

    def __exit__(self, *_):
        import socket
        self.closed.set()
        self.reply_release.set()
        for connection in self.sockets:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()
        for worker in self.threads:
            worker.join(timeout=1)


class AppFixture:
    def __init__(self, settings=None, *, prefix="gmux-test-", shell_script=None, env=None):
        self.directory = tempfile.TemporaryDirectory(prefix=prefix)
        self.path = Path(self.directory.name)
        self.config = self.path / "config/gmux"
        self.config.mkdir(parents=True)
        self.settings_path = self.config / "settings.json"
        self.set_settings(settings or {})
        shell = self.path / "shell"
        shell.write_text(shell_script or "#!/bin/sh\nexport PS1=''\nstty -echo\nexec /bin/bash --noprofile --norc -i\n")
        shell.chmod(0o700)
        self.env = dict(os.environ, GMUX_SOCKET=str(self.path / "socket"),
                        XDG_CONFIG_HOME=str(self.path / "config"), SHELL=str(shell))
        self.env.update(env or {})
        self.processes = []

    def set_settings(self, settings):
        self.settings_path.write_text(json.dumps(settings))

    def start(self, binary, *args, **kwargs):
        process = subprocess.Popen([str(binary), *map(str, args)], env=self.env, **kwargs)
        self.processes.append(process)
        return process

    def start_app(self, binary, *args, **kwargs):
        process = self.start(binary, *args, **kwargs)
        wait_for(lambda: process.poll() is not None or self.rpc("daemon-info")["attached"],
                 message="Application did not attach to its daemon")
        assert process.poll() is None, "Application exited before attaching to its daemon"
        wait_for(lambda: self.rpc("renderer-info")["gtk"], message="Application did not construct its window")
        return process

    @contextmanager
    def delayed_app(self, binary, method, *, match=None, **kwargs):
        """Restart with a command proxy before the app connects to its daemon."""
        self.stop()
        self.start(Path(binary).with_name("gmux-daemon"), **kwargs)
        wait_for(lambda: self.rpc("daemon-info"))
        with delayed_command_reply(self, method, match=match) as (arrived, release):
            self.start_app(binary, **kwargs)
            yield DelayedCommand(self, method, arrived, release)

    def stop(self):
        try:
            stop_daemon(self.env["GMUX_SOCKET"])
        finally:
            for process in reversed(self.processes):
                stop_process(process)

    def rpc(self, method, **params):
        return rpc(self.env["GMUX_SOCKET"], method, **params)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        try:
            self.stop()
        finally:
            self.directory.cleanup()
