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
pub(crate) struct HistorySearch {
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
        self.dirty = !query.is_empty();
        self.query = query;
        self.matcher.clear();
        self.matches.clear();
        self.scanning.clear();
        self.current = 0;
        self.pending = false;
    }
    pub fn invalidate(&mut self) {
        self.dirty |= !self.query.is_empty();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::Engine, theme::Theme};

    fn screen(text: &str, cols: u16, rows: u16) -> Engine {
        let mut engine = Engine::new().unwrap();
        engine.set_theme(&Theme::adwaita(true)).unwrap();
        engine.resize(cols, rows, 8, 16).unwrap();
        engine.write(text.as_bytes());
        engine.snapshot().unwrap();
        engine
    }
    fn history_find(engine: &mut Engine, query: &str) -> FindState {
        engine.find(query);
        for _ in 0..1000 {
            if !engine.find_pending() {
                engine.snapshot().unwrap();
                return engine.find_state();
            }
            engine.poll_find();
        }
        panic!("Find did not finish");
    }
    fn status(engine: &Engine) -> (usize, usize) {
        let state = engine.find_state();
        (state.current, state.total)
    }
    #[test]
    fn history_find_joins_soft_wraps_but_preserves_hard_line_breaks_and_wide_spacers() {
        let mut engine = screen("123456needle\r\nneed\r\nle", 8, 5);
        let found = history_find(&mut engine, "needle");
        assert_eq!(found.total, 1);
        assert_eq!(found.highlights, vec![(6, 12, 2)]);
        engine.resize(32, 5, 8, 16).unwrap();
        assert_eq!(history_find(&mut engine, "needle").total, 1);
        let mut engine = screen("1234567界É", 8, 3);
        let found = history_find(&mut engine, "7界é");
        assert_eq!(found.total, 1);
        assert_eq!(found.highlights, vec![(6, 11, 2)]);
    }
    #[test]
    fn wrapped_matches_cross_scan_batches_without_duplicate_or_overlapping_matches() {
        let mut engine = screen(&("\r\n".repeat(63) + "123456needle"), 8, 3);
        assert_eq!(history_find(&mut engine, "needle").total, 1);
        let mut engine = screen("aaaaaaaa", 3, 4);
        assert_eq!(history_find(&mut engine, "aa").total, 4);
        assert_eq!(history_find(&mut engine, "aaaaaaaa").total, 1);
        engine.write(b"\x1b[3J\x1b[2J\x1b[Hnew");
        assert_eq!(history_find(&mut engine, "aaaaaaaa").total, 0);
    }

    #[test]
    fn counts_case_insensitive_matches_and_wraps_in_both_directions() {
        let mut engine = screen("one ONE one", 20, 2);
        history_find(&mut engine, "One");
        assert_eq!(status(&engine), (1, 3));
        engine.find_advance(true);
        assert_eq!(status(&engine), (3, 3));
        engine.find_advance(false);
        assert_eq!(status(&engine), (1, 3));
        engine.find_advance(false);
        history_find(&mut engine, "one");
        assert_eq!(status(&engine), (2, 3));
        history_find(&mut engine, "missing");
        engine.find_advance(true);
        assert_eq!(status(&engine), (0, 0));
    }

    #[test]
    fn handles_unicode_and_highlights_the_whole_wide_character() {
        let mut engine = screen("界 É e\u{301} İ", 20, 2);
        let state = history_find(&mut engine, "界");
        assert_eq!(status(&engine), (1, 1));
        assert_eq!(state.highlights, vec![(0, 2, 2)]);
        for query in ["é", "e\u{301}", "i\u{307}"] {
            history_find(&mut engine, query);
            assert_eq!(status(&engine), (1, 1), "{query}");
        }
    }

    #[test]
    fn updates_when_output_changes_and_does_not_join_separate_rows() {
        let mut engine = screen("one one\r\none", 12, 3);
        history_find(&mut engine, "one");
        engine.find_advance(false);
        engine.write(b"\x1b[1;1Hxxx");
        let state = history_find(&mut engine, "one");
        assert_eq!(status(&engine), (1, 2));
        assert_eq!(state.highlights, vec![(4, 7, 2), (12, 15, 1)]);
        history_find(&mut engine, "one one");
        assert_eq!(status(&engine), (0, 0));
        assert!(history_find(&mut engine, "").highlights.is_empty());
    }
    #[test]
    fn highlights_follow_navigation_query_changes_and_viewport_updates() {
        let mut engine = screen("one one", 12, 2);
        assert_eq!(
            history_find(&mut engine, "one").highlights,
            vec![(0, 3, 2), (4, 7, 1)]
        );
        engine.find_advance(false);
        assert_eq!(engine.find_state().highlights, vec![(0, 3, 1), (4, 7, 2)]);
        engine.write(b"\x1b[1;1Hxxx");
        assert_eq!(history_find(&mut engine, "one").highlights, vec![(4, 7, 2)]);
        assert!(history_find(&mut engine, "").highlights.is_empty());
        assert_eq!(history_find(&mut engine, "xxx").highlights, vec![(0, 3, 2)]);
    }
}
