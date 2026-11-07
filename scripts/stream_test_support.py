"""Stream clients for local and direct remote terminal checks."""
import json
from pathlib import Path
import re
import socket
import struct
import time

# Keep raw fixture clients aligned with the Rust protocol definition.
PROTOCOL_VERSION = int(re.search(
    r"^pub const VERSION: u32 = (\d+);$",
    (Path(__file__).resolve().parent.parent / "crates/protocol/src/transport.rs").read_text(),
    re.MULTILINE,
)[1])


def decode_cells(data):
    """Length-prefixed UTF-8 and native-width/rendition flags, never reparse VT."""
    cells = []
    data = memoryview(data)
    while data:
        assert len(data) >= 13
        length, = struct.unpack_from("<H", data)
        assert length <= 260 and len(data) >= 13 + length
        cells.append((bytes(data[13:13 + length]).decode(), bytes(data[2:5]),
                      bytes(data[5:8]), data[8], data[9], bytes(data[10:13])))
        data = data[13 + length:]
    return cells


def decode_packet(body):
    """Decode a stream body into its header and cell bytes."""
    header_size, kind, allowance, count = struct.unpack("<IIII", body[:16])
    assert kind in (1, 2, 3, 4, 5)
    assert header_size <= len(body) - 16
    header = json.loads(body[16:16 + header_size])
    header["event_type"] = {1: "pane", 2: "execution", 3: "clipboard", 4: "git", 5: "clipboard_read"}[kind]
    if kind == 3:
        assert count == 0
        write = header
        offset = 16 + header_size
        if write["content"] is not None:
            representations = {}
            for item in write["content"]:
                end = offset + item["len"]
                assert end <= len(body)
                representations[item["mime"]] = bytes(body[offset:end])
                offset = end
            # Keep existing selected-text fixture assertions readable.
            # Unrecognized formats retain their exact MIME key and bytes.
            write["content"] = {}
            for mime, data in representations.items():
                if mime == "text/plain":
                    write["content"]["text"] = data.decode()
                elif mime == "text/html":
                    write["content"]["html"] = data.decode()
                else:
                    write["content"][mime] = data
        assert offset == len(body), (offset, len(body))
        return header, b""
    if kind != 1:
        assert count == 0 and len(body) == 16 + header_size
    return header, body[16 + header_size:]


def clipboard_value(event):
    """Return write fields for clipboard assertions, or None for another kind."""
    if event.get("event_type") == "clipboard":
        return {key: event[key] for key in ("destination", "content")}
    return None


class Client:
    def __init__(self, fixture, pane, *, connection=None, address=None, token=None):
        if connection:
            catalog = json.loads((fixture.config / "connections.json").read_text())
            remote = next(remote for remote in catalog["remotes"] if remote["name"] == connection)
            address = remote["address"]
            if remote["authentication"]["mode"] == "token":
                token = Path(remote["authentication"]["file"]).read_text().rstrip("\r\n")
        self.pane = pane
        if address:
            if address.startswith("["):
                host, _, suffix = address[1:].partition("]")
                port = int(suffix[1:]) if suffix else 7331
            elif address.count(":") == 1:
                host, port = address.rsplit(":", 1)
                port = int(port)
            else:
                host, port = address, 7331
            self.socket = socket.create_connection((host, port), 6)
        else:
            self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(6)
        if address:
            self.socket.sendall((json.dumps(dict(version=PROTOCOL_VERSION, channel="stream", token=token)) + "\n").encode())
            line = bytearray()
            while not line.endswith(b"\n"):
                line.extend(self.exact(1))
            hello = json.loads(line)
            assert "error" not in hello, hello
            assert hello["version"] == PROTOCOL_VERSION, hello
        else:
            self.socket.connect(fixture.env["GMUX_SOCKET"] + ".stream")
            hello = fixture.rpc("_hello")
        self.cells = {}
        self.cell_values = {}
        self.cached_cells = []
        self.git = {}
        self.frame = None
        self.links = dict(targets=[], spans=[])
        self.heartbeats = 0
        self.send("attach", version=PROTOCOL_VERSION, daemon=hello["daemon"])
        self.send("subscribe", panes=[pane])

    def send(self, method, **params):
        self.socket.sendall((json.dumps(dict(method=method, pane=self.pane, **params)) + "\n").encode())

    def exact(self, size):
        data = bytearray()
        while len(data) < size:
            part = self.socket.recv(size - len(data))
            assert part, "Client disconnected"
            data.extend(part)
        return data

    def until(self, predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            size, = struct.unpack("<I", self.exact(4))
            if size == 0:
                self.heartbeats += 1
                continue
            assert 4 <= size <= 64 * 1024 * 1024
            body = self.exact(size)
            header, cells = decode_packet(body)
            if header["event_type"] == "git":
                self.git[header["id"]] = {k: v for k, v in header.items() if k != "event_type"}
            if header.get("meta", {}).get("id") == self.pane and header.get("frame"):
                frame = header.get("frame")
                if self.frame is None or (frame["cols"], frame["rows"]) != (self.frame["cols"], self.frame["rows"]):
                    self.cells.clear()
                    self.cell_values.clear()
                    self.links = dict(targets=[], spans=[])
                shift = header.get("row_shift", 0)
                if shift:
                    self.cells = {(row, col): self.cells.get((row + shift, col), " ")
                                  for row in range(frame["rows"]) for col in range(frame["cols"])}
                    self.cell_values = {(row, col): self.cell_values.get((row + shift, col), ("", bytes(3), bytes(3), 0, 0, bytes(3)))
                                       for row in range(frame["rows"]) for col in range(frame["cols"])}
                self.frame = frame
                if "hyperlinks" in header:
                    self.links = header["hyperlinks"]
                cells = decode_cells(cells)
                for index, row in enumerate(header["rows"]):
                    for col in range(frame["cols"]):
                        start = index * frame["cols"] + col
                        self.cells[row, col] = cells[start][0]
                        self.cell_values[row, col] = cells[start]
                self.cached_cells = cells[len(header["rows"]) * frame["cols"]:]
            if predicate(header):
                return header
        raise AssertionError("Client did not receive expected state")

    def text(self):
        return "\n".join("".join(self.cells.get((row, col), " ") for col in range(self.frame["cols"]))
                         for row in range(self.frame["rows"]))

    def output(self, marker):
        return self.until(lambda h: h.get("meta", {}).get("id") == self.pane and h.get("frame") and marker in self.text())

    def state(self, revision):
        return self.until(lambda h: h["event_type"] == "execution" and h["revision"] >= revision)

    def barrier(self, serial):
        # A rejected request acknowledges prior commands even for hidden panes.
        method = f"test-barrier-{serial}"
        self.send(method)
        self.until(lambda h: h.get("meta", {}).get("id") == self.pane and h.get("error") and method in h.get("error"))

    def close(self):
        self.socket.close()

