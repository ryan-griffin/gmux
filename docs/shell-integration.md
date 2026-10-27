# Shell integration

Gmux automatically loads its packaged hooks in new Bash and Zsh login shells.
It preserves startup files and existing prompt/preexec hooks. The hooks report
OSC 133 prompt, input, command-start, and command-end markers. Gmux also accepts
markers from other shell integrations, including shells with native OSC 133
support. Unsupported shells start normally and use foreground-process detection.

## Navigation and output

Use Ctrl+Shift+Up and Ctrl+Shift+Down to go to the previous and next prompt.
The Terminal Actions menu and command palette also offer these actions, plus
Select Last Command Output and Copy Last Command Output. Right-click an output
cell to select or copy the command output containing that cell. Output selection
excludes the prompt and input and copies wrapped text as logical lines.

Navigation and selection belong to the requesting window. They do not move
another window's viewport. Command state belongs to the shared terminal session
and survives frontend reconnects.

Gmux retains at most 2,048 primary prompt anchors per pane. History pruning
releases discarded anchors. Right, continuation, and secondary prompts do not
create navigation stops. A repeated marker on the same tracked row updates that
prompt's anchor. OSC 133 supplies no command identity, so Gmux cannot distinguish
every possible custom prompt rewrite from a new prompt. Navigation and command
output actions are unavailable in alternate screens and during synchronized
output holds.

## CLI and socket API

```sh
gmux metadata --pane 2
gmux command-output --pane 2
gmux command-output --pane 2 --column 0 --row 12
```

`metadata` returns `meta.command` with `phase`, `at_prompt`, `command`,
`command_truncated`, and `last_exit_code`. Phases are `unknown`, `prompt`,
`input`, `running`, and `complete`. The command text is present only when the
shell sends it, with a 4 KiB UTF-8 bound. The packaged hooks currently omit it.
The last exit code is `null` until reported and after a command finishes without
a status. A prompt following an unfinished command clears its unknown exit
status. Empty input and prompt redraws in the packaged hooks preserve the last
reported status. `meta.busy` combines semantic command-start information with
the existing foreground-process check.

`gmux list` includes command state and busy status for each pane.
`command-output` prints output without changing another frontend's selection.
Without coordinates, it uses the last completed command's output, or current
output when no completed output is available. This also works when invoked
inside the target pane. Coordinates
refer to the retained screen grid, starting at row zero in scrollback. Output
can still be selected at a cell when an integration omits the command-start
event, provided libghostty can identify its semantic boundaries. Missing or
discarded output produces an error.

The control socket accepts `metadata` with `pane`, and `command-output` with
`pane` and an optional `point: [column, row]`. The frontend stream accepts
`prompt` with `previous` and a scroll `serial`, and `select-output` with an
optional `point`, a selection `serial`, and `copy: null`, `"text"`, or
`"text-html"`. A missing or null `copy` only selects. A format value copies
through the frontend clipboard channel using that stream's view. GTK requests
`"text-html"`. `command-output` also accepts `formats: "text-html"` to return
plain text and optional HTML; `gmux command-output --html` prints this JSON. Context menus send `anchor-output` with `point`
when opened, then `select-output` with `anchored: true` when chosen. This keeps
the target attached to its command across output and reflow. Discarded or replaced
commands fail instead of selecting text that reuses their cells. Targets older
than the prompt index remain usable until a prompt rewrite makes their identity
ambiguous. Navigation is a frontend stream operation because
the control socket has no requesting viewport.

Full terminal reset clears command metadata and anchors. Clearing the screen
removes anchors while keeping execution metadata. Daemon restoration starts a
fresh shell and restores text history without old command state or prompt anchors.

## Manual and nested shells

For Bash, source this early in `~/.bashrc`:

```bash
if [[ -n ${GMUX_RESOURCES_DIR-} ]]; then
    source "$GMUX_RESOURCES_DIR/shell-integration/bash/ghostty.bash"
fi
```

For Zsh, add this to `~/.zshrc`:

```zsh
if [[ -n ${GMUX_RESOURCES_DIR-} ]]; then
    source "$GMUX_RESOURCES_DIR/shell-integration/zsh/ghostty-integration"
fi
```

Repeated sourcing is harmless. Manual sourcing also covers nested shells and
systems whose global Zsh startup overrides `ZDOTDIR`. Zsh requires version 5.1
or later. For an SSH shell, install or copy `share/gmux/shell-integration` on the
remote machine and source the appropriate script there. Alternatively, use an
existing remote OSC 133 integration. Gmux does not modify remote startup files.

Set `GMUX_SHELL_INTEGRATION=0` in the daemon's environment to disable automatic
hook injection. Existing OSC 133 markers remain supported. `GMUX_RESOURCES_DIR`
can override the resource directory. Installed builds look beside the executable
under `../share/gmux`; development builds also find the repository's `data`.
Relative overrides resolve in the daemon's working directory before shell launch.

Gmux uses the startup mechanisms described in the official
[Ghostty shell integration documentation](https://ghostty.org/docs/features/shell-integration)
and the pinned resource instructions. The native VT library parses markers;
Gmux installs the shell scripts separately. Resource origins, changes, and
licenses are recorded in [the resource notices](../data/shell-integration/README.md).
