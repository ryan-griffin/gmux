#define _POSIX_C_SOURCE 200809L
#include "../src/ghostty_bridge.h"
#include <ghostty/vt.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Weak navigation permits a comparison with the bridge before shell support.
int gmux_engine_prompt(GmuxEngine *, bool) __attribute__((weak));
static size_t live_bytes, peak_bytes, allocations, events, refs, peak_refs, render_updates;
static size_t retained_lines;
static bool counting;
static void *allocate(void *ctx, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ctx; (void)ra;
    size_t align = (size_t)1 << alignment;
    if (align < sizeof(void *)) align = sizeof(void *);
    void *p = NULL;
    if (posix_memalign(&p, align, len)) return NULL;
    live_bytes += len; allocations++;
    if (live_bytes > peak_bytes) peak_bytes = live_bytes;
    return p;
}
static bool resize_allocation(void *ctx, void *p, size_t n, uint8_t a, size_t to, uintptr_t ra) {
    (void)ctx; (void)p; (void)n; (void)a; (void)to; (void)ra;
    return false;
}
static void *remap_allocation(void *ctx, void *p, size_t n, uint8_t a, size_t to, uintptr_t ra) {
    (void)ctx; (void)p; (void)n; (void)a; (void)to; (void)ra;
    return NULL;
}
static void free_allocation(void *ctx, void *p, size_t n, uint8_t a, uintptr_t ra) {
    (void)ctx; (void)a; (void)ra;
    live_bytes -= n;
    free(p);
}
static const GhosttyAllocatorVtable vtable = {allocate, resize_allocation, remap_allocation, free_allocation};
static const GhosttyAllocator allocator = {.vtable = &vtable};
GhosttyResult __real_ghostty_terminal_new(const GhosttyAllocator *, GhosttyTerminal *, uint16_t, uint16_t);
GhosttyResult __wrap_ghostty_terminal_new(const GhosttyAllocator *a, GhosttyTerminal *t, uint16_t cols, uint16_t rows) {
    GhosttyResult result = __real_ghostty_terminal_new(counting ? &allocator : a, t, cols, rows);
    if (!result && retained_lines) {
        size_t bytes = 512 * 1024 * 1024;
        assert(!ghostty_terminal_set(*t, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &bytes));
        assert(!ghostty_terminal_set(*t, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &retained_lines));
    }
    return result;
}
GhosttyResult __real_ghostty_terminal_grid_ref_track(GhosttyTerminal, GhosttyPoint, GhosttyTrackedGridRef *);
GhosttyResult __wrap_ghostty_terminal_grid_ref_track(GhosttyTerminal t, GhosttyPoint p, GhosttyTrackedGridRef *out) {
    GhosttyResult r = __real_ghostty_terminal_grid_ref_track(t, p, out);
    if (!r && ++refs > peak_refs) peak_refs = refs;
    return r;
}
void __real_ghostty_tracked_grid_ref_free(GhosttyTrackedGridRef);
void __wrap_ghostty_tracked_grid_ref_free(GhosttyTrackedGridRef ref) {
    if (ref) refs--;
    __real_ghostty_tracked_grid_ref_free(ref);
}
GhosttyResult __real_ghostty_render_state_update(GhosttyRenderState, GhosttyTerminal);
GhosttyResult __wrap_ghostty_render_state_update(GhosttyRenderState s, GhosttyTerminal t) {
    render_updates++;
    return __real_ghostty_render_state_update(s, t);
}
static void event(void *ud, int kind, const uint8_t *p, size_t n) {
    (void)ud; (void)p; (void)n;
    if (kind == 5) events++;
}
static double cpu(void) {
    struct timespec t;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: %s MARKERS COMMANDS RETAINED_LINES COUNT_ALLOCATIONS\n", argv[0]);
        return 2;
    }
    bool markers = atoi(argv[1]) != 0;
    size_t commands = strtoul(argv[2], NULL, 10);
    retained_lines = strtoul(argv[3], NULL, 10);
    counting = atoi(argv[4]) != 0;
    GmuxEngine *e = gmux_engine_new(NULL, event);
    assert(e && !gmux_engine_resize(e, 80, 24, 0, 0));
    // Hidden clients must not require viewport extraction for shell events.
    GmuxView *hidden[24];
    for (size_t i = 0; i < 24; i++) {
        hidden[i] = gmux_engine_view_new(e);
        assert(hidden[i]);
        gmux_engine_view_set_active(hidden[i], false);
    }
    const char *text = markers ?
        "\x1b]133;A\x07$ \x1b]133;B\x07" "command\r\n\x1b]133;C\x07result\r\n\x1b]133;D;0\x07" :
        "$ command\r\nresult\r\n";
    size_t len = strlen(text);
    double begin = cpu();
    for (size_t i = 0; i < commands; i++) gmux_engine_write(e, (const uint8_t *)text, len);
    double write_ms = (cpu() - begin) * 1000;
    size_t before_prune_refs = refs, before_prune_bytes = live_bytes;
    double navigation_ms = 0;
    size_t jumps = 0;
    if (gmux_engine_prompt && markers) {
        begin = cpu();
        for (size_t i = 0; i < 10000; i++) {
            bool previous = (i / 100) % 2 == 0;
            jumps += !gmux_engine_prompt(e, previous);
        }
        navigation_ms = (cpu() - begin) * 1000;
    }
    GmuxMemoryUsage memory = {0};
    assert(!gmux_engine_memory_usage(e, &memory));
    size_t output_rows = 0;
    uint16_t cols;
    assert(!gmux_engine_history_size(e, &cols, &output_rows));
    // Discard history without resetting the host bookkeeping explicitly.
    const char prune[] = "\x1b[2J\x1b[3J";
    gmux_engine_write(e, (const uint8_t *)prune, sizeof(prune) - 1);
    size_t after_prune_refs = refs;
    for (size_t i = 0; i < 24; i++) gmux_engine_view_free(hidden[i]);
    gmux_engine_free(e);
    assert(!refs && !live_bytes);
    assert(!render_updates);
    if (gmux_engine_prompt) assert(peak_refs <= 2052);
    printf("{\"markers\":%d,\"commands\":%zu,\"retained_rows\":%zu,\"counting\":%d,"
        "\"write_ms\":%.3f,\"navigation_ms\":%.3f,\"jumps\":%zu,\"events\":%zu,"
        "\"peak_refs\":%zu,\"retained_refs\":%zu,\"after_prune_refs\":%zu,"
        "\"prompt_anchors\":%llu,\"prompt_anchor_bytes\":%llu,"
        "\"native_live_bytes\":%zu,\"native_peak_bytes\":%zu,\"native_allocations\":%zu,\"render_updates\":%zu}\n",
        markers, commands, output_rows, counting, write_ms, navigation_ms, jumps, events,
        peak_refs, before_prune_refs, after_prune_refs,
        (unsigned long long)memory.prompt_anchors, (unsigned long long)memory.prompt_anchor_bytes,
        before_prune_bytes, peak_bytes, allocations, render_updates);
}
