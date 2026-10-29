"""Validate formatted clipboard content independently of the native formatter."""
from html.parser import HTMLParser
import base64
from pathlib import Path


PNG = (Path(__file__).resolve().parent / "fixtures/clipboard.png").read_bytes()


def kitty_write(contents, *, destination="clipboard", commit=True, aliases=()):
    """Encode test application requests; libghostty owns all protocol parsing."""
    def packet(metadata, data=None):
        return "\x1b]5522;" + metadata + (";" + data if data is not None else "") + "\x1b\\"

    def encoded(data):
        return base64.b64encode(data).decode()

    parts = [packet("type=write:loc=" + destination)]
    for mime, data in contents:
        payload = encoded(data)
        metadata = "type=wdata:mime=" + encoded(mime.encode())
        # Split the base64 stream into bounded OSC sequences, including empties.
        for start in range(0, max(1, len(payload)), 4096):
            parts.append(packet(metadata, payload[start:start + 4096]))
    for target, names in aliases:
        parts.append(packet("type=walias:mime=" + encoded(target.encode()), encoded(" ".join(names).encode())))
    if commit:
        parts.append(packet("type=wdata"))
    return "".join(parts)


class ClipboardHTML(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.parts = []
        self.stack = []
        self.styles = []
        self.active_styles = []
        self.runs = []
        self.feed(html)
        self.close()
        assert not self.stack, self.stack
        self.text = "".join(self.parts)
        assert "white-space:pre" in html and "monospace" in html, html
        assert "var(" not in html and "<style" not in html, html

    def handle_starttag(self, tag, attrs):
        assert tag in ("div", "span"), (tag, attrs)
        assert len(attrs) == 1 and attrs[0][0] == "style", attrs
        self.styles.append(attrs[0][1])
        self.stack.append(tag)
        self.active_styles.append(attrs[0][1])

    def handle_endtag(self, tag):
        assert self.stack and self.stack.pop() == tag, (tag, self.stack)
        self.active_styles.pop()

    def handle_data(self, data):
        self.parts.append(data)
        self.runs.append((data, tuple(self.active_styles)))

    def handle_comment(self, data):
        raise AssertionError(data)


def check_html(content, expected=None):
    parsed = ClipboardHTML(content["html"])
    assert parsed.text == content["text"], (parsed.text, content["text"])
    if expected is not None:
        assert parsed.text == expected, (parsed.text, expected)
    return parsed


import json
import shlex
import sys
from test_support import wait_for


class ReadProbe:
    def __init__(self, fixture, pane=None):
        self.fixture = fixture
        self.pane = pane if pane is not None else fixture.rpc("new", directory=str(fixture.path))["pane"]
        self.control = fixture.path / f"read-{self.pane}.json"
        self.received = fixture.path / f"read-{self.pane}.input"
        self.serial = 0
        self.control.write_text(json.dumps([0, ""]))
        script = fixture.path / "read-probe.py"
        script.write_text('''import json, os, select, sys, time, tty
from pathlib import Path
control, received = map(Path, sys.argv[1:])
tty.setraw(0)
os.write(1, b'\\x1b]2;READ_PROBE_READY\\x07')
last = 0
with received.open('ab', buffering=0) as output:
    while True:
        serial, data = json.loads(control.read_text())
        if serial != last:
            os.write(1, data.encode())
            last = serial
        if select.select([0], [], [], .005)[0]:
            data = os.read(0, 65536)
            if not data: break
            output.write(data)
''')
        fixture.rpc("send", pane=self.pane, text=" ".join(map(shlex.quote, [sys.executable, str(script), str(self.control), str(self.received)])), enter=True)
        wait_for(lambda: fixture.rpc("_metadata", pane=self.pane)["meta"]["title"] == "READ_PROBE_READY")

    def write(self, text):
        self.serial += 1
        temporary = self.control.with_suffix(".tmp")
        temporary.write_text(json.dumps([self.serial, text]))
        temporary.replace(self.control)

    def bytes(self):
        return self.received.read_bytes() if self.received.exists() else b""
