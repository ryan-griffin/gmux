# Clipboard

Copy and Copy Command Output in GTK offer plain UTF-8 text and HTML together.
Destinations that accept HTML can preserve terminal colors and formatting.
Text editors and terminal paste receive plain text. The same behavior applies
to local and remote panes, keyboard shortcuts, and context menus.

Both representations use the requesting window's tracked terminal selection.
Linear selections join soft wraps. Rectangles preserve physical rows, selected
blank cells, trailing padding, and complete wide glyphs at their edges. Copy
uses retained terminal graphemes rather than the renderer's abbreviated cell
text. Selection highlight colors never enter the clipboard.

Plain text is limited to 4 MiB. HTML generation and its final representation
are each limited to 8 MiB. If formatting fails, text validation fails, or HTML
would exceed its generation or transport budget, Gmux offers plain text alone.
The plain-text limit still applies. Clipboard offers are transient and are not
saved or replayed when a window reconnects.

The pinned libghostty formatter supplies escaping and terminal style markup.
Gmux preserves whitespace, replaces its page blocks with inline wrappers,
resolves palette colors into inline RGB values, and checks the resulting text
against the plain copy. Unwritten gaps start a fresh formatter range so they
cannot inherit the preceding text's background.

Formatting follows the pinned formatter's capabilities. Reverse text uses its
CSS color-inversion filter, and faint text uses opacity; these can differ from
terminal rendering. Background colors on entirely unwritten rows are omitted.
Blink and some text decorations depend on the destination. OSC 8 hyperlink targets are omitted because the formatter can cross hyperlink
and style tag boundaries. Images, terminal font settings, and exact glyph
metrics are not included. HTML cannot guarantee identical layout in every
application. If the formatter's HTML cannot represent the same text, only
plain text is offered.

`gmux command-output` prints plain text without generating HTML. Add `--html`
to return JSON containing `text` and optional `html`. Socket clients can request
`formats: "text-html"`; the default is `"text"`. See the
[daemon API](daemon-api.md) for stream requests and clipboard events.

Terminal applications can submit UTF-8 text through OSC 52. They cannot submit
rich MIME content or read the desktop clipboard. An empty OSC 52 payload clears
the destination; offering an empty text value is a separate operation. See the
[README](../README.md) for destination and recipient rules.
