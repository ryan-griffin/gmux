// Exercise the installed native API directly, independently of cell_style().
#include <ghostty/vt/terminal.h>
#include <ghostty/vt/grid_ref.h>
#include <assert.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    GhosttyTerminal terminal;
    assert(!ghostty_terminal_new(NULL, &terminal, 12, 3));
    const char *sequences[] = {"\x1b[5mA", "\x1b[25mB", "\x1b[6mC", "\x1b[0mD"};
    const bool expected[] = {true, false, true, false};
    for (size_t x = 0; x < 4; x++) {
        ghostty_terminal_vt_write(terminal, (const uint8_t *)sequences[x], strlen(sequences[x]));
        GhosttyStyle cursor = GHOSTTY_INIT_SIZED(GhosttyStyle);
        assert(!ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_STYLE, &cursor));
        assert(cursor.blink == expected[x]);
        GhosttyGridRef ref;
        GhosttyPoint point = {.tag = GHOSTTY_POINT_TAG_ACTIVE, .value.coordinate = {.x = x, .y = 0}};
        assert(!ghostty_terminal_grid_ref(terminal, point, &ref));
        GhosttyStyle cell = GHOSTTY_INIT_SIZED(GhosttyStyle);
        assert(!ghostty_grid_ref_style(&ref, &cell));
        assert(cell.blink == expected[x]);
    }
    ghostty_terminal_free(terminal);
    puts("PASS: native SGR 5/6 set the same retained blink flag; SGR 25/0 reset it");
}
