//! Sorted match ranges encoded as unsigned start deltas and lengths. Each
//! fixed-size page has enough metadata for ordinal and viewport lookup without
//! decoding the preceding pages. Match bytes grow one page at a time; the
//! directory contains only page metadata.
use super::{Range, Storage};

const PAGE_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct MatchIndex {
    pages: Vec<Page>,
    len: usize,
    last_start: usize,
}
struct Page {
    first: usize,
    before: usize,
    last_end: usize,
    bytes: Vec<u8>,
}
impl Page {
    fn ranges(&self) -> impl Iterator<Item = (usize, Range<usize>)> + '_ {
        let mut bytes = self.bytes.as_slice();
        let mut start = self.before;
        let mut index = self.first;
        std::iter::from_fn(move || {
            if bytes.is_empty() {
                return None;
            }
            start += decode(&mut bytes);
            let end = start + decode(&mut bytes);
            let current = index;
            index += 1;
            Some((current, start..end))
        })
    }
}
impl MatchIndex {
    pub(super) fn len(&self) -> usize {
        self.len
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(super) fn memory_size(&self) -> usize {
        self.pages.capacity() * size_of::<Page>()
            + self.pages.iter().map(|p| p.bytes.capacity()).sum::<usize>()
    }
    pub(super) fn clear(&mut self, storage: &mut Storage) {
        let bytes = self.memory_size();
        *self = Self::default();
        storage.release(bytes);
    }
    #[inline]
    pub(super) fn push(&mut self, range: Range<usize>, storage: &mut Storage) -> Result<(), ()> {
        // Different text matches within a grapheme can map to identical or
        // overlapping cell ranges. Start deltas, unlike end gaps, preserve them.
        debug_assert!(range.start >= self.last_start && range.end > range.start);
        let delta = range.start - self.last_start;
        let width = range.end - range.start;
        if delta < 128 && width < 128 {
            self.append(&[delta as u8, width as u8], range, storage)
        } else {
            let mut encoded = [0; 20];
            let first = encode(delta, &mut encoded);
            let len = first + encode(width, &mut encoded[first..]);
            self.append(&encoded[..len], range, storage)
        }
    }
    #[inline]
    fn append(
        &mut self,
        encoded: &[u8],
        range: Range<usize>,
        storage: &mut Storage,
    ) -> Result<(), ()> {
        if self
            .pages
            .last()
            .is_none_or(|p| p.bytes.len() + encoded.len() > PAGE_BYTES)
        {
            self.new_page(storage)?;
        }
        let page = self.pages.last_mut().unwrap();
        page.bytes.extend_from_slice(encoded);
        page.last_end = range.end;
        self.last_start = range.start;
        self.len += 1;
        Ok(())
    }
    #[cold]
    fn new_page(&mut self, storage: &mut Storage) -> Result<(), ()> {
        let needed = self.pages.len() + 1;
        storage.grow(&mut self.pages, needed)?;
        storage.acquire(PAGE_BYTES)?;
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(PAGE_BYTES).is_err() || bytes.capacity() > PAGE_BYTES {
            storage.release(PAGE_BYTES);
            return Err(());
        }
        storage.release(PAGE_BYTES - bytes.capacity());
        self.pages.push(Page {
            first: self.len,
            before: self.last_start,
            last_end: 0,
            bytes,
        });
        Ok(())
    }
    pub(super) fn get(&self, index: usize) -> Option<Range<usize>> {
        if index >= self.len {
            return None;
        }
        let page = self.pages.partition_point(|p| p.first <= index) - 1;
        let page = &self.pages[page];
        page.ranges().nth(index - page.first).map(|(_, r)| r)
    }
    pub(super) fn ranges_from(
        &self,
        base: usize,
    ) -> impl Iterator<Item = (usize, Range<usize>)> + '_ {
        let first = self.pages.partition_point(|p| p.last_end <= base);
        self.pages[first..]
            .iter()
            .flat_map(Page::ranges)
            .skip_while(move |(_, r)| r.end <= base)
    }
    pub(super) fn position(&self, range: &Range<usize>) -> Option<usize> {
        self.ranges_from(range.start)
            .take_while(|(_, r)| r.start <= range.start)
            .find_map(|(i, r)| (r == *range).then_some(i))
    }
}
fn encode(mut value: usize, out: &mut [u8]) -> usize {
    let mut len = 0;
    loop {
        out[len] = (value & 0x7f) as u8;
        value >>= 7;
        len += 1;
        if value == 0 {
            return len;
        }
        out[len - 1] |= 0x80;
    }
}
#[inline]
fn decode(bytes: &mut &[u8]) -> usize {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[0];
        *bytes = &bytes[1..];
        value |= ((byte & 0x7f) as usize) << shift;
        if byte < 0x80 {
            return value;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_preserve_duplicate_ranges_large_deltas_and_random_navigation() {
        let mut index = MatchIndex::default();
        let mut storage = Storage::default();
        let mut expected: Vec<Range<usize>> = Vec::new();
        for i in 0..20000 {
            // Both duplicate cell ranges and gaps larger than u32 must survive.
            let previous = expected.last().cloned().unwrap_or(0..0);
            let gap = match i % 6 {
                0 => 0,
                1 => 1,
                2 => 127,
                3 => 128,
                4 => 16384,
                _ => (u32::MAX as usize).min(usize::MAX / 40000),
            };
            let range = if i % 6 == 0 && i != 0 {
                previous
            } else {
                let start = previous.end + gap;
                start..start + 1 + i % 16384
            };
            index.push(range.clone(), &mut storage).unwrap();
            expected.push(range);
        }
        assert_eq!(index.memory_size(), storage.used);
        assert_eq!(index.len(), expected.len());
        for i in (0..expected.len()).step_by(79) {
            assert_eq!(index.get(i), Some(expected[i].clone()));
            let first = expected.iter().position(|r| r == &expected[i]).unwrap();
            assert_eq!(index.position(&expected[i]), Some(first));
            assert_eq!(
                index.ranges_from(expected[i].start).next(),
                Some((first, expected[i].clone()))
            );
        }
        assert_eq!(index.get(expected.len()), None);
        index.clear(&mut storage);
        assert_eq!(storage.used, 0);
    }
}
