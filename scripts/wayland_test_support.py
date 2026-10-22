"""Native input and composited frames in an isolated Mutter Wayland session."""
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import gi
gi.require_version("Gst", "1.0")
gi.require_version("GstVideo", "1.0")
from gi.repository import Gio, GLib, Gst, GstVideo


REMOTE = "org.gnome.Mutter.RemoteDesktop"
CAST = "org.gnome.Mutter.ScreenCast"


def wait_for(predicate, timeout=12):
    deadline = time.monotonic() + timeout
    context = GLib.MainContext.default()
    while time.monotonic() < deadline:
        while context.pending():
            context.iteration(False)
        if value := predicate():
            return value
        time.sleep(0.01)
    raise AssertionError("Native Wayland test did not reach the expected state")


def session_environment(directory):
    """Private desktop state, retaining deliberate renderer and appearance options."""
    env = dict(os.environ, XDG_RUNTIME_DIR=directory, GDK_BACKEND="wayland", DISPLAY="",
               WAYLAND_DISPLAY="gmux-test", GDK_SCALE="1", GDK_DPI_SCALE="1",
               XDG_SESSION_TYPE="wayland", PIPEWIRE_RUNTIME_DIR=directory,
               PIPEWIRE_REMOTE="pipewire-0", GSETTINGS_BACKEND="memory", GIO_USE_VFS="local")
    # Keep build tool locations while isolating ~/.fonts and desktop state.
    env.setdefault("CARGO_HOME", str(Path.home() / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(Path.home() / ".rustup"))
    env["HOME"] = str(Path(directory, "home"))
    Path(env["HOME"]).mkdir()
    env.update(LANG="C.UTF-8", LC_ALL="C.UTF-8", XDG_DATA_DIRS="/usr/local/share:/usr/share",
               XDG_CONFIG_DIRS="/etc/xdg", HISTFILE=str(Path(directory, "bash_history")))
    for category in ("CONFIG", "CACHE", "STATE", "DATA"):
        env[f"XDG_{category}_HOME"] = str(Path(directory, category.lower()))
        Path(env[f"XDG_{category}_HOME"]).mkdir()
    wp_config = Path(env["XDG_CONFIG_HOME"], "wireplumber/wireplumber.conf.d")
    wp_config.mkdir(parents=True)
    (wp_config / "gmux-tests.conf").write_text(
        "wireplumber.profiles = { video-only = { hardware.video-capture = disabled } }\n")
    for name in ("GDK_DEBUG", "WAYLAND_SOCKET", "AT_SPI_BUS_ADDRESS", "LANGUAGE",
                 "FONTCONFIG_FILE", "FONTCONFIG_PATH", "GTK_THEME", "GTK_PATH", "GSETTINGS_SCHEMA_DIR",
                 "DBUS_SESSION_BUS_ADDRESS", "DBUS_SESSION_BUS_PID", "DBUS_SESSION_BUS_WINDOWID"):
        env.pop(name, None)
    # The isolated compositor owns these tests; avoid starting desktop portals
    # whose document FUSE mount can outlive the private D-Bus session.
    env["GDK_DEBUG"] = "no-portals"
    env["ADW_DISABLE_PORTAL"] = "1"
    return env


def ensure_wayland_session():
    if os.environ.get("GMUX_WAYLAND_TEST") == "1":
        assert os.environ["GDK_BACKEND"] == "wayland" and not os.environ.get("DISPLAY")
        assert Path(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"]).is_socket()
        return
    for command in ("dbus-run-session", "mutter", "pipewire", "wireplumber"):
        if not shutil.which(command):
            raise SystemExit(f"Native Wayland tests require {command}; see docs/testing.md")
    with tempfile.TemporaryDirectory(prefix="gmux-wayland-") as directory:
        env = session_environment(directory)
        command = ["dbus-run-session", "--", sys.executable, __file__, str(Path(sys.argv[0]).resolve()), *sys.argv[1:]]
        artifacts = Path(__file__).resolve().parent.parent / "artifacts"
        artifacts.mkdir(exist_ok=True)
        logfile = artifacts / f"{Path(sys.argv[0]).stem}-session.log"
        with logfile.open("w") as log:
            status = subprocess.call(command, env=env, stderr=log)
        if status:
            print(logfile.read_text()[-6000:], file=sys.stderr)
        raise SystemExit(status)


def _run(script, arguments):
    artifacts = Path(__file__).resolve().parent.parent / "artifacts"
    artifacts.mkdir(exist_ok=True)
    processes = []
    with (artifacts / f"{Path(script).stem}-wayland.log").open("w") as log:
        try:
            processes.append(subprocess.Popen(["pipewire"], stdout=log, stderr=log))
            wait_for(lambda: Path(os.environ["XDG_RUNTIME_DIR"], "pipewire-0").exists())
            processes.append(subprocess.Popen(["wireplumber", "--profile=video-only"], stdout=log, stderr=log))
            processes.append(subprocess.Popen([
                "mutter", "--headless", "--wayland", "--no-x11",
                "--virtual-monitor=1280x960@" + os.environ.get("GMUX_TEST_REFRESH_RATE", "60"),
                "--wayland-display=gmux-test",
            ], stdout=log, stderr=log))
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            def ready():
                if any(process.poll() is not None for process in processes):
                    raise RuntimeError(f"Wayland fixture exited; see {log.name}")
                return bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
                    "org.freedesktop.DBus", "NameHasOwner", GLib.Variant("(s)", (REMOTE,)),
                    GLib.VariantType.new("(b)"), Gio.DBusCallFlags.NONE, 1000, None).unpack()[0]
            wait_for(ready)
            # GTK needs a keyboard seat for native window and dialog focus.
            # Keep it available for suites that inspect focus through AT-SPI.
            seat = bus.call_sync(REMOTE, "/org/gnome/Mutter/RemoteDesktop", REMOTE,
                "CreateSession", None, GLib.VariantType.new("(o)"),
                Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
            bus.call_sync(REMOTE, seat, REMOTE + ".Session", "Start", None,
                None, Gio.DBusCallFlags.NONE, 5000, None)
            for pressed in (True, False):
                bus.call_sync(REMOTE, seat, REMOTE + ".Session", "NotifyKeyboardKeysym",
                    GLib.Variant("(ub)", (65505, pressed)), None,
                    Gio.DBusCallFlags.NONE, 5000, None)
            env = dict(os.environ, GMUX_WAYLAND_TEST="1")
            process = subprocess.Popen([sys.executable, script, *arguments], env=env)
            processes.append(process)
            return process.wait()
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


class Frame:
    def __init__(self, sample):
        buffer = sample.get_buffer()
        info = GstVideo.VideoInfo.new_from_caps(sample.get_caps())
        self.width, self.height, self.stride = info.width, info.height, info.stride[0]
        self.data = buffer.extract_dup(0, buffer.get_size())

    def pixel(self, x, y):
        assert 0 <= x < self.width and 0 <= y < self.height
        offset = y * self.stride + x * 4
        return tuple(self.data[offset:offset + 3])

    def color_bounds(self, rgb):
        needle = bytes((*rgb, 255))
        offset = self.data.find(needle)
        points = []
        while offset >= 0:
            if offset % 4 == 0:
                points.append((offset % self.stride // 4, offset // self.stride))
            offset = self.data.find(needle, offset + 4)
        if not points:
            return None
        xs, ys = zip(*points)
        return min(xs), min(ys), max(xs) + 1, max(ys) + 1


class WaylandInput:
    def __init__(self):
        assert os.environ.get("GMUX_WAYLAND_TEST") == "1", "Use the isolated Wayland fixture"
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.remote = self.call(REMOTE, "/org/gnome/Mutter/RemoteDesktop", REMOTE,
                                "CreateSession").unpack()[0]
        session_id = self.property(REMOTE, self.remote, REMOTE + ".Session", "SessionId")
        self.cast = self.call(CAST, "/org/gnome/Mutter/ScreenCast", CAST, "CreateSession",
            "(a{sv})", ({"remote-desktop-session-id": GLib.Variant("s", session_id)},)).unpack()[0]
        self.stream = self.call(CAST, self.cast, CAST + ".Session", "RecordMonitor",
            "(sa{sv})", ("", {"cursor-mode": GLib.Variant("u", 0)})).unpack()[0]
        self.parameters = self.property(CAST, self.stream, CAST + ".Stream", "Parameters")
        self.node = None
        self.subscription = self.bus.signal_subscribe(CAST, CAST + ".Stream", "PipeWireStreamAdded",
            self.stream, None, Gio.DBusSignalFlags.NONE, self._stream_added)
        self.call(REMOTE, self.remote, REMOTE + ".Session", "Start")
        wait_for(lambda: self.node)
        self.call(REMOTE, self.remote, REMOTE + ".Session", "EnableClipboard", "(a{sv})", ({},))
        self.clipboard_subscription = None
        # Headless Mutter has no physical seat devices. Advertise a keyboard
        # and pointer before GTK connects, rather than on the first test key.
        self.key(0xffe1, True)  # Shift_L
        self.key(0xffe1, False)
        self.move(0, 0)
        Gst.init(None)
        self.pipeline = Gst.parse_launch(f"pipewiresrc path={self.node} do-timestamp=true ! "
            "videoconvert ! video/x-raw,format=RGBA ! appsink name=frames max-buffers=1 drop=true sync=false")
        self.sink = self.pipeline.get_by_name("frames")
        if self.pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            message = self.pipeline.get_bus().pop_filtered(Gst.MessageType.ERROR)
            raise RuntimeError(message.parse_error() if message else "Could not start Wayland capture")
        self.latest_frame = self.frame()

    def call(self, destination, path, interface, method, signature=None, arguments=()):
        return self.bus.call_sync(destination, path, interface, method,
            GLib.Variant(signature, arguments) if signature else None, None,
            Gio.DBusCallFlags.NONE, 5000, None)

    def property(self, destination, path, interface, name):
        return self.call(destination, path, "org.freedesktop.DBus.Properties", "Get",
                         "(ss)", (interface, name)).unpack()[0]

    def _stream_added(self, connection, sender, path, interface, signal, parameters):
        self.node = parameters.unpack()[0]

    def frame(self, timeout=5, fresh=True):
        sample = self.sink.emit("try-pull-sample", int(timeout * Gst.SECOND))
        if sample is None:
            message = self.pipeline.get_bus().pop_filtered(Gst.MessageType.ERROR)
            if message:
                raise RuntimeError(message.parse_error())
            if not fresh:
                return self.latest_frame
            raise AssertionError("No frame from the Wayland compositor")
        self.latest_frame = Frame(sample)
        return self.latest_frame

    def drain_frames(self):
        while self.sink.emit("try-pull-sample", 0) is not None:
            pass

    def key(self, keysym, pressed):
        self.call(REMOTE, self.remote, REMOTE + ".Session", "NotifyKeyboardKeysym", "(ub)",
                  (keysym, pressed))

    def keycode(self, code, pressed):
        """Send a Linux evdev code, independent of the current keymap/modifiers."""
        self.call(REMOTE, self.remote, REMOTE + ".Session", "NotifyKeyboardKeycode", "(ub)",
                  (code, pressed))

    def button(self, pressed):
        self.call(REMOTE, self.remote, REMOTE + ".Session", "NotifyPointerButton", "(ib)",
                  (272, pressed))

    def move(self, x, y):
        self.call(REMOTE, self.remote, REMOTE + ".Session", "NotifyPointerMotionAbsolute",
                  "(sdd)", (self.stream, x, y))

    def click_color(self, rgb):
        bounds = wait_for(lambda: self.frame(timeout=0.1, fresh=False).color_bounds(rgb))
        left, top, right, bottom = bounds
        self.move((left + right) / 2, (top + bottom) / 2)
        time.sleep(0.05)
        self.button(True)
        self.button(False)
        return bounds

    def clipboard_text(self):
        result, descriptors = self.bus.call_with_unix_fd_list_sync(REMOTE, self.remote,
            REMOTE + ".Session", "SelectionRead", GLib.Variant("(s)", ("text/plain;charset=utf-8",)),
            GLib.VariantType.new("(h)"), Gio.DBusCallFlags.NONE, 5000, None, None)
        fd = descriptors.get(result.unpack()[0])
        try:
            deadline = time.monotonic() + 5
            chunks = []
            while True:
                remaining = max(0, deadline - time.monotonic())
                assert select.select([fd], [], [], remaining)[0], "Clipboard transfer timed out"
                chunk = os.read(fd, 65536)
                if not chunk:
                    return b"".join(chunks).decode()
                chunks.append(chunk)
        finally:
            os.close(fd)

    def set_clipboard_text(self, text):
        data = text.encode()
        if self.clipboard_subscription is not None:
            self.bus.signal_unsubscribe(self.clipboard_subscription)
        def transfer(connection, sender, path, interface, signal, parameters):
            mime, serial = parameters.unpack()
            assert mime in ("text/plain;charset=utf-8", "text/plain"), mime
            result, descriptors = self.bus.call_with_unix_fd_list_sync(REMOTE, self.remote,
                REMOTE + ".Session", "SelectionWrite", GLib.Variant("(u)", (serial,)),
                GLib.VariantType.new("(h)"), Gio.DBusCallFlags.NONE, 5000, None, None)
            fd = descriptors.get(result.unpack()[0])
            def write():
                success = False
                try:
                    os.set_blocking(fd, False)
                    remaining = memoryview(data)
                    deadline = time.monotonic() + 5
                    while remaining:
                        assert select.select([], [fd], [], max(0, deadline - time.monotonic()))[1], "Clipboard write timed out"
                        written = os.write(fd, remaining[:65536])
                        assert written > 0
                        remaining = remaining[written:]
                    success = True
                finally:
                    os.close(fd)
                    self.call(REMOTE, self.remote, REMOTE + ".Session", "SelectionWriteDone", "(ub)", (serial, success))
            threading.Thread(target=write, daemon=True).start()
        self.clipboard_subscription = self.bus.signal_subscribe(REMOTE, REMOTE + ".Session",
            "SelectionTransfer", self.remote, None, Gio.DBusSignalFlags.NONE, transfer)
        self.call(REMOTE, self.remote, REMOTE + ".Session", "SetSelection", "(a{sv})",
            ({"mime-types": GLib.Variant("as", ["text/plain;charset=utf-8", "text/plain"])},))

    def close(self):
        self.pipeline.set_state(Gst.State.NULL)
        self.bus.signal_unsubscribe(self.subscription)
        if self.clipboard_subscription is not None:
            self.bus.signal_unsubscribe(self.clipboard_subscription)
        self.call(REMOTE, self.remote, REMOTE + ".Session", "Stop")

    def __enter__(self):
        return self

    def __exit__(self, *exception):
        self.close()


if __name__ == "__main__":
    raise SystemExit(_run(sys.argv[1], sys.argv[2:]))
