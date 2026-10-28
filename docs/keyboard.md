# Keyboard input

Gmux forwards keys delivered to its terminal by GTK through the same keyboard
path for local and remote sessions. The terminal program's negotiated keyboard
mode controls encoding, including modifiers and press, repeat, and release events.

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
