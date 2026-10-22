// Exercise our normalized-request policy and synchronous reply contract.
#include "../src/ghostty_bridge.c"
#include <assert.h>
#include <stdio.h>

static int calls, replies, admission = GMUX_CLIPBOARD_SUCCESS;
static bool cleared;
static uint8_t destination;
static GhosttyClipboardWriteResult result;
static void event(void *ud, int kind, const uint8_t *data, size_t len) {
    (void)ud; (void)kind; (void)data; (void)len;
}
static int accept(void *ud, uint8_t location, const uint8_t *data, size_t len) {
    (void)ud;
    assert(len == 0 || (len == 4 && !memcmp(data, "text", 4)));
    calls++;
    cleared = !data;
    destination = location;
    return admission;
}
static void reply(const GhosttyClipboardWrite *write, const GhosttyClipboardWriteReply *answer) {
    (void)write;
    assert(answer->size == sizeof(*answer) && !answer->remember);
    replies++;
    result = answer->result;
}
static void request(GmuxEngine *engine, GhosttyClipboardWrite *write,
    GhosttyClipboardWriteResult expected, int accepted) {
    calls = replies = 0;
    clipboard_write(engine->terminal, engine, write);
    assert(replies == 1 && result == expected && calls == accepted);
}
int main(void) {
    GmuxEngine *engine = gmux_engine_new(NULL, event);
    assert(engine);
    gmux_engine_clipboard_callback(engine, accept);
    size_t max = 0;
    assert(!ghostty_terminal_get(engine->terminal, GHOSTTY_TERMINAL_DATA_CLIPBOARD_WRITE_MAX_BYTES, &max));
    assert(max == GMUX_CLIPBOARD_MAX_BYTES);
    GhosttyClipboardContent contents[] = {
        {.mime = {.ptr = (const uint8_t *)"text/plain", .len = 10},
         .data = {.ptr = (const uint8_t *)"text", .len = 4}},
        {.mime = {.ptr = (const uint8_t *)"text/html", .len = 9},
         .data = {.ptr = (const uint8_t *)"text", .len = 4}},
    };
    GhosttyClipboardWrite write = {
        .size = sizeof(write), .location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
        .contents = contents, .contents_len = 1, .reply = reply,
    };
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS, 1);
    assert(!cleared && destination == GMUX_CLIPBOARD_STANDARD);
    write.location = GHOSTTY_CLIPBOARD_LOCATION_PRIMARY;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS, 1);
    assert(destination == GMUX_CLIPBOARD_PRIMARY);
    write.location = GHOSTTY_CLIPBOARD_LOCATION_SELECTION;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED, 0);
    write.location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD;
    write.contents_len = 2;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED, 0);
    write.contents = contents + 1;
    write.contents_len = 1;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED, 0);
    write.contents = contents;
    contents[0].data.len = GMUX_CLIPBOARD_MAX_BYTES + 1;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA, 0);
    contents[0].data.len = 0;
    contents[0].data.ptr = NULL;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS, 1);
    assert(!cleared);
    write.contents_len = 0;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS, 1);
    assert(cleared);
    admission = GMUX_CLIPBOARD_BUSY;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_BUSY, 1);
    admission = GMUX_CLIPBOARD_INVALID;
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA, 1);
    gmux_engine_clipboard_callback(engine, NULL);
    request(engine, &write, GHOSTTY_CLIPBOARD_WRITE_RESULT_BUSY, 0);
    calls = replies = 0;
    write.size = offsetof(GhosttyClipboardWrite, reply);
    clipboard_write(engine->terminal, engine, &write);
    assert(!calls && !replies); // Unknown layouts follow the library's default denial.
    gmux_engine_free(engine);
    puts("PASS: atomic text policy, destinations, clear/empty, limits, synchronous replies and unavailable recipient");
}
