# Archived review results

These notes summarize earlier review passes. Their test counts and measured
behavior belong to those passes. Use the [UI design notes](../code-review.md),
[code organization](../code-organization.md), and [testing guide](../testing.md)
for the maintained design and commands. The original review notes did not record
a source revision for each pass.

## Recorded validation

| Review pass | Recorded checks |
| --- | --- |
| Initial UI review | 36 unit tests, formatting, build, Clippy, and native UI and session checks. |
| Daemon and renderer cleanup | 37 library tests, 11 application tests, example tests, 14 native checks, and release builds. |
| Notification reliability | 40 library tests, 11 application tests, example tests, native lifecycle and alert checks, and release builds. |
| Native Wayland input and capture | 40 library tests, 11 application tests, eight example tests, native input, and debug and release builds. |
| Command and history cleanup | 73 Rust tests, 21 smoke suites, formatting, Clippy, and debug and release builds. |
| Accessible selection cleanup | 74 Rust tests, 21 smoke suites, formatting, Clippy, and debug and release builds. |

## Regression findings

Keyboard review found that a hardcoded shortcut filter swallowed unbound
Ctrl+Shift presses and leaked shortcut releases into applications using extended
keyboard reporting. Matching GTK's registered accelerators fixed both paths.
Checks included Ctrl-minus, Ctrl+Shift+U, and Shift+Page Up/Down.

Initial divider notifications could replace a restored split ratio before GTK
applied it. Native restore checks compared the saved layout before and after
drawing. Fresh screenshot checks also caught cached `GtkWidgetPaintable` output
for occluded windows, leading to the confined CSS snapshot exception documented
in the [UI design notes](../code-review.md#limits).

An accessible selection regression included an extra newline at a cached row
boundary. Fresh and cached selection updates now share the same mapping.
Native checks exercised selection across 120 scrollback lines; engine checks
covered reflow and reversed endpoints.

Transport and persistence checks covered a complete 4 MiB bracketed paste through
a real PTY, recovery after oversized commands, damaged files before and after
startup, live changes after save errors, and Quit after a save error.

The benchmark moved from XWayland input synthesis and window readback to an
isolated native Wayland compositor with PipeWire capture. Those methods measure
different paths. Their results are recorded separately in
[application performance](../performance.md).
