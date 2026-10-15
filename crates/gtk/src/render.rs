pub type Rect = (f64, f64, f64, f64);

// Block elements are terminal graphics, not font glyphs. Their geometry must
// reach the cell edges even when the selected font has bearings or line spacing.
fn block_mask(ch: char) -> Option<u64> {
    let cp = ch as u32;
    let mut mask = 0u64;
    match cp {
        0x2580 => return Some(0xffff_ffff),
        0x2581..=0x2588 => return Some(u64::MAX << ((0x2588 - cp) * 8)),
        0x2589..=0x258f | 0x2590 | 0x2594 | 0x2595 => {
            let row = match cp {
                0x2589..=0x258f => (1u64 << (0x2590 - cp)) - 1,
                0x2590 => 0xf0,
                0x2594 => return Some(0xff),
                _ => 0x80,
            };
            for y in 0..8 {
                mask |= row << (y * 8);
            }
        }
        0x2591..=0x2593 => {
            for y in 0..8 {
                for x in 0..8 {
                    let paint = match cp {
                        0x2591 => (x + 2 * y) % 4 == 0,
                        0x2592 => (x + y) % 2 == 0,
                        _ => (x + 2 * y) % 4 != 0,
                    };
                    if paint {
                        mask |= 1 << (y * 8 + x);
                    }
                }
            }
        }
        0x2596..=0x259f => {
            // Quadrants in reading order: upper left, upper right, lower left,
            // lower right. Each bit contributes exactly one quarter of the cell.
            let quadrants = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14][(cp - 0x2596) as usize];
            for y in 0..8 {
                let shift = if y < 4 { 0 } else { 2 };
                let row = if quadrants & (1 << shift) != 0 {
                    0x0f
                } else {
                    0
                } | if quadrants & (2 << shift) != 0 {
                    0xf0
                } else {
                    0
                };
                mask |= row << (y * 8);
            }
        }
        _ => return None,
    }
    Some(mask)
}

pub fn graphic_rects(text: &str, rect: Rect, mut paint: impl FnMut(Rect)) -> bool {
    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }
    box_rects(ch, rect, &mut paint) || block_rects(ch, rect, &mut paint)
}

fn box_rects(ch: char, (x, y, width, height): Rect, paint: &mut impl FnMut(Rect)) -> bool {
    // Connections: left, right, up, down. Common single and heavy borders
    // share the same stroke width and centers in every neighboring cell.
    let (arms, heavy) = match ch {
        '─' => (3, false),
        '━' => (3, true),
        '│' => (12, false),
        '┃' => (12, true),
        '┌' => (10, false),
        '┏' => (10, true),
        '┐' => (9, false),
        '┓' => (9, true),
        '└' => (6, false),
        '┗' => (6, true),
        '┘' => (5, false),
        '┛' => (5, true),
        '├' => (14, false),
        '┣' => (14, true),
        '┤' => (13, false),
        '┫' => (13, true),
        '┬' => (11, false),
        '┳' => (11, true),
        '┴' => (7, false),
        '┻' => (7, true),
        '┼' => (15, false),
        '╋' => (15, true),
        '╴' => (1, false),
        '╸' => (1, true),
        '╶' => (2, false),
        '╺' => (2, true),
        '╵' => (4, false),
        '╹' => (4, true),
        '╷' => (8, false),
        '╻' => (8, true),
        _ => return false,
    };
    let stroke = (width / if heavy { 4.0 } else { 8.0 }).ceil().max(1.0);
    let cx = x + width / 2.0;
    let cy = y + height / 2.0;
    // Non-overlapping arms also preserve faint colors in the GPU path.
    paint((cx - stroke / 2.0, cy - stroke / 2.0, stroke, stroke));
    if arms & 1 != 0 {
        paint((x, cy - stroke / 2.0, (width - stroke) / 2.0, stroke));
    }
    if arms & 2 != 0 {
        paint((
            cx + stroke / 2.0,
            cy - stroke / 2.0,
            (width - stroke) / 2.0,
            stroke,
        ));
    }
    if arms & 4 != 0 {
        paint((cx - stroke / 2.0, y, stroke, (height - stroke) / 2.0));
    }
    if arms & 8 != 0 {
        paint((
            cx - stroke / 2.0,
            cy + stroke / 2.0,
            stroke,
            (height - stroke) / 2.0,
        ));
    }
    true
}

fn block_rects(ch: char, (x, y, width, height): Rect, paint: &mut impl FnMut(Rect)) -> bool {
    let Some(mask) = block_mask(ch) else {
        return false;
    };
    let mut row = 0;
    while row < 8 {
        let bits = (mask >> (row * 8)) as u8;
        let mut end = row + 1;
        while end < 8 && (mask >> (end * 8)) as u8 == bits {
            end += 1;
        }
        let mut column = 0;
        while column < 8 {
            if bits & (1 << column) == 0 {
                column += 1;
                continue;
            }
            let start = column;
            while column < 8 && bits & (1 << column) != 0 {
                column += 1;
            }
            paint((
                x + width * start as f64 / 8.0,
                y + height * row as f64 / 8.0,
                width * (column - start) as f64 / 8.0,
                height * (end - row) as f64 / 8.0,
            ));
        }
        row = end;
    }
    true
}

pub fn cursor_rects(style: u8, focused: bool, (x, y, w, h): Rect, mut paint: impl FnMut(Rect)) {
    match style {
        0 => paint((x, y, 2.0, h)),
        1 if focused => paint((x, y, w, h)),
        2 => paint((x, y + h - 2.0, w, 2.0)),
        _ => {
            // Non-overlapping edges keep an unfocused cursor's opacity uniform.
            for rect in [
                (x, y, w, 1.0),
                (x, y + h - 1.0, w, 1.0),
                (x, y + 1.0, 1.0, h - 2.0),
                (x + w - 1.0, y + 1.0, 1.0, h - 2.0),
            ] {
                paint(rect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(text: &str) -> Vec<Rect> {
        let mut result = Vec::new();
        assert!(graphic_rects(text, (0.0, 0.0, 9.0, 21.0), |rect| result.push(rect)));
        result
    }

    fn assert_disjoint(rects: &[Rect]) {
        for (i, &(x, y, w, h)) in rects.iter().enumerate() {
            assert!(w > 0.0 && h > 0.0);
            for &(a, b, c, d) in &rects[i + 1..] {
                assert!(
                    x + w <= a || a + c <= x || y + h <= b || b + d <= y,
                    "Overlapping rectangles would paint faint cells twice"
                );
            }
        }
    }

    #[test]
    fn blocks_cover_cell_edges_and_complementary_halves_join() {
        assert_eq!(rects("█"), [(0.0, 0.0, 9.0, 21.0)]);
        let halves = [rects("▀"), rects("▄")].concat();
        assert_disjoint(&halves);
        assert_eq!(halves.iter().map(|r| r.2 * r.3).sum::<f64>(), 9.0 * 21.0);
        assert_eq!(
            block_mask('▚').unwrap() | block_mask('▞').unwrap(),
            u64::MAX
        );
        assert_eq!(block_mask('░').unwrap().count_ones(), 16);
        assert_eq!(block_mask('▒').unwrap().count_ones(), 32);
        assert_eq!(block_mask('▓').unwrap().count_ones(), 48);
        for cp in 0x2580..=0x259f {
            assert_disjoint(&rects(&char::from_u32(cp).unwrap().to_string()));
        }
    }

    #[test]
    fn box_arms_connect_at_cell_edges_without_overpainting() {
        for text in [
            "─", "━", "│", "┃", "┌", "┏", "┐", "┓", "└", "┗", "┘", "┛", "├", "┣", "┤", "┫", "┬",
            "┳", "┴", "┻", "┼", "╋",
        ] {
            assert_disjoint(&rects(text));
        }
        let vertical = rects("┃");
        assert!(vertical.iter().any(|r| r.1 == 0.0));
        assert!(vertical.iter().any(|r| r.1 + r.3 == 21.0));
        let horizontal = rects("─");
        assert!(horizontal.iter().any(|r| r.0 == 0.0));
        assert!(horizontal.iter().any(|r| r.0 + r.2 == 9.0));
    }

    #[test]
    fn cluster_text_uses_font_fallback_instead_of_graphic_geometry() {
        for text in ["", "A", "界", "█\u{fe0f}", "──"] {
            assert!(!graphic_rects(text, (0.0, 0.0, 9.0, 21.0), |_| panic!()));
        }
    }

    #[test]
    fn cursor_shapes_preserve_uniform_opacity_and_wide_cell_width() {
        for focused in [true, false] {
            let mut result = Vec::new();
            cursor_rects(1, focused, (0.0, 0.0, 18.0, 21.0), |rect| result.push(rect));
            assert_disjoint(&result);
            if focused {
                assert_eq!(result, [(0.0, 0.0, 18.0, 21.0)]);
            } else {
                assert_eq!(
                    result.iter().map(|r| r.2 * r.3).sum::<f64>(),
                    2.0 * 18.0 + 2.0 * 19.0
                );
            }
        }
    }
}
