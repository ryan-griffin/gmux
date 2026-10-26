#define _POSIX_C_SOURCE 200809L
#include "ghostty_bridge.h"
#include <ghostty/vt.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "ghostty_links.h"

// Explicit conversion keeps wire values independent of upstream enum ordering.
static uint8_t mouse_shape_code(GhosttyMouseShape shape) {
    switch (shape) {
        case GHOSTTY_MOUSE_SHAPE_DEFAULT: return GMUX_MOUSE_SHAPE_DEFAULT;
        case GHOSTTY_MOUSE_SHAPE_CONTEXT_MENU: return GMUX_MOUSE_SHAPE_CONTEXT_MENU;
        case GHOSTTY_MOUSE_SHAPE_HELP: return GMUX_MOUSE_SHAPE_HELP;
        case GHOSTTY_MOUSE_SHAPE_POINTER: return GMUX_MOUSE_SHAPE_POINTER;
        case GHOSTTY_MOUSE_SHAPE_PROGRESS: return GMUX_MOUSE_SHAPE_PROGRESS;
        case GHOSTTY_MOUSE_SHAPE_WAIT: return GMUX_MOUSE_SHAPE_WAIT;
        case GHOSTTY_MOUSE_SHAPE_CELL: return GMUX_MOUSE_SHAPE_CELL;
        case GHOSTTY_MOUSE_SHAPE_CROSSHAIR: return GMUX_MOUSE_SHAPE_CROSSHAIR;
        case GHOSTTY_MOUSE_SHAPE_TEXT: return GMUX_MOUSE_SHAPE_TEXT;
        case GHOSTTY_MOUSE_SHAPE_VERTICAL_TEXT: return GMUX_MOUSE_SHAPE_VERTICAL_TEXT;
        case GHOSTTY_MOUSE_SHAPE_ALIAS: return GMUX_MOUSE_SHAPE_ALIAS;
        case GHOSTTY_MOUSE_SHAPE_COPY: return GMUX_MOUSE_SHAPE_COPY;
        case GHOSTTY_MOUSE_SHAPE_MOVE: return GMUX_MOUSE_SHAPE_MOVE;
        case GHOSTTY_MOUSE_SHAPE_NO_DROP: return GMUX_MOUSE_SHAPE_NO_DROP;
        case GHOSTTY_MOUSE_SHAPE_NOT_ALLOWED: return GMUX_MOUSE_SHAPE_NOT_ALLOWED;
        case GHOSTTY_MOUSE_SHAPE_GRAB: return GMUX_MOUSE_SHAPE_GRAB;
        case GHOSTTY_MOUSE_SHAPE_GRABBING: return GMUX_MOUSE_SHAPE_GRABBING;
        case GHOSTTY_MOUSE_SHAPE_ALL_SCROLL: return GMUX_MOUSE_SHAPE_ALL_SCROLL;
        case GHOSTTY_MOUSE_SHAPE_COL_RESIZE: return GMUX_MOUSE_SHAPE_COL_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_ROW_RESIZE: return GMUX_MOUSE_SHAPE_ROW_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_N_RESIZE: return GMUX_MOUSE_SHAPE_N_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_E_RESIZE: return GMUX_MOUSE_SHAPE_E_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_S_RESIZE: return GMUX_MOUSE_SHAPE_S_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_W_RESIZE: return GMUX_MOUSE_SHAPE_W_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_NE_RESIZE: return GMUX_MOUSE_SHAPE_NE_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_NW_RESIZE: return GMUX_MOUSE_SHAPE_NW_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_SE_RESIZE: return GMUX_MOUSE_SHAPE_SE_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_SW_RESIZE: return GMUX_MOUSE_SHAPE_SW_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_EW_RESIZE: return GMUX_MOUSE_SHAPE_EW_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_NS_RESIZE: return GMUX_MOUSE_SHAPE_NS_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_NESW_RESIZE: return GMUX_MOUSE_SHAPE_NESW_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_NWSE_RESIZE: return GMUX_MOUSE_SHAPE_NWSE_RESIZE;
        case GHOSTTY_MOUSE_SHAPE_ZOOM_IN: return GMUX_MOUSE_SHAPE_ZOOM_IN;
        case GHOSTTY_MOUSE_SHAPE_ZOOM_OUT: return GMUX_MOUSE_SHAPE_ZOOM_OUT;
        default: return GMUX_MOUSE_SHAPE_TEXT;
    }
}
static uint8_t underline_code(GhosttySgrUnderline style) {
    switch (style) {
        case GHOSTTY_SGR_UNDERLINE_NONE: return GMUX_UNDERLINE_NONE;
        case GHOSTTY_SGR_UNDERLINE_SINGLE: return GMUX_UNDERLINE_SINGLE;
        case GHOSTTY_SGR_UNDERLINE_DOUBLE: return GMUX_UNDERLINE_DOUBLE;
        case GHOSTTY_SGR_UNDERLINE_CURLY: return GMUX_UNDERLINE_CURLY;
        case GHOSTTY_SGR_UNDERLINE_DOTTED: return GMUX_UNDERLINE_DOTTED;
        case GHOSTTY_SGR_UNDERLINE_DASHED: return GMUX_UNDERLINE_DASHED;
        default: return GMUX_UNDERLINE_NONE;
    }
}

static GhosttyColorRgb resolve_color(GhosttyStyleColor color, GhosttyColorRgb fallback,
    const GhosttyRenderStateColors *colors) {
    switch (color.tag) {
        case GHOSTTY_STYLE_COLOR_PALETTE: return colors->palette[color.value.palette];
        case GHOSTTY_STYLE_COLOR_RGB: return color.value.rgb;
        default: return fallback;
    }
}
static inline void cell_style(GmuxCell *cell, const GhosttyStyle *style,
    const GhosttyRenderStateColors *colors) {
    cell->flags = style->bold | (style->italic << 1) | (style->faint << 2) |
        (style->strikethrough << 4) | (style->invisible << 5);
    cell->decoration = underline_code(style->underline) |
        (style->overline ? GMUX_DECORATION_OVERLINE : 0);
    const GhosttyColorRgb *underline = NULL;
    switch (style->underline_color.tag) {
        case GHOSTTY_STYLE_COLOR_PALETTE:
            underline = &colors->palette[style->underline_color.value.palette]; break;
        case GHOSTTY_STYLE_COLOR_RGB:
            underline = &style->underline_color.value.rgb; break;
        default: break;
    }
    if (underline) {
        cell->decoration |= GMUX_DECORATION_UNDERLINE_COLOR;
        memcpy(cell->underline_color, underline, 3);
    }
}
static void replacement_text(GmuxCell *cell) {
    memset(cell->text, 0, sizeof(cell->text));
    memcpy(cell->text, "\xef\xbf\xbd", 3);
}

// Keep the moving libghostty ABI on this side of the Rust boundary.
typedef struct {
    uint8_t background[3], foreground[3], cursor[3], ansi[16][3];
    uint8_t cursor_style;
    bool dark, cursor_blink;
} Appearance;

typedef struct { uint64_t start, end; } SelectionBounds;
typedef struct {
    SelectionBounds anchor, current;
    uint64_t generation;
    uint8_t mode, valid;
    bool anchor_empty, current_empty;
} SelectionCache;

typedef struct {
    GmuxHistory state;
    GhosttyTrackedGridRef anchor, previous;
    size_t history_rows;
    uint16_t cols, rows;
    GhosttyTerminalScreen screen;
    GhosttyColorRgb foreground, background, palette[256];
    bool reverse;
    uint64_t reset;
    uint64_t text_generation;
} HistoryCache;

// Two distinct cells let us detect a discarded location even when Ghostty
// clamps an erased partial page's tracked references to its first cell.
typedef struct {
    GhosttyTrackedGridRef point, guard;
    uint64_t prompt_id, rewrite;
} OutputAnchor;
typedef struct { GhosttyTrackedGridRef point; uint64_t id; } PromptAnchor;

struct GmuxEngine {
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
    GmuxClipboardFn clipboard;
    void *userdata;
    uint32_t cell_width, cell_height;
    bool dark;
    bool held;
    bool status_sequence;
    bool status_escape;
    bool status_reported;
    GhosttyTrackedGridRef selection_start, selection_end, selection_anchor;
    OutputAnchor output_anchor;
    SelectionCache selection_cache;
    bool selection_rectangle;
    GhosttyTerminalScreen selection_screen;
    uint64_t text_generation;
    uint64_t hold_started_ms;
    GmuxView *views;
    bool view_held;
    Links links, held_links;
    LinkScratch link_scratch;
    uint64_t link_revision;
    bool clear_pending;
    Appearance appearance, applied;
    int appearance_error;
    HistoryCache history;
    HistoryCache *view_history;
    uint64_t history_reset;
    PromptAnchor *prompts;
    OutputAnchor output, completed_output;
    size_t prompt_head, prompt_count, prompt_capacity;
    uint64_t prompt_sequence, prompt_rewrites;
    bool auxiliary_prompt, output_open, prompt_executed;
};

struct GmuxView {
    GmuxEngine *owner;
    GmuxView *next;
    GhosttyRenderState render;
    GhosttyTerminalScrollbar scrollbar;
    GhosttyTrackedGridRef viewport, selection_start, selection_end, selection_anchor;
    OutputAnchor output_anchor;
    SelectionCache selection_cache;
    bool selection_rectangle;
    GhosttyTerminalScreen selection_screen;
    Links links;
    bool held_used;
    bool captured;
    bool active;
    Appearance appearance;
    HistoryCache history;
};

#include "ghostty_shell.h"

static int apply_appearance(GmuxEngine *e, const Appearance *appearance) {
    // Identical defaults leave Ghostty's incremental row tracking untouched.
    // Updating defaults preserves OSC colors and explicit program cursor styles.
    if (e->applied.cursor_style == UINT8_MAX ||
        memcmp(e->applied.background, appearance->background, 3) ||
        memcmp(e->applied.foreground, appearance->foreground, 3) ||
        memcmp(e->applied.cursor, appearance->cursor, 3) ||
        memcmp(e->applied.ansi, appearance->ansi, sizeof(appearance->ansi))) {
        GhosttyColorRgb bg, fg, cursor, palette[256];
        memcpy(&bg, appearance->background, 3);
        memcpy(&fg, appearance->foreground, 3);
        memcpy(&cursor, appearance->cursor, 3);
        GhosttyResult result = ghostty_terminal_get(e->terminal,
            GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT, palette);
        if (result) return result;
        memcpy(palette, appearance->ansi, sizeof(appearance->ansi));
        if ((result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg)) ||
            (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg)) ||
            (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor)) ||
            (result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, palette)))
            return result;
    }
    if (e->applied.cursor_style != appearance->cursor_style) {
        GhosttyTerminalCursorStyle style = (GhosttyTerminalCursorStyle)appearance->cursor_style;
        GhosttyResult result = ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE, &style);
        if (result) return result;
    }
    if (e->applied.cursor_blink != appearance->cursor_blink) {
        GhosttyResult result = ghostty_terminal_set(e->terminal,
            GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, &appearance->cursor_blink);
        if (result) return result;
    }
    e->applied = *appearance;
    return 0;
}

static void swap_view(GmuxEngine *e, GmuxView *v) {
#define SWAP(field, type) do { type tmp=e->field; e->field=v->field; v->field=tmp; } while (0)
    SWAP(selection_start, GhosttyTrackedGridRef);
    SWAP(selection_end, GhosttyTrackedGridRef);
    SWAP(selection_anchor, GhosttyTrackedGridRef);
    SWAP(output_anchor, OutputAnchor);
    SWAP(selection_cache, SelectionCache);
    SWAP(selection_rectangle, bool);
    SWAP(selection_screen, GhosttyTerminalScreen);
#undef SWAP
}

GmuxView *gmux_engine_view_new(GmuxEngine *e) {
    GmuxView *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    v->owner=e; v->next=e->views; e->views=v;
    v->appearance=e->appearance;
    v->active=true;
    return v;
}
void gmux_engine_view_set_active(GmuxView *v, bool active) {
    v->active=active;
    if (!active) {
        ghostty_render_state_free(v->render); v->render=NULL;
        v->captured=false;
        ghostty_tracked_grid_ref_free(v->history.anchor); v->history.anchor=NULL;
        ghostty_tracked_grid_ref_free(v->history.previous); v->history.previous=NULL;
        v->history.state.generation=0;
        links_free(&v->links);
    }
}
void gmux_engine_view_free(GmuxView *v) {
    if (!v) return;
    if (v->owner) {
        GmuxView **link=&v->owner->views;
        while (*link && *link != v) link=&(*link)->next;
        if (*link) *link=v->next;
    }
    ghostty_tracked_grid_ref_free(v->viewport);
    ghostty_tracked_grid_ref_free(v->selection_start);
    ghostty_tracked_grid_ref_free(v->selection_end);
    ghostty_tracked_grid_ref_free(v->selection_anchor);
    shell_output_clear(&v->output_anchor);
    ghostty_tracked_grid_ref_free(v->history.anchor);
    ghostty_tracked_grid_ref_free(v->history.previous);
    ghostty_render_state_free(v->render);
    links_free(&v->links);
    free(v);
}
void gmux_engine_view_enter(GmuxEngine *e, GmuxView *v) {
    gmux_engine_expire_render_hold(e);
    swap_view(e, v);
    e->view_history = &v->history;
    e->appearance_error=apply_appearance(e, &v->appearance);
    v->held_used=e->held && v->captured; e->view_held=e->held;
    if (v->held_used) {
        Links links=e->held_links; e->held_links=v->links; v->links=links;
        GhosttyRenderState render=e->render; e->render=v->render; v->render=render;
        GhosttyTerminalScrollbar scrollbar=e->scrollbar; e->scrollbar=v->scrollbar; v->scrollbar=scrollbar;
    }
    GhosttyPointCoordinate point;
    if (v->viewport && !ghostty_tracked_grid_ref_point(v->viewport, GHOSTTY_POINT_TAG_SCREEN, &point))
        gmux_engine_scroll_to(e, point.y);
    else
        gmux_engine_scroll(e, 0, true);
}
void gmux_engine_view_leave(GmuxEngine *e, GmuxView *v) {
    GhosttyTerminalScrollbar bar;
    if (!ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &bar)) {
        if (bar.offset + bar.len >= bar.total) {
            ghostty_tracked_grid_ref_free(v->viewport); v->viewport=NULL;
        } else {
            GhosttyPoint point={.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={0,bar.offset}};
            if (v->viewport) ghostty_tracked_grid_ref_set(v->viewport, e->terminal, point);
            else ghostty_terminal_grid_ref_track(e->terminal, point, &v->viewport);
        }
    }
    if (v->held_used) {
        Links links=e->held_links; e->held_links=v->links; v->links=links;
        GhosttyRenderState render=e->render; e->render=v->render; v->render=render;
        GhosttyTerminalScrollbar scrollbar=e->scrollbar; e->scrollbar=v->scrollbar; v->scrollbar=scrollbar;
    }
    e->view_held=false;
    e->view_history = NULL;
    // Restore the size owner's defaults before parsing PTY output or answering queries.
    e->appearance_error=apply_appearance(e, &e->appearance);
    swap_view(e, v);
    gmux_engine_scroll(e, 0, true);
}

static uint64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static void end_render_hold(GmuxEngine *e) {
    e->held=false;
    e->links.valid=false;
    links_free(&e->link_scratch.spare);
    links_free(&e->held_links);
    for (GmuxView *v=e->views; v; v=v->next) links_free(&v->links);
}

static void render_hold(GhosttyTerminal terminal, void *ud, bool held) {
    GmuxEngine *e = ud;
    if (held) {
        // Capture at the exact start of the hold, even if the same PTY chunk
        // contains the next frame. Keep it visible until that frame is complete.
        if (ghostty_render_state_update(e->render, terminal)) return;
        e->links.valid=false;
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &e->scrollbar);
        links_capture(terminal, e->render, e->iterator, &e->held_links, &e->link_scratch, ++e->link_revision, false);
        for (GmuxView *v=e->views; v; v=v->next) {
            if (!v->active) continue;
            gmux_engine_view_enter(e, v);
            v->captured=false;
            if (!v->render) ghostty_render_state_new(NULL, &v->render);
            // Reapplying the current default dirties Ghostty's terminal
            // palette without changing OSC overrides. DIRTY_FULL on a render
            // cache only controls extraction; other caches consumed the rows.
            GhosttyColorRgb foreground;
            memcpy(&foreground, e->applied.foreground, 3);
            if (!e->appearance_error && v->render &&
                !ghostty_terminal_set(terminal, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &foreground) &&
                !ghostty_render_state_update(v->render, terminal) &&
                !ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &v->scrollbar)) {
                links_capture(terminal, v->render, e->iterator, &v->links, &e->link_scratch, ++e->link_revision, false);
                v->captured=true;
            } else {
                // An unsuccessful update may leave an incomplete cache.
                ghostty_render_state_free(v->render); v->render=NULL;
            }
            gmux_engine_view_leave(e, v);
        }
        e->hold_started_ms = now_ms();
    }
    if (held) e->held=true;
    else end_render_hold(e);
}

bool gmux_engine_expire_render_hold(GmuxEngine *e) {
    if (!e->held || now_ms() - e->hold_started_ms < 1000) return false;
    GhosttyTerminalModeConfig mode = {.mode = GHOSTTY_MODE_SYNC_OUTPUT, .value = false};
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_MODE, &mode);
    end_render_hold(e);
    return true;
}

static void write_pty(GhosttyTerminal t, void *ud, const uint8_t *data, size_t len) {
    (void)t;
    GmuxEngine *e = ud;
    e->callback(e->userdata, 0, data, len);
}
static bool terminal_size(GhosttyTerminal t, void *ud, GhosttySizeReportSize *out) {
    GmuxEngine *e = ud;
    if (ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_COLS, &out->columns) ||
        ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_ROWS, &out->rows)) return false;
    out->cell_width = e->cell_width;
    out->cell_height = e->cell_height;
    return true;
}
static GhosttyString xtversion(GhosttyTerminal t, void *ud) {
    (void)t; (void)ud;
    static const uint8_t version[] = "gmux " GMUX_VERSION;
    return (GhosttyString){.ptr = version, .len = sizeof(version) - 1};
}
static bool string_equal(GhosttyString value, const char *text) {
    size_t len = strlen(text);
    return value.len == len && value.ptr && !memcmp(value.ptr, text, len);
}
static void clipboard_write(GhosttyTerminal t, void *ud, const GhosttyClipboardWrite *write) {
    (void)t;
    // No request or reply pointer may escape this callback. Unknown layouts
    // are denied by returning without a reply, per the library contract.
    if (write->size < offsetof(GhosttyClipboardWrite, reply) + sizeof(write->reply) || !write->reply) return;
    GmuxEngine *e = ud;
    GhosttyClipboardWriteReply reply = {
        .size = sizeof(reply), .result = GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED,
    };
    uint8_t destination;
    switch (write->location) {
        case GHOSTTY_CLIPBOARD_LOCATION_STANDARD: destination = GMUX_CLIPBOARD_STANDARD; break;
        case GHOSTTY_CLIPBOARD_LOCATION_PRIMARY: destination = GMUX_CLIPBOARD_PRIMARY; break;
        default: goto done;
    }
    // Accept the whole atomic request, never just one representation of rich
    // clipboard contents. Zero representations means clear, not empty text.
    const uint8_t *data = NULL;
    size_t len = 0;
    if (write->contents_len == 1 && write->contents) {
        const GhosttyClipboardContent *content = write->contents;
        if (!string_equal(content->mime, "text/plain") &&
            !string_equal(content->mime, "text/plain;charset=utf-8")) goto done;
        data = content->data.ptr;
        len = content->data.len;
        if (len > GMUX_CLIPBOARD_MAX_BYTES || (len && !data)) {
            reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
            goto done;
        }
        // Keep empty text distinct even if the library uses a NULL slice.
        if (!data) data = (const uint8_t *)"";
    } else if (write->contents_len != 0) goto done;
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_BUSY;
    if (e->clipboard) {
        switch (e->clipboard(e->userdata, destination, data, len)) {
            case GMUX_CLIPBOARD_SUCCESS: reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS; break;
            case GMUX_CLIPBOARD_INVALID: reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA; break;
            default: break;
        }
    }
done:
    write->reply(write, &reply);
}
void gmux_engine_clipboard_callback(GmuxEngine *e, GmuxClipboardFn callback) {
    if (e->clipboard == callback) return;
    e->clipboard = callback;
    // With no recipient, the library can skip OSC 52 decoding and refuse rich
    // transactions before buffering their contents. No request is retained.
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
        callback ? clipboard_write : NULL);
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
static void progress_report(GhosttyTerminal t, void *ud, const GhosttyTerminalProgressReport *report) {
    (void)t;
    if (report->size < offsetof(GhosttyTerminalProgressReport, progress) + sizeof(report->progress)) return;
    uint8_t state;
    switch (report->state) {
        case GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE: state = GMUX_PROGRESS_INACTIVE; break;
        case GHOSTTY_TERMINAL_PROGRESS_STATE_SET: state = GMUX_PROGRESS_DETERMINATE; break;
        case GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR: state = GMUX_PROGRESS_ERROR; break;
        case GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE: state = GMUX_PROGRESS_INDETERMINATE; break;
        case GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE: state = GMUX_PROGRESS_PAUSED; break;
        default: return;
    }
    const uint8_t data[] = {state, (uint8_t)report->progress};
    GmuxEngine *e = ud;
    e->status_reported = true;
    e->callback(e->userdata, 4, data, sizeof(data));
}
static bool color_scheme(GhosttyTerminal t, void *ud, GhosttyColorScheme *out) {
    (void)t;
    GmuxEngine *e = ud;
    *out = e->dark ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT;
    return true;
}
static void program_status(GhosttyTerminal t, void *ud, const GhosttyTerminalProgramStatus *report) {
    (void)t;
    if (report->size < offsetof(GhosttyTerminalProgramStatus, message) + sizeof(report->message)) return;
    GmuxProgramStatusEvent event = {0};
    switch (report->state) {
        case GHOSTTY_PROGRAM_STATUS_STATE_IDLE: event.state = 0; break;
        case GHOSTTY_PROGRAM_STATUS_STATE_WORKING: event.state = 1; break;
        case GHOSTTY_PROGRAM_STATUS_STATE_DONE: event.state = 2; break;
        case GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED: event.state = 3; break;
        case GHOSTTY_PROGRAM_STATUS_STATE_ERROR: event.state = 4; break;
        case GHOSTTY_PROGRAM_STATUS_STATE_CLEAR: event.state = 5; break;
        default: return;
    }
    switch (report->kind) {
        case GHOSTTY_PROGRAM_STATUS_KIND_PERMISSION: event.kind = 1; break;
        case GHOSTTY_PROGRAM_STATUS_KIND_QUESTION: event.kind = 2; break;
        case GHOSTTY_PROGRAM_STATUS_KIND_AUTH: event.kind = 3; break;
        default: break;
    }
    event.progress = report->progress;
    event.id = (GmuxText){report->id.ptr, report->id.len};
    event.app = (GmuxText){report->app.ptr, report->app.len};
    event.title = (GmuxText){report->title.ptr, report->title.len};
    event.message = (GmuxText){report->message.ptr, report->message.len};
    GmuxEngine *e = ud;
    e->status_reported = true;
    e->callback(e->userdata, 7, (const uint8_t *)&event, sizeof(event));
}
void gmux_engine_free(GmuxEngine *e) {
    if (!e) return;
    for (GmuxView *v=e->views; v; v=v->next) v->owner=NULL;
    ghostty_tracked_grid_ref_free(e->selection_start);
    ghostty_tracked_grid_ref_free(e->selection_end);
    ghostty_tracked_grid_ref_free(e->selection_anchor);
    shell_output_clear(&e->output_anchor);
    ghostty_tracked_grid_ref_free(e->history.anchor);
    ghostty_tracked_grid_ref_free(e->history.previous);
    shell_clear(e);
    ghostty_key_event_free(e->key);
    ghostty_mouse_event_free(e->mouse);
    ghostty_mouse_encoder_free(e->mouse_encoder);
    ghostty_key_encoder_free(e->encoder);
    ghostty_render_state_row_cells_free(e->cells);
    ghostty_render_state_row_iterator_free(e->iterator);
    ghostty_render_state_free(e->render);
    ghostty_terminal_free(e->terminal);
    links_free(&e->links);
    links_free(&e->held_links);
    links_scratch_free(&e->link_scratch);
    free(e);
}
GmuxEngine *gmux_engine_new(void *ud, GmuxEventFn cb) {
    GmuxEngine *e = calloc(1, sizeof(*e));
    if (!e) return NULL;
    e->userdata = ud;
    e->callback = cb;
    e->text_generation = 1;
    e->applied.cursor_style = UINT8_MAX;
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
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, progress_report);
    if (ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS, program_status) ||
        ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT, semantic_prompt) ||
        ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_RESET, shell_reset)) {
        gmux_engine_free(e); return NULL;
    }
    static const uint8_t terminfo[] = "xterm-256color";
    GhosttyString terminfo_name = {.ptr = terminfo, .len = sizeof(terminfo) - 1};
    if (ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_SIZE, terminal_size) ||
        ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_XTVERSION, xtversion) ||
        ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, &terminfo_name)) {
        gmux_engine_free(e); return NULL;
    }
    // Track only a bounded unfinished sequence for shutdown snapshots.
    size_t continuation_max = 64 * 1024;
    size_t clipboard_max = GMUX_CLIPBOARD_MAX_BYTES;
    if (ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES,
        &clipboard_max)) { gmux_engine_free(e); return NULL; }
    if (ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
        &continuation_max)) { gmux_engine_free(e); return NULL; }
    return e;
}

int gmux_engine_memory_usage(GmuxEngine *e, GmuxMemoryUsage *out) {
    GhosttyTerminalMemoryUsage usage = GHOSTTY_INIT_SIZED(GhosttyTerminalMemoryUsage);
    int result = ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MEMORY_USAGE, &usage);
    if (result) return result;
    *out = (GmuxMemoryUsage){
        .compression_supported = usage.compression_supported,
        .pages = usage.primary_pages + usage.alternate_pages,
        .virtual_bytes = usage.primary_virtual_bytes + usage.alternate_virtual_bytes,
        .resident_bytes = usage.primary_resident_bytes + usage.alternate_resident_bytes,
        .compressed_pages = usage.primary_compressed_pages + usage.alternate_compressed_pages,
        .compressed_bytes = usage.primary_compressed_bytes + usage.alternate_compressed_bytes,
        .image_bytes = usage.primary_image_bytes + usage.alternate_image_bytes,
        .prompt_anchors = e->prompt_count,
        .prompt_anchor_bytes = e->prompt_capacity * sizeof(*e->prompts),
    };
    return 0;
}

int gmux_engine_compression_activity(GmuxEngine *e, uint64_t *activity) {
    return ghostty_terminal_compression_activity(e->terminal, activity);
}

int gmux_engine_compress(GmuxEngine *e, int *out) {
    GhosttyTerminalCompressionResult result;
    int error = ghostty_terminal_compress(e->terminal,
        GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL, &result);
    if (error) return error;
    switch (result) {
    case GHOSTTY_TERMINAL_COMPRESSION_RESULT_UNSUPPORTED: *out = GMUX_COMPRESSION_UNSUPPORTED; break;
    case GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING: *out = GMUX_COMPRESSION_PENDING; break;
    case GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE: *out = GMUX_COMPRESSION_COMPLETE; break;
    default: return -1;
    }
    return 0;
}

// Serialized terminal state, distinct from gmux_engine_snapshot's viewport.
int gmux_engine_checkpoint(GmuxEngine *e, uint8_t *out, size_t cap, size_t *written) {
    GhosttyResult result = ghostty_snapshot_encode_buf(e->terminal, out, cap, written);
    return result == GHOSTTY_OUT_OF_SPACE ? 1 : (result == GHOSTTY_SUCCESS ? 0 : -1);
}

// A file-size bound alone cannot bound allocations declared by a damaged file.
// This allocator lives until the temporary decoded terminal and formatter die.
typedef struct { size_t used, limit; } SnapshotBudget;
static void *snapshot_alloc(void *ctx, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)ra;
    SnapshotBudget *budget = ctx;
    if (len > budget->limit - budget->used) return NULL;
    // The pinned Zig adapter passes log2 alignment, including aligned pages.
    if (alignment >= sizeof(size_t) * 8) return NULL;
    size_t bytes_alignment = (size_t)1 << alignment;
    if (bytes_alignment < sizeof(void *)) bytes_alignment = sizeof(void *);
    void *ptr = NULL;
    if (posix_memalign(&ptr, bytes_alignment, len)) return NULL;
    if (ptr) budget->used += len;
    return ptr;
}
static bool snapshot_resize(void *ctx, void *ptr, size_t len, uint8_t alignment,
    size_t new_len, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)new_len; (void)ra;
    return false;
}
static void *snapshot_remap(void *ctx, void *ptr, size_t len, uint8_t alignment,
    size_t new_len, uintptr_t ra) {
    (void)ctx; (void)ptr; (void)len; (void)alignment; (void)new_len; (void)ra;
    return NULL;
}
static void snapshot_free(void *ctx, void *ptr, size_t len, uint8_t alignment, uintptr_t ra) {
    (void)alignment; (void)ra;
    ((SnapshotBudget *)ctx)->used -= len;
    free(ptr);
}
static const GhosttyAllocatorVtable snapshot_vtable = {
    .alloc = snapshot_alloc, .resize = snapshot_resize,
    .remap = snapshot_remap, .free = snapshot_free,
};

static int format_history_range(const GhosttyAllocator *allocator, GhosttyTerminal terminal,
    GhosttyFormatterTerminalOptions options, const GhosttySelection *selection, GhosttyWriter writer) {
    GhosttyFormatter formatter = NULL;
    options.selection = selection;
    if (ghostty_formatter_terminal_new(allocator, &formatter, terminal, options)) return -1;
    int result = ghostty_formatter_format(formatter, writer) == GHOSTTY_SUCCESS ? 0 : -1;
    ghostty_formatter_free(formatter);
    return result;
}

static int format_history(const GhosttyAllocator *allocator, GhosttyTerminal terminal,
    GhosttyFormatterTerminalOptions options, uint16_t cols, GhosttyWriter writer) {
    // The pinned formatter flushes deferred empty cells before switching style,
    // so unwritten gaps after styled text can inherit its rendition. End a
    // selection before each such gap. The next selection starts with default
    // rendition and still lets Ghostty handle graphemes, wraps and blank rows.
    // Each range resets its final style; all refs borrow this disposable terminal.
    size_t rows = 0;
    if (!cols || ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, &rows) ||
        !rows || rows > UINT32_MAX) return -1;
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    GhosttyGridRef previous = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    bool styled = false;
    for (size_t y = 0; y < rows; y++) {
        GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
        GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN,
            .value.coordinate = {.x = 0, .y = (uint32_t)y}};
        GhosttyRow row;
        bool wrapped = false;
        if (ghostty_terminal_grid_ref(terminal, point, &ref) ||
            ghostty_grid_ref_row(&ref, &row) || ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &wrapped)) return -1;
        // The pinned raw ref accessor assumes valid coordinates. A damaged or
        // incompletely reflowed page can be narrower than terminal.cols.
        GhosttyGridRef end = GHOSTTY_INIT_SIZED(GhosttyGridRef);
        point.value.coordinate.x = cols - 1;
        if (ghostty_terminal_grid_ref(terminal, point, &end) ||
            end.node != ref.node || end.y != ref.y) return -1;
        if (y == 0) selection.start = ref;
        for (uint16_t x = 0; x < cols; x++) {
            ref.x = x;
            GhosttyCell cell;
            GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
            bool text = false, styling = false;
            if (ghostty_grid_ref_cell(&ref, &cell) ||
                ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide) ||
                ghostty_cell_get(cell, GHOSTTY_CELL_DATA_HAS_TEXT, &text) ||
                ghostty_cell_get(cell, GHOSTTY_CELL_DATA_HAS_STYLING, &styling)) return -1;
            if (wide == GHOSTTY_CELL_WIDE_NARROW || wide == GHOSTTY_CELL_WIDE_WIDE) {
                if (!text && !styling && styled) {
                    selection.end = previous;
                    if (format_history_range(allocator, terminal, options, &selection, writer)) return -1;
                    selection.start = ref;
                }
                styled = styling;
            }
            previous = ref;
        }
        if (!wrapped) styled = false;
    }
    selection.end = previous;
    return format_history_range(allocator, terminal, options, &selection, writer);
}

// Decode into a temporary terminal. Export only primary-screen rendition so the
// new engine never inherits modes, metadata, graphics, or parser continuation.
int gmux_checkpoint_history(const uint8_t *data, size_t len, GmuxWriterFn write,
    void *userdata, uint16_t *cols, uint16_t *rows) {
    SnapshotBudget budget = {.limit = 64 * 1024 * 1024};
    GhosttyAllocator allocator = {.ctx = &budget, .vtable = &snapshot_vtable};
    GhosttySnapshotDecoder decoder = NULL;
    GhosttyTerminal terminal = NULL;
    size_t continuation_max = 64 * 1024, consumed = 0;
    int result = -1;
    if (ghostty_snapshot_decoder_new_buf(&allocator, &decoder, data, len) ||
        ghostty_snapshot_decoder_set(decoder, GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
            &continuation_max) ||
        ghostty_snapshot_decoder_decode(decoder, &terminal) ||
        ghostty_snapshot_decoder_get(decoder, GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET, &consumed) ||
        consumed != len ||
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_COLS, cols) ||
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_ROWS, rows)) goto done;
    // Never replay a full-screen program into the new shell's primary screen.
    // terminal_set(MODE) changes flags without running screen-switch effects.
    // Cancel continuation on this disposable terminal before sending DECRST.
    const uint8_t cancel[] = {0x18, 0x1b, '\\'};
    size_t cancelled = 0;
    if (ghostty_terminal_vt_write_until_ground(terminal, cancel, sizeof(cancel), &cancelled)) goto done;
    const char primary[] = "\x1b[?1049l\x1b[?1047l\x1b[?47l";
    ghostty_terminal_vt_write(terminal, (const uint8_t *)primary, sizeof(primary) - 1);
    GhosttyFormatterTerminalOptions options = GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalOptions);
    options.emit = GHOSTTY_FORMATTER_FORMAT_VT;
    options.unwrap = true;
    options.trim = true;
    // Keep every extra disabled, including palette and cursor style. The pinned
    // formatter emits indexed SGR references and RGB, with no per-cell OSC 8.
    // Indexed/default colors therefore use the new terminal's current theme.
    options.extra = GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalExtra);
    options.extra.screen = GHOSTTY_INIT_SIZED(GhosttyFormatterScreenExtra);
    GhosttyWriter writer = {.write = write, .userdata = userdata};
    result = format_history(&allocator, terminal, options, *cols, writer);
done:
    ghostty_snapshot_decoder_free(decoder);
    ghostty_terminal_free(terminal);
    return result;
}
int gmux_engine_theme(GmuxEngine *e, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *ansi, bool dark) {
    Appearance appearance=e->appearance;
    memcpy(appearance.background, background, 3);
    memcpy(appearance.foreground, foreground, 3);
    memcpy(appearance.cursor, cursor, 3);
    memcpy(appearance.ansi, ansi, sizeof(appearance.ansi));
    appearance.dark=dark;
    int result=apply_appearance(e, &appearance);
    if (result) return result;
    e->appearance=appearance;
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
void gmux_engine_view_theme(GmuxView *v, const uint8_t *background,
    const uint8_t *foreground, const uint8_t *cursor, const uint8_t *ansi, bool dark) {
    memcpy(v->appearance.background, background, 3);
    memcpy(v->appearance.foreground, foreground, 3);
    memcpy(v->appearance.cursor, cursor, 3);
    memcpy(v->appearance.ansi, ansi, sizeof(v->appearance.ansi));
    v->appearance.dark=dark;
}
int gmux_engine_view_cursor_blink(GmuxView *v, bool blink) {
    v->appearance.cursor_blink = blink;
    return 0;
}
int gmux_engine_cursor_blink(GmuxEngine *e, bool blink) {
    Appearance appearance = e->appearance;
    appearance.cursor_blink = blink;
    int result = apply_appearance(e, &appearance);
    if (!result) e->appearance = appearance;
    return result;
}
int gmux_engine_view_cursor_style(GmuxView *v, uint8_t style) {
    if (style > GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK_HOLLOW) return GHOSTTY_INVALID_VALUE;
    v->appearance.cursor_style=style;
    return 0;
}
int gmux_engine_use_view_appearance(GmuxEngine *e, GmuxView *v) {
    int result=gmux_engine_theme(e, v->appearance.background, v->appearance.foreground,
        v->appearance.cursor, (const uint8_t *)v->appearance.ansi, v->appearance.dark);
    if (result) return result;
    if ((result = gmux_engine_cursor_style(e, v->appearance.cursor_style))) return result;
    return gmux_engine_cursor_blink(e, v->appearance.cursor_blink);
}
bool gmux_engine_write(GmuxEngine *e, const uint8_t *data, size_t len) {
    bool changed = false;
    if (e->clear_pending) {
        size_t consumed = 0;
        if (ghostty_terminal_vt_write_until_ground(e->terminal, data, len, &consumed)) return true;
        gmux_engine_clear_screen(e);
        e->status_sequence = false;
        e->status_escape = false;
        changed = true;
        data += consumed;
        len -= consumed;
    }
    // Let the upstream stream find sequence boundaries. A chunk containing only
    // status callbacks cannot change cells, selection, history or search. No
    // OSC body is parsed here, and mixed output takes the ordinary path.
    if (e->status_escape && len) {
        e->status_escape = false;
        if (data[0] == ']') {
            ghostty_terminal_vt_write(e->terminal, data, 1);
            data++; len--;
            e->status_sequence = true;
            e->status_reported = false;
        } else {
            ghostty_terminal_vt_write(e->terminal, data, len);
            changed = true;
            len = 0;
        }
    }
    while (len) {
        if (!e->status_sequence) {
            bool ground = false;
            if (data[0] != 0x1b || (len >= 2 && data[1] != ']') ||
                ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground) || !ground) {
                ghostty_terminal_vt_write(e->terminal, data, len);
                changed = true;
                break;
            }
            if (len == 1) {
                // A read can split even the OSC introducer. ESC alone has no
                // grid effect; let the next byte decide which path to take.
                ghostty_terminal_vt_write(e->terminal, data, 1);
                e->status_escape = true;
                break;
            }
            ghostty_terminal_vt_write(e->terminal, data, 2);
            data += 2; len -= 2;
            e->status_sequence = true;
            e->status_reported = false;
        }
        size_t consumed = 0;
        GhosttyResult result = ghostty_terminal_vt_write_until_ground(e->terminal, data, len, &consumed);
        if (result == GHOSTTY_NO_VALUE) break;
        if (result != GHOSTTY_SUCCESS) {
            changed = true;
            e->status_sequence = false;
            break;
        }
        data += consumed; len -= consumed;
        // SUCCESS already guarantees ground, including the terminating byte.
        changed |= !e->status_reported;
        e->status_sequence = false;
    }
    if (changed) {
        e->text_generation++;
        if (e->prompt_count || e->output.point || e->completed_output.point) shell_prune(e);
    }
    return changed;
}
int gmux_engine_clear_screen(GmuxEngine *e) {
    e->clear_pending = false;
    GhosttyTerminalScreen screen;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen)
        || screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return -1;
    bool ground = false;
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground);
    if (!ground) {
        // Never splice host commands into a partial OSC, CSI, or UTF-8 character.
        e->clear_pending = true;
        return 0;
    }
    GhosttyTerminalModeConfig hold = {.mode = GHOSTTY_MODE_SYNC_OUTPUT, .value = false};
    ghostty_terminal_set(e->terminal, GHOSTTY_TERMINAL_OPT_MODE, &hold);
    end_render_hold(e);
    // ED2 can move prompt rows into history, so erase history afterwards.
    const char clear[] = "\x1b[2J\x1b[3J\x1b[H";
    e->text_generation++;
    e->history_reset++;
    shell_clear(e);
    ghostty_terminal_vt_write(e->terminal, (const uint8_t *)clear, sizeof(clear) - 1);
    gmux_engine_clear_selection(e);
    shell_output_clear(&e->output_anchor);
    for (GmuxView *v = e->views; v; v = v->next) {
        ghostty_tracked_grid_ref_free(v->viewport); v->viewport = NULL;
        ghostty_tracked_grid_ref_free(v->selection_start); v->selection_start = NULL;
        ghostty_tracked_grid_ref_free(v->selection_end); v->selection_end = NULL;
        ghostty_tracked_grid_ref_free(v->selection_anchor); v->selection_anchor = NULL;
        shell_output_clear(&v->output_anchor);
        v->selection_cache.valid = 0;
    }
    gmux_engine_scroll(e, 0, true);
    return 0;
}
int gmux_engine_cursor_style(GmuxEngine *e, uint8_t style) {
    if (style > GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK_HOLLOW) return GHOSTTY_INVALID_VALUE;
    Appearance appearance=e->appearance;
    appearance.cursor_style=style;
    int result=apply_appearance(e, &appearance);
    if (!result) e->appearance=appearance;
    return result;
}
int gmux_engine_resize(GmuxEngine *e, uint16_t cols, uint16_t rows, uint32_t cw, uint32_t ch) {
    uint16_t previous_cols = 0, previous_rows = 0;
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &previous_cols);
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ROWS, &previous_rows);
    int result = ghostty_terminal_resize(e->terminal, cols, rows, cw, ch);
    if (result) return result;
    if (cols != previous_cols || rows != previous_rows) {
        e->text_generation++;
        e->history_reset++;
        shell_prune(e);
    }
    // Libghostty sends the resize report itself, using these same metrics.
    e->cell_width = cw;
    e->cell_height = ch;
    return 0;
}
void gmux_engine_scroll(GmuxEngine *e, intptr_t delta, bool bottom) {
    // View enter/leave restores the live viewport around every client frame.
    // A redundant scroll still changes Ghostty's compression activity token.
    bool active = false;
    if (bottom && !ghostty_terminal_get(e->terminal,
        GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE, &active) && active) return;
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
static int update_history_cache(GmuxEngine *e, const GmuxFrame *frame,
    const GhosttyRenderStateColors *colors) {
    HistoryCache *history = e->view_history ? e->view_history : &e->history;
    GhosttyTerminalScreen screen;
    GhosttyTerminalModeConfig reverse = {.mode = GHOSTTY_MODE_REVERSE_COLORS};
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MODE, &reverse)) return -1;
    bool reset = !history->state.generation || history->reset != e->history_reset ||
        history->cols != frame->cols || history->rows != frame->rows || history->screen != screen ||
        history->reverse != reverse.value ||
        memcmp(&history->foreground, &colors->foreground, sizeof(history->foreground)) ||
        memcmp(&history->background, &colors->background, sizeof(history->background)) ||
        memcmp(history->palette, colors->palette, sizeof(history->palette));
    if (!reset && history->text_generation == e->text_generation) return 0;
    if (!reset && history->history_rows) {
        // Two adjacent history rows distinguish pruning from references that
        // collapse to the same fallback position after history is erased.
        GhosttyPointCoordinate point, previous;
        if (!history->anchor || ghostty_tracked_grid_ref_point(history->anchor,
                GHOSTTY_POINT_TAG_SCREEN, &point) || point.y >= history->history_rows ||
            point.y >= frame->scrollback.total - frame->rows ||
            (history->history_rows == 1 && history->text_generation != e->text_generation) ||
            (history->history_rows > 1 && (!history->previous ||
                ghostty_tracked_grid_ref_point(history->previous, GHOSTTY_POINT_TAG_SCREEN, &previous) ||
                previous.y + 1 != point.y))) {
            reset = true;
        } else {
            history->state.origin += history->history_rows - 1 - point.y;
        }
    }
    if (reset) {
        if (++history->state.generation == 0) history->state.generation = 1;
        history->state.origin = 0;
    }
    history->history_rows = frame->scrollback.total - frame->rows;
    if (history->history_rows) {
        if (history->history_rows > UINT32_MAX) return -1;
        GhosttyPoint point = {.tag=GHOSTTY_POINT_TAG_SCREEN,
            .value.coordinate={0, (uint32_t)(history->history_rows - 1)}};
        if (history->anchor) {
            if (ghostty_tracked_grid_ref_set(history->anchor, e->terminal, point)) return -1;
        } else if (ghostty_terminal_grid_ref_track(e->terminal, point, &history->anchor)) return -1;
    } else {
        ghostty_tracked_grid_ref_free(history->anchor); history->anchor = NULL;
    }
    if (history->history_rows > 1) {
        GhosttyPoint point = {.tag=GHOSTTY_POINT_TAG_SCREEN,
            .value.coordinate={0, (uint32_t)(history->history_rows - 2)}};
        if (history->previous) {
            if (ghostty_tracked_grid_ref_set(history->previous, e->terminal, point)) return -1;
        } else if (ghostty_terminal_grid_ref_track(e->terminal, point, &history->previous)) return -1;
    } else {
        ghostty_tracked_grid_ref_free(history->previous); history->previous = NULL;
    }
    history->cols = frame->cols; history->rows = frame->rows;
    history->screen = screen; history->reverse = reverse.value;
    history->reset = e->history_reset;
    history->text_generation = e->text_generation;
    history->foreground = colors->foreground; history->background = colors->background;
    memcpy(history->palette, colors->palette, sizeof(history->palette));
    return 0;
}
GmuxHistory gmux_engine_history_state(GmuxEngine *e) {
    return (e->view_history ? e->view_history : &e->history)->state;
}
int gmux_engine_snapshot(GmuxEngine *e, GmuxFrame *frame, GmuxCell *out, size_t cap, uint8_t *row_flags) {
    if (e->appearance_error) return -1;
    if (!e->view_held) gmux_engine_expire_render_hold(e);
    if (e->held) {
        GhosttyRenderStateDirty dirty=GHOSTTY_RENDER_STATE_DIRTY_FULL;
        ghostty_render_state_set(e->render, GHOSTTY_RENDER_STATE_OPTION_DIRTY, &dirty);
    }
    if (!e->held && ghostty_render_state_update(e->render, e->terminal)) return -1;
    if (!e->held && ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &e->scrollbar)) return -1;
    frame->scrollback = (GmuxScrollback){e->scrollbar.total, e->scrollbar.offset, e->scrollbar.len};
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_COLS, &frame->cols);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_ROWS, &frame->rows);
    if ((size_t)frame->cols * frame->rows > cap) return -1;
    GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    GhosttyRenderStateCursor cursor = GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors);
    if (!e->held && update_history_cache(e, frame, &colors)) return -1;
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_CURSOR, &cursor);
    memcpy(frame->background, &colors.background, 3);
    memcpy(frame->foreground, &colors.foreground, 3);
    memcpy(frame->cursor, colors.cursor_has_value ? &colors.cursor : &colors.foreground, 3);
    frame->cursor_x = cursor.viewport_x;
    frame->cursor_y = cursor.viewport_y;
    frame->cursor_visible = cursor.visible && cursor.viewport_has_value;
    frame->cursor_style = cursor.visual_style;
    frame->cursor_blink = cursor.blinking;
    GhosttyMouseShape shape = GHOSTTY_MOUSE_SHAPE_TEXT;
    // Pointer requests affect the OS cursor immediately, independently of a
    // synchronized-output hold on cells and render-cursor state.
    ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE, &shape);
    frame->mouse_shape = mouse_shape_code(shape);
    GhosttyRenderStateDirty dirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_DIRTY, &dirty);
    ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &e->iterator);
    uint16_t y;
    // Rust keeps the cell buffer between snapshots. Only changed rows need
    // extraction; resize, scrolling and palette changes invalidate the full grid.
    while (ghostty_render_state_row_iterator_next_dirty(e->iterator, &y)) {
        GhosttyRow row;
        bool wrapped = false;
        if (ghostty_render_state_row_get(e->iterator, GHOSTTY_RENDER_STATE_ROW_DATA_RAW, &row) ||
            ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &wrapped)) return -1;
        row_flags[y] = wrapped;
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
            cell_style(cell, &style, &colors);
            GhosttyCell raw;
            GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
            ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw);
            ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide);
            if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL) cell->flags |= 64;
            if (wide == GHOSTTY_CELL_WIDE_SPACER_HEAD) cell->flags |= 128;
            GhosttyBuffer buffer = {.ptr = (uint8_t *)cell->text, .cap = sizeof(cell->text) - 1};
            // A cluster too large for this cell is shown as a replacement character.
            if (ghostty_render_state_row_cells_get(e->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8, &buffer))
                replacement_text(cell);
        }
    }
    if (!e->held && (dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE || !e->links.valid))
        links_capture(e->terminal, e->render, e->iterator, &e->links, &e->link_scratch,
            ++e->link_revision, dirty == GHOSTTY_RENDER_STATE_DIRTY_PARTIAL);
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
    // Extended keyboard mode can report these keys even though they produce
    // no Unicode text. Keep left and right modifiers distinct.
    KEY("Shift_L", SHIFT_LEFT); KEY("Shift_R", SHIFT_RIGHT);
    KEY("Control_L", CONTROL_LEFT); KEY("Control_R", CONTROL_RIGHT);
    KEY("Alt_L", ALT_LEFT); KEY("Alt_R", ALT_RIGHT);
    KEY("Super_L", META_LEFT); KEY("Super_R", META_RIGHT);
    KEY("Meta_L", META_LEFT); KEY("Meta_R", META_RIGHT);
    KEY("Caps_Lock", CAPS_LOCK); KEY("Num_Lock", NUM_LOCK);
    KEY("Scroll_Lock", SCROLL_LOCK); KEY("Print", PRINT_SCREEN); KEY("Pause", PAUSE);
    KEY("Menu", CONTEXT_MENU);
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
    uint8_t action, const char *text, size_t len, uint8_t *out, size_t cap) {
    ghostty_key_encoder_setopt_from_terminal(e->encoder, e->terminal);
    GhosttyKeyAction ghostty_action;
    switch (action) {
        case GMUX_KEY_ACTION_PRESS: ghostty_action = GHOSTTY_KEY_ACTION_PRESS; break;
        case GMUX_KEY_ACTION_REPEAT: ghostty_action = GHOSTTY_KEY_ACTION_REPEAT; break;
        case GMUX_KEY_ACTION_RELEASE: ghostty_action = GHOSTTY_KEY_ACTION_RELEASE; break;
        default: return 0;
    }
    ghostty_key_event_set_action(e->key, ghostty_action);
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
int gmux_engine_styled_rows(GmuxEngine *e, uint32_t start, size_t rows,
    GmuxCell *out, size_t cols, uint8_t *flags) {
    // Read a bounded window without moving any client's viewport. Resolve the
    // palette once for the batch, using the same rendition as viewport cells.
    if (e->held || e->appearance_error) return -1;
    uint16_t actual_cols = 0;
    size_t total = 0;
    GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &actual_cols) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, &total) ||
        cols != actual_cols || start > total || rows > total - start ||
        ghostty_render_state_get(e->render, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors)) return -1;
    for (size_t index=0; index<rows; index++, out+=cols) {
        GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
        GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN,
            .value.coordinate = {.x=0, .y=start + (uint32_t)index}};
        GhosttyRow row;
        bool wrapped = false;
        if (ghostty_terminal_grid_ref(e->terminal, point, &ref) ||
            ghostty_grid_ref_row(&ref, &row) || ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &wrapped)) return -1;
        flags[index] = wrapped;
        memset(out, 0, sizeof(*out) * cols);
        for (size_t x=0; x<cols; x++) {
            ref.x = x;
            GmuxCell *cell = &out[x];
            GhosttyCell raw;
            GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
            GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
            if (ghostty_grid_ref_cell(&ref, &raw) || ghostty_grid_ref_style(&ref, &style) ||
                ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide)) return -1;
            GhosttyColorRgb fg = resolve_color(style.fg_color, colors.foreground, &colors);
            GhosttyColorRgb bg = resolve_color(style.bg_color, colors.background, &colors);
            GhosttyCellContentTag tag;
            if (ghostty_cell_get(raw, GHOSTTY_CELL_DATA_CONTENT_TAG, &tag)) return -1;
            if (tag == GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE) {
                uint8_t palette;
                if (ghostty_cell_get(raw, GHOSTTY_CELL_DATA_COLOR_PALETTE, &palette)) return -1;
                bg = colors.palette[palette];
            } else if (tag == GHOSTTY_CELL_CONTENT_BG_COLOR_RGB) {
                if (ghostty_cell_get(raw, GHOSTTY_CELL_DATA_COLOR_RGB, &bg)) return -1;
            }
            if (style.inverse) { GhosttyColorRgb swap = fg; fg = bg; bg = swap; }
            memcpy(cell->fg, &fg, 3); memcpy(cell->bg, &bg, 3);
            cell_style(cell, &style, &colors);
            if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL) cell->flags |= 64;
            if (wide == GHOSTTY_CELL_WIDE_SPACER_HEAD) cell->flags |= 128;
            uint32_t cps[64]; size_t count=0, len=0;
            if (ghostty_grid_ref_graphemes(&ref, cps, 64, &count)) {
                replacement_text(cell); continue;
            }
            for (size_t i=0; i<count; i++) {
                uint8_t bytes[4]; size_t n=utf8(cps[i], bytes);
                if (len+n >= sizeof(cell->text)) {
                    replacement_text(cell); break;
                }
                memcpy(cell->text+len, bytes, n); len+=n;
            }
        }
    }
    return 0;
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
static int track_selection_point(GmuxEngine *e, GhosttyTrackedGridRef *ref, GhosttyPoint point) {
    return *ref ? ghostty_tracked_grid_ref_set(*ref, e->terminal, point) :
        ghostty_terminal_grid_ref_track(e->terminal, point, ref);
}

static void clear_selection_bounds(GmuxEngine *e) {
    ghostty_tracked_grid_ref_free(e->selection_start); e->selection_start=NULL;
    ghostty_tracked_grid_ref_free(e->selection_end); e->selection_end=NULL;
}

static GhosttyPoint selection_point(uint64_t index, uint16_t cols) {
    return (GhosttyPoint){.tag=GHOSTTY_POINT_TAG_SCREEN,
        .value.coordinate={(uint16_t)(index % cols), (uint32_t)(index / cols)}};
}
static int selection_unit(GmuxEngine *e, uint64_t from, uint64_t toward, uint16_t cols,
    uint8_t mode, SelectionBounds *out, bool *empty) {
    SelectionCache *cache = &e->selection_cache;
    *empty = false;
    if ((cache->valid & 1) && from >= cache->anchor.start && from <= cache->anchor.end) {
        *out = cache->anchor; *empty = cache->anchor_empty; return GHOSTTY_SUCCESS;
    }
    if ((cache->valid & 2) && from >= cache->current.start && from <= cache->current.end) {
        *out = cache->current; *empty = cache->current_empty; return GHOSTTY_SUCCESS;
    }
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    if (ghostty_terminal_grid_ref(e->terminal, selection_point(from, cols), &ref)) return GHOSTTY_INVALID_VALUE;
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    GhosttyResult result;
    if (mode == 1) {
        GhosttyTerminalSelectWordBetweenOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectWordBetweenOptions);
        options.start = ref;
        options.end.size = sizeof(GhosttyGridRef);
        if (ghostty_terminal_grid_ref(e->terminal, selection_point(toward, cols), &options.end)) return GHOSTTY_INVALID_VALUE;
        result = ghostty_terminal_select_word_between(e->terminal, &options, &selection);
    } else {
        GhosttyTerminalSelectLineOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectLineOptions);
        // An explicit empty trim set preserves wrapped lines of written spaces.
        const uint32_t no_trim = 0;
        options.whitespace = &no_trim;
        options.ref = ref;
        result = ghostty_terminal_select_line(e->terminal, &options, &selection);
        if (result == GHOSTTY_NO_VALUE) {
            selection.start = selection.end = ref;
            *empty = true;
            result = GHOSTTY_SUCCESS;
        }
    }
    if (result) return result;
    GhosttyPointCoordinate start, end;
    if (ghostty_terminal_point_from_grid_ref(e->terminal, &selection.start, GHOSTTY_POINT_TAG_SCREEN, &start) ||
        ghostty_terminal_point_from_grid_ref(e->terminal, &selection.end, GHOSTTY_POINT_TAG_SCREEN, &end))
        return GHOSTTY_INVALID_VALUE;
    if (mode == 2) { start.x = 0; end.x = cols - 1; }
    *out = (SelectionBounds){(uint64_t)start.y * cols + start.x, (uint64_t)end.y * cols + end.x};
    return GHOSTTY_SUCCESS;
}
int gmux_engine_select(GmuxEngine *e, uint16_t sx, uint32_t sy, uint16_t ex, uint32_t ey, uint8_t mode) {
    GhosttyPoint start = {.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={sx,sy}};
    GhosttyPoint end = {.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={ex,ey}};
    GhosttyTerminalScreen screen;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) || mode > 3) return -1;
    if ((e->selection_start || e->selection_anchor) &&
        (e->selection_screen != screen || e->selection_rectangle != (mode == 3))) gmux_engine_clear_selection(e);
    e->selection_screen = screen;
    e->selection_rectangle = mode == 3;
    if (mode == 0 || mode == 3) {
        if (!e->selection_start && track_selection_point(e, &e->selection_start, start)) return -1;
        return track_selection_point(e, &e->selection_end, end);
    }
    if (mode > 2) return -1;
    // Track the click separately so reversal and reflow retain its complete unit.
    if (!e->selection_anchor && track_selection_point(e, &e->selection_anchor, start)) return -1;
    GhosttyPointCoordinate anchor;
    uint16_t cols = 0;
    if (ghostty_tracked_grid_ref_point(e->selection_anchor, GHOSTTY_POINT_TAG_SCREEN, &anchor) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols) || !cols || ex >= cols) return -1;
    SelectionCache *cache = &e->selection_cache;
    if (cache->generation != e->text_generation || cache->mode != mode) {
        cache->generation = e->text_generation;
        cache->mode = mode;
        cache->valid = 0;
    }
    uint64_t origin = (uint64_t)anchor.y * cols + anchor.x;
    uint64_t endpoint = (uint64_t)ey * cols + ex;
    SelectionBounds a, b;
    bool a_empty, b_empty;
    int result = selection_unit(e, origin, endpoint, cols, mode, &a, &a_empty);
    if (!result) {
        cache->anchor = a; cache->anchor_empty = a_empty; cache->valid |= 1;
        result = selection_unit(e, endpoint, origin, cols, mode, &b, &b_empty);
    }
    if (result == GHOSTTY_NO_VALUE ||
        (!result && a_empty && b_empty && a.start == b.start)) {
        // Empty rows extend line drags, but cannot start a selection alone.
        // Retain the tracked click for the next drag movement.
        clear_selection_bounds(e);
        return 0;
    }
    if (result) return -1;
    cache->current = b; cache->current_empty = b_empty; cache->valid |= 2;
    start = selection_point(a.start < b.start ? a.start : b.start, cols);
    end = selection_point(a.end > b.end ? a.end : b.end, cols);
    if (track_selection_point(e, &e->selection_start, start)) return -1;
    return track_selection_point(e, &e->selection_end, end);
}
void gmux_engine_clear_selection(GmuxEngine *e) {
    e->selection_cache.valid = 0;
    clear_selection_bounds(e);
    ghostty_tracked_grid_ref_free(e->selection_anchor); e->selection_anchor=NULL;
}
int gmux_engine_selection(GmuxEngine *e, uint64_t *start, uint64_t *end, bool *rectangle) {
    GhosttyTerminalScreen screen;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) || screen != e->selection_screen) return -1;
    if (!e->selection_start || !e->selection_end) return -1;
    GhosttyPointCoordinate a,b; uint16_t cols=0;
    if (ghostty_tracked_grid_ref_point(e->selection_start, GHOSTTY_POINT_TAG_SCREEN, &a) ||
        ghostty_tracked_grid_ref_point(e->selection_end, GHOSTTY_POINT_TAG_SCREEN, &b) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols)) return -1;
    *rectangle = e->selection_rectangle;
    if (*rectangle) {
        uint16_t left = a.x < b.x ? a.x : b.x, right = a.x > b.x ? a.x : b.x;
        uint32_t top = a.y < b.y ? a.y : b.y, bottom = a.y > b.y ? a.y : b.y;
        a = (GhosttyPointCoordinate){left, top}; b = (GhosttyPointCoordinate){right, bottom};
    }
    *start = (uint64_t)a.y*cols+a.x; *end=(uint64_t)b.y*cols+b.x;
    if (*start > *end) { uint64_t swap=*start; *start=*end; *end=swap; }
    return 0;
}
int gmux_engine_selection_origin(GmuxEngine *e, uint64_t *origin) {
    GhosttyTerminalScreen screen;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) || screen != e->selection_screen) return -1;
    GhosttyTrackedGridRef ref = e->selection_anchor ? e->selection_anchor : e->selection_start;
    GhosttyPointCoordinate point;
    uint16_t cols = 0;
    if (!ref || ghostty_tracked_grid_ref_point(ref, GHOSTTY_POINT_TAG_SCREEN, &point) ||
        ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols)) return -1;
    *origin = (uint64_t)point.y * cols + point.x;
    return 0;
}
// The pinned formatter defers unwritten trailing cells even with trim=false.
// Restore just that physical padding; Ghostty still formats all text/graphemes.
static int rectangle_padding(GhosttyGridRef ref, uint16_t left, uint16_t right, size_t *padding) {
    *padding = 0;
    for (uint32_t x = (uint32_t)right + 1; x > left;) {
        ref.x = --x;
        GhosttyCell cell;
        GhosttyCellWide wide;
        bool text;
        if (ghostty_grid_ref_cell(&ref, &cell) ||
            ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide) ||
            ghostty_cell_get(cell, GHOSTTY_CELL_DATA_HAS_TEXT, &text)) return -1;
        if (text || wide == GHOSTTY_CELL_WIDE_SPACER_TAIL) break;
        (*padding)++;
    }
    return 0;
}
int gmux_engine_copy(GmuxEngine *e, uint8_t *out, size_t cap, size_t *written) {
    *written = 0;
    uint64_t start, end;
    bool rectangle;
    if (gmux_engine_selection(e, &start, &end, &rectangle)) return 0;
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    selection.rectangle = rectangle;
    GhosttyTerminalSelectionFormatOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
    options.emit=GHOSTTY_FORMATTER_FORMAT_PLAIN; options.unwrap=!rectangle; options.trim=!rectangle; options.selection=&selection;
    if (!rectangle) {
        if (ghostty_tracked_grid_ref_snapshot(e->selection_start, &selection.start) ||
            ghostty_tracked_grid_ref_snapshot(e->selection_end, &selection.end)) return -1;
        GhosttyResult result=ghostty_terminal_selection_format_buf(e->terminal, options, out, cap, written);
        return result == GHOSTTY_OUT_OF_SPACE ? 1 : (result == GHOSTTY_SUCCESS ? 0 : -1);
    }
    uint16_t cols;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols) || !cols) return -1;
    const uint16_t left = start % cols, right = end % cols;
    const uint32_t bottom = end / cols;
    for (uint32_t y = start / cols;; y++) {
        GhosttyPoint point = {.tag=GHOSTTY_POINT_TAG_SCREEN, .value.coordinate={left,y}};
        if (ghostty_terminal_grid_ref(e->terminal, point, &selection.start)) return -1;
        selection.end = selection.start;
        selection.end.x = right;
        size_t padding, bytes = 0;
        if (rectangle_padding(selection.end, left, right, &padding)) return -1;
        size_t available = *written < cap ? cap - *written : 0;
        GhosttyResult result = ghostty_terminal_selection_format_buf(e->terminal, options,
            available ? out + *written : NULL, available, &bytes);
        if (result != GHOSTTY_SUCCESS && result != GHOSTTY_OUT_OF_SPACE) return -1;
        if (bytes > SIZE_MAX - *written) return -1;
        *written += bytes;
        if (padding > SIZE_MAX - *written) return -1;
        available = *written < cap ? cap - *written : 0;
        if (available) memset(out + *written, ' ', padding < available ? padding : available);
        *written += padding;
        if (y == bottom) break;
        if (*written == SIZE_MAX) return -1;
        if (*written < cap) out[*written] = '\n';
        (*written)++;
    }
    return *written > cap ? 1 : 0;
}

void gmux_engine_links(GmuxEngine *e, GmuxLinkFn visit, void *userdata) {
    const Links *links=e->held ? &e->held_links : &e->links;
    for (size_t i=0; i<links->targets_len; i++) {
        const LinkTarget *target=&links->targets[i];
        visit(userdata, 0, 0, (uint32_t)i, links->bytes+target->offset, target->len);
    }
    for (size_t i=0; i<links->spans_len; i++) {
        const LinkSpan *span=&links->spans[i];
        visit(userdata, span->start, span->end, span->target, NULL, 0);
    }
}

uint64_t gmux_engine_links_revision(GmuxEngine *e) {
    return (e->held ? &e->held_links : &e->links)->revision;
}
