# Shell integration resources

The Bash and Zsh scripts come from Ghostty revision
`8f0dd3709050b1026f6324368805033197d8b4a5`, matching
`scripts/bootstrap-ghostty.py`:

- `bash/ghostty.bash`, `zsh/.zshenv`, and `zsh/ghostty-integration` are
  GPL-3.0-or-later. Their original license notices remain in each file.
  See `COPYING.GPL-3`.
- `bash/bash-preexec.sh` is bash-preexec 0.7.0, distributed under the MIT
  license in `COPYING.bash-preexec`.

Gmux changes the Bash script to preserve an explicitly empty `ENV` and to
skip initialization when `__ghostty_precmd` already exists. This makes manual
sourcing and startup-file reentrancy harmless. It also emits command-start
markers when Bash history is disabled or contains no command text.
The Zsh integration omits idle command-end markers when its preexec hook did
not run. Empty input and prompt redraws then preserve the last reported status.
The Zsh startup loader is unchanged.

These scripts have their own licenses, separate from the Gmux application.
The installer ships the scripts, source notices, and license texts together.
For usage, see [shell integration](../../docs/shell-integration.md).
