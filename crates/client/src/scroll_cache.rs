//! Bounded, generation-scoped rows for local scroll prediction.
use gmux_protocol::{Cell, Frame};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROWS: usize = 512;

fn capacity(cols: usize) -> usize {
    if cols == 0 {
        return 0;
    }
    (MAX_BYTES
        / (cols * std::mem::size_of::<Cell>()
            + std::mem::size_of::<Option<u64>>()
            + size_of::<Vec<Cell>>()
            + size_of::<usize>()
            + 1))
        .min(MAX_ROWS)
}

// All windows and client threads share this pool. Lock order is pool, then
// rows. Read-only lookups take only rows and never acquire the pool afterward.
const PROCESS_BYTES: usize = 32 * 1024 * 1024;
static CLOCK: AtomicU64 = AtomicU64::new(1);

struct Pool {
    limit: usize,
    bytes: usize,
    rows: Vec<Weak<Mutex<Rows>>>,
}
impl Pool {
    fn make_room(&mut self, bytes: usize, protected: &Arc<Mutex<Rows>>) -> bool {
        if self.bytes + bytes <= self.limit {
            return true;
        }
        self.rows.retain(|rows| rows.strong_count() != 0);
        let mut victims: Vec<_> = self
            .rows
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|rows| !Arc::ptr_eq(rows, protected))
            .filter_map(|rows| {
                let data = rows.lock().unwrap();
                if data.retained_bytes() == 0 {
                    return None;
                }
                let priority = (data.focused, data.touched);
                drop(data);
                Some((priority, rows))
            })
            .collect();
        victims.sort_unstable_by_key(|(priority, _)| *priority);
        for (_, rows) in victims {
            let mut rows = rows.lock().unwrap();
            self.bytes -= rows.retained_bytes();
            rows.evict();
            if self.bytes + bytes <= self.limit {
                return true;
            }
        }
        false
    }
}
fn process_pool() -> Arc<Mutex<Pool>> {
    static POOL: OnceLock<Arc<Mutex<Pool>>> = OnceLock::new();
    POOL.get_or_init(|| {
        Arc::new(Mutex::new(Pool {
            limit: PROCESS_BYTES,
            bytes: 0,
            rows: Vec::new(),
        }))
    })
    .clone()
}

pub struct RowCache {
    rows: Arc<Mutex<Rows>>,
    pool: Arc<Mutex<Pool>>,
}
impl Default for RowCache {
    fn default() -> Self {
        Self::with_pool(process_pool())
    }
}
impl RowCache {
    fn with_pool(pool: Arc<Mutex<Pool>>) -> Self {
        let rows = Arc::new(Mutex::new(Rows::default()));
        pool.lock().unwrap().rows.push(Arc::downgrade(&rows));
        Self { rows, pool }
    }
    fn update<T>(&mut self, change: impl FnOnce(&mut Rows) -> T) -> T {
        let mut pool = self.pool.lock().unwrap();
        let mut rows = self.rows.lock().unwrap();
        pool.bytes -= rows.retained_bytes();
        let result = change(&mut rows);
        rows.touched = CLOCK.fetch_add(1, Ordering::Relaxed);
        let bytes = rows.retained_bytes();
        drop(rows);
        // A synchronous mutation can temporarily hold one local cache above
        // the pool limit. Eviction completes before returning to the caller.
        if pool.make_room(bytes, &self.rows) {
            pool.bytes += bytes;
        } else {
            self.rows.lock().unwrap().evict();
        }
        result
    }
    pub fn set_focused(&self, focused: bool) {
        self.rows.lock().unwrap().focused = focused;
    }
    pub fn retained_bytes(&self) -> usize {
        self.rows.lock().unwrap().retained_bytes()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.lock().unwrap().is_empty()
    }
    pub fn clear(&mut self) {
        self.update(Rows::clear);
    }
    pub fn invalidate(&mut self, generation: u64) -> bool {
        self.update(|r| r.invalidate(generation))
    }
    pub fn rebase(&mut self, origin: u64) -> u64 {
        self.update(|r| r.rebase(origin))
    }
    pub fn truncate(&mut self, end: u64) {
        self.update(|r| r.truncate(end));
    }
    pub fn configure(&mut self, generation: u64, frame: Frame) -> bool {
        self.update(|r| r.configure(generation, frame))
    }
    pub fn insert(&mut self, start: u64, cells: &[Cell], flags: &[u8]) {
        self.update(|r| r.insert(start, cells, flags));
    }
    #[cfg(test)]
    pub fn evict(&mut self) {
        self.update(Rows::evict);
    }
    pub fn has_margin(&self, frame: Frame, row: u64) -> bool {
        self.rows.lock().unwrap().has_margin(frame, row)
    }
    #[cfg(test)]
    pub fn copy_view(
        &self,
        start: u64,
        rows: usize,
        cells: &mut Vec<Cell>,
        flags: &mut Vec<u8>,
    ) -> bool {
        let mut data = self.rows.lock().unwrap();
        data.touched = CLOCK.fetch_add(1, Ordering::Relaxed);
        data.copy_view(start, rows, cells, flags)
    }
    pub fn copy_prediction(
        &self,
        start: u64,
        rows: usize,
        cells: &mut Vec<Cell>,
        flags: &mut Vec<u8>,
        reservation: &mut Option<PredictionReservation>,
    ) -> bool {
        let mut pool = self.pool.lock().unwrap();
        let data = self.rows.lock().unwrap();
        if !data.contains(start, rows) {
            return false;
        }
        let count = rows * data.cols;
        let heap: usize = (data.origin + start..data.origin + start + rows as u64)
            .map(|row| {
                data.row_bytes[(row % data.tags.len() as u64) as usize]
                    - data.cells[(row % data.tags.len() as u64) as usize].capacity()
                        * size_of::<Cell>()
            })
            .sum();
        let bytes =
            cells.capacity().max(count) * size_of::<Cell>() + flags.capacity().max(rows) + heap;
        let old = reservation.as_ref().map_or(0, |r| r.bytes);
        drop(data);
        if !pool.make_room(bytes.saturating_sub(old), &self.rows) {
            return false;
        }
        pool.bytes = pool.bytes - old + bytes;
        if let Some(r) = reservation {
            r.bytes = bytes;
        } else {
            *reservation = Some(PredictionReservation {
                pool: self.pool.clone(),
                bytes,
            });
        }
        // reserve_exact prevents Vec growth from retaining uncharged spare capacity.
        if cells.capacity() < count {
            cells.reserve_exact(count - cells.len());
        }
        if flags.capacity() < rows {
            flags.reserve_exact(rows - flags.len());
        }
        let mut data = self.rows.lock().unwrap();
        data.touched = CLOCK.fetch_add(1, Ordering::Relaxed);
        data.copy_view(start, rows, cells, flags)
    }
}
impl Drop for RowCache {
    fn drop(&mut self) {
        let mut pool = self.pool.lock().unwrap();
        let mut rows = self.rows.lock().unwrap();
        pool.bytes -= rows.retained_bytes();
        rows.clear();
        // Keep registry storage proportional to live views, including after churn.
        pool.rows
            .retain(|rows| rows.as_ptr() != Arc::as_ptr(&self.rows) && rows.strong_count() != 0);
        if pool.rows.capacity() > (pool.rows.len() * 4).max(16) {
            pool.rows.shrink_to_fit();
        }
    }
}
pub struct PredictionReservation {
    pool: Arc<Mutex<Pool>>,
    bytes: usize,
}
impl Drop for PredictionReservation {
    fn drop(&mut self) {
        self.pool.lock().unwrap().bytes -= self.bytes;
    }
}

#[derive(Default)]
struct Rows {
    focused: bool,
    touched: u64,
    generation: u64,
    origin: u64,
    cols: usize,
    configured_rows: usize,
    tags: Vec<Option<u64>>,
    cells: Vec<Vec<Cell>>,
    row_bytes: Vec<usize>,
    bytes: usize,
    flags: Vec<u8>,
}

impl Rows {
    pub fn retained_bytes(&self) -> usize {
        self.cells.capacity() * size_of::<Vec<Cell>>()
            + self.tags.capacity() * size_of::<Option<u64>>()
            + self.row_bytes.capacity() * size_of::<usize>()
            + self.flags.capacity()
            + self.bytes
    }
    fn discard(&mut self, slot: usize) {
        self.tags[slot] = None;
        self.bytes -= self.row_bytes[slot];
        self.row_bytes[slot] = 0;
        self.cells[slot] = Vec::new();
    }

    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
    // Pressure discards allocations, not the generation/origin needed to
    // reconcile an independently retained displayed prediction.
    fn evict(&mut self) {
        *self = Self {
            focused: self.focused,
            touched: self.touched,
            generation: self.generation,
            origin: self.origin,
            cols: self.cols,
            configured_rows: self.configured_rows,
            ..Self::default()
        };
    }
    pub fn clear(&mut self) {
        let focused = self.focused;
        *self = Self {
            focused,
            ..Self::default()
        };
    }
    pub fn invalidate(&mut self, generation: u64) -> bool {
        if self.generation == generation {
            return false;
        }
        self.generation = generation;
        for slot in 0..self.tags.len() {
            self.discard(slot);
        }
        true
    }

    pub fn rebase(&mut self, origin: u64) -> u64 {
        if self.origin == origin {
            return 0;
        }
        let removed = origin.saturating_sub(self.origin);
        self.origin = origin;
        for slot in 0..self.tags.len() {
            if self.tags[slot].is_some_and(|row| row < origin) {
                self.discard(slot);
            }
        }
        removed
    }

    pub fn truncate(&mut self, end: u64) {
        let end = self.origin.saturating_add(end);
        for slot in 0..self.tags.len() {
            if self.tags[slot].is_some_and(|row| row >= end) {
                self.discard(slot);
            }
        }
    }

    pub fn configure(&mut self, generation: u64, frame: Frame) -> bool {
        let cols = frame.cols as usize;
        let count = capacity(cols).min(frame.rows as usize * 5);
        let changed =
            self.generation != generation || self.cols != cols || self.configured_rows != count;
        if count < frame.rows as usize || frame.scrollback.total <= frame.rows as u64 {
            self.clear();
        } else if self.cols != cols || self.tags.len() != count {
            self.cols = cols;
            self.configured_rows = count;
            self.generation = generation;
            self.tags = vec![None; count];
            self.cells = vec![Vec::new(); count];
            self.row_bytes = vec![0; count];
            self.bytes = 0;
            self.flags = vec![0; count];
        } else if self.generation != generation {
            self.invalidate(generation);
        }
        changed
    }

    pub fn insert(&mut self, start: u64, cells: &[Cell], flags: &[u8]) {
        if self.tags.is_empty() {
            return;
        }
        for (index, row) in cells.chunks_exact(self.cols).enumerate() {
            let number = self.origin + start + index as u64;
            let slot = (number % self.tags.len() as u64) as usize;
            self.tags[slot] = Some(number);
            self.bytes -= self.row_bytes[slot];
            self.cells[slot].resize(row.len(), Cell::default());
            self.cells[slot].clone_from_slice(row);
            self.row_bytes[slot] = self.cells[slot].capacity() * size_of::<Cell>()
                + row.iter().map(Cell::heap_bytes).sum::<usize>();
            self.bytes += self.row_bytes[slot];
            let mut victim = (slot + 1) % self.tags.len();
            while self.retained_bytes() > MAX_BYTES {
                self.discard(victim);
                victim = (victim + 1) % self.tags.len();
            }
            self.flags[slot] = flags.get(index).copied().unwrap_or(0);
        }
    }

    fn copy_view(
        &self,
        start: u64,
        rows: usize,
        cells: &mut Vec<Cell>,
        flags: &mut Vec<u8>,
    ) -> bool {
        if !self.contains(start, rows) {
            return false;
        }
        let start = self.origin + start;
        cells.resize(rows * self.cols, Cell::default());
        flags.resize(rows, 0);
        for index in 0..rows {
            let slot = ((start + index as u64) % self.tags.len() as u64) as usize;
            cells[index * self.cols..(index + 1) * self.cols].clone_from_slice(&self.cells[slot]);
            flags[index] = self.flags[slot];
        }
        true
    }
    fn contains(&self, start: u64, rows: usize) -> bool {
        let Some(start) = self.origin.checked_add(start) else {
            return false;
        };
        if self.tags.is_empty()
            || rows > self.tags.len()
            || start.checked_add(rows as u64).is_none()
        {
            return false;
        }
        let slot = (start % self.tags.len() as u64) as usize;
        let first = rows.min(self.tags.len() - slot);
        self.tags[slot..slot + first]
            .iter()
            .chain(&self.tags[..rows - first])
            .zip(start..)
            .all(|(tag, number)| *tag == Some(number))
    }
    pub fn has_margin(&self, frame: Frame, row: u64) -> bool {
        let row = row.min(frame.scrollback.total.saturating_sub(frame.scrollback.len));
        let start = row.saturating_sub(frame.rows as u64);
        let end = row
            .saturating_add(2 * frame.rows as u64)
            .min(frame.scrollback.total)
            // Output can continuously invalidate the live screen. A cached
            // historical viewport need not keep requesting that mutable margin.
            .min(
                frame
                    .scrollback
                    .total
                    .saturating_sub(frame.rows as u64)
                    .max(row.saturating_add(frame.rows as u64)),
            );
        self.contains(start, end.saturating_sub(start) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_admission_does_not_require_retaining_every_transmitted_row() {
        let cols = 1000;
        let count = gmux_protocol::transport::cache_row_limit(cols);
        assert!(count > capacity(cols));
        let frame = Frame {
            cols: cols as u16,
            rows: 20,
            scrollback: gmux_protocol::Scrollback {
                total: 1000,
                offset: 0,
                len: 20,
            },
            ..Frame::default()
        };
        for text in ["a".into(), format!("🧑{}", "\u{1d185}".repeat(64))] {
            let mut cell = Cell::default();
            cell.set_text(&text).unwrap();
            let mut cache = Rows::default();
            cache.configure(1, frame);
            cache.insert(0, &vec![cell; count * cols], &vec![1; count]);
            assert!(cache.retained_bytes() <= MAX_BYTES);
            let (mut cells, mut flags) = (Vec::new(), Vec::new());
            assert!(!cache.copy_view(0, 1, &mut cells, &mut flags));
            assert!(cache.copy_view(count as u64 - 1, 1, &mut cells, &mut flags));
            assert_eq!(cells[0].text(), text);
            assert_eq!(flags, [1]);
            cache.clear();
            assert_eq!(cache.retained_bytes(), 0);
        }
    }

    #[test]
    fn overflow_is_charged_evicted_and_released_with_its_row() {
        let frame = Frame {
            cols: 1000,
            rows: 20,
            scrollback: gmux_protocol::Scrollback {
                total: 1000,
                offset: 0,
                len: 20,
            },
            ..Frame::default()
        };
        let mut cache = Rows::default();
        cache.configure(1, frame);
        let mut cell = Cell::default();
        cell.set_text(&format!("🧑{}", "\u{1d185}".repeat(64)))
            .unwrap();
        cell.flags = gmux_protocol::cells::flags::BLINK;
        let row = vec![cell; 1000];
        for index in 0..capacity(1000) {
            cache.insert(index as u64, &row, &[0]);
            assert!(cache.retained_bytes() <= MAX_BYTES);
        }
        let (mut cells, mut flags) = (Vec::new(), Vec::new());
        assert!(!cache.copy_view(0, 1, &mut cells, &mut flags));
        assert!(cache.copy_view(capacity(1000) as u64 - 1, 1, &mut cells, &mut flags));
        assert!(cells == row);
        cache.invalidate(2);
        assert_eq!(cache.bytes, 0);
        assert!(cache.cells.iter().all(Vec::is_empty));
    }

    #[test]
    fn pruning_and_live_screen_edits_preserve_only_unchanged_history() {
        let mut cache = Rows::default();
        let frame = Frame {
            cols: 2,
            rows: 2,
            scrollback: gmux_protocol::Scrollback {
                total: 20,
                offset: 4,
                len: 2,
            },
            ..Frame::default()
        };
        cache.configure(1, frame);
        for row in 0..10 {
            let mut cell = Cell::default();
            cell.set_text_bytes(&[b'a' + row]).unwrap();
            cell.flags = if row % 2 == 1 {
                gmux_protocol::cells::flags::BLINK
            } else {
                0
            };
            cache.insert(row as u64, &[cell.clone(), cell], &[row % 2]);
        }
        cache.truncate(8);
        assert!(cache.has_margin(
            Frame {
                scrollback: gmux_protocol::Scrollback {
                    total: 10,
                    ..frame.scrollback
                },
                ..frame
            },
            6
        ));
        assert_eq!(cache.rebase(3), 3);
        cache.truncate(4);
        let (mut cells, mut flags) = (Vec::new(), Vec::new());
        assert!(cache.copy_view(0, 4, &mut cells, &mut flags));
        assert_eq!(cells[0].text(), "d");
        assert_eq!(cells[6].text(), "g");
        assert_eq!(flags, [1, 0, 1, 0]);
        assert_eq!(
            cells
                .chunks(2)
                .map(|row| row[0].blink())
                .collect::<Vec<_>>(),
            [true, false, true, false]
        );
        assert!(!cache.copy_view(3, 2, &mut cells, &mut flags));
        cache.invalidate(2);
        assert!(!cache.copy_view(0, 1, &mut cells, &mut flags));
    }

    #[test]
    fn ring_eviction_and_generation_changes_never_reuse_unrelated_rows() {
        let mut cache = Rows::default();
        let frame = Frame {
            cols: 1000,
            rows: 20,
            scrollback: gmux_protocol::Scrollback {
                total: 1000,
                offset: 200,
                len: 20,
            },
            ..Frame::default()
        };
        cache.configure(1, frame);
        assert!(cache.retained_bytes() <= MAX_BYTES);
        let count = capacity(1000);
        let mut row = vec![Cell::default(); 1000];
        for index in 0..count + 5 {
            row[0].set_text(&index.to_string()).unwrap();
            cache.insert(index as u64, &row, &[index as u8 % 2]);
        }
        let (mut cells, mut flags) = (Vec::new(), Vec::new());
        assert!(!cache.copy_view(0, 1, &mut cells, &mut flags));
        assert!(cache.copy_view(5, 20, &mut cells, &mut flags));
        assert_eq!(cells[0].text(), "5");
        assert_eq!(flags[0], 1);
        assert!(cache.copy_view(count as u64 - 3, 6, &mut cells, &mut flags));
        for index in 0..6 {
            assert_eq!(cells[index * 1000].text(), (count - 3 + index).to_string());
            assert_eq!(flags[index], ((count - 3 + index) % 2) as u8);
        }
        cache.configure(2, frame);
        assert!(!cache.copy_view(5, 1, &mut cells, &mut flags));
        // A maximum-sized viewport must not force an oversized history cache.
        cache.configure(3, Frame { rows: 131, ..frame });
        assert_eq!(cache.retained_bytes(), 0);
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    fn pool() -> Arc<Mutex<Pool>> {
        Arc::new(Mutex::new(Pool {
            limit: usize::MAX,
            bytes: 0,
            rows: Vec::new(),
        }))
    }
    fn fill(cache: &mut RowCache) {
        let frame = Frame {
            cols: 80,
            rows: 4,
            scrollback: gmux_protocol::Scrollback {
                total: 100,
                offset: 10,
                len: 4,
            },
            ..Frame::default()
        };
        cache.configure(1, frame);
        cache.insert(0, &vec![Cell::default(); 80 * 20], &[0; 20]);
    }
    #[test]
    fn pressure_evicts_unfocused_windows_before_recently_used_focused_views() {
        let pool = pool();
        let mut focused = RowCache::with_pool(pool.clone());
        focused.set_focused(true);
        fill(&mut focused);
        let size = focused.retained_bytes();
        let mut background = RowCache::with_pool(pool.clone());
        fill(&mut background);
        pool.lock().unwrap().limit = size * 2;
        let mut newest = RowCache::with_pool(pool.clone());
        newest.set_focused(true);
        fill(&mut newest);
        assert!(background.is_empty());
        assert_eq!(focused.retained_bytes(), size);
        assert_eq!(newest.retained_bytes(), size);
        assert_eq!(pool.lock().unwrap().bytes, size * 2);
        // Among equally focused views, a read makes the older view recent.
        focused.copy_view(0, 4, &mut Vec::new(), &mut Vec::new());
        fill(&mut background);
        assert!(newest.is_empty());
        assert!(!focused.is_empty());
        drop((focused, background, newest));
        assert_eq!(pool.lock().unwrap().bytes, 0);
        assert!(pool.lock().unwrap().rows.is_empty());
    }
    #[test]
    fn displayed_predictions_remain_charged_after_source_eviction_and_release_on_drop() {
        let pool = pool();
        let mut cache = RowCache::with_pool(pool.clone());
        fill(&mut cache);
        let mut rich = Cell::default();
        rich.set_text(&format!("🧑{}", "\u{1d185}".repeat(64)))
            .unwrap();
        cache.insert(0, &vec![rich; 80], &[0]);
        let mut cells = Vec::new();
        let mut flags = Vec::new();
        let mut reservation = None;
        assert!(cache.copy_prediction(0, 4, &mut cells, &mut flags, &mut reservation));
        let prediction_bytes = reservation.as_ref().unwrap().bytes;
        assert_eq!(
            prediction_bytes,
            cells.capacity() * size_of::<Cell>()
                + flags.capacity()
                + cells.iter().map(Cell::heap_bytes).sum::<usize>()
        );
        assert_eq!(
            pool.lock().unwrap().bytes,
            cache.retained_bytes() + prediction_bytes
        );
        cache.clear();
        assert_eq!(pool.lock().unwrap().bytes, prediction_bytes);
        // Insufficient optional storage leaves the displayed prediction intact.
        fill(&mut cache);
        let bytes = pool.lock().unwrap().bytes;
        pool.lock().unwrap().limit = bytes;
        assert!(!cache.copy_prediction(0, 20, &mut cells, &mut flags, &mut reservation));
        assert_eq!(cells.len(), 80 * 4);
        drop(cache);
        assert_eq!(pool.lock().unwrap().bytes, prediction_bytes);
        drop((cells, flags, reservation));
        assert_eq!(pool.lock().unwrap().bytes, 0);
    }
    #[test]
    fn independently_created_windows_and_threads_share_the_process_pool() {
        let cache = RowCache::default();
        let other = std::thread::spawn(RowCache::default).join().unwrap();
        assert!(Arc::ptr_eq(&cache.pool, &other.pool));
    }
}
