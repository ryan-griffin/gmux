"""Validate formatted clipboard content independently of the native formatter."""
from html.parser import HTMLParser


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
