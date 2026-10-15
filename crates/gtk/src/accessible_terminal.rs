//! Character offsets for GTK's accessible text interface, including wide cells.
use gmux_client::{Cell, Frame};
use std::ops::Range;

#[derive(Default)]
pub struct Text {
    pub value: String,
    pub offsets: Vec<u32>,
    pub caret: u32,
    pub selection: Option<(u32, u32)>,
    pub cols: usize,
    cell_ends: Vec<u32>,
    spans: Vec<Range<usize>>,
}
impl Text {
    pub fn from_frame(frame: Frame, cells: &[Cell], selection: Option<(usize, usize)>) -> Self {
        let mut text = Self::default();
        text.rebuild(frame, cells, selection);
        text
    }
    pub fn rebuild(&mut self, frame: Frame, cells: &[Cell], selection: Option<(usize, usize)>) {
        let cols = frame.cols as usize;
        self.cols = cols;
        self.value.clear();
        self.offsets.clear();
        self.cell_ends.clear();
        self.spans.clear();
        self.value.reserve(cells.len());
        self.offsets.reserve(cells.len() + 1);
        self.cell_ends.reserve(cells.len());
        self.spans.reserve(cells.len());
        let mut offset = 0;
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 && cols > 0 && i % cols == 0 {
                self.value.push('\n');
                self.spans.push(i - 1..i);
                offset += 1;
            }
            if cell.flags & 64 != 0 && i > 0 && cols > 0 && i % cols != 0 {
                self.offsets.push(self.offsets[i - 1]);
                self.cell_ends.push(offset);
            } else {
                self.offsets.push(offset);
                let glyph = cell.text();
                let glyph = if glyph.is_empty() { " " } else { glyph };
                self.value.push_str(glyph);
                let len = glyph.chars().count();
                let wide = cols > 0
                    && (i + 1) % cols != 0
                    && cells.get(i + 1).is_some_and(|cell| cell.flags & 64 != 0);
                self.spans
                    .extend(std::iter::repeat_n(i..i + if wide { 2 } else { 1 }, len));
                offset += len as u32;
                self.cell_ends.push(offset);
            }
        }
        self.offsets.push(offset);
        self.update_positions(frame, selection);
        if self.has_excess_capacity(self.used_bytes()) {
            self.value.shrink_to_fit();
            self.offsets.shrink_to_fit();
            self.cell_ends.shrink_to_fit();
            self.spans.shrink_to_fit();
        }
    }
    /// Heap capacity, excluding allocator overhead and the Text struct itself.
    pub fn capacity_bytes(&self) -> usize {
        self.value.capacity()
            + (self.offsets.capacity() + self.cell_ends.capacity()) * size_of::<u32>()
            + self.spans.capacity() * size_of::<Range<usize>>()
    }
    pub fn used_bytes(&self) -> usize {
        self.value.len()
            + (self.offsets.len() + self.cell_ends.len()) * size_of::<u32>()
            + self.spans.len() * size_of::<Range<usize>>()
    }
    pub fn has_excess_capacity(&self, used_bytes: usize) -> bool {
        // Allow slack for ordinary resizes. Reclaim capacity above four times
        // the current text and mappings' live storage.
        self.capacity_bytes() > used_bytes.saturating_mul(4)
    }
    pub fn char_count(&self) -> u32 {
        self.offsets.last().copied().unwrap_or(0)
    }
    pub fn update_positions(&mut self, frame: Frame, selection: Option<(usize, usize)>) {
        self.caret = self
            .offsets
            .get(frame.cursor_y as usize * self.cols + frame.cursor_x as usize)
            .copied()
            .unwrap_or(self.char_count());
        self.selection = selection
            .and_then(|(start, end)| Some((*self.offsets.get(start)?, *self.cell_ends.get(end)?)));
    }
    pub fn bytes(&self, start: u32, end: u32) -> Vec<u8> {
        let mut bytes: Vec<_> = self
            .value
            .chars()
            .skip(start as usize)
            .take(end.saturating_sub(start) as usize)
            .collect::<String>()
            .into_bytes();
        bytes.push(0);
        bytes
    }
    pub fn extents(&self, start: u32, end: u32, cw: f32, ch: f32) -> Option<(f32, f32, f32, f32)> {
        if self.cols == 0
            || !cw.is_finite()
            || !ch.is_finite()
            || cw <= 0.0
            || ch <= 0.0
            || start > end
            || end as usize > self.spans.len()
        {
            return None;
        }
        if start == end {
            let cell = self
                .spans
                .get(start as usize)
                .map(|span| span.start)
                .unwrap_or(self.offsets.len().checked_sub(1)?);
            let (x, y) = if cell > 0 && cell == self.offsets.len() - 1 {
                (self.cols, (cell - 1) / self.cols)
            } else {
                (cell % self.cols, cell / self.cols)
            };
            return Some((x as f32 * cw, y as f32 * ch, 0.0, ch));
        }
        let first = self.spans[start as usize].start;
        let last = self.spans[end as usize - 1].end - 1;
        let same_row = first / self.cols == last / self.cols;
        Some((
            if same_row {
                (first % self.cols) as f32 * cw
            } else {
                0.0
            },
            (first / self.cols) as f32 * ch,
            if same_row {
                (last - first + 1) as f32 * cw
            } else {
                self.cols as f32 * cw
            },
            (last / self.cols - first / self.cols + 1) as f32 * ch,
        ))
    }
    pub fn offset_at(&self, x: f32, y: f32, cw: f32, ch: f32) -> Option<u32> {
        let rows = self
            .offsets
            .len()
            .saturating_sub(1)
            .checked_div(self.cols)?;
        if ![x, y, cw, ch].iter().all(|n| n.is_finite())
            || cw <= 0.0
            || ch <= 0.0
            || x < 0.0
            || y < 0.0
            || x >= self.cols as f32 * cw
            || y >= rows as f32 * ch
        {
            return None;
        }
        self.offsets
            .get((y / ch) as usize * self.cols + (x / cw) as usize)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocations(text: &Text) -> (*const u8, *const u32, *const u32, *const Range<usize>) {
        (
            text.value.as_ptr(),
            text.offsets.as_ptr(),
            text.cell_ends.as_ptr(),
            text.spans.as_ptr(),
        )
    }

    #[test]
    fn identical_text_with_different_cell_layout_updates_positions_and_geometry() {
        let frame = Frame {
            cols: 2,
            rows: 1,
            cursor_x: 1,
            ..Frame::default()
        };
        let mut cells = vec![Cell::default(); 2];
        cells[0].text[..3].copy_from_slice("e\u{301}".as_bytes());
        cells[1].text[0] = b'X';
        let mut text = Text::from_frame(frame, &cells, Some((0, 0)));
        assert_eq!(text.value, "e\u{301}X");
        assert_eq!(text.caret, 2);
        assert_eq!(text.selection, Some((0, 2)));
        assert_eq!(text.extents(1, 2, 8.0, 16.0), Some((0.0, 0.0, 8.0, 16.0)));
        cells[0].text = [0; 64];
        cells[0].text[0] = b'e';
        cells[1].text[..3].copy_from_slice("\u{301}X".as_bytes());
        text.rebuild(frame, &cells, Some((0, 0)));
        assert_eq!(text.value, "e\u{301}X");
        assert_eq!(text.caret, 1);
        assert_eq!(text.selection, Some((0, 1)));
        assert_eq!(text.extents(1, 2, 8.0, 16.0), Some((8.0, 0.0, 8.0, 16.0)));
        assert_eq!(text.offset_at(12.0, 8.0, 8.0, 16.0), Some(1));
    }

    #[test]
    fn rebuild_reuses_storage_and_updates_unicode_row_mappings() {
        let frame = Frame {
            cols: 4,
            rows: 2,
            ..Frame::default()
        };
        let mut cells = vec![Cell::default(); 8];
        for (cell, glyph) in cells
            .iter_mut()
            .zip(["界", "", "e\u{301}", "X", "👩‍💻", "", "b", ""])
        {
            cell.text[..glyph.len()].copy_from_slice(glyph.as_bytes());
        }
        cells[1].flags = 64;
        cells[5].flags = 64;
        let mut text = Text::from_frame(frame, &cells, Some((0, 5)));
        assert_eq!(text.value, "界e\u{301}X\n👩‍💻b ");
        let storage = allocations(&text);
        // A one-row update leaves the first row's offsets and geometry intact.
        cells[6].text[0] = b'c';
        text.rebuild(
            Frame {
                cursor_x: 2,
                cursor_y: 1,
                ..frame
            },
            &cells,
            Some((5, 6)),
        );
        assert_eq!(allocations(&text), storage);
        assert_eq!(text.value, "界e\u{301}X\n👩‍💻c ");
        assert_eq!(text.offsets, [0, 0, 1, 3, 5, 5, 8, 9, 10]);
        assert_eq!(text.caret, 8);
        assert_eq!(text.selection, Some((5, 9)));
        assert_eq!(text.bytes(5, 8), "👩‍💻\0".as_bytes());
        assert_eq!(text.extents(5, 8, 8.0, 16.0), Some((0.0, 16.0, 16.0, 16.0)));
        assert_eq!(text.offset_at(12.0, 24.0, 8.0, 16.0), Some(5));
        // Smaller and empty grids must not retain old offsets or selections.
        text.rebuild(
            Frame {
                cols: 2,
                rows: 1,
                ..Frame::default()
            },
            &cells[..2],
            None,
        );
        assert_eq!(text.value, "界");
        assert_eq!(text.offsets, [0, 0, 1]);
        assert_eq!(text.selection, None);
        assert_eq!(text.extents(0, 1, 8.0, 16.0), Some((0.0, 0.0, 16.0, 16.0)));
        text.rebuild(Frame::default(), &[], Some((0, 0)));
        assert!(text.value.is_empty());
        assert_eq!(text.offsets, [0]);
        assert_eq!(text.char_count(), 0);
        assert_eq!(text.caret, 0);
        assert_eq!(text.selection, None);
        assert!(text.spans.is_empty());
        assert!(text.cell_ends.is_empty());
    }

    #[test]
    fn shrinking_a_large_viewport_releases_capacity_and_preserves_unicode_mappings() {
        let mut text = Text::from_frame(
            Frame {
                cols: 512,
                rows: 256,
                ..Frame::default()
            },
            &vec![Cell::default(); 512 * 256],
            None,
        );
        assert!(text.capacity_bytes() > 5 * 1024 * 1024);
        let frame = Frame {
            cols: 80,
            rows: 24,
            cursor_x: 2,
            ..Frame::default()
        };
        let mut cells = vec![Cell::default(); 80 * 24];
        cells[0].text[..3].copy_from_slice("界".as_bytes());
        cells[1].flags = 64;
        cells[2].text[..3].copy_from_slice("e\u{301}".as_bytes());
        let expected = Text::from_frame(frame, &cells, Some((0, 2)));
        // The canvas must drop its old, large text instead of keeping it as a
        // spare, even if there are no further text changes after this resize.
        assert!(text.has_excess_capacity(expected.used_bytes()));
        text.rebuild(frame, &cells, Some((0, 2)));
        assert!(text.capacity_bytes() <= expected.capacity_bytes());
        assert!(text.capacity_bytes() < 100 * 1024);
        assert!(!text.has_excess_capacity(expected.used_bytes()));
        assert_eq!(text.value, expected.value);
        assert_eq!(text.offsets, expected.offsets);
        assert_eq!(text.cell_ends, expected.cell_ends);
        assert_eq!(text.spans, expected.spans);
        assert_eq!(text.caret, 1);
        assert_eq!(text.selection, Some((0, 3)));
        assert_eq!(text.extents(0, 1, 8.0, 16.0), Some((0.0, 0.0, 16.0, 16.0)));
        assert_eq!(text.extents(1, 3, 8.0, 16.0), Some((16.0, 0.0, 8.0, 16.0)));
        assert_eq!(text.offset_at(12.0, 8.0, 8.0, 16.0), Some(0));
        assert_eq!(text.offset_at(20.0, 8.0, 8.0, 16.0), Some(1));
    }
    #[test]
    fn text_offsets_count_characters_and_preserve_wide_clusters() {
        let mut cells = vec![Cell::default(); 4];
        cells[0].text[..3].copy_from_slice("界".as_bytes());
        cells[1].flags = 64;
        cells[2].text[..3].copy_from_slice("e\u{301}".as_bytes());
        let text = Text::from_frame(
            Frame {
                cols: 4,
                rows: 1,
                cursor_x: 2,
                ..Frame::default()
            },
            &cells,
            Some((0, 2)),
        );
        assert_eq!(text.value, "界e\u{301} ");
        assert_eq!(text.caret, 1);
        assert_eq!(text.selection, Some((0, 3)));
        assert_eq!(text.bytes(0, 1), "界\0".as_bytes());
        assert_eq!(text.extents(0, 1, 8.0, 16.0), Some((0.0, 0.0, 16.0, 16.0)));
        assert_eq!(text.extents(1, 2, 8.0, 16.0), Some((16.0, 0.0, 8.0, 16.0)));
        assert_eq!(text.extents(2, 3, 8.0, 16.0), text.extents(1, 2, 8.0, 16.0));
        assert_eq!(text.offset_at(12.0, 8.0, 8.0, 16.0), Some(0));
        assert_eq!(text.offset_at(20.0, 8.0, 8.0, 16.0), Some(1));
    }
    #[test]
    fn selection_updates_respect_row_ends_and_wide_cells() {
        let frame = Frame {
            cols: 4,
            rows: 2,
            ..Frame::default()
        };
        let mut cells = vec![Cell::default(); 8];
        for (cell, value) in cells
            .iter_mut()
            .zip(["a", "b", "界", "", "e\u{301}", "X", "", ""])
        {
            cell.text[..value.len()].copy_from_slice(value.as_bytes());
        }
        cells[3].flags = 64;
        let mut text = Text::from_frame(frame, &cells, None);
        assert_eq!(text.value, "ab界\ne\u{301}X  ");
        assert_eq!(text.char_count(), 9);
        text.update_positions(
            Frame {
                cursor_x: 3,
                ..frame
            },
            Some((1, 3)),
        );
        assert_eq!(text.caret, 2);
        assert_eq!(text.selection, Some((1, 3)));
        assert_eq!(text.bytes(1, 3), "b界\0".as_bytes());
        text.update_positions(
            Frame {
                cursor_x: 1,
                cursor_y: 1,
                ..frame
            },
            Some((3, 4)),
        );
        assert_eq!(text.caret, 6);
        assert_eq!(text.selection, Some((2, 6)));
        assert_eq!(text.bytes(2, 6), "界\ne\u{301}\0".as_bytes());
        text.update_positions(
            Frame {
                cursor_y: 2,
                ..frame
            },
            None,
        );
        assert_eq!(text.caret, text.char_count());
        assert_eq!(text.selection, None);
    }
    #[test]
    fn hit_testing_rejects_off_grid_points_and_extents_validate_ranges() {
        let text = Text::from_frame(
            Frame {
                cols: 2,
                rows: 2,
                ..Frame::default()
            },
            &vec![Cell::default(); 4],
            None,
        );
        assert_eq!(text.offset_at(1.0, 17.0, 8.0, 16.0), Some(3));
        for (x, y) in [
            (16.0, 1.0),
            (1.0, 32.0),
            (-1.0, 1.0),
            (1.0, -1.0),
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
        ] {
            assert_eq!(text.offset_at(x, y, 8.0, 16.0), None);
        }
        assert_eq!(text.extents(0, 4, 8.0, 16.0), Some((0.0, 0.0, 16.0, 32.0)));
        assert_eq!(text.extents(5, 5, 8.0, 16.0), Some((16.0, 16.0, 0.0, 16.0)));
        assert_eq!(text.extents(4, 3, 8.0, 16.0), None);
        assert_eq!(text.extents(0, 6, 8.0, 16.0), None);
        assert_eq!(Text::default().offset_at(0.0, 0.0, 8.0, 16.0), None);
    }
}
