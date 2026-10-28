#ifndef GMUX_GHOSTTY_BRIDGE_H
#define GMUX_GHOSTTY_BRIDGE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define GMUX_KEY_ACTION_PRESS 0
#define GMUX_KEY_ACTION_REPEAT 1
#define GMUX_KEY_ACTION_RELEASE 2

#define GMUX_PROGRESS_INACTIVE 0
#define GMUX_PROGRESS_DETERMINATE 1
#define GMUX_PROGRESS_ERROR 2
#define GMUX_PROGRESS_INDETERMINATE 3
#define GMUX_PROGRESS_PAUSED 4

#define GMUX_COMPRESSION_UNSUPPORTED 0
#define GMUX_COMPRESSION_PENDING 1
#define GMUX_COMPRESSION_COMPLETE 2

#define GMUX_LINK_MAX_URI_BYTES (16u * 1024)
#define GMUX_LINK_MAX_TARGET_BYTES (256u * 1024)
#define GMUX_LINK_MAX_SPANS 16384u

#define GMUX_UNDERLINE_NONE 0
#define GMUX_UNDERLINE_SINGLE 1
#define GMUX_UNDERLINE_DOUBLE 2
#define GMUX_UNDERLINE_CURLY 3
#define GMUX_UNDERLINE_DOTTED 4
#define GMUX_UNDERLINE_DASHED 5

#define GMUX_MOUSE_SHAPE_DEFAULT 0
#define GMUX_MOUSE_SHAPE_CONTEXT_MENU 1
#define GMUX_MOUSE_SHAPE_HELP 2
#define GMUX_MOUSE_SHAPE_POINTER 3
#define GMUX_MOUSE_SHAPE_PROGRESS 4
#define GMUX_MOUSE_SHAPE_WAIT 5
#define GMUX_MOUSE_SHAPE_CELL 6
#define GMUX_MOUSE_SHAPE_CROSSHAIR 7
#define GMUX_MOUSE_SHAPE_TEXT 8
#define GMUX_MOUSE_SHAPE_VERTICAL_TEXT 9
#define GMUX_MOUSE_SHAPE_ALIAS 10
#define GMUX_MOUSE_SHAPE_COPY 11
#define GMUX_MOUSE_SHAPE_MOVE 12
#define GMUX_MOUSE_SHAPE_NO_DROP 13
#define GMUX_MOUSE_SHAPE_NOT_ALLOWED 14
#define GMUX_MOUSE_SHAPE_GRAB 15
#define GMUX_MOUSE_SHAPE_GRABBING 16
#define GMUX_MOUSE_SHAPE_ALL_SCROLL 17
#define GMUX_MOUSE_SHAPE_COL_RESIZE 18
#define GMUX_MOUSE_SHAPE_ROW_RESIZE 19
#define GMUX_MOUSE_SHAPE_N_RESIZE 20
#define GMUX_MOUSE_SHAPE_E_RESIZE 21
#define GMUX_MOUSE_SHAPE_S_RESIZE 22
#define GMUX_MOUSE_SHAPE_W_RESIZE 23
#define GMUX_MOUSE_SHAPE_NE_RESIZE 24
#define GMUX_MOUSE_SHAPE_NW_RESIZE 25
#define GMUX_MOUSE_SHAPE_SE_RESIZE 26
#define GMUX_MOUSE_SHAPE_SW_RESIZE 27
#define GMUX_MOUSE_SHAPE_EW_RESIZE 28
#define GMUX_MOUSE_SHAPE_NS_RESIZE 29
#define GMUX_MOUSE_SHAPE_NESW_RESIZE 30
#define GMUX_MOUSE_SHAPE_NWSE_RESIZE 31
#define GMUX_MOUSE_SHAPE_ZOOM_IN 32
#define GMUX_MOUSE_SHAPE_ZOOM_OUT 33

#define GMUX_DECORATION_UNDERLINE_MASK 7
#define GMUX_DECORATION_OVERLINE 8
#define GMUX_DECORATION_UNDERLINE_COLOR 16

// Source of truth for the Gmux C/Rust ABI. Keep upstream Ghostty types in the
// implementation; build.rs generates Rust declarations and layout checks here.
typedef struct {
    uint8_t text[64];
    uint8_t fg[3];
    uint8_t bg[3];
    uint8_t flags; // bold, italic, faint, reserved, strike, invisible, wide tail/head
    uint8_t decoration; // underline style (bits 0-2), overline (3), underline color (4)
    uint8_t underline_color[3];
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
    uint8_t cursor_blink;
    uint8_t mouse_shape; // GMUX_MOUSE_SHAPE_* protocol codes
    uint8_t background[3];
    uint8_t foreground[3];
    uint8_t cursor[3];
    GmuxScrollback scrollback;
} GmuxFrame;

typedef struct GmuxEngine GmuxEngine;
typedef struct GmuxView GmuxView;
// Page and image totals for both screens. Excludes bridge/render/client caches
// and terminal bookkeeping. Prompt counters separately report the bridge index.
// Reading these walks pages without decompressing.
typedef struct {
    bool compression_supported;
    uint64_t pages;
    uint64_t virtual_bytes;
    uint64_t resident_bytes;
    uint64_t compressed_pages;
    uint64_t compressed_bytes;
    uint64_t image_bytes;
    uint64_t prompt_anchors;
    uint64_t prompt_anchor_bytes;
} GmuxMemoryUsage;
// Stable optional size values. Zero is a finite limit, separate from unlimited.
typedef struct { size_t value; bool unlimited; } GmuxRetentionLimit;
typedef struct {
    GmuxRetentionLimit max_bytes, max_lines;
} GmuxRetentionLimits;
int gmux_engine_retention(GmuxEngine *engine, const GmuxRetentionLimits *limits);
int gmux_engine_memory_usage(GmuxEngine *engine, GmuxMemoryUsage *out);
int gmux_engine_compression_activity(GmuxEngine *engine, uint64_t *activity);
// One bounded incremental step, serialized with all other terminal access.
int gmux_engine_compress(GmuxEngine *engine, int *result);
typedef struct {
    uint64_t generation;
    uint64_t origin;
} GmuxHistory;
// Per-frontend rendering, scroll anchors, and selections over one terminal.
GmuxView *gmux_engine_view_new(GmuxEngine *engine);
void gmux_engine_view_free(GmuxView *view);
// Inactive subscribers retain presentation state without a frozen render cache.
// Call outside view_enter/view_leave, while the view owns its render cache.
void gmux_engine_view_set_active(GmuxView *view, bool active);
void gmux_engine_view_enter(GmuxEngine *engine, GmuxView *view);
void gmux_engine_view_leave(GmuxEngine *engine, GmuxView *view);
void gmux_engine_view_theme(GmuxView *view, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *palette, bool dark);
int gmux_engine_view_cursor_blink(GmuxView *view, bool blink);
int gmux_engine_cursor_blink(GmuxEngine *engine, bool blink);
int gmux_engine_view_cursor_style(GmuxView *view, uint8_t style);
// The size owner supplies defaults for shared program queries and resets.
int gmux_engine_use_view_appearance(GmuxEngine *engine, GmuxView *view);
// Called synchronously by the owning engine. Data is borrowed for the call.
typedef void (*GmuxEventFn)(void *userdata, int kind, const uint8_t *data, size_t len);
// Event kind 7. Stable codes, independent of upstream enum representation.
typedef struct { const uint8_t *ptr; size_t len; } GmuxText;
typedef struct {
    uint8_t state; // 0 idle, 1 working, 2 done, 3 blocked, 4 error, 5 clear
    uint8_t kind; // 0 absent, 1 permission, 2 question, 3 auth
    int8_t progress; // -1 absent, otherwise 0..100
    GmuxText id, app, title, message;
} GmuxProgramStatusEvent;
// Event kind 5. Strings are borrowed for the synchronous event callback.
typedef struct {
    uint8_t kind; // 1 prompt, 2 input, 3 output, 4 command end
    bool has_exit_code;
    int32_t exit_code;
    const uint8_t *command;
    size_t command_len;
} GmuxCommandEvent;
#define GMUX_CLIPBOARD_STANDARD 0
#define GMUX_CLIPBOARD_PRIMARY 1
#define GMUX_CLIPBOARD_SUCCESS 0
#define GMUX_CLIPBOARD_BUSY 1
#define GMUX_CLIPBOARD_INVALID 2
#define GMUX_CLIPBOARD_MAX_BYTES (4 * 1024 * 1024)
// Synchronous admission only. Validate and copy borrowed text before success.
// A NULL pointer clears the destination; non-NULL with len == 0 is empty text.
typedef int (*GmuxClipboardFn)(void *userdata, uint8_t destination, const uint8_t *data, size_t len);
void gmux_engine_clipboard_callback(GmuxEngine *engine, GmuxClipboardFn callback);
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
// mode: 0 = cells, 1 = words, 2 = logical lines, 3 = rectangle.
// Clear to begin a new gesture. Bounds are inclusive physical history cells.
int gmux_engine_select(GmuxEngine *engine, uint16_t sx, uint32_t sy, uint16_t ex, uint32_t ey, uint8_t mode);
void gmux_engine_clear_selection(GmuxEngine *engine);
// Clear primary screen/history at the next VT ground boundary. Reject alternate
// screens so full-screen applications retain their cursor and display state.
int gmux_engine_clear_screen(GmuxEngine *engine);
// The active screen must own both tracked corners. Rectangle bounds normalize
// to top-left/bottom-right using the current grid, including after reflow.
int gmux_engine_selection(GmuxEngine *engine, uint64_t *start, uint64_t *end, bool *rectangle);
typedef struct {
    uint8_t foreground[3], background[3], palette[256][3];
} GmuxCopyColors;
// Formats this view's tracked selection synchronously. A rejecting writer stops
// generation immediately. HTML callers receive the effective terminal colors;
// Ghostty's CSS palette references use these colors, never selection colors.
int gmux_engine_copy(GmuxEngine *engine, bool html, GmuxWriterFn write,
    void *userdata, GmuxCopyColors *colors);
int gmux_engine_theme(GmuxEngine *engine, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *palette, bool dark);
// False for status-only input or an unfinished OSC introducer/body.
bool gmux_engine_write(GmuxEngine *engine, const uint8_t *data, size_t len);
int gmux_engine_cursor_style(GmuxEngine *engine, uint8_t style);
int gmux_engine_resize(GmuxEngine *engine, uint16_t cols, uint16_t rows, uint32_t cw, uint32_t ch);
int gmux_engine_snapshot(GmuxEngine *engine, GmuxFrame *frame, GmuxCell *cells, size_t cap, uint8_t *row_flags);
GmuxHistory gmux_engine_history_state(GmuxEngine *engine);
int gmux_engine_styled_rows(GmuxEngine *engine, uint32_t start, size_t rows, GmuxCell *cells, size_t cols, uint8_t *flags);
// Targets first (end == 0), then sorted spans, borrowed for each callback.
typedef void (*GmuxLinkFn)(void *userdata, uint32_t start, uint32_t end,
    uint32_t target, const uint8_t *uri, size_t len);
void gmux_engine_links(GmuxEngine *engine, GmuxLinkFn visit, void *userdata);
uint64_t gmux_engine_links_revision(GmuxEngine *engine);
int gmux_engine_selection_origin(GmuxEngine *engine, uint64_t *origin);
void gmux_engine_scroll(GmuxEngine *engine, intptr_t delta, bool bottom);
void gmux_engine_scroll_to(GmuxEngine *engine, uint64_t row);
// Primary-screen semantic operations; return 1 when no target is available.
int gmux_engine_prompt(GmuxEngine *engine, bool previous);
// target: 0 last completed output (current if unavailable), 1 screen point,
// 2 this view's captured output point.
int gmux_engine_select_output(GmuxEngine *engine, uint8_t target, uint16_t x, uint32_t y);
int gmux_engine_output_anchor(GmuxEngine *engine, uint16_t x, uint32_t y);
bool gmux_engine_cursor_at_prompt(GmuxEngine *engine);
bool gmux_engine_expire_render_hold(GmuxEngine *engine);
size_t gmux_engine_string(GmuxEngine *engine, int pwd, uint8_t *out, size_t cap);
int gmux_engine_mode(GmuxEngine *engine, int mode);
int gmux_engine_paste(GmuxEngine *engine, const uint8_t *data, size_t len, bool clipboard);
size_t gmux_engine_key(GmuxEngine *engine, const char *name, uint32_t cp, uint16_t mods,
    uint8_t action, const char *text, size_t len, uint8_t *out, size_t cap);
size_t gmux_engine_mouse(GmuxEngine *engine, float x, float y, uint32_t cw, uint32_t ch,
    int action, int button, uint16_t mods, bool pressed, uint8_t *out, size_t cap);

#endif
