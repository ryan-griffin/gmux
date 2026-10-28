# Scrollback retention

The daemon owns history retention because all windows share each terminal's
history. One saved policy applies independently to every pane on that daemon,
including detached panes. Windows keep their own scroll positions, selections,
appearance, and Find queries.

Gmux defaults to 10,000,000 retained allocation bytes per terminal with no
physical-line limit. The pinned libghostty construction default is only 10,000
bytes with no line limit. Gmux sets its own larger default explicitly to retain
useful command output. This policy is separate from idle history compression.

```sh
gmux scrollback
gmux scrollback --max-bytes 10000000 --max-lines 10000
gmux --connection server scrollback --max-lines 5000
gmux scrollback --max-bytes unlimited
gmux scrollback --max-bytes default --max-lines default
```

Calling `scrollback` without options inspects the running policy. An omitted
option leaves that dimension unchanged. Each option accepts an unsigned integer,
`default`, or `unlimited`. Byte values are bytes, with no unit suffix. Physical
lines count wrapped rows separately.

- `default` restores that dimension's Gmux default.
- `unlimited` removes that dimension's limit. The other limit still applies.
- Byte zero disables scrollback and immediately erases all retained history.
  The active screen remains intact and subsequent output retains no history.
- Line zero retains the native minimum page allowance. It does not disable
  scrollback. Use byte zero when no retained history is wanted.

Both limits work together. Either can prune history. They are approximate,
whole-page retention limits, not exact line counts or process RSS caps.
Libghostty preserves the active screen and the page overlapping it, permits at
least one standard page of history for a line limit, and raises small byte limits
to its minimum allocation. Actual retention depends on terminal width, styles,
Unicode graphemes, and page boundaries. Small line limits can retain hundreds of
extra physical rows. Images, engine bookkeeping, frontend caches, and other
process allocations are outside these limits. Compression changes resident
storage without changing the logical retention limit.

Lowering a limit prunes eligible history as soon as the update completes, even
without new terminal output. Discarded selections and command-output targets
become unavailable. Surviving selections and scrolled positions keep following
their content; a discarded viewport moves to the oldest retained row. Find
restarts against retained history, and local and remote history caches receive
updated origins and bounds. Raising or resetting a limit cannot recover removed
history. Running applications and their active screens continue unchanged.
An application's synchronized-output hold keeps its captured frame until release
or the existing one-second timeout. History pruning still happens immediately;
view metadata refreshes when the hold ends.

## Socket API and storage

Local sockets and authenticated remote control connections accept the same
request. The CLI's existing `--connection` selector addresses the chosen daemon.

```sh
gmux rpc '{"method":"scrollback"}'
gmux rpc '{"method":"scrollback","max_bytes":20000000,"max_lines":10000}'
gmux --connection server rpc '{"method":"scrollback","max_lines":"default"}'
```

Inspection returns `{"limits":{"max_bytes":10000000,"max_lines":"unlimited"}}`
with the current resolved policy. Updates additionally return `saved` and
`applied`, both true on success. `status` includes the same policy in
`scrollback_limits`. Numeric strings, null, negatives, fractions, unknown fields,
and integers outside the daemon's native size range are errors. The native
`SIZE_MAX` sentinel is reserved; use `"unlimited"` instead. The daemon validates
the complete request before saving or pruning.

Settings live in `$XDG_CONFIG_HOME/gmux/daemon-settings.json`, normally
`~/.config/gmux/daemon-settings.json`. Missing settings use defaults. Invalid or
unreadable settings prevent startup before any sessions restore. Updates use the
ordered disk worker and existing locked, synced atomic writes. PTY processing,
input, and read-only controls continue while saving. The daemon applies an
update to its terminals only after durable publication succeeds.

A save error returns `saved:false` and `applied:false`; running limits and history
remain unchanged. A durability error after rename can leave the new file
published, so inspect the file before restarting after such an error. If native
application unexpectedly fails after publication, the response has `saved:true`,
`applied:false`, an error, and per-pane failures. The saved policy remains in
force for new terminals. Retrying the update reapplies the policy; reverting it
cannot restore discarded history.

Restoration applies the policy before replaying saved text into a fresh engine.
Existing checkpoint safety budgets still apply: 8 MiB per pane, 64 MiB per
layout, 128 panes, and a bounded decoder. Larger runtime retention does not
increase those budgets or resume shell processes or terminal modes. See
[scrollback checkpoints](terminal-sessions.md#scrollback-checkpoints).
