#define _POSIX_C_SOURCE 200809L
#define GMUX_VERSION "benchmark"
// Count formatter construction separately from ingestion and terminal setup.
#ifndef GMUX_BRIDGE_SOURCE
#define GMUX_BRIDGE_SOURCE "../src/ghostty_bridge.c"
#endif
#include GMUX_BRIDGE_SOURCE
#include <assert.h>
#include <stdio.h>
#include <inttypes.h>

static size_t creations;
GhosttyResult __real_ghostty_formatter_terminal_new(const GhosttyAllocator *, GhosttyFormatter *, GhosttyTerminal);
GhosttyResult __wrap_ghostty_formatter_terminal_new(const GhosttyAllocator *a, GhosttyFormatter *f, GhosttyTerminal t) {
    creations++;
    return __real_ghostty_formatter_terminal_new(a, f, t);
}
typedef struct { size_t bytes; uint64_t hash; } Output;
static bool write_bytes(void *ud, const uint8_t *data, size_t len) {
    Output *out=ud;
    if (len > GMUX_CLIPBOARD_MAX_BYTES-out->bytes) return false;
    out->bytes+=len;
    for (size_t i=0;i<len;i++) out->hash=(out->hash^data[i])*UINT64_C(1099511628211);
    return true;
}
static Output copy(GmuxEngine *e, bool html) {
    Output out={.hash=UINT64_C(14695981039346656037)};
    GmuxCopyColors colors;
    assert(!gmux_engine_copy(e,html,write_bytes,&out,html ? &colors : NULL));
    return out;
}
static double cpu(void) {
    struct timespec t; clock_gettime(CLOCK_PROCESS_CPUTIME_ID,&t);
    return t.tv_sec+t.tv_nsec/1e9;
}
int main(int argc,char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s HTML RECTANGLE (each 0 or 1)\n", argv[0]);
        return 2;
    }
    bool html=atoi(argv[1])!=0, rectangle=atoi(argv[2])!=0;
    GmuxEngine *e=gmux_engine_new(NULL,NULL);
    assert(e && !gmux_engine_resize(e,80,24,8,16));
    uint8_t bg[3]={20,20,20}, fg[3]={240,240,240}, palette[48]={0};
    assert(!gmux_engine_theme(e,bg,fg,fg,palette,true));
    GmuxRetentionLimits limits={.max_bytes={.unlimited=true},.max_lines={.unlimited=true}};
    assert(!gmux_engine_retention(e,&limits));
    const char row[]="\x1b[31mred\x1b[0m\x1b[8C\x1b[44mblue\x1b[0m\x1b[8Cend\r\n";
    for (size_t i=0;i<64;i++) gmux_engine_write(e,(const uint8_t *)row,sizeof(row)-1);
    assert(!gmux_engine_select(e,0,0,39,63,rectangle?3:0));
    Output expected=copy(e,html);
    creations=0;
    size_t iterations=100;
    double begin=cpu();
    for (size_t i=0;i<iterations;i++) {
        Output out=copy(e,html);
        assert(out.bytes==expected.bytes && out.hash==expected.hash);
    }
    double us=(cpu()-begin)*1e6/iterations;
    printf("{\"html\":%s,\"rectangle\":%s,\"rows\":64,\"copy_us\":%.3f,\"formatter_creations_per_copy\":%zu,\"output_bytes\":%zu,\"output_hash\":\"%016" PRIx64 "\"}\n",html?"true":"false",rectangle?"true":"false",us,creations/iterations,expected.bytes,expected.hash);
    gmux_engine_free(e);
}
