#define _POSIX_C_SOURCE 200809L
// Inspect the native copy path with a larger retained terminal than the default.
#ifndef GMUX_BRIDGE_SOURCE
#define GMUX_BRIDGE_SOURCE "../src/ghostty_bridge.c"
#endif
#include GMUX_BRIDGE_SOURCE
#include <assert.h>
#include <stdio.h>
#include <time.h>
#include <sys/resource.h>

typedef struct { uint8_t *bytes; size_t len, cap; } Output;
#ifndef GMUX_COPY_BASELINE
static bool copy_bytes(void *ud, const uint8_t *data, size_t len) {
    Output *out = ud;
    if (len > GMUX_CLIPBOARD_MAX_BYTES - out->len) return false;
    size_t size = out->len + len;
    if (size > out->cap) {
        size_t cap = out->cap ? out->cap * 2 : 4096;
        if (cap < size) cap = size;
        if (cap > GMUX_CLIPBOARD_MAX_BYTES) cap = GMUX_CLIPBOARD_MAX_BYTES;
        out->bytes = realloc(out->bytes, cap);
        assert(out->bytes);
        out->cap = cap;
    }
    memcpy(out->bytes + out->len, data, len);
    out->len = size;
    return true;
}
#endif
static Output copy(GmuxEngine *engine) {
    Output out = {0};
#ifdef GMUX_COPY_BASELINE
    assert(gmux_engine_copy(engine, NULL, 0, &out.len) >= 0);
    assert(out.len <= GMUX_CLIPBOARD_MAX_BYTES);
    out.bytes = calloc(1, out.len ? out.len : 1);
    out.cap = out.len;
    assert(!gmux_engine_copy(engine, out.bytes, out.cap, &out.len));
#else
    assert(!gmux_engine_copy(engine, false, copy_bytes, &out, NULL));
#endif
    return out;
}
static double seconds(void) {
    struct timespec t;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
int main(int argc, char **argv) {
    size_t cells = argc > 1 ? strtoul(argv[1], NULL, 10) : 100000;
    bool styled = argc > 2 && !strcmp(argv[2], "styled");
    assert(cells && cells % 1000 == 0 && cells <= GMUX_CLIPBOARD_MAX_BYTES);
    GmuxEngine *engine = gmux_engine_new(NULL, NULL);
    assert(engine && !gmux_engine_resize(engine, 1000, 100, 0, 0));
    size_t retention = 64 * 1024 * 1024;
    assert(!ghostty_terminal_set(engine->terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &retention));
    const char *pair = styled ? "\x1b[31mx\x1b[32mx" : "xx";
    // A single write buffer also keeps fixture ingestion out of the timed path.
    size_t pair_len = strlen(pair), input_len = cells / 2 * pair_len;
    uint8_t *input = malloc(input_len);
    assert(input);
    for (size_t i = 0; i < cells / 2; i++) memcpy(input + i * pair_len, pair, pair_len);
    gmux_engine_write(engine, input, input_len);
    free(input);
    assert(!gmux_engine_select(engine, 0, 0, 999, cells / 1000 - 1, 0));
    // Reset the ingestion high-water mark before measuring copy scratch.
    FILE *reset = fopen("/proc/self/clear_refs", "w");
    if (reset) { fputs("5", reset); fclose(reset); }
    struct rusage before, during, after;
    getrusage(RUSAGE_SELF, &before);
    Output first = copy(engine);
    getrusage(RUSAGE_SELF, &during);
    assert(first.len == cells);
    free(first.bytes);
    const size_t iterations = cells > 1000000 ? 20 : 100;
    double began = seconds();
    for (size_t i = 0; i < iterations; i++) {
        Output out = copy(engine);
        assert(out.len == cells);
        free(out.bytes);
    }
    double copy_us = (seconds() - began) * 1e6 / iterations;
    getrusage(RUSAGE_SELF, &after);
    printf("{\"cells\":%zu,\"styled\":%s,\"copy_us\":%.3f,\"peak_growth_kib\":%ld,\"offer_growth_kib\":%ld}\n",
        cells, styled ? "true" : "false", copy_us, after.ru_maxrss - before.ru_maxrss, during.ru_maxrss - before.ru_maxrss);
    gmux_engine_free(engine);
}
