#define _POSIX_C_SOURCE 200809L
#include "ghostty_bridge.h"
#include <ghostty/vt.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Count creation of tracked references, whose allocations persist throughout a
// drag. Compile with --wrap=ghostty_terminal_grid_ref_track.
static size_t tracks;
GhosttyResult __real_ghostty_terminal_grid_ref_track(GhosttyTerminal, GhosttyPoint, GhosttyTrackedGridRef *);
GhosttyResult __wrap_ghostty_terminal_grid_ref_track(GhosttyTerminal terminal, GhosttyPoint point, GhosttyTrackedGridRef *ref) {
    tracks++;
    return __real_ghostty_terminal_grid_ref_track(terminal, point, ref);
}
static double seconds(void) {
    struct timespec time;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &time);
    return time.tv_sec + time.tv_nsec / 1e9;
}
int main(int argc, char **argv) {
    const uint32_t history = argc > 1 ? strtoul(argv[1], NULL, 10) : 512;
    const size_t iterations = argc > 2 ? strtoul(argv[2], NULL, 10) : 100000;
    GmuxEngine *engine = gmux_engine_new(NULL, NULL);
    assert(engine && !gmux_engine_resize(engine, 160, 48, 8, 16));
    const char *line = "0123456789 table column e\xcc\x81 \xe7\x95\x8c\r\n";
    for (uint32_t row = 0; row < history; row++)
        gmux_engine_write(engine, (const uint8_t *)line, strlen(line));
    for (uint8_t mode = 0; mode <= 3; mode += 3) {
#ifdef GMUX_LINEAR_BASELINE
        if (mode) break;
#endif
        gmux_engine_clear_selection(engine);
        assert(!gmux_engine_select(engine, 4, 0, 8, 47, mode));
        const size_t initial_tracks = tracks;
        double started = seconds();
        for (size_t i = 0; i < iterations; i++) {
            assert(!gmux_engine_select(engine, 4, 0, 5 + i % 150, 1 + i % 47, mode));
            uint64_t start, end;
#ifdef GMUX_LINEAR_BASELINE
            assert(!gmux_engine_selection(engine, &start, &end));
#else
            bool rectangle;
            assert(!gmux_engine_selection(engine, &start, &end, &rectangle));
            assert(rectangle == (mode == 3));
#endif
        }
        double select_us = (seconds() - started) * 1e6 / iterations;
        size_t written = 0;
        assert(gmux_engine_copy(engine, NULL, 0, &written) >= 0);
        uint8_t *copy = malloc(written ? written : 1);
        started = seconds();
        for (size_t i = 0; i < 1000; i++)
            assert(!gmux_engine_copy(engine, copy, written, &written));
        double copy_us = (seconds() - started) * 1e3;
        printf("{\"history\":%u,\"mode\":%u,\"select_us\":%.4f,\"copy_us\":%.4f,\"copy_bytes\":%zu,\"new_tracked_refs\":%zu}\n",
            history, mode, select_us, copy_us, written, tracks - initial_tracks);
        free(copy);
    }
    gmux_engine_free(engine);
}
