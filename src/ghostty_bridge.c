#define _POSIX_C_SOURCE 200809L
#include <ghostty/vt.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Keep the moving libghostty ABI on this side of the Rust boundary.
typedef struct {
    char text[64];
    uint8_t fg[3], bg[3];
    uint8_t flags; // bold, italic, faint, underline, strike, invisible
} GmuxCell;
typedef struct {
    uint64_t total, offset, len;
} GmuxScrollback;
typedef struct {
    uint16_t cols, rows, cursor_x, cursor_y;
    uint8_t cursor_visible, cursor_style, background[3], foreground[3], cursor[3];
    GmuxScrollback scrollback;
} GmuxFrame;
typedef void (*GmuxEventFn)(void *, int, const uint8_t *, size_t);
typedef struct {
    GhosttyTerminal terminal;
    GhosttyRenderState render;
    GhosttyTerminalScrollbar scrollbar;
    GhosttyRenderStateRowIterator iterator;
    GhosttyRenderStateRowCells cells;
    GhosttyKeyEncoder encoder;
    GhosttyKeyEvent key;
    GhosttyMouseEncoder mouse_encoder;
    GhosttyMouseEvent mouse;
    GmuxEventFn callback;
    void *userdata;
    bool dark;
    bool held;
    GhosttyTrackedGridRef selection_start, selection_end;
    uint64_t hold_started_ms;
} GmuxEngine;

static uint64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static void render_hold(GhosttyTerminal terminal, void *ud, bool held) {
    GmuxEngine *e = ud;
    if (held) {
        // Capture at the exact start of the hold, even if the same PTY chunk
        // contains the next frame. Keep it visible until that frame is complete.
        if (ghostty_render_state_update(e->render, terminal)) return;
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &e->scrollbar);
        e->hold_started_ms = now_ms();
    }
    e->held = held;
}

bool gmux_engine_expire_render_hold(GmuxEngine *e) {
    if (!e->held || now_ms() - e->hold_started_ms < 1000) return false;
    GhosttyTerminalModeConfig mode = {.mode = GHOSTTY_MODE_SYNC_OUTPUT, .value = false};
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_MODE, &mode);
    e->held = false;
    return true;
}

static void write_pty(GhosttyTerminal t, void *ud, const uint8_t *data, size_t len) {
    (void)t;
    GmuxEngine *e = ud;
    e->callback(e->userdata, 0, data, len);
}
static void bell(GhosttyTerminal t, void *ud) {
    (void)t;
    GmuxEngine *e = ud;
    const char *message = "Terminal bell";
    e->callback(e->userdata, 1, (const uint8_t *)message, strlen(message));
}
static void title_changed(GhosttyTerminal t, void *ud) {
    (void)t;
    GmuxEngine *e = ud;
    // The new title is available after this callback returns.
    e->callback(e->userdata, 2, NULL, 0);
}
static void pwd_changed(GhosttyTerminal t, void *ud) {
    (void)t;
    GmuxEngine *e = ud;
    e->callback(e->userdata, 3, NULL, 0);
}
static void notify(GhosttyTerminal t, void *ud, const GhosttyTerminalDesktopNotification *n) {
    (void)t;
    GmuxEngine *e = ud;
    GhosttyString s = n->body.len ? n->body : n->title;
    e->callback(e->userdata, 1, (const uint8_t *)s.ptr, s.len);
}
static bool color_scheme(GhosttyTerminal t, void *ud, GhosttyColorScheme *out) {
    (void)t;
    GmuxEngine *e = ud;
    *out = e->dark ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT;
    return true;
}
void gmux_engine_free(GmuxEngine *e) {
    if (!e) return;
    ghostty_tracked_grid_ref_free(e->selection_start);
    ghostty_tracked_grid_ref_free(e->selection_end);
    ghostty_key_event_free(e->key);
    ghostty_mouse_event_free(e->mouse);
    ghostty_mouse_encoder_free(e->mouse_encoder);
    ghostty_key_encoder_free(e->encoder);
    ghostty_render_state_row_cells_free(e->cells);
    ghostty_render_state_row_iterator_free(e->iterator);
    ghostty_render_state_free(e->render);
    ghostty_terminal_free(e->terminal);
    free(e);
}
GmuxEngine *gmux_engine_new(void *ud, GmuxEventFn cb) {
    GmuxEngine *e = calloc(1, sizeof(*e));
    if (!e) return NULL;
    e->userdata = ud;
    e->callback = cb;
    if (ghostty_terminal_new(NULL, &e->terminal, 80, 24) ||
        ghostty_render_state_new(NULL, &e->render) ||
        ghostty_render_state_row_iterator_new(NULL, &e->iterator) ||
        ghostty_render_state_row_cells_new(NULL, &e->cells) ||
        ghostty_key_encoder_new(NULL, &e->encoder) ||
        ghostty_key_event_new(NULL, &e->key) ||
        ghostty_mouse_encoder_new(NULL, &e->mouse_encoder) ||
        ghostty_mouse_event_new(NULL, &e->mouse)) {
        gmux_engine_free(e); return NULL;
    }
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_USERDATA, e);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_WRITE_PTY, write_pty);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_BELL, bell);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, title_changed);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_PWD_CHANGED, pwd_changed);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, notify);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, color_scheme);
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, render_hold);
    return e;
}
int gmux_engine_theme(GmuxEngine *e, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *ansi, bool dark) {
    GhosttyColorRgb bg, fg, cursor_color, palette[256];
    memcpy(&bg, background, 3);
    memcpy(&fg, foreground, 3);
    memcpy(&cursor_color, cursor, 3);
    GhosttyResult result = ghostty_terminal_get(e->terminal,
        GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT, palette);
    if (result) return result;
    // Retain the standard extended palette and any program's OSC overrides.
    memcpy(palette, ansi, 16 * sizeof(GhosttyColorRgb));
    if ((result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg)) ||
        (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg)) ||
        (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor_color)) ||
        (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, palette)))
        return result;
    bool changed = e->dark != dark;
    e->dark = dark;
    GhosttyTerminalModeConfig mode = {.mode = ghostty_mode_new(2031, false)};
    if (changed && !ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MODE, &mode) && mode.value) {
        char report[32];
        size_t len = 0;
        if (!ghostty_color_scheme_report_encode(dark ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT,
            report, sizeof(report), &len))
            e->callback(e->userdata, 0, (const uint8_t *)report, len);
    }
    return 0;
}
void gmux_engine_write(GmuxEngine *e, const uint8_t *data, size_t len) {
    ghostty_terminal_vt_write(e->terminal, data, len);
}
int gmux_engine_cursor_style(GmuxEngine *e, uint8_t style) {
    if (style > GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK_HOLLOW) return GHOSTTY_INVALID_VALUE;
    GhosttyTerminalCursorStyle value = (GhosttyTerminalCursorStyle)style;
    return ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE, &value);
}
int gmux_engine_resize(GmuxEngine *e, uint16_t cols, uint16_t rows, uint32_t cw, uint32_t ch) {
    return ghostty_terminal_resize(e->terminal, cols, rows, cw, ch);
}
void gmux_engine_scroll(GmuxEngine *e, intptr_t delta, bool bottom) {
    GhosttyTerminalScrollViewport scroll = {
        .tag = bottom ? GHOSTTY_SCROLL_VIEWPORT_BOTTOM : GHOSTTY_SCROLL_VIEWPORT_DELTA,
        .value = {.delta = delta}
    };
    ghostty_terminal_scroll_viewport(e->terminal, scroll);
}
void gmux_engine_scroll_to(GmuxEngine *e, uint64_t row) {
    GhosttyTerminalScrollViewport scroll = {
        .tag = GHOSTTY_SCROLL_VIEWPORT_ROW,
        .value = {.row = row > SIZE_MAX ? SIZE_MAX : (size_t)row}
    };
    ghostty_terminal_scroll_viewport(e->terminal, scroll);
}
size_t gmux_engine_string(GmuxEngine *e, int pwd, uint8_t *out, size_t cap) {
    GhosttyString s = {0};
    ghostty_terminal_get(e->terminal, pwd ? GHOSTTY_TERMINAL_DATA_PWD : GHOSTTY_TERMINAL_DATA_TITLE, &s);
    size_t len = s.len < cap ? s.len : cap;
    if (len) memcpy(out, s.ptr, len);
    return len;
}
int gmux_engine_mode(GmuxEngine *e, int mode) {
    if (!mode) {
        bool value = false;
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &value);
        return value;
    }
    GhosttyTerminalModeConfig m = {.mode = ghostty_mode_new((uint16_t)mode, false)};
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MODE, &m)) return 0;
    return m.value;
}
static bool read_paste(void *ud, GhosttyString mime, GhosttyWriter writer) {
    (void)mime;
    const GhosttyString *text = ud;
    return !text->len || writer.write(writer.userdata, (const uint8_t *)text->ptr, text->len);
}
int gmux_engine_paste(GmuxEngine *e, const uint8_t *data, size_t len, bool clipboard) {
    GhosttyString text = {.ptr = data, .len = len};
    GhosttyString mime = {.ptr = (const uint8_t *)"text/plain", .len = 10};
    GhosttyPaste paste = GHOSTTY_INIT_SIZED(GhosttyPaste);
    paste.location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD;
    paste.source = clipboard ? GHOSTTY_PASTE_SOURCE_CLIPBOARD : GHOSTTY_PASTE_SOURCE_TEXT;
    paste.mimes = &mime;
    paste.mimes_len = 1;
    paste.reader = (GhosttyMimeReader){.read = read_paste, .userdata = &text};
    // Preserve explicit paste/send behavior, including multi-line input.
    // Ghostty still encodes newlines and strips control bytes for the active mode.
    paste.allow_unsafe = true;
    return ghostty_terminal_paste(e->terminal, &paste, NULL);
}
int gmux_engine_snapshot(GmuxEngine *e, GmuxFrame *frame, GmuxCell *out, size_t cap) {
    gmux_engine_expire_render_hold(e);
    if (!e->held && ghostty_render_state_update(e->render, e->terminal)) return -1;
    if (!e->held && ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &e->scrollbar)) return -1;
    frame->scrollback = (GmuxScrollback){e->scrollbar.total, e->scrollbar.offset, e->scrollbar.len};
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_COLS, &frame->cols);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_ROWS, &frame->rows);
    if ((size_t)frame->cols * frame->rows > cap) return -1;
    GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    GhosttyRenderStateCursor cursor = GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_CURSOR, &cursor);
    memcpy(frame->background, &colors.background, 3);
    memcpy(frame->foreground, &colors.foreground, 3);
    memcpy(frame->cursor, colors.cursor_has_value ? &colors.cursor : &colors.foreground, 3);
    frame->cursor_x = cursor.viewport_x;
    frame->cursor_y = cursor.viewport_y;
    frame->cursor_visible = cursor.visible && cursor.viewport_has_value;
    frame->cursor_style = cursor.visual_style;
    GhosttyRenderStateDirty dirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_DIRTY, &dirty);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &e->iterator);
    uint16_t y;
    // Rust keeps the cell buffer between snapshots. Only changed rows need
    // extraction; resize, scrolling and palette changes invalidate the full grid.
    while (ghostty_render_state_row_iterator_next_dirty(e->iterator, &y)) {
        ghostty_render_state_row_get(e->iterator, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &e->cells);
        size_t x = 0;
        while (ghostty_render_state_row_cells_next(e->cells)) {
            if (x >= frame->cols || y >= frame->rows) return -1;
            GmuxCell *cell = &out[y * frame->cols + x++];
            memset(cell, 0, sizeof(*cell));
            GhosttyColorRgb fg = colors.foreground, bg = colors.background;
            ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &fg);
            ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &bg);
            GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
            ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &style);
            if (style.inverse) { GhosttyColorRgb swap = fg; fg = bg; bg = swap; }
            memcpy(cell->fg, &fg, 3); memcpy(cell->bg, &bg, 3);
            cell->flags = style.bold | (style.italic << 1) | (style.faint << 2) |
                ((style.underline != 0) << 3) | (style.strikethrough << 4) | (style.invisible << 5);
            GhosttyCell raw;
            GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
            ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw);
            ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide);
            if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL) cell->flags |= 64;
            GhosttyBuffer buffer = {.ptr = (uint8_t *)cell->text, .cap = sizeof(cell->text) - 1};
            // A cluster too large for this cell is shown as a replacement character.
            if (ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8, &buffer))
                memcpy(cell->text, "\xef\xbf\xbd", 3);
        }
    }
    ghostty_render_state_clean(e->render);
    return dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE;
}
static GhosttyKey key_for(uint32_t cp, const char *name) {
    // Keypad keys must stay distinct even when they produce the same Unicode
    // character as a writing-system key. Application keypad mode relies on it.
    #define KEY(n, k) if (!strcmp(name, n)) return GHOSTTY_KEY_##k
    KEY("KP_0", NUMPAD_0); KEY("KP_1", NUMPAD_1); KEY("KP_2", NUMPAD_2);
    KEY("KP_3", NUMPAD_3); KEY("KP_4", NUMPAD_4); KEY("KP_5", NUMPAD_5);
    KEY("KP_6", NUMPAD_6); KEY("KP_7", NUMPAD_7); KEY("KP_8", NUMPAD_8);
    KEY("KP_9", NUMPAD_9); KEY("KP_Add", NUMPAD_ADD); KEY("KP_Subtract", NUMPAD_SUBTRACT);
    KEY("KP_Multiply", NUMPAD_MULTIPLY); KEY("KP_Divide", NUMPAD_DIVIDE);
    KEY("KP_Decimal", NUMPAD_DECIMAL); KEY("KP_Separator", NUMPAD_SEPARATOR);
    KEY("KP_Equal", NUMPAD_EQUAL); KEY("KP_Enter", NUMPAD_ENTER);
    KEY("KP_Up", NUMPAD_UP); KEY("KP_Down", NUMPAD_DOWN);
    KEY("KP_Left", NUMPAD_LEFT); KEY("KP_Right", NUMPAD_RIGHT);
    KEY("KP_Home", NUMPAD_HOME); KEY("KP_End", NUMPAD_END);
    KEY("KP_Page_Up", NUMPAD_PAGE_UP); KEY("KP_Page_Down", NUMPAD_PAGE_DOWN);
    KEY("KP_Insert", NUMPAD_INSERT); KEY("KP_Delete", NUMPAD_DELETE);
    KEY("KP_Begin", NUMPAD_BEGIN);
    if (cp >= 'A' && cp <= 'Z') cp += 'a' - 'A';
    if (cp >= 'a' && cp <= 'z') return GHOSTTY_KEY_A + cp - 'a';
    if (cp >= '0' && cp <= '9') return GHOSTTY_KEY_DIGIT_0 + cp - '0';
    switch (cp) {
        case ' ': return GHOSTTY_KEY_SPACE;
        case '-': case '_': return GHOSTTY_KEY_MINUS;
        case '=': case '+': return GHOSTTY_KEY_EQUAL;
        case '[': case '{': return GHOSTTY_KEY_BRACKET_LEFT;
        case ']': case '}': return GHOSTTY_KEY_BRACKET_RIGHT;
        case '\\': case '|': return GHOSTTY_KEY_BACKSLASH;
        case ';': case ':': return GHOSTTY_KEY_SEMICOLON;
        case '\'': case '"': return GHOSTTY_KEY_QUOTE;
        case ',': case '<': return GHOSTTY_KEY_COMMA;
        case '.': case '>': return GHOSTTY_KEY_PERIOD;
        case '/': case '?': return GHOSTTY_KEY_SLASH;
        case '`': case '~': return GHOSTTY_KEY_BACKQUOTE;
    }
    KEY("Return", ENTER); KEY("BackSpace", BACKSPACE);
    KEY("Tab", TAB); KEY("ISO_Left_Tab", TAB); KEY("Escape", ESCAPE);
    KEY("Up", ARROW_UP); KEY("Down", ARROW_DOWN); KEY("Left", ARROW_LEFT); KEY("Right", ARROW_RIGHT);
    KEY("Home", HOME); KEY("End", END); KEY("Insert", INSERT); KEY("Delete", DELETE);
    KEY("Page_Up", PAGE_UP); KEY("Page_Down", PAGE_DOWN);
    #undef KEY
    if (name[0] == 'F') { int n = atoi(name + 1); if (n >= 1 && n <= 25) return GHOSTTY_KEY_F1 + n - 1; }
    return GHOSTTY_KEY_UNIDENTIFIED;
}
size_t gmux_engine_key(GmuxEngine *e, const char *name, uint32_t cp, uint16_t mods,
    bool release, const char *text, size_t len, uint8_t *out, size_t cap) {
    ghostty_key_encoder_setopt_from_terminal(e->encoder, e->terminal);
    ghostty_key_event_set_action(e->key, release ? GHOSTTY_KEY_ACTION_RELEASE : GHOSTTY_KEY_ACTION_PRESS);
    ghostty_key_event_set_key(e->key, key_for(cp, name));
    ghostty_key_event_set_mods(e->key, mods);
    ghostty_key_event_set_consumed_mods(e->key, len ? mods & GHOSTTY_MODS_SHIFT : 0);
    ghostty_key_event_set_utf8(e->key, text, len);
    ghostty_key_event_set_unshifted_codepoint(e->key, cp >= 'A' && cp <= 'Z' ? cp + 32 : cp);
    size_t written = 0;
    if (ghostty_key_encoder_encode(e->encoder, e->key, (char *)out, cap, &written)) return 0;
    return written;
}
size_t gmux_engine_mouse(GmuxEngine *e, float x, float y, uint32_t cw, uint32_t ch,
    int action, int button, uint16_t mods, bool pressed, uint8_t *out, size_t cap) {
    ghostty_mouse_encoder_setopt_from_terminal(e->mouse_encoder, e->terminal);
    uint16_t cols = 80, rows = 24;
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols);
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ROWS, &rows);
    GhosttyMouseEncoderSize size = GHOSTTY_INIT_SIZED(GhosttyMouseEncoderSize);
    size.cell_width = cw; size.cell_height = ch;
    size.screen_width = cw * cols; size.screen_height = ch * rows;
    ghostty_mouse_encoder_setopt(e->mouse_encoder, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size);
    ghostty_mouse_encoder_setopt(e->mouse_encoder, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &pressed);
    ghostty_mouse_event_set_position(e->mouse, (GhosttyMousePosition){x, y});
    ghostty_mouse_event_set_action(e->mouse, (GhosttyMouseAction)action);
    if (button) ghostty_mouse_event_set_button(e->mouse, (GhosttyMouseButton)button);
    else ghostty_mouse_event_clear_button(e->mouse);
    ghostty_mouse_event_set_mods(e->mouse, mods);
    size_t written = 0;
    if (ghostty_mouse_encoder_encode(e->mouse_encoder, e->mouse, (char *)out, cap, &written)) return 0;
    return written;
}

int gmux_engine_history_size(GmuxEngine *e, uint16_t *cols, size_t *rows) {
    return ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, cols) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, rows);
}
static size_t utf8(uint32_t cp, uint8_t *out) {
    if (cp <= 0x7f) { out[0] = cp; return 1; }
    if (cp <= 0x7ff) { out[0] = 0xc0 | (cp >> 6); out[1] = 0x80 | (cp & 63); return 2; }
    if (cp <= 0xffff) { out[0] = 0xe0 | (cp >> 12); out[1] = 0x80 | ((cp >> 6)&63); out[2] = 0x80 | (cp&63); return 3; }
    out[0] = 0xf0 | (cp >> 18); out[1] = 0x80 | ((cp >> 12)&63); out[2] = 0x80 | ((cp >> 6)&63); out[3] = 0x80 | (cp&63); return 4;
}
int gmux_engine_history_row(GmuxEngine *e, uint32_t y, GmuxCell *out, size_t cols, bool *wrapped) {
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = {.x=0, .y=y}};
    if (ghostty_terminal_grid_ref(e->terminal, point, &ref)) return -1;
    GhosttyRow row;
    if (ghostty_grid_ref_row(&ref, &row) || ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, wrapped)) return -1;
    memset(out, 0, sizeof(GmuxCell)*cols);
    for (size_t x=0; x<cols; x++) {
        ref.x = x;
        GhosttyCell raw;
        GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
        if (ghostty_grid_ref_cell(&ref, &raw)) return -1;
        ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide);
        if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL) out[x].flags = 64;
        if (wide == GHOSTTY_CELL_WIDE_SPACER_HEAD) out[x].flags = 128;
        uint32_t cps[64]; size_t count = 0, len = 0;
        if (ghostty_grid_ref_graphemes(&ref, cps, 64, &count)) {
            memcpy(out[x].text, "\xef\xbf\xbd", 3); continue;
        }
        for (size_t i=0; i<count; i++) {
            uint8_t bytes[4]; size_t n = utf8(cps[i], bytes);
            if (len+n >= sizeof(out[x].text)) { memcpy(out[x].text, "\xef\xbf\xbd", 3); out[x].text[3]=0; break; }
            memcpy(out[x].text+len, bytes, n); len += n;
        }
    }
    return 0;
}
int gmux_engine_select(GmuxEngine *e, uint16_t sx, uint32_t sy, uint16_t ex, uint32_t ey) {
    GhosttyPoint start = {.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={sx,sy}};
    GhosttyPoint end = {.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={ex,ey}};
    if (!e->selection_start && ghostty_terminal_grid_ref_track(e->terminal, start, &e->selection_start)) return -1;
    if (!e->selection_end && ghostty_terminal_grid_ref_track(e->terminal, end, &e->selection_end)) return -1;
    return ghostty_tracked_grid_ref_set(e->selection_end, e->terminal, end);
}
void gmux_engine_clear_selection(GmuxEngine *e) {
    ghostty_tracked_grid_ref_free(e->selection_start); e->selection_start=NULL;
    ghostty_tracked_grid_ref_free(e->selection_end); e->selection_end=NULL;
}
int gmux_engine_selection(GmuxEngine *e, uint64_t *start, uint64_t *end) {
    if (!e->selection_start || !e->selection_end) return -1;
    GhosttyPointCoordinate a,b; uint16_t cols=0;
    if (ghostty_tracked_grid_ref_point(e->selection_start, GHOSTTY_POINT_TAG_SCREEN, &a) ||
        ghostty_tracked_grid_ref_point(e->selection_end, GHOSTTY_POINT_TAG_SCREEN, &b) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols)) return -1;
    *start = (uint64_t)a.y*cols+a.x; *end=(uint64_t)b.y*cols+b.x;
    if (*start > *end) { uint64_t swap=*start; *start=*end; *end=swap; }
    return 0;
}
int gmux_engine_copy(GmuxEngine *e, uint8_t *out, size_t cap, size_t *written) {
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    if (!e->selection_start || !e->selection_end || ghostty_tracked_grid_ref_snapshot(e->selection_start, &selection.start) ||
        ghostty_tracked_grid_ref_snapshot(e->selection_end, &selection.end)) { *written=0; return 0; }
    GhosttyTerminalSelectionFormatOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
    options.emit=GHOSTTY_FORMATTER_FORMAT_PLAIN; options.unwrap=true; options.trim=true; options.selection=&selection;
    GhosttyResult result=ghostty_terminal_selection_format_buf(e->terminal, options, out, cap, written);
    return result == GHOSTTY_OUT_OF_SPACE ? 1 : (result == GHOSTTY_SUCCESS ? 0 : -1);
}
