// Measure native metadata retained before the normalized write callback runs.
#include "../src/ghostty_bridge.c"
#include <assert.h>
#include <stdio.h>

static size_t admissions;
static void event(void *userdata, int kind, const uint8_t *data, size_t len) {
    (void)userdata; (void)kind; (void)data; (void)len;
}
static int accept(void *userdata, uint8_t destination,
    const GmuxClipboardContent *contents, size_t count) {
    (void)userdata; (void)destination;
    assert(count == 2);
    for (size_t i = 0; i < count; i++) {
        assert(contents[i].data.len == 1 && contents[i].data.ptr[0] == 'x');
    }
    admissions++;
    return GMUX_CLIPBOARD_SUCCESS;
}
static void send_sequence(GmuxEngine *engine, const char *sequence) {
    gmux_engine_write(engine, (const uint8_t *)sequence, strlen(sequence));
}
static void report(const char *phase, size_t aliases) {
    FILE *status = fopen("/proc/self/status", "r");
    assert(status);
    char line[256];
    while (fgets(line, sizeof(line), status)) {
        if (!strncmp(line, "VmRSS:", 6)) {
            printf("%s aliases=%zu admissions=%zu %s", phase, aliases, admissions, line);
        }
    }
    fclose(status);
}
int main(int argc, char **argv) {
    size_t aliases = argc > 1 ? strtoul(argv[1], NULL, 10) : 1000000;
    assert(aliases > 0 && aliases <= 10000000);
    GmuxEngine *engine = gmux_engine_new(NULL, event);
    assert(engine);
    gmux_engine_clipboard_callback(engine, accept);
    send_sequence(engine, "\x1b]5522;type=write\x1b\\"
        "\x1b]5522;type=wdata:mime=dGV4dC9odG1s;eA==\x1b\\");
    report("begin", 0);
    for (size_t i = 0; i < aliases; i++) {
        send_sequence(engine,
            "\x1b]5522;type=walias:mime=dGV4dC9odG1s;dGV4dC9wbGFpbg==\x1b\\");
        assert(!admissions);
    }
    report("uncommitted", aliases);
    send_sequence(engine, "\x1b]5522;type=wdata\x1b\\");
    assert(admissions == 1);
    report("committed", aliases);
    gmux_engine_free(engine);
}
