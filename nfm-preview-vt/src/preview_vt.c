// Adapted from ghostty2 example/rust-egui-terminal/src/picker/preview_vt.c.
// See ../LICENSE-ghostty for the original source license.
#include <ghostty/vt/terminal.h>
#include <ghostty/vt/grid_ref.h>
#include <ghostty/vt/style.h>
#include <ghostty/vt/color.h>
#include <string.h>

typedef struct {
    uint32_t codepoints[8];
    uint32_t foreground;
    uint32_t background;
    uint32_t underline_color;
    uint8_t length;
    uint8_t flags;
    uint8_t underline;
    uint8_t reserved;
} NfmPreviewCell;

static uint32_t rgb(GhosttyColorRgb color) {
    return ((uint32_t)color.r << 16) | ((uint32_t)color.g << 8) | color.b;
}

static uint32_t style_color(GhosttyStyleColor color,
                            const GhosttyColorRgb *palette,
                            uint32_t fallback) {
    if (color.tag == GHOSTTY_STYLE_COLOR_RGB) return rgb(color.value.rgb);
    if (color.tag == GHOSTTY_STYLE_COLOR_PALETTE)
        return rgb(palette[color.value.palette]);
    return fallback;
}

// Immutable after construction; owned by AnsiDocument in Rust.
void *nfm_preview_vt_new(const uint8_t *bytes, size_t length,
                         uint16_t columns, uint16_t rows, size_t max_lines, size_t *out_total_rows) {
    if (!columns || !rows || !out_total_rows || (length && !bytes)) return NULL;
    GhosttyTerminal terminal = NULL;
    if (ghostty_terminal_new(NULL, &terminal, columns, rows) != GHOSTTY_SUCCESS)
        return NULL;
    // Use the line bound below. The terminal's default 10,000-byte bound
    // would prune earlier rows and invalidate output-relative center rows.
    if (ghostty_terminal_set(terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                             NULL) != GHOSTTY_SUCCESS) goto fail;
    // Preserve the existing bounded preview scrollback policy.

    if (ghostty_terminal_set(terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                             &max_lines) != GHOSTTY_SUCCESS) goto fail;
    ghostty_terminal_vt_write(terminal, bytes, length);
    if (ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS,
                             out_total_rows) != GHOSTTY_SUCCESS) goto fail;
    return terminal;
fail:
    ghostty_terminal_free(terminal);
    return NULL;
}

void nfm_preview_vt_free(void *terminal) {
    ghostty_terminal_free(terminal);
}

bool nfm_preview_vt_viewport(void *terminal, uint16_t columns, uint16_t rows,
                            size_t first_row, uint32_t default_foreground,
                            uint32_t default_background, NfmPreviewCell *cells) {
    if (!terminal || !cells || !columns || !rows) return false;
    memset(cells, 0, (size_t)columns * rows * sizeof(*cells));
    GhosttyColorRgb palette[256];
    ghostty_color_palette_default(palette);
    for (uint32_t y = 0; y < rows; ++y) {
        for (uint16_t x = 0; x < columns; ++x) {
            GhosttyPoint point = {0};
            point.tag = GHOSTTY_POINT_TAG_SCREEN;
            point.value.coordinate.x = x;
            point.value.coordinate.y = first_row + y;
            GhosttyGridRef ref = {0};
            ref.size = sizeof(ref);
            if (ghostty_terminal_grid_ref(terminal, point, &ref) != GHOSTTY_SUCCESS)
                continue;
            NfmPreviewCell *out = &cells[(size_t)y * columns + x];
            size_t count = 0;
            if (ghostty_grid_ref_graphemes(&ref, out->codepoints, 8, &count) == GHOSTTY_SUCCESS)
                out->length = (uint8_t)(count > 8 ? 8 : count);
            GhosttyStyle style = {0};
            style.size = sizeof(style);
            if (ghostty_grid_ref_style(&ref, &style) != GHOSTTY_SUCCESS)
                continue;
            out->foreground = style_color(style.fg_color, palette, default_foreground);
            out->background = style_color(style.bg_color, palette, default_background);
            out->underline_color = style_color(style.underline_color, palette, out->foreground);
            out->flags = (style.bold ? 1 : 0) | (style.italic ? 2 : 0) |
                         (style.strikethrough ? 4 : 0) | (style.inverse ? 8 : 0) |
                         (style.invisible ? 16 : 0) | (style.faint ? 32 : 0);
            out->underline = style.underline ? 1 : 0;
        }
    }
    return true;
}

// Read one physical row. The first call measures the codepoint buffer; the
// second fills it. Offsets delimit each cell's full grapheme (no fixed limit).
bool nfm_preview_vt_text_row(void *terminal, size_t y, uint16_t columns,
                            uint32_t *codepoints, size_t capacity,
                            size_t *offsets, uint8_t *widths,
                            size_t *required, bool *wraps) {
    if (!terminal || !columns || !offsets || !widths || !required || !wraps)
        return false;
    size_t used = 0;
    for (uint16_t x = 0; x < columns; ++x) {
        GhosttyPoint point = {0};
        point.tag = GHOSTTY_POINT_TAG_SCREEN;
        point.value.coordinate.x = x;
        point.value.coordinate.y = y;
        GhosttyGridRef ref = {0};
        ref.size = sizeof(ref);
        if (ghostty_terminal_grid_ref(terminal, point, &ref) != GHOSTTY_SUCCESS)
            return false;
        if (x == 0) {
            GhosttyRow row;
            if (ghostty_grid_ref_row(&ref, &row) != GHOSTTY_SUCCESS ||
                ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, wraps) != GHOSTTY_SUCCESS)
                return false;
        }
        GhosttyCell cell;
        GhosttyCellWide wide;
        if (ghostty_grid_ref_cell(&ref, &cell) != GHOSTTY_SUCCESS ||
            ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide) != GHOSTTY_SUCCESS)
            return false;
        widths[x] = wide == GHOSTTY_CELL_WIDE_WIDE ? 2 :
            (wide == GHOSTTY_CELL_WIDE_SPACER_HEAD ||
             wide == GHOSTTY_CELL_WIDE_SPACER_TAIL ? 0 : 1);
        offsets[x] = used;
        size_t count = 0;
        GhosttyResult result = ghostty_grid_ref_graphemes(&ref, NULL, 0, &count);
        if (result != GHOSTTY_SUCCESS && result != GHOSTTY_OUT_OF_SPACE) return false;
        if (count > SIZE_MAX - used) return false;
        if (codepoints && count) {
            if (used > capacity || count > capacity - used) return false;
            if (ghostty_grid_ref_graphemes(&ref, codepoints + used, count, &count)
                != GHOSTTY_SUCCESS) return false;
        }
        used += count;
    }
    offsets[columns] = used;
    *required = used;
    return true;
}
