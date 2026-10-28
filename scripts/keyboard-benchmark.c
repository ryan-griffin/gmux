#define _POSIX_C_SOURCE 200809L
#define GMUX_VERSION "benchmark"
#ifndef GMUX_BRIDGE_SOURCE
#define GMUX_BRIDGE_SOURCE "../src/ghostty_bridge.c"
#endif
#include GMUX_BRIDGE_SOURCE
#include <assert.h>
#include <stdio.h>

// The key objects use the public allocator API, including allocations performed
// inside the shared library. Engine/terminal initialization is outside timing.
static size_t allocations;
static void *allocate(void *ctx, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ctx; (void)ra;
    size_t align = (size_t)1 << alignment;
    if (align < sizeof(void *)) align = sizeof(void *);
    void *ptr = NULL;
    if (posix_memalign(&ptr, align, len)) return NULL;
    allocations++;
    return ptr;
}
static bool resize_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment, size_t to, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)to; (void)ra;
    return false;
}
static void *remap_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment, size_t to, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)to; (void)ra;
    return NULL;
}
static void free_allocation(void *ctx, void *ptr, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ctx; (void)len; (void)alignment; (void)ra;
    free(ptr);
}
static const GhosttyAllocatorVtable vtable = {allocate, resize_allocation, remap_allocation, free_allocation};
static const GhosttyAllocator allocator = {.vtable = &vtable};
static double seconds(clockid_t clock) {
    struct timespec t;
    clock_gettime(clock, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
int main(int argc, char **argv) {
    size_t iterations = argc > 1 ? strtoul(argv[1], NULL, 10) : 1000000;
    GmuxEngine *engine = gmux_engine_new(NULL, NULL);
    assert(engine);
    ghostty_key_event_free(engine->key);
    ghostty_key_encoder_free(engine->encoder);
    assert(!ghostty_key_event_new(&allocator, &engine->key));
    assert(!ghostty_key_encoder_new(&allocator, &engine->encoder));
    // Match the FFI call boundary. Do not let the compiler specialize constant
    // key names/identities or inline the entire bridge into this benchmark.
    __typeof__(&gmux_engine_key) volatile encode = gmux_engine_key;
#ifndef GMUX_KEYBOARD_BASELINE
    // These physical distinctions have no separate functional code in Kitty.
    assert(key_for(86, "less") == GHOSTTY_KEY_INTL_BACKSLASH);
    assert(key_for(89, "backslash") == GHOSTTY_KEY_INTL_RO);
    assert(key_for(124, "backslash") == GHOSTTY_KEY_INTL_YEN);
    assert(key_for(0, "a") == GHOSTTY_KEY_UNIDENTIFIED);
    assert(key_for(65535, "unknown") == GHOSTTY_KEY_UNIDENTIFIED);
#endif
    for (unsigned kitty = 0; kitty < 2; kitty++) {
        const char *mode = kitty ? "\x1b[=31u" : "\x1b[=0u";
        gmux_engine_write(engine, (const uint8_t *)mode, strlen(mode));
        uint8_t out[1024];
        size_t bytes = 0;
        allocations = 0;
        double cpu = seconds(CLOCK_PROCESS_CPUTIME_ID), wall = seconds(CLOCK_MONOTONIC);
        for (size_t i = 0; i < iterations; i++) {
            uint8_t action = i % 3;
            const char *text = action == GMUX_KEY_ACTION_RELEASE ? "" : "A";
#ifdef GMUX_KEYBOARD_BASELINE
            bytes += encode(engine, "A", 65, 1, action, text, strlen(text), out, sizeof(out));
#else
            bytes += encode(engine, "A", 30, 97, 1, 1, action, text, strlen(text), out, sizeof(out));
#endif
        }
        wall = seconds(CLOCK_MONOTONIC) - wall;
        cpu = seconds(CLOCK_PROCESS_CPUTIME_ID) - cpu;
        printf("{\"mode\":\"%s\",\"iterations\":%zu,\"cpu_ns_per_key\":%.1f,\"wall_ns_per_key\":%.1f,\"native_key_allocations\":%zu,\"bytes\":%zu}\n",
               kitty ? "kitty" : "legacy", iterations, cpu * 1e9 / iterations, wall * 1e9 / iterations, allocations, bytes);
    }
    gmux_engine_free(engine);
}
