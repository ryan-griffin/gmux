# Native terminal library

Gmux builds [libghostty-vt 1.0.0](https://lib.ghostty.org/vt/1.0.0/) from its
official source and dependency tarballs. The release corresponds to commit
`0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`, the peeled `vt-1.0.0` tag.
Both archives come from `https://release.files.ghostty.org/libghostty-vt/1.0.0/`:

| Archive | SHA-256 |
| --- | --- |
| `libghostty-vt-1.0.0.tar.gz` | `4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676` |
| `libghostty-vt-1.0.0-deps.tar.gz` | `4d5ea5e448e4c7ea8fb55ccbaa8668f2a97d44f438cee831e2b7ecd162d85c29` |

The bootstrap pins Zig 0.17.0 and its Linux x86-64 and AArch64 archive checksums.

```sh
./scripts/bootstrap-ghostty.sh
```

Bootstrap builds only the VT library, with SIMD enabled, `ReleaseFast`
optimization, and the architecture's baseline CPU target. After downloading the
archives and compiler, the build resolves all Zig packages offline through
`--system` and the release dependency bundle. Source archives,
patched trees, compiler packages, and build caches
stay under this checkout's `.deps/`. It does not install Zig or Ghostty globally.
Source cache names include the library version and patch digest; the build stamp
also covers the bootstrap and architecture. Run and install scripts refresh it.
CI caches `.deps/` by the bootstrap and patch contents.

The [media-key patch](../scripts/patches/ghostty-media-keys.patch) adds seven Kitty
functional codes still absent in the release. It changes no public headers or
ABI. The [clipboard-read patch](../scripts/patches/ghostty-clipboard-reads.patch)
adds owned deferred requests for asynchronous desktop access. The
[snapshot-state patch](../scripts/patches/ghostty-snapshot-state.patch) adds
checkpoint encoding that omits unfinished parser input. There are no additional
security backports on top of the release source. An external `GHOSTTY_PREFIX`
must provide this exact revision with all three integration patches.
Ghostty implementation types stay inside the C bridge; Rust and frontend clients
use Gmux's declarations.

The installed daemon loads `libghostty-vt.so.1` from its private `lib/gmux`
directory. Upstream's stable 1.x C ABI does not cover Gmux's added patch functions,
so an unpatched system library cannot replace this build. The library has its
own release cycle, independent of the Ghostty desktop app.

[Saved checkpoints](terminal-sessions.md#scrollback-checkpoints) require the exact
source revision. Upstream versions snapshot bytes separately from its C ABI and
may change them in any release. History from another pin is rejected with a
warning. Layout, directories, and retention settings still restore. Export
needed text with `gmux read` before restarting the old daemon. No snapshot
migration or cross-revision decoding is provided. The current formatter's
hyperlinks are discarded and its blank-cell operations become bounded spaces
before replay. Only validated UTF-8, CRLF, and whitelisted SGR reach the fresh
terminal. Text/SGR-only history reuses the formatted buffer. Content that needs
normalization uses at most another 8 MiB of temporary output per pane. The 8 MiB
formatted-input/replay and 64 MiB decoder limits remain in force.

## Grapheme storage

The pinned engine retains one base codepoint and at most 64 suffix codepoints
per cell. Gmux preserves that complete value, up to 260 UTF-8 bytes, in live
frames, history, search, copies, and saved scrollback. Additional suffixes follow
Ghostty's own retention behavior. Column width and wide spacers always come
from the engine.

Rust cells hold up to 15 text bytes inline and share longer text through an
`Arc<str>`. Ordinary cells allocate no text storage. The C bridge lends each
changed cell synchronously using a single bounded stack buffer; native pointers
never enter retained Rust cells or the wire format. Unchanged rows keep their
owned text, including while synchronized output freezes a view.

The stream uses explicit byte lengths and a 64 MiB record cap. Each daemon output
buffer and client input queue has a 64 MiB ceiling within its process-wide
[resource budgets](resource-budgets.md). These limits accommodate a
maximum-sized grid of maximum-length native clusters plus history prefetch.
The client history cache retains at most 2 MiB including overflow text and row
storage, evicting complete rows when needed. Shared text is charged separately
for every owning cell to keep admission conservative. Long-heavy history can
therefore offer less local scroll prediction; daemon-provided views remain
complete. Existing copy and checkpoint limits still apply to total output;
remote control JSON replies retain their independent 16 MiB limit.

## Release security fixes

The library includes the terminal fixes from the
[Ghostty 1.4.0 release](https://ghostty.org/docs/install/release-notes/1-4-0).
The vendored Bash and Zsh integration also comes from that release. Applicability
of the [upstream advisories](https://github.com/ghostty-org/ghostty/security/advisories)
follows Gmux's daemon-owned terminal and separate GTK/GSK frontend.

| Advisory | Gmux applicability |
| --- | --- |
| [w9q4-5jvv-mcv4](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-w9q4-5jvv-mcv4), cursor-page corruption during clear | Applies to native terminal output with small scrollback limits. The release migrates cursor style and hyperlink ownership before pruning. |
| [qjfx-r533-6vx3](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-qjfx-r533-6vx3), one-row line-limit use-after-free | Applies because Gmux supports line limits and one-row terminals. The release rebuilds cursor state after pruning. |
| [2r63-vmw6-3m3c](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-2r63-vmw6-3m3c), shell directory escape injection | Applies to the vendored Bash/Zsh scripts. Release scripts strip control characters from directory and hostname reports; Zsh prints backslashes literally. |
| [jqr7-69mm-v35h](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-jqr7-69mm-v35h), narrowing very wide pages | The native decoder and resize fix is included. Gmux already bounds live grids to 1,000 cells per axis and validates snapshot dimensions before replay. |
| [3r5j-c6rj-xq2v](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-3r5j-c6rj-xq2v), Kitty clipboard alias growth | Relevant when Gmux enables its clipboard-write callback for attached clients. Native Kitty transactions now have bounded alias storage; Gmux separately bounds its supported text, HTML, PNG, and clear offers. |
| [mhff-mv4q-fp63](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-mhff-mv4q-fp63), Kitty graphics validation | Native parsing fixes are included. Gmux uses the library's direct-data loading defaults and does not enable file, temporary-file, or shared-memory loading. Its GTK renderer does not draw Kitty images. |
| [4hwh-wx3w-3fqg](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-4hwh-wx3w-3fqg), carriage-return paste checks | The native safety predicate is fixed. Gmux's explicit paste/send path allows unsafe text intentionally and has no Ghostty paste-confirmation UI, so this update does not add a confirmation policy. |
| [hp9c-qp4q-cpw3](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-hp9c-qp4q-cpw3), [hcr9-7hcr-mjm8](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-hcr9-7hcr-mjm8), tmux control mode | Gmux does not use Ghostty's tmux control-mode client. Ordinary tmux output is parsed as terminal data. |
| [v8cw-fg4j-jcc4](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-v8cw-fg4j-jcc4), [mxqx-236q-wrv7](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-mxqx-236q-wrv7), [mfw9-xcr3-7gfp](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-mfw9-xcr3-7gfp), [cw69-wm67-5pv8](https://github.com/ghostty-org/ghostty/security/advisories/GHSA-cw69-wm67-5pv8), macOS paste/file/URL handling | Gmux is Linux-only and does not use Ghostty's desktop drag/drop, file execution, URL opener, or renderer. These desktop fixes do not change Gmux's GTK behavior. |
