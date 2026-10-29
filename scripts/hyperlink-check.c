// Inspect native ownership without adding production-only diagnostic APIs.
#include "../src/ghostty_bridge.c"
#include <assert.h>
#include <stdio.h>

static bool discard_cell(void *ud, size_t index, const GmuxCell *cell) {
    (void)ud; (void)index; (void)cell; return true;
}
static void event(void *ud, int kind, const uint8_t *data, size_t len) {
    (void)ud; (void)kind; (void)data; (void)len;
}
static void write_text(GmuxEngine *e, const char *text) {
    gmux_engine_write(e, (const uint8_t *)text, strlen(text));
}
static void snapshot(GmuxEngine *e) {
    GmuxFrame frame={0};

    uint8_t flags[24];
    assert(gmux_engine_snapshot(e,&frame,discard_cell,NULL,80*24,flags)>=0);
}
static size_t storage(const Links *links) {
    return links->spans_cap*sizeof(LinkSpan)+links->targets_cap*sizeof(LinkTarget)+links->bytes_cap;
}
int main(void) {
    GmuxEngine *e=gmux_engine_new(NULL,event);
    assert(e);
    GmuxView *a=gmux_engine_view_new(e), *b=gmux_engine_view_new(e);
    assert(a && b);
    write_text(e,"\x1b]8;;test:held\x07visible\x1b]8;;\x07");
    snapshot(e);
    write_text(e,"\x1b[?2026h\x1b[H\x1b[2Jplain");
    assert(storage(&e->held_links) && storage(&a->links) && storage(&b->links));
    snapshot(e);
    assert(e->held_links.spans_len == 1);
    e->hold_started_ms=now_ms()-1001;
    assert(gmux_engine_expire_render_hold(e));
    assert(!storage(&e->held_links) && !storage(&a->links) && !storage(&b->links));
    snapshot(e);
    assert(!storage(&e->links) && !storage(&e->link_scratch.spare) && !e->link_scratch.cap);
    // Explicit release and host clear share the same cleanup, even without a snapshot.
    for (int clear=0; clear<2; clear++) {
        write_text(e,"\x1b[H\x1b]8;;test:next\x07visible\x1b]8;;\x07\x1b[?2026h");
        assert(storage(&e->held_links));
        if (clear) assert(!gmux_engine_clear_screen(e));
        else write_text(e,"\x1b[?2026l");
        assert(!storage(&e->held_links) && !storage(&a->links) && !storage(&b->links));
    }
    gmux_engine_view_free(a); gmux_engine_view_free(b); gmux_engine_free(e);
    puts("PASS: held OSC 8 targets release on expiry, explicit release and clear");
}
