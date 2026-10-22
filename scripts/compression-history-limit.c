// Benchmark-only interposer. Increase retention through the public pinned API
// without changing Gmux's production scrollback limit or terminal ABI.
#define _GNU_SOURCE
#include <ghostty/vt.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>

GhosttyResult ghostty_terminal_new(const GhosttyAllocator *allocator,
    GhosttyTerminal *terminal, uint16_t cols, uint16_t rows) {
    typedef GhosttyResult (*NewFn)(const GhosttyAllocator *, GhosttyTerminal *, uint16_t, uint16_t);
    NewFn original = (NewFn)dlsym(RTLD_NEXT, "ghostty_terminal_new");
    if (!original) abort();
    GhosttyResult result = original(allocator, terminal, cols, rows);
    if (result != GHOSTTY_SUCCESS) return result;
    const char *setting = getenv("GMUX_BENCH_SCROLLBACK_BYTES");
    if (!setting) return result;
    char *end;
    errno = 0;
    unsigned long long parsed = strtoull(setting, &end, 10);
    if (errno || !*setting || *end || parsed > SIZE_MAX) abort();
    size_t bytes = (size_t)parsed;
    result = ghostty_terminal_set(*terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &bytes);
    if (result != GHOSTTY_SUCCESS) {
        ghostty_terminal_free(*terminal);
        *terminal = NULL;
    }
    return result;
}
