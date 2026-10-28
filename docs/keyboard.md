# Keyboard input

Gmux forwards keys delivered to its terminal by GTK through the same keyboard
path for local and remote sessions. The terminal program's negotiated keyboard
mode controls encoding, including modifiers and press, repeat, and release events.

Physical identity, produced UTF-8 text, and the active layout's base-level Unicode
character travel separately. Shift+1 on US supplies physical `Digit1`, base `1`,
and text `!`. The same physical key on French supplies base `&` and text `1`.
Kitty applications that request alternate keys can receive the layout character
and the US physical-key alternate that libghostty supports. Gmux uses GDK's
[layout translation](https://docs.gtk.org/gdk4/method.Display.translate_key.html)
and consumed modifiers, including for synchronous input-method commits.
Composition and asynchronous commits remain text without a stale physical key.

On Linux Wayland, Gmux converts GDK's evdev-plus-eight keycodes to physical
identities. It translates the base character in the event's active layout
through GDK, without building or caching a separate keymap.

Num Lock uses the public
[GDK keyboard-device state](https://docs.gtk.org/gdk4/method.Device.get_num_lock_state.html),
queried on focus entry and observed through property notifications. Key events
use that shared device snapshot; events from another device query its state.
Keys after an unobserved lock toggle still carry its current state. Caps Lock
also remains available. Meta and Hyper affect local actions without occupying
terminal protocol modifier bits.

ISO `IntlBackslash` and JIS `IntlRo`/`IntlYen` remain distinct physical keys at
the native boundary. Kitty has no separate functional codes for these positions;
their text and layout codepoints determine their terminal encoding. The pinned
encoder cannot give every international function or level-selection key a
separate protocol identity. Dead keys without a Unicode base report that base
as unavailable. Callers without a hardware event report an unavailable physical
identity while retaining text and any layout information they can supply.
CLI/socket callers without a hardware event use the same explicit fallback.

Programs that enable the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
can receive play/pause, stop, next/previous track, volume down/up, and mute.
For example, play/pause sends `CSI 57430 u`. With event reporting enabled, its
repeat sends `CSI 57430 ; 1 : 2 u` and its release sends `CSI 57430 ; 1 : 3 u`.
Without event reporting, repeats encode like presses and releases send nothing.
These keys send nothing in legacy terminal modes.

GDK names the play/pause key `AudioPlay`. The separate `AudioPause` key has no
matching identifier in the pinned Ghostty API and sends nothing. Gmux does not
substitute the toggle's code for a dedicated pause command.

Gmux recognizes browser Back, Forward, Refresh/Reload, Stop, Search, Favorites,
and HomePage as Ghostty browser keys. They send no terminal bytes. Kitty has no
standard functional-key codes for them, and the pinned Ghostty encoder has no
browser encoding. They are not substitutes for arrow keys, Backspace, or Home.

The desktop or compositor may reserve media, volume, browser, or shortcut keys
before GTK sees them. Gmux does not change system key grabs.

## Native library

The pinned Ghostty revision defines the media/audio identifiers but omits their
Kitty encoding entries. Bootstrap applies the checked-in
[media-key patch](../scripts/patches/ghostty-media-keys.patch) to add the seven
standard [functional-key codes](https://sw.kovidgoyal.net/kitty/keyboard-protocol/#functional-key-definitions).
It changes no public headers or C/Rust ABI. The source archive revision and
checksum remain pinned; patched source caches include the patch digest.

Run `./scripts/bootstrap-ghostty.sh` after changing the patch. The run and install
scripts also refresh the default library when the bootstrap or patch changes.
An external `GHOSTTY_PREFIX` must contain the same revision with this patch applied
to provide media/audio encoding.
