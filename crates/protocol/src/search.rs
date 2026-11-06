mod index;
use crate::Cell;
use index::MatchIndex;
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const MAX_SEARCH_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEARCH_POOL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_QUERY_BYTES: usize = 4096;

/// Nonblocking admission shared by all retained searches in a daemon.
#[derive(Clone)]
pub struct SearchBudget(Arc<SearchPool>);
struct SearchPool {
    used: AtomicUsize,
    limit: usize,
}
impl Default for SearchBudget {
    fn default() -> Self {
        Self::new(MAX_SEARCH_POOL_BYTES)
    }
}
impl SearchBudget {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(SearchPool {
            used: AtomicUsize::new(0),
            limit,
        }))
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Relaxed)
    }
    fn acquire(&self, bytes: usize) -> Result<(), ()> {
        self.0
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&next| next <= self.0.limit)
            })
            .map(|_| ())
            .map_err(|_| ())
    }
    fn release(&self, bytes: usize) {
        self.0.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct Storage {
    budget: SearchBudget,
    used: usize,
}
impl Storage {
    fn acquire(&mut self, bytes: usize) -> Result<(), ()> {
        if bytes > MAX_SEARCH_BYTES.saturating_sub(self.used) {
            return Err(());
        }
        self.budget.acquire(bytes)?;
        self.used += bytes;
        Ok(())
    }
    fn release(&mut self, bytes: usize) {
        self.used -= bytes;
        self.budget.release(bytes);
    }
    fn grow<T>(&mut self, vec: &mut Vec<T>, needed: usize) -> Result<(), ()> {
        if needed <= vec.capacity() {
            return Ok(());
        }
        let old = vec.capacity() * size_of::<T>();
        let capacity = needed.max(vec.capacity().saturating_mul(2)).max(16);
        let bytes = capacity.checked_mul(size_of::<T>()).ok_or(())?;
        // Admit replacement storage while the old allocation remains charged.
        self.acquire(bytes)?;
        if vec.try_reserve_exact(capacity - vec.len()).is_err() {
            self.release(bytes);
            return Err(());
        }
        let actual = vec.capacity() * size_of::<T>();
        if actual > bytes {
            // Do not retain unexpected allocator excess outside the reservation.
            *vec = Vec::new();
            self.release(old + bytes);
            return Err(());
        }
        self.release(old + bytes - actual);
        Ok(())
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        self.budget.release(self.used);
    }
}

/// Keep only the unmatched suffix needed to search across a soft wrap.
#[derive(Default)]
struct Matcher {
    text: Vec<u8>,
    cells: Vec<(usize, usize)>,
}
impl Matcher {
    fn feed(
        &mut self,
        row: &[Cell],
        base: usize,
        query: &str,
        wrapped: bool,
        matches: &mut MatchIndex,
        storage: &mut Storage,
    ) -> Result<(), ()> {
        if query.is_empty() {
            self.clear();
            return Ok(());
        }
        for (x, cell) in row.iter().enumerate() {
            if cell.flags & (64 | 128) != 0 {
                continue;
            }
            let start = self.text.len();
            let value = cell.text();
            // Lowercase one bounded grapheme at a time. No row-sized
            // unaccounted String or temporary match vector.
            let mut append = |byte| -> Result<(), ()> {
                let needed = self.text.len() + 1;
                storage.grow(&mut self.text, needed)?;
                self.text.push(byte);
                Ok(())
            };
            if value.is_empty() {
                append(b' ')?;
            } else if value.is_ascii() {
                for byte in value.bytes() {
                    append(byte.to_ascii_lowercase())?;
                }
            } else {
                for ch in value.chars().flat_map(char::to_lowercase) {
                    let mut encoded = [0; 4];
                    for &byte in ch.encode_utf8(&mut encoded).as_bytes() {
                        append(byte)?;
                    }
                }
            }
            let mut end = x + 1;
            while end < row.len() && row[end].flags & 64 != 0 {
                end += 1;
            }
            storage.grow(&mut self.cells, self.text.len())?;
            self.cells.extend(std::iter::repeat_n(
                (base + x, base + end),
                self.text.len() - start,
            ));
        }
        let mut matched_end = 0;
        let text = std::str::from_utf8(&self.text).unwrap();
        for (offset, _) in text.match_indices(query) {
            matched_end = offset + query.len();
            matches.push(self.cells[offset].0..self.cells[matched_end - 1].1, storage)?;
        }
        if wrapped {
            let mut start = matched_end.max(self.text.len().saturating_sub(query.len() - 1));
            while !text.is_char_boundary(start) {
                start += 1;
            }
            self.text.drain(..start);
            self.cells.drain(..start);
        } else {
            self.clear();
        }
        Ok(())
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
    /// The scan stopped without a complete result. No partial index is exposed.
    pub resource_limited: bool,
    pub highlights: Vec<(usize, usize, u8)>,
}

/// Scalar status retained by frontends without duplicating viewport highlights.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct FindStatus {
    pub current: usize,
    pub total: usize,
    pub pending: bool,
    pub resource_limited: bool,
}
impl FindState {
    pub fn status(&self) -> FindStatus {
        FindStatus {
            current: self.current,
            total: self.total,
            pending: self.pending,
            resource_limited: self.resource_limited,
        }
    }
}

/// Scan history in bounded row batches, using the same Unicode matching as Find.
#[derive(Default)]
pub struct HistorySearch {
    matcher: Matcher,
    query: Vec<u8>,
    storage: Storage,
    resource_limited: bool,
    matches: MatchIndex,
    scanning: MatchIndex,
    current: usize,
    pub next: u64,
    pub rows: u64,
    pub cols: usize,
    pub pending: bool,
    pub dirty: bool,
}
impl HistorySearch {
    pub fn with_budget(budget: SearchBudget) -> Self {
        Self {
            storage: Storage { budget, used: 0 },
            ..Self::default()
        }
    }
    pub fn memory_size(&self) -> usize {
        self.storage.used
    }
    fn release_index(&mut self) {
        let old = self.query.capacity();
        self.matches = MatchIndex::default();
        self.scanning = MatchIndex::default();
        self.matcher = Matcher::default();
        self.storage.release(self.storage.used - old);
    }
    fn limited(&mut self) {
        self.release_index();
        self.query = Vec::new();
        self.storage.release(self.storage.used);
        self.pending = false;
        self.dirty = false;
        self.resource_limited = true;
        self.current = 0;
    }
    pub fn set_query(&mut self, query: &str) {
        if query.len() > MAX_QUERY_BYTES {
            self.limited();
            return;
        }
        // Bounded by the input limit and Unicode lowercase expansion.
        let query = query.to_lowercase();
        if self.query == query.as_bytes() && !self.resource_limited {
            return;
        }
        self.restart();
        self.query = Vec::new();
        self.storage.release(self.storage.used);
        if self.storage.grow(&mut self.query, query.len()).is_err() {
            self.limited();
            return;
        }
        self.query.extend_from_slice(query.as_bytes());
        self.resource_limited = false;
        self.dirty = !self.query.is_empty();
    }
    pub fn invalidate(&mut self) {
        self.dirty |= !self.query.is_empty() && !self.resource_limited;
    }
    pub fn restart(&mut self) {
        self.dirty = !self.query.is_empty();
        self.release_index();
        self.current = 0;
        self.pending = false;
    }
    pub fn begin(&mut self, rows: u64, cols: usize) {
        if self.resource_limited || self.query.is_empty() {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.next = 0;
        self.scanning.clear(&mut self.storage);
        self.matcher.clear();
        self.pending = true;
        self.dirty = false;
    }
    pub fn feed(&mut self, cells: &[Cell], wrapped: bool) {
        let base = self.next as usize * self.cols;
        if !self.pending {
            return;
        }
        if self
            .matcher
            .feed(
                cells,
                base,
                std::str::from_utf8(&self.query).unwrap(),
                wrapped,
                &mut self.scanning,
                &mut self.storage,
            )
            .is_err()
        {
            self.limited();
            return;
        }
        self.next += 1;
        if self.next >= self.rows {
            self.finish();
        }
    }
    pub fn finish(&mut self) {
        debug_assert!(self.next >= self.rows);
        let selected = self.matches.get(self.current);
        std::mem::swap(&mut self.matches, &mut self.scanning);
        self.current = selected
            .and_then(|r| self.matches.position(&r))
            .unwrap_or_else(|| self.current.min(self.matches.len().saturating_sub(1)));
        self.pending = false;
        let bytes = self.scanning.memory_size()
            + self.matcher.text.capacity()
            + self.matcher.cells.capacity() * size_of::<(usize, usize)>();
        self.scanning = MatchIndex::default();
        self.matcher = Matcher::default();
        self.storage.release(bytes);
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
        Some((self.matches.get(self.current).unwrap().start / self.cols) as u64)
    }
    fn highlights(&self, base: usize, cells: usize) -> Vec<(usize, usize, u8)> {
        let limit = base.saturating_add(cells);
        let mut highlights: Vec<(usize, usize, u8)> = Vec::new();
        for (_, range) in self
            .matches
            .ranges_from(base)
            .take_while(|(_, r)| r.start < limit)
        {
            let start = range.start.max(base) - base;
            let end = range.end.min(limit) - base;
            if let Some(last) = highlights.last_mut()
                && last.1 >= start
            {
                last.1 = last.1.max(end);
            } else {
                reserve_highlights(&mut highlights, 1, cells);
                highlights.push((start, end, 1));
            }
        }
        if let Some(selected) = self.matches.get(self.current) {
            let start = selected.start.max(base);
            let end = selected.end.min(limit);
            if start < end {
                let (start, end) = (start - base, end - base);
                let index = highlights.partition_point(|r| r.1 <= start);
                let (left, right, _) = highlights[index];
                // Normal ranges form a union. Overlay the selected match once,
                // preserving its priority over overlapping grapheme matches.
                let mut pieces = [(start, end, 2); 3];
                let mut count = 0;
                if left < start {
                    pieces[count] = (left, start, 1);
                    count += 1;
                }
                pieces[count] = (start, end, 2);
                count += 1;
                if end < right {
                    pieces[count] = (end, right, 1);
                    count += 1;
                }
                reserve_highlights(&mut highlights, count - 1, cells);
                highlights[index] = pieces[0];
                for (offset, &piece) in pieces[1..count].iter().enumerate() {
                    highlights.insert(index + offset + 1, piece);
                }
            }
        }
        highlights
    }
    pub fn state(&self, offset: u64, cells: usize) -> FindState {
        let base = offset as usize * self.cols;
        FindState {
            current: if self.matches.is_empty() {
                0
            } else {
                self.current + 1
            },
            total: self.matches.len(),
            pending: self.pending || self.dirty,
            resource_limited: self.resource_limited,
            highlights: self.highlights(base, cells),
        }
    }
}

// The emitted ranges do not overlap and each covers at least one cell. Cap
// growth before allocation, including spare capacity, at the viewport cell count.
fn reserve_highlights(highlights: &mut Vec<(usize, usize, u8)>, additional: usize, cells: usize) {
    let needed = highlights.len() + additional;
    if needed > highlights.capacity() {
        let capacity = needed
            .max(highlights.capacity().saturating_mul(2))
            .max(4)
            .min(cells);
        debug_assert!(capacity >= needed);
        highlights.reserve_exact(capacity - highlights.len());
    }
}

#[cfg(test)]
mod measurements {
    use super::*;
    #[test]
    #[ignore]
    fn dense_search_storage() {
        let rows: u64 = std::env::var("GMUX_SEARCH_ROWS").map_or(12000, |n| n.parse().unwrap());
        let mut cell = Cell::default();
        cell.set_text_bytes(b"a").unwrap();
        let row = vec![cell; 160];
        let pool = SearchBudget::default();
        let mut views: Vec<_> = (0..4)
            .map(|_| HistorySearch::with_budget(pool.clone()))
            .collect();
        let mut peak = 0;
        for view in &mut views {
            view.set_query("a");
            view.begin(rows, row.len());
            while view.pending {
                view.feed(&row, false);
                peak = peak.max(pool.used());
            }
        }
        eprintln!(
            "dense retained bytes: {}, limited views: {}, observed peak: {}",
            pool.used(),
            views.iter().filter(|v| v.resource_limited).count(),
            peak
        );
        for view in &mut views {
            view.begin(rows, row.len());
            for _ in 0..rows.saturating_sub(1) {
                view.feed(&row, false);
                peak = peak.max(pool.used());
            }
        }
        eprintln!(
            "rescan retained bytes: {}, observed peak: {}",
            pool.used(),
            peak
        );
        for view in &mut views {
            if view.pending {
                view.feed(&row, false);
            }
            view.set_query("");
        }
        eprintln!("cleared retained bytes: {}", pool.used());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn retained(search: &HistorySearch) -> usize {
        search.query.capacity()
            + search.matcher.text.capacity()
            + search.matcher.cells.capacity() * size_of::<(usize, usize)>()
            + search.matches.memory_size()
            + search.scanning.memory_size()
    }
    fn row(cols: usize) -> Vec<Cell> {
        let mut cell = Cell::default();
        cell.set_text("a").unwrap();
        vec![cell; cols]
    }
    fn scan(search: &mut HistorySearch, rows: u64, cells: &[Cell]) {
        search.begin(rows, cells.len());
        while search.pending {
            search.feed(cells, false);
        }
    }
    #[test]
    fn admission_never_exposes_partial_results_and_releases_every_capacity() {
        let pool = SearchBudget::new(16 * 1024);
        let mut first = HistorySearch::with_budget(pool.clone());
        first.set_query("a");
        scan(&mut first, 1, &row(160));
        assert_eq!(first.state(0, 160).total, 160);
        assert_eq!(pool.used(), retained(&first));
        first.invalidate();
        first.begin(100, 160);
        while first.pending {
            first.feed(&row(160), false);
        }
        let state = first.state(0, 160);
        assert!(state.resource_limited && !state.pending);
        assert_eq!(state.total, 0);
        assert!(state.highlights.is_empty());
        assert_eq!(pool.used(), 0);
        first.invalidate();
        assert!(!first.dirty);
        first.set_query("a");
        scan(&mut first, 1, &row(160));
        assert_eq!(first.state(0, 160).total, 160);
        first.set_query("");
        assert_eq!(pool.used(), 0);
    }
    #[test]
    fn shared_admission_cancellation_and_rescan_completion_return_credit() {
        let pool = SearchBudget::new(64 * 1024);
        let mut first = HistorySearch::with_budget(pool.clone());
        first.set_query("a");
        scan(&mut first, 5, &row(160));
        let complete = first.memory_size();
        first.begin(5, 160);
        first.feed(&row(160), false);
        assert!(first.memory_size() > complete);
        while first.pending {
            first.feed(&row(160), false);
        }
        assert_eq!(first.memory_size(), complete);
        let mut second = HistorySearch::with_budget(pool.clone());
        second.set_query("a");
        scan(&mut second, 1000, &row(160));
        assert!(second.state(0, 160).resource_limited);
        assert_eq!(pool.used(), retained(&first));
        first.begin(100, 160);
        first.feed(&row(160), true);
        first.set_query("other");
        assert_eq!(pool.used(), retained(&first));
        drop(first);
        drop(second);
        assert_eq!(pool.used(), 0);
    }
    #[test]
    fn complete_rescans_keep_navigation_and_duplicate_grapheme_matches() {
        let mut search = HistorySearch::default();
        search.set_query("a");
        scan(&mut search, 600, &row(160));
        assert_eq!(search.state(0, 160).total, 96000);
        assert!(search.memory_size() < 256 * 1024);
        search.advance(true);
        assert_eq!(search.state(0, 160).current, 96000);
        search.begin(600, 160);
        for _ in 0..599 {
            search.feed(&row(160), false);
        }
        // Navigation and highlights still refer to the complete old index.
        assert_eq!(
            search.state(599, 160).highlights.last(),
            Some(&(159, 160, 2))
        );
        assert!(search.state(0, 160).pending);
        search.feed(&row(160), false);
        assert_eq!(search.state(0, 160).current, 96000);
        assert!(!search.state(0, 160).resource_limited);
        search.set_query("\u{301}");
        let mut cell = Cell::default();
        cell.set_text("e\u{301}\u{301}").unwrap();
        scan(&mut search, 1, &[cell]);
        assert_eq!(search.state(0, 1).highlights, [(0, 1, 2)]);
        search.advance(false);
        assert_eq!(search.state(0, 1).current, 2);
    }

    #[test]
    fn highlights_merge_runs_and_bound_duplicate_grapheme_storage_before_allocation() {
        let mut search = HistorySearch::default();
        let mut cell = Cell::default();
        cell.set_text(&format!("e{}", "\u{301}".repeat(32)))
            .unwrap();
        search.set_query("\u{301}");
        scan(&mut search, 1, &vec![cell; 1000]);
        let state = search.state(0, 1000);
        assert_eq!(state.total, 32000);
        assert_eq!(state.highlights, [(0, 1, 2), (1, 1000, 1)]);
        assert!(state.highlights.capacity() <= 1000);
        for _ in 0..16000 {
            search.advance(false);
        }
        assert_eq!(
            search.state(0, 1000).highlights,
            [(0, 500, 1), (500, 501, 2), (501, 1000, 1)]
        );
        assert_eq!(search.state(0, 0).highlights, []);
    }

    #[test]
    fn query_limit_and_local_limit_are_explicit_and_retryable() {
        let mut search = HistorySearch::default();
        search.set_query(&"a".repeat(MAX_QUERY_BYTES + 1));
        assert!(search.state(0, 160).resource_limited);
        search.set_query("a");
        scan(&mut search, 40000, &row(160));
        assert!(search.state(0, 160).resource_limited);
        assert_eq!(search.memory_size(), 0);
        search.set_query("aa");
        scan(&mut search, 1, &row(160));
        assert_eq!(search.state(0, 160).total, 80);
        assert_eq!(search.advance(true), Some(0));
        assert_eq!(search.state(0, 160).current, 80);
    }
}
