#define _GNU_SOURCE
// cc -O2 -Wall -Wextra -Werror -I.deps/ghostty/include scripts/clipboard-read-check.c
//   -L.deps/ghostty/lib -Wl,-rpath,$PWD/.deps/ghostty/lib -lghostty-vt -o /tmp/read-check
#define _POSIX_C_SOURCE 200809L
#include <ghostty/vt.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#define GS(s) ((GhosttyString){(const uint8_t *)(s), sizeof(s)-1})
static uint8_t output[8 * 1024 * 1024], expected[8 * 1024 * 1024];
static size_t size, calls, allocated;
static uint64_t pending;
static bool deferred, granted;
static const GhosttyClipboardRead *expired;
static GhosttyClipboardReadReply response;
static void write_pty(GhosttyTerminal t, void *ud, const uint8_t *data, size_t len) {
    (void)t; (void)ud; assert(len <= sizeof(output) - size); memcpy(output + size, data, len); size += len;
}
static void read_clipboard(GhosttyTerminal t, void *ud, const GhosttyClipboardRead *read) {
    (void)ud; calls++; granted = read->granted;
    if (deferred) {
        assert(ghostty_terminal_clipboard_read_defer(t, read, &pending) == GHOSTTY_SUCCESS);
        expired = read; // Only passed back for pointer identity validation, never dereferenced.
    } else read->reply(read, &response);
}
static GhosttyTerminal terminal(const GhosttyAllocator *allocator) {
    GhosttyTerminal t; assert(ghostty_terminal_new(allocator, &t, 80, 24) == 0);
    assert(ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_WRITE_PTY, write_pty) == 0);
    assert(ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ, read_clipboard) == 0);
    return t;
}
static void feed(GhosttyTerminal t, const char *s) { ghostty_terminal_vt_write(t, (const uint8_t *)s, strlen(s)); }
static void *alloc_fn(void *ctx, size_t len, uint8_t a, uintptr_t ra) {
    (void)ctx; (void)ra; void *p; size_t alignment = (size_t)1 << a;
    if (alignment < sizeof(void *)) alignment = sizeof(void *);
    if (posix_memalign(&p, alignment, len)) return NULL;
    allocated += len; return p;
}
static bool resize_fn(void *c, void *p, size_t l, uint8_t a, size_t n, uintptr_t r) { (void)c; (void)p; (void)l; (void)a; (void)n; (void)r; return false; }
static void *remap_fn(void *c, void *p, size_t l, uint8_t a, size_t n, uintptr_t r) { (void)c; (void)p; (void)l; (void)a; (void)n; (void)r; return NULL; }
static void free_fn(void *c, void *p, size_t l, uint8_t a, uintptr_t r) { (void)c; (void)a; (void)r; assert(allocated >= l); allocated -= l; free(p); }
static double now(void) { struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts); return ts.tv_sec + ts.tv_nsec / 1e9; }
int main(void) {
    const GhosttyAllocatorVtable vtable = {alloc_fn, resize_fn, remap_fn, free_fn};
    const GhosttyAllocator allocator = {.vtable = &vtable};
    GhosttyTerminal t = terminal(&allocator), other = terminal(NULL);
    uint8_t *data = malloc(4 * 1024 * 1024); assert(data); memset(data, 'X', 4 * 1024 * 1024);
    GhosttyClipboardContent content = {GS("text/plain"), {data, 0}};
    GhosttyString targets[] = {GS("text/plain"), GS("image/png")};
    response = (GhosttyClipboardReadReply){.size = sizeof(response), .result = GHOSTTY_CLIPBOARD_READ_RESULT_SUCCESS,
        .contents = &content, .contents_len = 1, .available = targets, .available_len = 2};
    for (int protocol = 0; protocol < 2; protocol++) {
        const char *query = protocol ? "\x1b]5522;type=read:id=owned:loc=primary;LiB0ZXh0L3BsYWlu\x1b\\" : "\x1b]52;p;?\x07";
        for (size_t len = 0; len <= 4 * 1024 * 1024; len = len < 4 ? len + 1 : len < 4097 ? (len == 4 ? 4094 : len + 1) : len < 65537 ? (len == 4097 ? 65534 : len + 1) : 4 * 1024 * 1024 + 1) {
            content.data.len = len;
            if (len == 65537) content.data.len = len = 4 * 1024 * 1024;
            deferred = false; size = 0; feed(t, query); size_t whole = size; memcpy(expected, output, size);
            size_t retained_before = allocated;
            deferred = true; size = 0; feed(t, query); uint64_t id = pending; assert(size == 0);
            uint64_t invalid; assert(ghostty_terminal_clipboard_read_defer(t, expired, &invalid) != 0);
            assert(ghostty_terminal_clipboard_read_complete(other, id, &response) != 0);
            for (int i = 0; i < 1000; i++) feed(other, "other pane\r\n");
            double begin = now();
            assert(ghostty_terminal_clipboard_read_complete(t, id, &response) == 0);
            double copy_ms = (now() - begin) * 1000;
            size_t retained_response = allocated - retained_before;
            assert(ghostty_terminal_clipboard_read_complete(t, id, &response) != 0);
            bool done = false; size_t steps = 0; double max = 0;
            while (!done) { double start = now(); assert(ghostty_terminal_clipboard_read_step(t, id, &done) == 0); double elapsed = now() - start; if (elapsed > max) max = elapsed; steps++; }
            assert(ghostty_terminal_clipboard_read_step(t, id, &done) != 0);
            assert(size == whole && memcmp(output, expected, size) == 0);
            if (len == 4 * 1024 * 1024) printf("%s 4 MiB: %zu steps, max %.3f ms, begin %.3f ms, retained %zu bytes, encoded %zu bytes\n", protocol ? "Kitty" : "OSC52", steps, max * 1000, copy_ms, retained_response, size);
        }
    }
    // Skewed MIME sizes and repeated requests cannot inflate native storage
    // through allocator growth. Compare both shapes with synchronous encoding.
    GhosttyClipboardContent parts[] = {
        {GS("application/a"), {data, 1800000}},
        {GS("application/b"), {data + 1800000, 2394302}},
        {GS("application/c"), {data + 4194302, 1}},
        {GS("application/d"), {data + 4194303, 1}},
    };
    const char *queries[] = {
        "\x1b]5522;type=read;YXBwbGljYXRpb24vYSBhcHBsaWNhdGlvbi9iIGFwcGxpY2F0aW9uL2MgYXBwbGljYXRpb24vZA==\x1b\\",
        "\x1b]5522;type=read;YXBwbGljYXRpb24vYSBhcHBsaWNhdGlvbi9hIGFwcGxpY2F0aW9uL2EgYXBwbGljYXRpb24vYQ==\x1b\\",
    };
    for (size_t shape = 0; shape < 2; shape++) {
        if (shape) parts[0].data.len = 1024 * 1024;
        response.contents = parts; response.contents_len = shape ? 1 : 4;
        deferred = false; size = 0; feed(t, queries[shape]); size_t whole = size; memcpy(expected, output, size);
        deferred = true; size = 0; size_t baseline = allocated; feed(t, queries[shape]);
        assert(ghostty_terminal_clipboard_read_complete(t, pending, &response) == 0);
        assert(allocated - baseline <= 4 * 1024 * 1024 + 16384);
        bool done = false;
        while (!done) assert(ghostty_terminal_clipboard_read_step(t, pending, &done) == 0);
        assert(size == whole && memcmp(output, expected, size) == 0);
    }
    response.contents = &content; response.contents_len = 1;
    // Both request and completed reply storage are owned independently of callers.
    deferred = true; size = 0; char query[] = "\x1b]5522;type=read:id=original;dGV4dC9wbGFpbg==\x1b\\\x1b[5n";
    GhosttyTerminalWriteStop stop = GHOSTTY_INIT_SIZED(GhosttyTerminalWriteStop);
    assert(ghostty_terminal_vt_write_until(t, (uint8_t *)query, strlen(query), GHOSTTY_TERMINAL_BOUNDARY_OSC_END, &stop) == 0);
    assert(query[stop.consumed] == '\\' && size == 0);
    char tail[16]; strcpy(tail, query + stop.consumed); memset(query, 0, sizeof(query));
    content.data.len = 5; memcpy(data, "hello", 5);
    assert(ghostty_terminal_clipboard_read_complete(t, pending, &response) == 0); memset(data, 0, 5);
    bool done; assert(ghostty_terminal_clipboard_read_step(t, pending, &done) == 0 && done);
    assert(memmem(output, size, "id=original", 11) && memmem(output, size, "aGVsbG8=", 8));
    size_t before = size; feed(t, tail); assert(size > before);
    for (int status = 1; status <= 4; status++) {
        size = 0; feed(t, "\x1b]5522;type=read;dGV4dC9wbGFpbg==\x1b\\");
        GhosttyClipboardReadReply denial = GHOSTTY_INIT_SIZED(GhosttyClipboardReadReply); denial.result = status;
        assert(ghostty_terminal_clipboard_read_complete(t, pending, &denial) == 0 && size > 0);
    }
    // Incremental metadata limits agree with duplicate-key semantics.
    size_t old_calls = calls;
    char *large = malloc(20000); assert(large);
    strcpy(large, "\x1b]5522;type=write:type=read;"); size_t n = strlen(large); memset(large + n, 'A', 16000); strcpy(large + n + 16000, "\x1b\\");
    feed(t, large); assert(calls == old_calls); free(large);
    size = 0; feed(t, "\x1b]52;c;?\x07"); assert(ghostty_terminal_clipboard_read_discard(t, pending) == 0 && size == 0);
    feed(t, "\x1b]52;c;?\x07"); ghostty_terminal_free(t); assert(allocated == 0);
    ghostty_terminal_free(other); free(data);
    puts("PASS: native ownership, exact stepped encoding, stale IDs, concurrent terminals, failure statuses, metadata bounds, cancellation and allocator cleanup");
}
