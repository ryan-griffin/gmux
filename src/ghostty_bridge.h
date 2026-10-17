#ifndef GMUX_GHOSTTY_BRIDGE_H
#define GMUX_GHOSTTY_BRIDGE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// Source of truth for the Gmux C/Rust ABI. Keep upstream Ghostty types in the
// implementation; build.rs generates Rust declarations and layout checks here.
typedef struct {
    uint8_t text[64];
    uint8_t fg[3];
    uint8_t bg[3];
    uint8_t flags; // bold, italic, faint, underline, strike, invisible
} GmuxCell;

typedef struct {
    uint64_t total;
    uint64_t offset;
    uint64_t len;
} GmuxScrollback;

typedef struct {
    uint16_t cols;
    uint16_t rows;
    uint16_t cursor_x;
    uint16_t cursor_y;
    uint8_t cursor_visible;
    uint8_t cursor_style;
    uint8_t background[3];
    uint8_t foreground[3];
    uint8_t cursor[3];
    GmuxScrollback scrollback;
} GmuxFrame;

typedef struct GmuxEngine GmuxEngine;
typedef struct GmuxView GmuxView;
// Per-frontend rendering, scroll anchors, and selections over one terminal.
GmuxView *gmux_engine_view_new(GmuxEngine *engine);
void gmux_engine_view_free(GmuxView *view);
void gmux_engine_view_enter(GmuxEngine *engine, GmuxView *view);
void gmux_engine_view_leave(GmuxEngine *engine, GmuxView *view);
void gmux_engine_view_theme(GmuxView *view, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *palette, bool dark);
int gmux_engine_view_cursor_style(GmuxView *view, uint8_t style);
// The size owner supplies defaults for shared program queries and resets.
int gmux_engine_use_view_appearance(GmuxEngine *engine, GmuxView *view);
// Called synchronously by the owning engine. Data is borrowed for the call.
typedef void (*GmuxEventFn)(void *userdata, int kind, const uint8_t *data, size_t len);
// Called synchronously; true accepts all borrowed bytes, false aborts formatting.
typedef bool (*GmuxWriterFn)(void *userdata, const uint8_t *data, size_t len);

GmuxEngine *gmux_engine_new(void *userdata, GmuxEventFn callback);
void gmux_engine_free(GmuxEngine *engine);
// Serialized terminal state, distinct from the viewport snapshot below.
int gmux_engine_checkpoint(GmuxEngine *engine, uint8_t *out, size_t cap, size_t *written);
// Format primary text/rendition without terminal extras. Before replay, the
// caller must validate the complete output, including untrusted cell text.
int gmux_checkpoint_history(const uint8_t *data, size_t len, GmuxWriterFn write,
    void *userdata, uint16_t *cols, uint16_t *rows);
int gmux_engine_history_size(GmuxEngine *engine, uint16_t *cols, size_t *rows);
int gmux_engine_history_row(GmuxEngine *engine, uint32_t row, GmuxCell *cells,
    size_t cols, bool *wrapped);
int gmux_engine_select(GmuxEngine *engine, uint16_t sx, uint32_t sy, uint16_t ex, uint32_t ey);
void gmux_engine_clear_selection(GmuxEngine *engine);
// Clear primary screen/history at the next VT ground boundary. Reject alternate
// screens so full-screen applications retain their cursor and display state.
int gmux_engine_clear_screen(GmuxEngine *engine);
int gmux_engine_selection(GmuxEngine *engine, uint64_t *start, uint64_t *end);
int gmux_engine_copy(GmuxEngine *engine, uint8_t *bytes, size_t cap, size_t *written);
int gmux_engine_theme(GmuxEngine *engine, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *palette, bool dark);
void gmux_engine_write(GmuxEngine *engine, const uint8_t *data, size_t len);
int gmux_engine_cursor_style(GmuxEngine *engine, uint8_t style);
int gmux_engine_resize(GmuxEngine *engine, uint16_t cols, uint16_t rows, uint32_t cw, uint32_t ch);
int gmux_engine_snapshot(GmuxEngine *engine, GmuxFrame *frame, GmuxCell *cells, size_t cap);
void gmux_engine_scroll(GmuxEngine *engine, intptr_t delta, bool bottom);
void gmux_engine_scroll_to(GmuxEngine *engine, uint64_t row);
bool gmux_engine_expire_render_hold(GmuxEngine *engine);
size_t gmux_engine_string(GmuxEngine *engine, int pwd, uint8_t *out, size_t cap);
int gmux_engine_mode(GmuxEngine *engine, int mode);
int gmux_engine_paste(GmuxEngine *engine, const uint8_t *data, size_t len, bool clipboard);
size_t gmux_engine_key(GmuxEngine *engine, const char *name, uint32_t cp, uint16_t mods,
    bool release, const char *text, size_t len, uint8_t *out, size_t cap);
size_t gmux_engine_mouse(GmuxEngine *engine, float x, float y, uint32_t cw, uint32_t ch,
    int action, int button, uint16_t mods, bool pressed, uint8_t *out, size_t cap);

#endif
