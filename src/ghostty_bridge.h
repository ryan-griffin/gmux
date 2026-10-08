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
// Called synchronously by the owning engine. Data is borrowed for the call.
typedef void (*GmuxEventFn)(void *userdata, int kind, const uint8_t *data, size_t len);

GmuxEngine *gmux_engine_new(void *userdata, GmuxEventFn callback);
void gmux_engine_free(GmuxEngine *engine);
int gmux_engine_history_size(GmuxEngine *engine, uint16_t *cols, size_t *rows);
int gmux_engine_history_row(GmuxEngine *engine, uint32_t row, GmuxCell *cells,
    size_t cols, bool *wrapped);
int gmux_engine_select(GmuxEngine *engine, uint16_t sx, uint32_t sy, uint16_t ex, uint32_t ey);
void gmux_engine_clear_selection(GmuxEngine *engine);
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
