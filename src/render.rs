use gtk::cairo::{Antialias, Context};

type Rect = (f64, f64, f64, f64);

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

pub fn draw_graphic(cr: &Context, text: &str, rect: Rect) -> bool {
    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }
    draw_box(cr, ch, rect) || draw_block(cr, ch, rect)
}

fn draw_box(cr: &Context, ch: char, (x, y, width, height): Rect) -> bool {
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
    let _ = cr.save();
    cr.set_antialias(Antialias::None);
    cr.new_path();
    if arms & 1 != 0 {
        cr.rectangle(x, cy - stroke / 2.0, width / 2.0 + stroke / 2.0, stroke);
    }
    if arms & 2 != 0 {
        cr.rectangle(
            cx - stroke / 2.0,
            cy - stroke / 2.0,
            width / 2.0 + stroke / 2.0,
            stroke,
        );
    }
    if arms & 4 != 0 {
        cr.rectangle(cx - stroke / 2.0, y, stroke, height / 2.0 + stroke / 2.0);
    }
    if arms & 8 != 0 {
        cr.rectangle(
            cx - stroke / 2.0,
            cy - stroke / 2.0,
            stroke,
            height / 2.0 + stroke / 2.0,
        );
    }
    let _ = cr.fill();
    let _ = cr.restore();
    true
}

fn draw_block(cr: &Context, ch: char, (x, y, width, height): Rect) -> bool {
    let Some(mask) = block_mask(ch) else {
        return false;
    };
    let _ = cr.save();
    // Shared boundaries snap to device pixels. Antialiasing each cell separately
    // would leave seams, especially at fractional display scales.
    cr.set_antialias(Antialias::None);
    cr.new_path();
    for row in 0..8 {
        let bits = (mask >> (row * 8)) as u8;
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
            cr.rectangle(
                x + width * start as f64 / 8.0,
                y + height * row as f64 / 8.0,
                width * (column - start) as f64 / 8.0,
                height / 8.0,
            );
        }
    }
    let _ = cr.fill();
    let _ = cr.restore();
    true
}

pub fn draw_cursor(cr: &Context, style: u8, focused: bool, (x, y, width, height): Rect) {
    let _ = cr.save();
    match style {
        0 => cr.rectangle(x, y, 2.0, height),
        1 if focused => {
            cr.set_antialias(Antialias::None);
            cr.rectangle(x, y, width, height);
        }
        2 => cr.rectangle(x, y + height - 2.0, width, 2.0),
        _ => {
            cr.rectangle(x + 0.5, y + 0.5, width - 1.0, height - 1.0);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            let _ = cr.restore();
            return;
        }
    }
    let _ = cr.fill();
    let _ = cr.restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::cairo::{Format, ImageSurface};

    #[test]
    fn adjacent_blocks_have_no_seams_at_fractional_scale() {
        let mut surface = ImageSurface::create(Format::ARgb32, 27, 63).unwrap();
        let cr = Context::new(&surface).unwrap();
        cr.scale(1.5, 1.5);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        for y in 0..2 {
            for x in 0..2 {
                assert!(draw_graphic(
                    &cr,
                    "█",
                    (x as f64 * 9.0, y as f64 * 21.0, 9.0, 21.0)
                ));
            }
        }
        drop(cr);
        let data = surface.data().unwrap();
        assert!(
            data.as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [255; 4])
        );
    }

    #[test]
    fn half_blocks_and_quadrants_join_without_gaps() {
        let mut surface = ImageSurface::create(Format::ARgb32, 9, 21).unwrap();
        let cr = Context::new(&surface).unwrap();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        assert!(draw_graphic(&cr, "▀", (0.0, 0.0, 9.0, 21.0)));
        assert!(draw_graphic(&cr, "▄", (0.0, 0.0, 9.0, 21.0)));
        drop(cr);
        assert!(
            surface
                .data()
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [255; 4])
        );
        assert_eq!(
            block_mask('▚').unwrap() | block_mask('▞').unwrap(),
            u64::MAX
        );
        assert_eq!(block_mask('░').unwrap().count_ones(), 16);
        assert_eq!(block_mask('▒').unwrap().count_ones(), 32);
        assert_eq!(block_mask('▓').unwrap().count_ones(), 48);
    }

    #[test]
    fn vertical_box_border_connects_across_rows() {
        let mut surface = ImageSurface::create(Format::ARgb32, 9, 42).unwrap();
        let cr = Context::new(&surface).unwrap();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        for y in 0..2 {
            assert!(draw_graphic(&cr, "┃", (0.0, y as f64 * 21.0, 9.0, 21.0)));
        }
        drop(cr);
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        for y in 0..42 {
            assert_eq!(data[y * stride + 4 * 4 + 3], 255);
            assert_eq!(data[y * stride + 3], 0);
        }
    }

    #[test]
    fn focused_block_cursor_is_filled_and_unfocused_cursor_is_hollow() {
        for focused in [true, false] {
            let mut surface = ImageSurface::create(Format::ARgb32, 10, 20).unwrap();
            let cr = Context::new(&surface).unwrap();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            draw_cursor(&cr, 1, focused, (0.0, 0.0, 10.0, 20.0));
            drop(cr);
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            assert_eq!(data[10 * stride + 5 * 4 + 3], if focused { 255 } else { 0 });
        }
    }
}
