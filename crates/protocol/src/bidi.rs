//! Visual order for terminal cells. The PTY and protocol keep logical order.
use crate::Cell;
use std::rc::Rc;
use unicode_bidi::{BidiClass, Level, ParagraphBidiInfo, bidi_class};

pub struct Row {
    visual_to_logical: Box<[u16]>,
    logical_to_visual: Box<[u16]>,
    rtl: Box<[bool]>,
    // NUL separates units. An empty unit is a wide-cell continuation.
    source: Box<str>,
}

fn units(cells: &[Cell]) -> impl Iterator<Item = (usize, usize, &str)> {
    let mut col = 0;
    std::iter::from_fn(move || {
        let text = cells.get(col)?.text();
        let span = if cells.get(col + 1).is_some_and(|cell| cell.flags & 64 != 0) {
            2
        } else {
            1
        };
        // All ASCII letters have the same Bidi class and no mirroring or
        // bracket pairing. Editing a Latin word needn't re-resolve the row.
        let text = match text.as_bytes() {
            [] => " ",
            [b'a'..=b'z' | b'A'..=b'Z'] => "a",
            _ => text,
        };
        let unit = (col, span, text);
        col += span;
        Some(unit)
    })
}

impl Row {
    fn matches(&self, cells: &[Cell]) -> bool {
        if cells.len() != self.visual_to_logical.len() {
            return false;
        }
        let mut old = self.source.split_terminator('\0');
        for (_, span, text) in units(cells) {
            if old.next() != Some(text) || (span == 2 && old.next() != Some("")) {
                return false;
            }
        }
        old.next().is_none()
    }

    pub fn new(cells: &[Cell]) -> Option<Self> {
        // ASCII and ordinary left-to-right rows don't allocate or run the
        // bidirectional algorithm. Trailing padding keeps its grid position.
        if !cells.iter().any(|cell| {
            !cell.text().is_ascii()
                && cell.text().chars().any(|ch| {
                    matches!(
                        bidi_class(ch),
                        BidiClass::R
                            | BidiClass::AL
                            | BidiClass::RLE
                            | BidiClass::RLO
                            | BidiClass::RLI
                            | BidiClass::FSI
                    )
                })
        }) {
            return None;
        }
        let mut text = String::with_capacity(cells.len());
        let mut positions = Vec::with_capacity(cells.len());
        for (col, span, glyph) in units(cells) {
            positions.push((col, span, text.len()));
            text.push_str(glyph);
        }
        let bidi = ParagraphBidiInfo::new(&text, Some(Level::ltr()));
        let levels = bidi.reordered_levels(0..text.len());
        let levels: Vec<_> = positions
            .iter()
            .map(|&(_, _, start)| levels[start])
            .collect();
        if !levels.iter().any(Level::is_rtl) {
            return None;
        }
        let order = ParagraphBidiInfo::reorder_visual(&levels);
        let mut visual_to_logical = Vec::with_capacity(cells.len());
        let mut logical_to_visual = vec![0; cells.len()];
        let mut rtl = Vec::with_capacity(cells.len());
        for unit in order {
            let (col, span, _) = positions[unit];
            for (offset, visual) in logical_to_visual[col..col + span].iter_mut().enumerate() {
                *visual = visual_to_logical.len() as u16;
                visual_to_logical.push((col + offset) as u16);
                rtl.push(levels[unit].is_rtl());
            }
        }
        let mut source = String::with_capacity(text.len() + cells.len());
        for (_, span, glyph) in units(cells) {
            source.push_str(glyph);
            source.push('\0');
            if span == 2 {
                source.push('\0');
            }
        }
        Some(Self {
            visual_to_logical: visual_to_logical.into(),
            logical_to_visual: logical_to_visual.into(),
            rtl: rtl.into(),
            source: source.into_boxed_str(),
        })
    }

    pub fn logical(&self, visual: usize) -> usize {
        self.visual_to_logical
            .get(visual)
            .map_or(visual, |&col| col as usize)
    }

    pub fn visual(&self, logical: usize) -> usize {
        self.logical_to_visual
            .get(logical)
            .map_or(logical, |&col| col as usize)
    }

    pub fn rtl(&self, visual: usize) -> bool {
        self.rtl.get(visual).copied().unwrap_or(false)
    }
}

pub struct Grid {
    cols: usize,
    rows: Vec<(usize, Rc<Row>)>,
}

impl Grid {
    pub fn update(cols: usize, cells: &[Cell], previous: Option<&Rc<Self>>) -> Option<Rc<Self>> {
        if cols == 0 {
            return None;
        }
        let previous = previous.filter(|grid| grid.cols == cols);
        let old_rows = previous.map_or(&[][..], |grid| grid.rows.as_slice());
        let mut old_index = 0;
        let mut updated: Option<Vec<_>> = None;
        for (y, cells) in cells.chunks(cols).enumerate() {
            let old = old_rows.get(old_index).filter(|(row, _)| *row == y);
            if let Some(entry) = old.filter(|(_, row)| row.matches(cells)) {
                if let Some(rows) = &mut updated {
                    rows.push(entry.clone());
                }
            } else {
                let row = Row::new(cells);
                if old.is_some() || row.is_some() {
                    // Only copy the sparse row list when a map actually changes.
                    let rows = updated.get_or_insert_with(|| old_rows[..old_index].to_vec());
                    if let Some(row) = row {
                        rows.push((y, Rc::new(row)));
                    }
                }
            }
            old_index += usize::from(old.is_some());
        }
        match updated {
            Some(rows) if rows.is_empty() => None,
            Some(rows) => Some(Rc::new(Self { cols, rows })),
            None if old_index == old_rows.len() => previous.cloned(),
            None if old_index == 0 => None,
            None => Some(Rc::new(Self {
                cols,
                rows: old_rows[..old_index].to_vec(),
            })),
        }
    }

    pub fn row(&self, y: usize) -> Option<&Row> {
        let at = self.rows.binary_search_by_key(&y, |(y, _)| *y).ok()?;
        Some(&self.rows[at].1)
    }

    pub fn logical(&self, visual: usize) -> usize {
        let y = visual / self.cols;
        self.row(y).map_or(visual, |row| {
            y * self.cols + row.logical(visual % self.cols)
        })
    }

    pub fn visual(&self, logical: usize) -> usize {
        let y = logical / self.cols;
        self.row(y).map_or(logical, |row| {
            y * self.cols + row.visual(logical % self.cols)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_reorders_cells_without_reversing_number_runs_or_splitting_wide_cells() {
        let mut cells: Vec<_> = ["س", "ل", "ا", "م", " ", "1", "2", " ", "界", "", "A"]
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let mut cell = Cell::default();
                cell.set_text(s).unwrap();
                if i == 9 {
                    cell.flags |= 64;
                }
                cell
            })
            .collect();
        let grid = Grid::update(cells.len(), &cells, None).unwrap();
        let displayed: String = (0..cells.len())
            .map(|x| cells[grid.logical(x)].text())
            .collect();
        assert_eq!(displayed, "12 مالس 界A");
        for x in 0..cells.len() {
            assert_eq!(grid.logical(grid.visual(x)), x);
        }
        assert_eq!(grid.visual(9), grid.visual(8) + 1);
        cells[10].set_text("Z").unwrap();
        let edited = Grid::update(cells.len(), &cells, Some(&grid)).unwrap();
        assert!(Rc::ptr_eq(&grid, &edited));
        cells[4].set_text("X").unwrap();
        let changed = Grid::update(cells.len(), &cells, Some(&edited)).unwrap();
        assert!(!std::ptr::eq(
            edited.row(0).unwrap(),
            changed.row(0).unwrap()
        ));
        assert_ne!(grid.visual(0), changed.visual(0));
    }

    #[test]
    fn updating_a_sparse_grid_removes_stale_maps_and_shares_unchanged_rows() {
        let mut rtl = Cell::default();
        rtl.set_text("س").unwrap();
        let mut cells = [rtl.clone(), Cell::default(), rtl.clone()];
        let old = Grid::update(1, &cells, None).unwrap();
        cells[0] = Cell::default();
        cells[1] = rtl;
        let updated = Grid::update(1, &cells, Some(&old)).unwrap();
        assert!(updated.row(0).is_none());
        assert!(updated.row(1).is_some());
        assert!(std::ptr::eq(old.row(2).unwrap(), updated.row(2).unwrap()));
        assert!(Grid::update(1, &cells[..1], Some(&updated)).is_none());
    }
}
