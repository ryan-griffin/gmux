// Private semantic shell support. Only libghostty parses OSC 133.
#define PROMPT_LIMIT 2048u

static bool shell_primary(GmuxEngine *e) {
    GhosttyTerminalScreen screen;
    return !ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) &&
        screen == GHOSTTY_TERMINAL_SCREEN_PRIMARY;
}

static PromptAnchor *shell_prompt(GmuxEngine *e, size_t i) {
    return &e->prompts[(e->prompt_head + i) % e->prompt_capacity];
}

static bool shell_grow_prompts(GmuxEngine *e) {
    size_t capacity = e->prompt_capacity ? e->prompt_capacity * 2 : 32;
    PromptAnchor *prompts = calloc(capacity, sizeof(*prompts));
    if (!prompts) return false;
    for (size_t i = 0; i < e->prompt_count; i++) prompts[i] = *shell_prompt(e, i);
    free(e->prompts);
    e->prompts = prompts;
    e->prompt_capacity = capacity;
    e->prompt_head = 0;
    return true;
}

static void shell_output_clear(OutputAnchor *anchor) {
    ghostty_tracked_grid_ref_free(anchor->point);
    ghostty_tracked_grid_ref_free(anchor->guard);
    *anchor = (OutputAnchor){0};
}

static bool shell_output_snapshot(OutputAnchor *anchor, GhosttyGridRef *out) {
    if (!anchor->point) return false;
    GhosttyGridRef guard = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    if (!ghostty_tracked_grid_ref_snapshot(anchor->point, out) &&
        !ghostty_tracked_grid_ref_snapshot(anchor->guard, &guard) &&
        !(out->node == guard.node && out->x == guard.x && out->y == guard.y)) return true;
    shell_output_clear(anchor);
    return false;
}

static bool shell_prompt_valid(GhosttyTrackedGridRef anchor) {
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    GhosttyRow row;
    GhosttyRowSemanticPrompt kind;
    return !ghostty_tracked_grid_ref_snapshot(anchor, &ref) &&
        !ghostty_grid_ref_row(&ref, &row) &&
        !ghostty_row_get(row, GHOSTTY_ROW_DATA_SEMANTIC_PROMPT, &kind) &&
        kind == GHOSTTY_ROW_SEMANTIC_PROMPT;
}

static bool shell_output_owned(GmuxEngine *e, OutputAnchor *anchor) {
    if (anchor->prompt_id && e->prompt_count) {
        // IDs stay ordered as the prompt ring grows, prunes, or rewinds. This
        // checks command identity without reading text or polling other views.
        size_t lo = 0, hi = e->prompt_count;
        while (lo < hi) {
            size_t mid = lo + (hi - lo) / 2;
            if (shell_prompt(e, mid)->id < anchor->prompt_id) lo = mid + 1;
            else hi = mid;
        }
        if (lo < e->prompt_count && shell_prompt(e, lo)->id == anchor->prompt_id) {
            // Erasing active rows can leave their tracked cells and IDs in
            // place. Validate the owning prompt only on selection requests.
            return shell_prompt_valid(shell_prompt(e, lo)->point);
        }
        if (anchor->prompt_id >= shell_prompt(e, 0)->id) return false;
    }
    // Output older than the bounded prompt index can still be selected. After
    // a rewrite, its ownership is ambiguous, so discard the captured target.
    return anchor->rewrite == e->prompt_rewrites;
}

static bool shell_output_get(GmuxEngine *e, OutputAnchor *anchor, GhosttyGridRef *out) {
    if (shell_output_owned(e, anchor)) return shell_output_snapshot(anchor, out);
    shell_output_clear(anchor);
    return false;
}

static void shell_clear(GmuxEngine *e) {
    for (size_t i = 0; i < e->prompt_count; i++)
        ghostty_tracked_grid_ref_free(shell_prompt(e, i)->point);
    free(e->prompts); e->prompts = NULL;
    e->prompt_head = e->prompt_count = e->prompt_capacity = 0;
    shell_output_clear(&e->output);
    shell_output_clear(&e->completed_output);
    e->prompt_rewrites++;
    e->auxiliary_prompt = e->output_open = e->prompt_executed = false;
}

static void shell_prune(GmuxEngine *e) {
    // Discarded prefixes cost O(discarded anchors), amortized over output.
    // has_value does not extract cells or decompress retained history.
    while (e->prompt_count) {
        bool discard = !ghostty_tracked_grid_ref_has_value(shell_prompt(e, 0)->point);
        if (!discard && e->prompt_count > 1) {
            // Erasing a partial page can clamp erased pins to its first cell
            // instead of invalidating them. Collapse that discarded prefix.
            // Snapshots compare opaque coordinates without reading page text.
            GhosttyGridRef a = GHOSTTY_INIT_SIZED(GhosttyGridRef);
            GhosttyGridRef b = GHOSTTY_INIT_SIZED(GhosttyGridRef);
            discard = !ghostty_tracked_grid_ref_snapshot(shell_prompt(e, 0)->point, &a) &&
                !ghostty_tracked_grid_ref_snapshot(shell_prompt(e, 1)->point, &b) &&
                a.node == b.node && a.y == b.y;
        }
        if (!discard) break;
        ghostty_tracked_grid_ref_free(shell_prompt(e, 0)->point);
        *shell_prompt(e, 0) = (PromptAnchor){0};
        e->prompt_head = (e->prompt_head + 1) % e->prompt_capacity;
        e->prompt_count--;
    }
    if (!e->prompt_count) {
        free(e->prompts); e->prompts = NULL;
        e->prompt_head = e->prompt_capacity = 0;
    }
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    shell_output_snapshot(&e->output, &ref);
    shell_output_snapshot(&e->completed_output, &ref);
}

static void shell_finish_output(GmuxEngine *e) {
    if (!e->output_open) return;
    OutputAnchor previous = e->completed_output;
    e->completed_output = e->output;
    e->output = previous;
    e->output_open = false;
}

static int shell_track(GmuxEngine *e, GhosttyTrackedGridRef *ref, GhosttyPoint point) {
    return *ref ? ghostty_tracked_grid_ref_set(*ref, e->terminal, point) :
        ghostty_terminal_grid_ref_track(e->terminal, point, ref);
}

static int shell_output_track(GmuxEngine *e, OutputAnchor *anchor, GhosttyPoint point) {
    uint16_t cols = 0;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols)) {
        shell_output_clear(anchor);
        return -1;
    }
    // Prefer a preceding cell. A guard in unwritten trailing space can merge
    // with the target when a short line reflows to one column.
    // Keep active coordinates for callbacks so tracking never walks history.
    GhosttyPoint guard = point;
    if (guard.value.coordinate.x) {
        guard.value.coordinate.x--;
    } else if (guard.value.coordinate.y) {
        guard.value.coordinate.y--;
    } else {
        size_t total = 0;
        uint16_t rows = 0;
        int result = point.tag == GHOSTTY_POINT_TAG_ACTIVE ?
            ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_ROWS, &rows) :
            ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, &total);
        if (result) { shell_output_clear(anchor); return -1; }
        if (rows > 1 || total > 1) guard.value.coordinate.y++;
        else if (cols > 1) guard.value.coordinate.x++;
        else { shell_output_clear(anchor); return 1; }
    }
    int result = shell_track(e, &anchor->point, point);
    if (!result) result = shell_track(e, &anchor->guard, guard);
    if (result) { shell_output_clear(anchor); return result; }
    anchor->prompt_id = 0;
    anchor->rewrite = e->prompt_rewrites;
    if (point.tag == GHOSTTY_POINT_TAG_ACTIVE) {
        if (e->prompt_count) anchor->prompt_id = shell_prompt(e, e->prompt_count - 1)->id;
    } else {
        // Context-menu coordinates may refer to any retained command. Find
        // its preceding primary prompt without scanning history or its output.
        size_t lo = 0, hi = e->prompt_count;
        while (lo < hi) {
            size_t mid = lo + (hi - lo) / 2;
            GhosttyPointCoordinate p;
            if (ghostty_tracked_grid_ref_point(shell_prompt(e, mid)->point,
                GHOSTTY_POINT_TAG_SCREEN, &p) || p.y <= point.value.coordinate.y) lo = mid + 1;
            else hi = mid;
        }
        // Erased prompt rows may still be indexed. A new capture there uses
        // the current rewrite epoch instead of adopting that obsolete owner.
        if (lo && shell_prompt_valid(shell_prompt(e, lo - 1)->point))
            anchor->prompt_id = shell_prompt(e, lo - 1)->id;
    }
    return result;
}

static void semantic_prompt(GhosttyTerminal terminal, void *ud,
    const GhosttyTerminalSemanticPrompt *event) {
    GmuxEngine *e = ud;
    if (!shell_primary(e)) return;
    GmuxCommandEvent report = {0};
    uint16_t x = 0, y = 0;
    ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_X, &x);
    ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &y);
    GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_ACTIVE, .value.coordinate = {x, y}};
    switch (event->kind) {
    case GHOSTTY_SEMANTIC_PROMPT_PROMPT_START:
        e->auxiliary_prompt = event->prompt_kind != GHOSTTY_SEMANTIC_PROMPT_PROMPT_PRIMARY;
        if (e->auxiliary_prompt) return;
        shell_finish_output(e);
        point.value.coordinate.x = 0;
        GhosttyPointCoordinate previous;
        bool previous_active = e->prompt_count && !ghostty_tracked_grid_ref_point(
            shell_prompt(e, e->prompt_count - 1)->point, GHOSTTY_POINT_TAG_ACTIVE, &previous);
        bool same_row = previous_active && previous.y == y;
        // An erased screen can be repainted below the former prompt, too.
        // Check only its active row, never a retained history page, so older
        // captured targets outside the index also see this rewrite epoch.
        bool rewritten = previous_active && (same_row ? e->prompt_executed :
            !shell_prompt_valid(shell_prompt(e, e->prompt_count - 1)->point));
        // A shell can clear/redraw above its former prompt. Remove superseded
        // active-area anchors so the retained index stays ordered. Each anchor
        // is released once; history pages are never visited here.
        while (!same_row && e->prompt_count &&
            !ghostty_tracked_grid_ref_point(shell_prompt(e, e->prompt_count - 1)->point, GHOSTTY_POINT_TAG_ACTIVE, &previous) &&
            previous.y >= y) {
            ghostty_tracked_grid_ref_free(shell_prompt(e, e->prompt_count - 1)->point);
            *shell_prompt(e, --e->prompt_count) = (PromptAnchor){0};
            rewritten = true;
        }
        if (rewritten) e->prompt_rewrites++;
        bool room = same_row || e->prompt_count < e->prompt_capacity || e->prompt_capacity == PROMPT_LIMIT;
        if (room || shell_grow_prompts(e)) {
            // A/P pairs and redraws on the same tracked row update its anchor.
            // A different row can be a new prompt even without C/D events.
            if (!same_row) {
                if (e->prompt_count == PROMPT_LIMIT) {
                    // Reuse the oldest handle as the new tail. Retaining a
                    // full index then needs no allocation on each command.
                    e->prompt_head = (e->prompt_head + 1) % e->prompt_capacity;
                } else {
                    e->prompt_count++;
                }
            }
            PromptAnchor *anchor = shell_prompt(e, e->prompt_count - 1);
            if (!same_row || rewritten) anchor->id = ++e->prompt_sequence;
            if (shell_track(e, &anchor->point, point)) {
                ghostty_tracked_grid_ref_free(anchor->point); *anchor = (PromptAnchor){0};
                e->prompt_count--;
            }
        }
        e->prompt_executed = false;
        e->output_open = false;
        report.kind = 1;
        break;
    case GHOSTTY_SEMANTIC_PROMPT_INPUT_START:
        // A B without A is useful, but B after a right/secondary prompt is
        // not a new primary lifecycle stage.
        if (e->auxiliary_prompt) return;
        report.kind = 2;
        break;
    case GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START:
        // Repeated C markers must not move the start of an existing output.
        if (!e->output_open) shell_output_track(e, &e->output, point);
        e->output_open = true;
        e->prompt_executed = true;
        e->auxiliary_prompt = false;
        report.kind = 3;
        report.command = event->command.ptr;
        report.command_len = event->command.len;
        break;
    case GHOSTTY_SEMANTIC_PROMPT_COMMAND_END:
        shell_finish_output(e);
        e->auxiliary_prompt = false;
        report.kind = 4;
        report.has_exit_code = event->has_exit_code;
        report.exit_code = event->exit_code;
        break;
    default: return;
    }
    e->callback(e->userdata, 5, (const uint8_t *)&report, sizeof(report));
}

static void shell_reset(GhosttyTerminal terminal, void *ud) {
    (void)terminal;
    GmuxEngine *e = ud;
    shell_clear(e);
    e->history_reset++;
    e->callback(e->userdata, 6, NULL, 0);
}

bool gmux_engine_cursor_at_prompt(GmuxEngine *e) {
    bool at_prompt = false;
    return shell_primary(e) && !ghostty_terminal_get(e->terminal,
        GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT, &at_prompt) && at_prompt;
}

int gmux_engine_prompt(GmuxEngine *e, bool previous) {
    if (!shell_primary(e) || e->held) return 1;
    shell_prune(e);
    GhosttyTerminalScrollbar scrollbar;
    if (ghostty_terminal_get(e->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &scrollbar)) return -1;
    // Binary search tracked rows. Ordinary output/pruning/reflow preserves
    // their order, and no retained text is extracted on this path.
    size_t lo = 0, hi = e->prompt_count;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        GhosttyPointCoordinate p;
        bool before = ghostty_tracked_grid_ref_point(shell_prompt(e, mid)->point, GHOSTTY_POINT_TAG_SCREEN, &p) ||
            (previous ? p.y < scrollbar.offset : p.y <= scrollbar.offset);
        if (before) lo = mid + 1; else hi = mid;
    }
    size_t i = previous ? lo : lo + 1;
    while (previous ? i > 0 : i <= e->prompt_count) {
        size_t index = previous ? --i : i++ - 1;
        GhosttyTrackedGridRef anchor = shell_prompt(e, index)->point;
        GhosttyPointCoordinate p;
        if (ghostty_tracked_grid_ref_point(anchor, GHOSTTY_POINT_TAG_SCREEN, &p) ||
            !shell_prompt_valid(anchor)) continue;
        gmux_engine_scroll_to(e, p.y);
        return 0;
    }
    return 1;
}

int gmux_engine_output_anchor(GmuxEngine *e, uint16_t x, uint32_t y) {
    if (!shell_primary(e) || e->held) { shell_output_clear(&e->output_anchor); return 1; }
    GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = {x, y}};
    return shell_output_track(e, &e->output_anchor, point);
}

static GhosttyResult shell_output_select(GmuxEngine *e, OutputAnchor *anchor, GhosttySelection *selection) {
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    if (!shell_output_get(e, anchor, &ref)) return GHOSTTY_NO_VALUE;
    return ghostty_terminal_select_output(e->terminal, ref, selection);
}

int gmux_engine_select_output(GmuxEngine *e, uint8_t target, uint16_t x, uint32_t y) {
    if (!shell_primary(e) || e->held) return 1;
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    GhosttyResult result;
    if (target == 1) {
        GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
        GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = {x, y}};
        if (ghostty_terminal_grid_ref(e->terminal, point, &ref)) return 1;
        result = ghostty_terminal_select_output(e->terminal, ref, &selection);
    } else {
        OutputAnchor *anchor = target == 2 ? &e->output_anchor : &e->completed_output;
        result = shell_output_select(e, anchor, &selection);
        // A completed target can still exist on the next prompt when its
        // command printed nothing. Fall back only for the default action.
        if (result == GHOSTTY_NO_VALUE && target == 0 && e->output_open)
            result = shell_output_select(e, &e->output, &selection);
    }
    if (result == GHOSTTY_NO_VALUE) return 1;
    if (result) return -1;
    GhosttyPointCoordinate start, end;
    if (ghostty_terminal_point_from_grid_ref(e->terminal, &selection.start, GHOSTTY_POINT_TAG_SCREEN, &start) ||
        ghostty_terminal_point_from_grid_ref(e->terminal, &selection.end, GHOSTTY_POINT_TAG_SCREEN, &end)) return -1;
    gmux_engine_clear_selection(e);
    return gmux_engine_select(e, start.x, start.y, end.x, end.y, 0);
}
