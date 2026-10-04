"""Desktop notification fixture for smoke tests on an isolated session bus."""
import threading

from gi.repository import Gio, GLib


class NotificationServer:
    def __init__(self):
        self.received = []
        self.closed = []
        self.connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        result = self.connection.call_sync(
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "RequestName", GLib.Variant("(su)", ("org.freedesktop.Notifications", 4)),
            GLib.VariantType.new("(u)"), Gio.DBusCallFlags.NONE, 3000, None,
        )
        assert result.unpack()[0] == 1, "Use dbus-run-session to isolate the notification test"
        interface = Gio.DBusNodeInfo.new_for_xml("""
        <node><interface name="org.freedesktop.Notifications">
          <method name="GetCapabilities"><arg type="as" direction="out"/></method>
          <method name="GetServerInformation">
            <arg type="s" direction="out"/><arg type="s" direction="out"/>
            <arg type="s" direction="out"/><arg type="s" direction="out"/>
          </method>
          <method name="Notify">
            <arg type="s" direction="in"/><arg type="u" direction="in"/>
            <arg type="s" direction="in"/><arg type="s" direction="in"/>
            <arg type="s" direction="in"/><arg type="as" direction="in"/>
            <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/>
            <arg type="u" direction="out"/>
          </method>
          <method name="CloseNotification"><arg type="u" direction="in"/></method>
          <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
        </interface></node>
        """).interfaces[0]
        self.registration = self.connection.register_object(
            "/org/freedesktop/Notifications", interface, self.handle, None, None,
        )
        self.loop = GLib.MainLoop()
        self.thread = threading.Thread(target=self.loop.run, daemon=True)
        self.thread.start()

    def handle(self, connection, sender, path, interface, method, parameters, invocation):
        if method == "GetCapabilities":
            invocation.return_value(GLib.Variant("(as)", (["actions", "body"],)))
        elif method == "GetServerInformation":
            invocation.return_value(GLib.Variant("(ssss)", ("Gmux test", "Gmux", "1", "1.2")))
        elif method == "Notify":
            app, replaces, icon, summary, body, actions, hints, timeout = parameters.unpack()
            identifier = len(self.received) + 1
            self.received.append(dict(id=identifier, replaces=replaces, summary=summary,
                                      body=body, actions=actions, hints=hints))
            invocation.return_value(GLib.Variant("(u)", (identifier,)))
        elif method == "CloseNotification":
            self.closed.append(parameters.unpack()[0])
            invocation.return_value(None)

    def click(self, notification):
        assert "default" in notification["actions"], notification
        self.connection.emit_signal(
            None, "/org/freedesktop/Notifications", "org.freedesktop.Notifications",
            "ActionInvoked", GLib.Variant("(us)", (notification["id"], "default")),
        )
        self.connection.flush_sync(None)

    def stop(self):
        self.loop.quit()
        self.thread.join(timeout=3)
        self.connection.unregister_object(self.registration)
        self.connection.call_sync(
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "ReleaseName", GLib.Variant("(s)", ("org.freedesktop.Notifications",)),
            GLib.VariantType.new("(u)"), Gio.DBusCallFlags.NONE, 3000, None,
        )
