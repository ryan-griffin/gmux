use crate::Cell;
use std::ops::Range;

/// Keep only the unmatched suffix needed to search across a soft wrap.
#[derive(Default)]
struct Matcher {
    text: String,
    cells: Vec<(usize, usize)>,
}
impl Matcher {
    fn feed(
        &mut self,
        row: &[Cell],
        base: usize,
        query: &str,
        wrapped: bool,
        matches: &mut Vec<Range<usize>>,
    ) {
        if query.is_empty() {
            self.clear();
            return;
        }
        for (x, cell) in row.iter().enumerate() {
            if cell.flags & (64 | 128) != 0 {
                continue;
            }
            let start = self.text.len();
            let value = cell.text();
            if value.is_empty() {
                self.text.push(' ');
            } else if value.is_ascii() {
                self.text
                    .extend(value.bytes().map(|byte| byte.to_ascii_lowercase() as char));
            } else {
                self.text.push_str(&value.to_lowercase());
            }
            let mut end = x + 1;
            while end < row.len() && row[end].flags & 64 != 0 {
                end += 1;
            }
            self.cells.extend(std::iter::repeat_n(
                (base + x, base + end),
                self.text.len() - start,
            ));
        }
        let mut matched_end = 0;
        for (offset, _) in self.text.match_indices(query) {
            matched_end = offset + query.len();
            matches.push(self.cells[offset].0..self.cells[matched_end - 1].1);
        }
        if wrapped {
            let mut start = matched_end.max(self.text.len().saturating_sub(query.len() - 1));
            while !self.text.is_char_boundary(start) {
                start += 1;
            }
            self.text.drain(..start);
            self.cells.drain(..start);
        } else {
            self.clear();
        }
    }
    fn clear(&mut self) {
        self.text.clear();
        self.cells.clear();
    }
}

#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FindState {
    pub current: usize,
    pub total: usize,
    pub pending: bool,
    pub highlights: Vec<(usize, usize, u8)>,
}

/// Scan history in bounded row batches, using the same Unicode matching as Find.
#[derive(Default)]
pub struct HistorySearch {
    matcher: Matcher,
    query: String,
    matches: Vec<Range<usize>>,
    scanning: Vec<Range<usize>>,
    current: usize,
    pub next: u64,
    pub rows: u64,
    pub cols: usize,
    pub pending: bool,
    pub dirty: bool,
}
impl HistorySearch {
    pub fn set_query(&mut self, query: &str) {
        let query = query.to_lowercase();
        if self.query == query {
            return;
        }
        self.query = query;
        self.restart();
    }
    pub fn invalidate(&mut self) {
        self.dirty |= !self.query.is_empty();
    }
    pub fn restart(&mut self) {
        self.dirty = !self.query.is_empty();
        self.matcher.clear();
        self.matches.clear();
        self.scanning.clear();
        self.current = 0;
        self.pending = false;
    }
    pub fn begin(&mut self, rows: u64, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.next = 0;
        self.scanning.clear();
        self.matcher.clear();
        self.pending = true;
        self.dirty = false;
    }
    pub fn feed(&mut self, cells: &[Cell], wrapped: bool) {
        let base = self.next as usize * self.cols;
        self.matcher
            .feed(cells, base, &self.query, wrapped, &mut self.scanning);
        self.next += 1;
        if self.next >= self.rows {
            self.finish();
        }
    }
    pub fn finish(&mut self) {
        let selected = self.matches.get(self.current).cloned();
        std::mem::swap(&mut self.matches, &mut self.scanning);
        self.current = selected
            .and_then(|r| self.matches.iter().position(|new| *new == r))
            .unwrap_or_else(|| self.current.min(self.matches.len().saturating_sub(1)));
        self.pending = false;
    }
    pub fn advance(&mut self, previous: bool) -> Option<u64> {
        let total = self.matches.len();
        if total == 0 {
            return None;
        }
        self.current = if previous {
            (self.current + total - 1) % total
        } else {
            (self.current + 1) % total
        };
        Some((self.matches[self.current].start / self.cols) as u64)
    }
    pub fn state(&self, offset: u64, cells: usize) -> FindState {
        let base = offset as usize * self.cols;
        let first = self.matches.partition_point(|range| range.end <= base);
        FindState {
            current: if self.matches.is_empty() {
                0
            } else {
                self.current + 1
            },
            total: self.matches.len(),
            pending: self.pending || self.dirty,
            highlights: self.matches[first..]
                .iter()
                .enumerate()
                .take_while(|(_, range)| range.start < base.saturating_add(cells))
                .filter_map(|(i, r)| {
                    let start = r.start.max(base);
                    let end = r.end.min(base.saturating_add(cells));
                    (start < end).then(|| {
                        (
                            start - base,
                            end - base,
                            if first + i == self.current { 2 } else { 1 },
                        )
                    })
                })
                .collect(),
        }
    }
}
