//! Bounded, generation-scoped rows for local scroll prediction.
use crate::{Cell, Frame};

pub const MAX_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ROWS: usize = 512;

pub fn capacity(cols: usize) -> usize {
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

/// Keep two screens on either side, within the per-pane memory limit.
pub fn window(frame: Frame) -> std::ops::Range<u64> {
    let count = capacity(frame.cols as usize).min(frame.rows as usize * 5) as u64;
    if count < frame.rows as u64 {
        return 0..0;
    }
    if frame.scrollback.total <= frame.rows as u64 {
        return 0..0;
    }
    let count = count.min(frame.scrollback.total);
    let start = frame
        .scrollback
        .offset
        .saturating_sub((count - frame.rows as u64) / 2)
        .min(frame.scrollback.total.saturating_sub(count));
    start..start + count
}

/// Reuse overlapping rows before applying a packet's changed rows.
pub fn shift<T>(data: &mut [T], cols: usize, rows: i16) {
    let count = rows.unsigned_abs() as usize * cols;
    // Exposed rows are overwritten by the packet. Move ownership instead of
    // cloning overflow Arcs into cells that will immediately be replaced.
    if rows > 0 {
        data.rotate_left(count);
    }
    if rows < 0 {
        data.rotate_right(count);
    }
}

#[derive(Default)]
pub struct RowCache {
    generation: u64,
    origin: u64,
    cols: usize,
    tags: Vec<Option<u64>>,
    cells: Vec<Vec<Cell>>,
    row_bytes: Vec<usize>,
    bytes: usize,
    flags: Vec<u8>,
}

impl RowCache {
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
    pub fn clear(&mut self) {
        *self = Self::default();
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
            self.generation != generation || self.cols != cols || self.tags.len() != count;
        if count < frame.rows as usize || frame.scrollback.total <= frame.rows as u64 {
            self.clear();
        } else if self.cols != cols || self.tags.len() != count {
            self.cols = cols;
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

    pub fn copy_view(
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
    fn overflow_is_charged_evicted_and_released_with_its_row() {
        let frame = Frame {
            cols: 1000,
            rows: 20,
            scrollback: crate::Scrollback {
                total: 1000,
                offset: 0,
                len: 20,
            },
            ..Frame::default()
        };
        let mut cache = RowCache::default();
        cache.configure(1, frame);
        let mut cell = Cell::default();
        cell.set_text(&format!("🧑{}", "\u{1d185}".repeat(64)))
            .unwrap();
        cell.flags = crate::cells::flags::BLINK;
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
        let mut cache = RowCache::default();
        let frame = Frame {
            cols: 2,
            rows: 2,
            scrollback: crate::Scrollback {
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
                crate::cells::flags::BLINK
            } else {
                0
            };
            cache.insert(row as u64, &[cell.clone(), cell], &[row % 2]);
        }
        cache.truncate(8);
        assert!(cache.has_margin(
            Frame {
                scrollback: crate::Scrollback {
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
        let mut cache = RowCache::default();
        let frame = Frame {
            cols: 1000,
            rows: 20,
            scrollback: crate::Scrollback {
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
