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

## Application writes

Terminal applications can write through OSC 52, iTerm2 OSC 1337 `Copy`, and Kitty
OSC 5522. The pinned libghostty normalizes these protocols into one atomic write
callback. Gmux does not parse a second copy of these protocols. Clipboard reads use the permission and lifetime rules below.

Kitty writes can offer these exact MIME names together:

| MIME name | Data |
| --- | --- |
| `text/plain` | UTF-8 without NUL |
| `text/plain;charset=utf-8` | UTF-8 without NUL |
| `text/html` | Exact bytes |
| `image/png` | Exact bytes |

Gmux preserves MIME names and data, including NUL in HTML and PNG. It does not
parse HTML, decode images, or derive text from either format. Only supplied plain
text adds GTK's String fallback and its text targets. If both plain MIME names
are supplied, their bytes must agree. Duplicate MIME names and any unsupported
representation reject the entire normalized request. Libghostty resolves Kitty
aliases and repeated protocol entries before admission.

Application requests have at most four representations and 4 MiB of decoded
data in total, counting each expanded alias. Libghostty also limits the Kitty
transaction's accumulated decoded data to 4 MiB before the callback. Each engine
poll admits at most 64 events with 4 MiB of data in total. Local and remote
streams carry small MIME/length descriptors followed by binary data, without
base64 or JSON byte arrays. Explicit user copy retains its separate 4 MiB text
and 8 MiB HTML limits. A stream packet is at most 64 MiB; clipboard metadata is
at most 16 KiB. Daemon output and client input queues are each limited to 64 MiB;
the client also limits its queue to 64 packets. A stalled recipient is detached
when its daemon output queue fills, without blocking terminal output.

The native parser separately limits each Kitty transaction to 64 MIME types and
64 aliases. Repeated aliases update their targets in place, so they do not
accumulate metadata before commit. Gmux's stricter four-representation and
expanded-data limits apply when the normalized callback runs at commit.

The standard clipboard and primary selection are supported. The separate
`selection` destination is unsupported. Existing recipient policy applies to
all application writes: select one attached, capable frontend viewing the pane,
prefer the focused frontend, then connection order. Headless observers do not
receive writes. Requests completed without an eligible frontend are discarded.
Offers are ordered across panes and explicit copies, transient, and never saved
or replayed when a frontend attaches or reconnects.

Zero representations clears the destination. An empty representation offers
that MIME type with zero bytes. An empty OSC 52 payload retains its existing
clear behavior. A single GDK union provider publishes all alternatives together.
Rejected requests leave the existing desktop clipboard unchanged.

Kitty receives `DONE` after synchronous bounded admission, `ENOSYS` for unsupported
destinations/formats or no eligible frontend, `EINVAL` for invalid data or an
oversized expanded offer, and `EBUSY` when engine admission is full. Libghostty
returns `EFBIG` if the transaction exceeds its decoded-data budget before commit.
OSC 52 and iTerm2 writes have no acknowledgement. Admission does not wait for GTK
and cannot promise delivery after a disconnect. A later transport failure
detaches the recipient; a GTK publication failure is logged and retains the old
clipboard. Gmux never retains a borrowed callback or reply pointer and never
records a session permission grant.

For example, run this inside a Gmux terminal to offer HTML and plain text:

```python
import base64
import sys

def packet(metadata, data=None):
    payload = "" if data is None else ";" + base64.b64encode(data).decode()
    sys.stdout.write("\x1b]5522;" + metadata + payload + "\x1b\\")

packet("type=write")
for mime, data in [("text/html", b"<b>Hello</b>"), ("text/plain", b"Hello")]:
    packet("type=wdata:mime=" + base64.b64encode(mime.encode()).decode(), data)
packet("type=wdata")
sys.stdout.flush()
```

For larger data, split each MIME's base64 stream across `wdata` packets rather
than putting the whole file in one OSC sequence. The same transaction can include
an `image/png` representation containing the file's exact bytes.

## Application reads

Terminal programs can request plain text through OSC 52 `?`, or MIME data and
available targets through Kitty OSC 5522. Gmux routes each request to one attached
clipboard-capable window viewing the pane, preferring the focused window. It
never broadcasts a request or transfers an outstanding approval to another
window. Headless observers have no clipboard capability by default.

Data reads require Allow Once in a permission strip in that pane. Remote reads
name the destination connection in the strip and access the viewing computer's
clipboard, not the daemon host's clipboard. Program-supplied names are not trusted
identities. Gmux does not remember approvals, prefetch clipboard content, or save
requests, grants, or clipboard bytes. History checkpoints omit unfinished parser
input as well, so partial clipboard control strings are never saved. Deny
suppresses unsolicited data reads from the pane for 30 seconds. A genuine user
paste still authorizes its one-use follow-up. Requests expire
after 15 seconds without an answer; content transfer has a five-second deadline.

A targets-only Kitty request is allowed without a content prompt. A request for
both targets and data still requires permission. Target discovery reads format
metadata only. Each request can read up to four MIME entries and 4 MiB of decoded
data, counting repeated requested entries. Listings contain at most 16 valid
names and 4096 bytes. MIME names are limited to 256 bytes. Unavailable formats
are omitted. Plain text must be NUL-free UTF-8; other MIME data preserves exact
bytes. Gmux does not render HTML, decode images, or open files named in clipboard
data.

The standard clipboard and primary selection are separate destinations. User
paste preserves that destination. With terminal mode 5522 enabled, a genuine
keyboard, menu, or middle-click paste sends a native targets event with a one-use
password. The application's follow-up data read needs no additional prompt while
its five-second paste authorization remains valid. A targets listing preserves
that authorization. A clipboard change, a new paste, detach, recipient change,
or pane exit revokes it. The authorization belongs to that pane, window,
destination, connection, and clipboard generation. GTK independently checks its
own recent paste gesture before honoring a daemon's grant. Scripted paste, CLI input, IME input,
and program-supplied names or passwords do not create user-paste authorization.
Ordinary text paste continues to use native terminal-mode handling. A deliberate
paste supersedes unanswered clipboard work in its window, including consent
pending in another split pane. A response already encoding finishes its ordered
PTY-input transaction.

A pending read pauses only its pane's output. The daemon continues polling other
PTYs and serving windows. GTK obtains permission and reads clipboard streams
asynchronously. Gmux owns a native deferred request instead of retaining the
borrowed callback or reply pointer. After validation, the native encoder emits
at most 64 KiB of raw data per daemon turn. Its response remains one ordered PTY
input transaction, including when keyboard input arrives during encoding.

Clipboard changes during content transfer abort the read. Detach, capability
loss, pane exit, and timeouts cancel pending work. No completed result is replayed
on reconnect. Kitty receives EPERM for denial, ENOSYS when no frontend or
supported destination is available, EBUSY for admission limits or timeouts, and
EIO for transfer failures. OSC 52 receives an empty response for each failure.
Malformed native protocol requests can be ignored before admission. Per-pane
and per-window rate limits, a maximum of 16 pending reads, bounded staging
memory, and existing transport queues limit malicious bursts.

The [pinned callback contract](https://github.com/ghostty-org/ghostty/blob/83fb95624b0439ee660c2679b43b38c8b63769cd/include/ghostty/vt/terminal.h)
defines borrowing, atomicity, and protocol replies. GDK documents
[byte providers](https://docs.gtk.org/gdk4/ctor.ContentProvider.new_for_bytes.html),
[union ordering](https://docs.gtk.org/gdk4/ctor.ContentProvider.new_union.html), and
[publication failure behavior](https://docs.gtk.org/gdk4/method.Clipboard.set_content.html).
