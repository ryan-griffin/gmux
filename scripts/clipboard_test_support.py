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
