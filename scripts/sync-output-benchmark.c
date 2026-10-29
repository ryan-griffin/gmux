#define _POSIX_C_SOURCE 200809L
#include "../src/ghostty_bridge.h"
#include <ghostty/vt.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Build against either revision; older bridges retain every visited view.
void gmux_engine_view_set_active(GmuxView *, bool) __attribute__((weak));

static size_t allocations, allocated_bytes, live_bytes;
static bool count_allocations;
static void *allocate(void *ctx, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ctx; (void)ra;
    size_t align = (size_t)1 << alignment;
    if (align < sizeof(void *)) align = sizeof(void *);
    void *ptr = NULL;
    if (posix_memalign(&ptr, align, len)) return NULL;
    allocations++;
    allocated_bytes += len;
    live_bytes += len;
    return ptr;
}
static bool resize_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment,
    size_t new_len, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)new_len; (void)ra;
    return false;
}
static void *remap_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment,
    size_t new_len, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)new_len; (void)ra;
    return NULL;
}
static void free_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ctx; (void)alignment; (void)ra;
    live_bytes -= len;
    free(ptr);
}
static const GhosttyAllocatorVtable vtable = {
    allocate, resize_allocation, remap_allocation, free_allocation
};
static const GhosttyAllocator allocator = {.vtable = &vtable};
GhosttyResult __real_ghostty_render_state_new(const GhosttyAllocator *, GhosttyRenderState *);
GhosttyResult __wrap_ghostty_render_state_new(const GhosttyAllocator *original, GhosttyRenderState *state) {
    return __real_ghostty_render_state_new(count_allocations ? &allocator : original, state);
}
static void event(void *ud, int kind, const uint8_t *data, size_t len) {
    (void)ud; (void)kind; (void)data; (void)len;
}
static void write_text(GmuxEngine *engine, const char *text) {
    gmux_engine_write(engine, (const uint8_t *)text, strlen(text));
}
// This fixture's longest glyph fits in 16 bytes; retain values, never pointers.
typedef struct { uint8_t text[16], style[11]; } SavedCell;
static bool save_cell(void *ud, size_t index, const GmuxCell *cell) {
    SavedCell *saved = &((SavedCell *)ud)[index];
    assert(cell->text_len <= sizeof(saved->text));
    memset(saved, 0, sizeof(*saved));
    memcpy(saved->text, cell->text, cell->text_len);
    memcpy(saved->style, cell->fg, 3);
    memcpy(saved->style + 3, cell->bg, 3);
    saved->style[6] = cell->flags;
    saved->style[7] = cell->decoration;
    memcpy(saved->style + 8, cell->underline_color, 3);
    return true;
}
static void snapshot(GmuxEngine *engine, GmuxView *view, SavedCell *cells, char expected) {
    GmuxFrame frame = {0};
    uint8_t row_flags[48] = {0};
    gmux_engine_view_enter(engine, view);
    assert(gmux_engine_snapshot(engine, &frame, save_cell, cells, 160 * 48, row_flags) >= 0);
    assert(frame.cols == 160 && frame.rows == 48);
    assert(cells[0].text[0] == expected);
    gmux_engine_view_leave(engine, view);
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: %s VISIBLE HIDDEN ITERATIONS COUNT_ALLOCATIONS\n", argv[0]);
        return 2;
    }
    size_t visible = strtoul(argv[1], NULL, 10), hidden = strtoul(argv[2], NULL, 10);
    size_t iterations = strtoul(argv[3], NULL, 10), count = visible + hidden;
    assert(visible && iterations && count < 10000);
    count_allocations = atoi(argv[4]) != 0;
    GmuxEngine *engine = gmux_engine_new(NULL, event);
    assert(engine);
    assert(!gmux_engine_resize(engine, 160, 48, 8, 16));
    SavedCell *cells = calloc(160 * 48, sizeof(*cells));
    SavedCell *held_cells = calloc(160 * 48, sizeof(*held_cells));
    GmuxView **views = calloc(count, sizeof(*views));
    uint8_t bg[3] = {20, 20, 20}, fg[3] = {240, 240, 240}, palette[48] = {0};
    assert(!gmux_engine_theme(engine, bg, fg, fg, palette, true));
    write_text(engine, "\x1b[?1049h\x1b[H");
    for (size_t row = 0; row < 48; row++) {
        char text[256];
        snprintf(text, sizeof(text), "\x1b[%zu;1HA row %02zu \x1b[38;5;%zumcolored 界🙂 status\x1b[0m", row + 1, row, row + 16);
        write_text(engine, text);
    }
    for (size_t i = 0; i < count; i++) {
        views[i] = gmux_engine_view_new(engine);
        assert(views[i]);
        // Both visible clients use the same theme to exercise dirty consumption.
        snapshot(engine, views[i], cells, 'A');
    }
    // Populate each visited view's frozen cache before switching tabs.
    write_text(engine, "\x1b[?2026h");
    for (size_t i = 0; i < count; i++) snapshot(engine, views[i], held_cells, 'A');
    write_text(engine, "\x1b[?2026l");
    for (size_t i = visible; i < count; i++) {
        if (gmux_engine_view_set_active) gmux_engine_view_set_active(views[i], false);
    }
    for (size_t i = 0; i < 30; i++) {
        write_text(engine, "\x1b[?2026h\x1b[HAX\x1b[?2026l");
        for (size_t j = 0; j < visible; j++) snapshot(engine, views[j], cells, 'A');
    }
    allocations = allocated_bytes = 0;
    struct timespec start, end;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &start);
    for (size_t i = 0; i < iterations; i++) {
        char before = i % 2 ? 'B' : 'A', after = i % 2 ? 'A' : 'B';
        char text[128];
        // One PTY read contains the hold and a partially drawn next frame.
        snprintf(text, sizeof(text), "\x1b[?2026h\x1b[H%c\x1b[24;10H\x1b[32m%08zu\x1b[0m", after, i);
        write_text(engine, text);
        for (size_t j = 0; j < visible; j++) {
            snapshot(engine, views[j], held_cells, before);
            assert(!memcmp(cells, held_cells, 160 * 48 * sizeof(*cells)));
        }
        write_text(engine, "\x1b[?2026l");
        for (size_t j = 0; j < visible; j++) snapshot(engine, views[j], cells, after);
    }
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &end);
    double seconds = end.tv_sec - start.tv_sec + (end.tv_nsec - start.tv_nsec) / 1e9;
    printf("visible=%zu hidden=%zu iterations=%zu counted=%d cpu_ms=%.3f render_allocations=%zu render_allocated_bytes=%zu render_live_bytes=%zu\n",
        visible, hidden, iterations, count_allocations, seconds * 1000, allocations, allocated_bytes, live_bytes);
    gmux_engine_free(engine);
    for (size_t i = 0; i < count; i++) gmux_engine_view_free(views[i]);
    assert(live_bytes == 0);
    free(views);
    free(cells);
    free(held_cells);
}
