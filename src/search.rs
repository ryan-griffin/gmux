use crate::Cell;
use std::ops::Range;

#[derive(Default)]
pub struct Search {
    query: String,
    matches: Vec<Range<usize>>,
    current: usize,
    highlights: Vec<u8>,
    highlights_dirty: bool,
}

impl Search {
    pub fn set_query(&mut self, query: &str) {
        let query = query.to_lowercase();
        if self.query != query {
            self.query = query;
            self.matches.clear();
            self.current = 0;
            self.highlights_dirty = true;
        }
    }

    pub fn refresh(&mut self, cells: &[Cell], cols: usize) {
        self.highlights_dirty = true;
        if self.query.is_empty() || cols == 0 {
            self.matches.clear();
            self.current = 0;
            return;
        }
        let selected = self.matches.get(self.current).cloned();
        self.matches.clear();
        let mut text = String::with_capacity(cols);
        let mut byte_cells = Vec::with_capacity(cols);
        for (y, row) in cells.chunks(cols).enumerate() {
            text.clear();
            byte_cells.clear();
            for (x, cell) in row.iter().enumerate() {
                if cell.flags & 64 != 0 {
                    continue;
                }
                let start = text.len();
                let value = cell.text();
                if value.is_empty() {
                    text.push(' ');
                } else if value.is_ascii() {
                    text.extend(value.bytes().map(|byte| byte.to_ascii_lowercase() as char));
                } else {
                    text.push_str(&value.to_lowercase());
                }
                byte_cells.extend(std::iter::repeat_n(x, text.len() - start));
            }
            for (offset, _) in text.match_indices(&self.query) {
                let start = byte_cells[offset];
                let mut end = byte_cells[offset + self.query.len() - 1] + 1;
                // Include a wide character's continuation cell in the highlight.
                while end < row.len() && row[end].flags & 64 != 0 {
                    end += 1;
                }
                self.matches.push(y * cols + start..y * cols + end);
            }
        }
        self.current = selected
            .and_then(|selected| self.matches.iter().position(|range| *range == selected))
            .unwrap_or_else(|| self.current.min(self.matches.len().saturating_sub(1)));
    }

    pub fn advance(&mut self, previous: bool) {
        let total = self.matches.len();
        if total > 0 {
            self.current = if previous {
                (self.current + total - 1) % total
            } else {
                (self.current + 1) % total
            };
            self.highlights_dirty = true;
        }
    }

    pub fn status(&self) -> (usize, usize) {
        let total = self.matches.len();
        (if total == 0 { 0 } else { self.current + 1 }, total)
    }

    // 0: no match, 1: another match, 2: the current match.
    pub fn highlights(&mut self, len: usize) -> &[u8] {
        if self.matches.is_empty() {
            return &[];
        }
        if !self.highlights_dirty && self.highlights.len() == len {
            return &self.highlights;
        }
        self.highlights.resize(len, 0);
        self.highlights.fill(0);
        for (index, range) in self.matches.iter().enumerate() {
            for cell in range.clone() {
                if let Some(highlight) = self.highlights.get_mut(cell) {
                    *highlight = if index == self.current { 2 } else { 1 };
                }
            }
        }
        self.highlights_dirty = false;
        &self.highlights
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

    #[test]
    fn counts_case_insensitive_matches_and_wraps_in_both_directions() {
        let engine = screen("one ONE one", 20, 2);
        let mut search = Search::default();
        search.set_query("One");
        search.refresh(&engine.cells, 20);
        assert_eq!(search.status(), (1, 3));
        search.advance(true);
        assert_eq!(search.status(), (3, 3));
        search.advance(false);
        assert_eq!(search.status(), (1, 3));
        search.advance(false);
        search.refresh(&engine.cells, 20);
        assert_eq!(search.status(), (2, 3));
        search.set_query("missing");
        search.refresh(&engine.cells, 20);
        search.advance(true);
        assert_eq!(search.status(), (0, 0));
    }

    #[test]
    fn handles_unicode_and_highlights_the_whole_wide_character() {
        let engine = screen("界 É e\u{301} İ", 20, 2);
        let mut search = Search::default();
        search.set_query("界");
        search.refresh(&engine.cells, 20);
        assert_eq!(search.status(), (1, 1));
        assert_eq!(&search.highlights(engine.cells.len())[..3], &[2, 2, 0]);
        for query in ["é", "e\u{301}", "i\u{307}"] {
            search.set_query(query);
            search.refresh(&engine.cells, 20);
            assert_eq!(search.status(), (1, 1), "{query}");
        }
    }

    #[test]
    fn updates_when_output_changes_and_does_not_join_separate_rows() {
        let mut engine = screen("one one\r\none", 12, 3);
        let mut search = Search::default();
        search.set_query("one");
        search.refresh(&engine.cells, 12);
        search.advance(false);
        engine.write(b"\x1b[1;1Hxxx");
        engine.snapshot().unwrap();
        search.refresh(&engine.cells, 12);
        assert_eq!(search.status(), (1, 2));
        assert_eq!(search.matches[search.current], 4..7);
        search.set_query("one one");
        search.refresh(&engine.cells, 12);
        assert_eq!(search.status(), (0, 0));
        search.set_query("");
        search.refresh(&engine.cells, 12);
        assert!(
            search
                .highlights(engine.cells.len())
                .iter()
                .all(|&x| x == 0)
        );
    }
    #[test]
    fn highlights_follow_navigation_query_changes_and_viewport_updates() {
        let mut engine = screen("one one", 12, 2);
        let mut search = Search::default();
        search.set_query("one");
        search.refresh(&engine.cells, 12);
        assert_eq!(
            &search.highlights(engine.cells.len())[..7],
            &[2, 2, 2, 0, 1, 1, 1]
        );
        search.advance(false);
        assert_eq!(
            &search.highlights(engine.cells.len())[..7],
            &[1, 1, 1, 0, 2, 2, 2]
        );
        engine.write(b"\x1b[1;1Hxxx");
        engine.snapshot().unwrap();
        search.refresh(&engine.cells, 12);
        assert_eq!(
            &search.highlights(engine.cells.len())[..7],
            &[0, 0, 0, 0, 2, 2, 2]
        );
        search.set_query("");
        search.refresh(&engine.cells, 12);
        assert!(
            search
                .highlights(engine.cells.len())
                .iter()
                .all(|&value| value == 0)
        );
        search.set_query("xxx");
        search.refresh(&engine.cells, 12);
        assert_eq!(
            &search.highlights(engine.cells.len())[..7],
            &[2, 2, 2, 0, 0, 0, 0]
        );
    }
}
