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
    (Path(__file__).resolve().parent.parent / "crates/client/src/transport.rs").read_text(),
    re.MULTILINE,
)[1])


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
        self.cell_bytes = {}
        self.cached_cell_bytes = b""
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
            assert 4 <= size <= 16 * 1024 * 1024
            body = self.exact(size)
            header_size, = struct.unpack("<I", body[:4])
            header = json.loads(body[4:4 + header_size])
            if (write := header.get("copy")) is not None:
                offset = 4 + header_size
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
            if value := header.get("git"):
                self.git[value["id"]] = value
            if header["meta"]["id"] == self.pane and header["frame"]:
                frame = header["frame"]
                if self.frame is None or (frame["cols"], frame["rows"]) != (self.frame["cols"], self.frame["rows"]):
                    self.cells.clear()
                    self.cell_bytes.clear()
                    self.links = dict(targets=[], spans=[])
                shift = header.get("row_shift", 0)
                if shift:
                    self.cells = {(row, col): self.cells.get((row + shift, col), " ")
                                  for row in range(frame["rows"]) for col in range(frame["cols"])}
                    self.cell_bytes = {(row, col): self.cell_bytes.get((row + shift, col), bytes(75))
                                       for row in range(frame["rows"]) for col in range(frame["cols"])}
                self.frame = frame
                if "hyperlinks" in header:
                    self.links = header["hyperlinks"]
                cells = body[4 + header_size:]
                for index, row in enumerate(header["rows"]):
                    for col in range(frame["cols"]):
                        start = (index * frame["cols"] + col) * 75
                        self.cells[row, col] = cells[start:start + 64].split(b"\0", 1)[0].decode()
                        self.cell_bytes[row, col] = cells[start:start + 75]
                self.cached_cell_bytes = cells[len(header["rows"]) * frame["cols"] * 75:]
            if predicate(header):
                return header
        raise AssertionError("Client did not receive expected state")

    def text(self):
        return "\n".join("".join(self.cells.get((row, col), " ") for col in range(self.frame["cols"]))
                         for row in range(self.frame["rows"]))

    def output(self, marker):
        return self.until(lambda h: h["meta"]["id"] == self.pane and h["frame"] and marker in self.text())

    def state(self, revision):
        return self.until(lambda h: h.get("state", {}).get("revision", -1) >= revision)["state"]

    def barrier(self, serial):
        # A rejected request acknowledges prior commands even for hidden panes.
        method = f"test-barrier-{serial}"
        self.send(method)
        self.until(lambda h: h["meta"]["id"] == self.pane and h["error"] and method in h["error"])

    def close(self):
        self.socket.close()

