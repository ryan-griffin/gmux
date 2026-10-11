"""Shared socket calls, waits, accessibility traversal, and private app fixtures."""
import json
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


def wait_accessible(predicate, timeout=12, **kwargs):
    from gi.repository import GLib
    return wait_for(predicate, timeout, context=GLib.MainContext.default(),
                    exceptions=(OSError, GLib.Error), **kwargs)


class AccessibilityFixture:
    """Activate the registry through the private accessibility bus's service file."""
    def __init__(self):
        self.connection = None
        self.process = None

    def start_service_command(self, address, error):
        from gi.repository import GLib
        # dbus-broker requires systemd even for Exec-only service files. An
        # isolated session has no user manager; use the installed service's
        # command rather than guessing a distribution's lib/libexec location.
        for directory in GLib.get_system_data_dirs():
            service = Path(directory, "dbus-1/accessibility-services/org.a11y.atspi.Registry.service")
            if not service.is_file():
                continue
            settings = GLib.KeyFile()
            settings.load_from_file(str(service), GLib.KeyFileFlags.NONE)
            assert settings.get_string("D-BUS Service", "Name") == "org.a11y.atspi.Registry", service
            _, command = GLib.shell_parse_argv(settings.get_string("D-BUS Service", "Exec"))
            print(f"AT-SPI activation unavailable ({error}); launching {command} from {service}", flush=True)
            self.process = subprocess.Popen(command, env=dict(os.environ, AT_SPI_BUS_ADDRESS=address))
            return
        raise RuntimeError("AT-SPI registry activation failed and its service file was not found in "
                           "XDG_DATA_DIRS. Install the AT-SPI registry service; see docs/testing.md.") from error

    def __enter__(self):
        from gi.repository import Gio, GLib
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        address = bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
            None, GLib.VariantType.new("(s)"), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
        self.connection = Gio.DBusConnection.new_for_address_sync(address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)

        def ready():
            if self.process is not None and self.process.poll() is not None:
                raise RuntimeError(f"AT-SPI registry exited with status {self.process.returncode}")
            return self.connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                "org.freedesktop.DBus", "NameHasOwner", GLib.Variant("(s)", ("org.a11y.atspi.Registry",)),
                GLib.VariantType.new("(b)"), Gio.DBusCallFlags.NONE, 1000, None).unpack()[0]

        try:
            if not ready():
                try:
                    self.connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                        "org.freedesktop.DBus", "StartServiceByName",
                        GLib.Variant("(su)", ("org.a11y.atspi.Registry", 0)),
                        GLib.VariantType.new("(u)"), Gio.DBusCallFlags.NONE, 5000, None)
                except GLib.Error as error:
                    if not error.matches(Gio.dbus_error_quark(), Gio.DBusError.SERVICE_UNKNOWN):
                        raise
                    if not ready():
                        self.start_service_command(address, error)
            wait_accessible(ready, message="Accessibility registry did not acquire its bus name")
        except Exception:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *_):
        stop_process(self.process)
        if self.connection is not None:
            self.connection.close_sync(None)


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
                        XDG_CONFIG_HOME=str(self.path / "config"), SHELL=str(shell),
                        HISTFILE=str(self.path / "bash_history"))
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
