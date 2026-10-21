// Private sparse viewport snapshots. Targets are interned, never stored in cells.
// Bounds match gmux-client::hyperlinks; overflow discards the whole map.
#define LINK_URI_MAX GMUX_LINK_MAX_URI_BYTES
#define LINK_BYTES_MAX GMUX_LINK_MAX_TARGET_BYTES
#define LINK_SPANS_MAX GMUX_LINK_MAX_SPANS

typedef struct { uint32_t start, end, target; } LinkSpan;
typedef struct { uint32_t offset, len; uint64_t hash; } LinkTarget;
typedef struct {
    LinkSpan *spans;
    LinkTarget *targets;
    uint8_t *bytes;
    size_t spans_len, targets_len, bytes_len;
    size_t spans_cap, targets_cap, bytes_cap;
    uint64_t revision, offset;
    uint16_t cols, rows;
    GhosttyTerminalScreen screen;
    bool valid;
} Links;

typedef struct { uint32_t *table; size_t cap; Links spare; } LinkScratch;
typedef struct { Links *links; LinkScratch *scratch; bool table_clean; } LinkBuilder;

static void links_free(Links *links) {
    free(links->spans); free(links->targets); free(links->bytes);
    memset(links, 0, sizeof(*links));
}
static void links_scratch_free(LinkScratch *scratch) {
    free(scratch->table);
    links_free(&scratch->spare);
    memset(scratch, 0, sizeof(*scratch));
}
static bool links_reserve(void **ptr, size_t *cap, size_t count, size_t item, size_t max) {
    if (count <= *cap) return true;
    size_t next = *cap ? *cap * 2 : 8;
    if (next < count) next = count;
    if (next > max) next = max;
    void *p = realloc(*ptr, next * item);
    if (!p) return false;
    *ptr = p; *cap = next;
    return true;
}
static uint64_t link_hash(const uint8_t *bytes, size_t len) {
    uint64_t hash = UINT64_C(14695981039346656037);
    for (size_t i=0; i<len; i++) hash = (hash ^ bytes[i]) * UINT64_C(1099511628211);
    return hash;
}
static bool links_add(LinkBuilder *builder, const uint8_t *uri, size_t len,
    uint64_t hash, bool hashed, uint32_t start, uint32_t end) {
    Links *links=builder->links;
    LinkScratch *scratch=builder->scratch;
    uint32_t target;
    LinkSpan *last=links->spans_len ? &links->spans[links->spans_len-1] : NULL;
    if (last) {
        const LinkTarget *previous=&links->targets[last->target];
        if (previous->len == len && !memcmp(links->bytes+previous->offset, uri, len)) {
            target=last->target;
            goto append;
        }
    }
    if (!hashed) hash=link_hash(uri,len);
    if (!builder->table_clean) {
        if (scratch->table) memset(scratch->table, 0, scratch->cap*sizeof(*scratch->table));
        builder->table_clean=true;
    }
    // At most 50% load. Hash equality is always checked against complete bytes.
    if (links->targets_len < LINK_SPANS_MAX && 2*(links->targets_len+1) > scratch->cap) {
        size_t cap=scratch->cap ? scratch->cap*2 : 16;
        uint32_t *next=calloc(cap, sizeof(*next));
        if (!next) return false;
        for (size_t i=0; i<links->targets_len; i++) {
            size_t slot=links->targets[i].hash & (cap-1);
            while (next[slot]) slot=(slot+1)&(cap-1);
            next[slot]=(uint32_t)i+1;
        }
        free(scratch->table); scratch->table=next; scratch->cap=cap;
    }
    size_t slot=hash & (scratch->cap-1);
    while (scratch->table[slot]) {
        LinkTarget *target=&links->targets[scratch->table[slot]-1];
        if (target->hash == hash && target->len == len &&
            !memcmp(links->bytes+target->offset, uri, len)) break;
        slot=(slot+1)&(scratch->cap-1);
    }
    target=scratch->table[slot] ? scratch->table[slot]-1 : (uint32_t)links->targets_len;
    if (!scratch->table[slot]) {
        if (links->targets_len == LINK_SPANS_MAX || len > LINK_BYTES_MAX-links->bytes_len ||
            !links_reserve((void **)&links->targets, &links->targets_cap, links->targets_len+1, sizeof(LinkTarget), LINK_SPANS_MAX) ||
            !links_reserve((void **)&links->bytes, &links->bytes_cap, links->bytes_len+len, 1, LINK_BYTES_MAX)) return false;
        links->targets[links->targets_len++]=(LinkTarget){links->bytes_len,len,hash};
        memcpy(links->bytes+links->bytes_len,uri,len); links->bytes_len+=len;
        scratch->table[slot]=target+1;
    }
append:
    last=links->spans_len ? &links->spans[links->spans_len-1] : NULL;
    if (last && last->end == start && last->target == target) last->end=end;
    else {
        if (links->spans_len == LINK_SPANS_MAX ||
            !links_reserve((void **)&links->spans, &links->spans_cap, links->spans_len+1, sizeof(LinkSpan), LINK_SPANS_MAX)) return false;
        links->spans[links->spans_len++]=(LinkSpan){start,end,target};
    }
    return true;
}
// Copy clean rows from the previous captured map, resolve dirty linked cells
// through grid refs, then intern both into one bounded map. The second packed
// buffer permits reuse without keeping stale targets or per-cell URI strings.
// A full render update or changed viewport key always resolves every linked row.
static void links_capture(GhosttyTerminal terminal, GhosttyRenderState render,
    GhosttyRenderStateRowIterator iterator, Links *links, LinkScratch *scratch,
    uint64_t revision, bool reuse_clean) {
    Links previous=*links;
    *links=scratch->spare;
    scratch->spare=(Links){0};
    links->spans_len=links->targets_len=links->bytes_len=0;
    links->valid=false;
    LinkBuilder builder={.links=links,.scratch=scratch};
    uint8_t uri[LINK_URI_MAX];
    uint16_t cols=0, rows=0;
    GhosttyTerminalScrollbar scrollbar;
    GhosttyTerminalScreen screen;
    if (ghostty_render_state_get(render, GHOSTTY_RENDER_STATE_DATA_COLS, &cols) ||
        ghostty_render_state_get(render, GHOSTTY_RENDER_STATE_DATA_ROWS, &rows) ||
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &scrollbar) ||
        ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen)) goto fail;
    reuse_clean = reuse_clean && previous.valid && previous.cols == cols && previous.rows == rows &&
        previous.offset == scrollbar.offset && previous.screen == screen;
    ghostty_render_state_get(render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &iterator);
    size_t old_span=0;
    while (ghostty_render_state_row_iterator_next(iterator)) {
        GhosttyRow row;
        bool has_links=false, dirty=true;
        int32_t y=0;
        if (ghostty_render_state_row_get(iterator, GHOSTTY_RENDER_STATE_ROW_DATA_RAW, &row) ||
            ghostty_row_get(row, GHOSTTY_ROW_DATA_HYPERLINK, &has_links) ||
            ghostty_render_state_row_get(iterator, GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y, &y)) goto fail;
        if (!has_links || y < 0 || y >= rows) continue;
        uint32_t start=(uint32_t)y*cols, end=start+cols;
        if (reuse_clean) {
            if (ghostty_render_state_row_get(iterator, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &dirty)) goto fail;
            if (!dirty) {
                while (old_span < previous.spans_len && previous.spans[old_span].end <= start) old_span++;
                for (size_t i=old_span; i<previous.spans_len && previous.spans[i].start < end; i++) {
                    const LinkSpan *span=&previous.spans[i];
                    const LinkTarget *target=&previous.targets[span->target];
                    if (!links_add(&builder, previous.bytes+target->offset, target->len, target->hash, true,
                        span->start < start ? start : span->start, span->end > end ? end : span->end)) goto fail;
                }
                continue;
            }
        }
        GhosttyCellsView cells;
        if (ghostty_render_state_row_get(iterator, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW, &cells) || cells.len < cols) goto fail;
        for (uint16_t x=0; x<cols; x++) {
            bool linked=false;
            if (ghostty_cell_get(cells.ptr[x], GHOSTTY_CELL_DATA_HAS_HYPERLINK, &linked)) goto fail;
            if (!linked) continue;
            GhosttyGridRef ref=GHOSTTY_INIT_SIZED(GhosttyGridRef);
            GhosttyPoint point={.tag=GHOSTTY_POINT_TAG_VIEWPORT, .value.coordinate={x,(uint32_t)y}};
            size_t len=0;
            if (ghostty_terminal_grid_ref(terminal, point, &ref) ||
                ghostty_grid_ref_hyperlink_uri(&ref, uri, sizeof(uri), &len)) goto fail;
            if (len && !links_add(&builder, uri, len, 0, false, start+x, start+x+1)) goto fail;
        }
    }
    if (links->spans_len) {
        links->revision=revision;
        scratch->spare=previous;
    } else {
        links_free(links);
        links_free(&previous);
        links_scratch_free(scratch);
    }
    links->cols=cols; links->rows=rows; links->offset=scrollbar.offset; links->screen=screen;
    links->valid=true;
    return;
fail:
    links_free(links);
    links_free(&previous);
    links_scratch_free(scratch);
}
