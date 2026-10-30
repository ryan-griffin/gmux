//! OSC 8 targets for one displayed viewport. No toolkit or terminal parser.
use serde::{Deserialize, Serialize};

pub const MAX_URI_BYTES: usize = 16 * 1024;
pub const MAX_TARGET_BYTES: usize = 256 * 1024;
pub const MAX_SPANS: usize = 16_384;

/// Sorted, disjoint half-open cell ranges `[start, end, target_index]`.
/// Indices address the displayed grid, including wide-cell continuations.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hyperlinks {
    pub targets: Vec<String>,
    pub spans: Vec<[u32; 3]>,
}

impl Clone for Hyperlinks {
    fn clone(&self) -> Self {
        Self {
            targets: self.targets.clone(),
            spans: self.spans.clone(),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        if source.spans.is_empty() {
            *self = Self::default();
            return;
        }
        self.targets.clone_from(&source.targets);
        self.spans.clone_from(&source.spans);
        self.bound_capacity();
    }
}

impl Hyperlinks {
    /// Release spare string capacity accumulated as long targets move between
    /// table indices. Call after editing a valid map; stable maps keep storage.
    pub fn bound_capacity(&mut self) {
        let mut retained = self.targets.iter().map(String::capacity).sum::<usize>();
        if retained <= MAX_TARGET_BYTES {
            return;
        }
        for target in &mut self.targets {
            let previous = target.capacity();
            if previous > target.len() {
                target.shrink_to_fit();
                retained -= previous - target.capacity();
            }
            if retained <= MAX_TARGET_BYTES {
                break;
            }
        }
    }

    /// Resolve a point in the displayed grid without clamping into margins.
    pub fn target_at_point(
        &self,
        grid: (u16, u16),
        metrics: (f64, f64),
        point: (f64, f64),
    ) -> Option<&str> {
        let span = self.span_at_point(grid, metrics, point)?;
        self.targets.get(span[2] as usize).map(String::as_str)
    }

    /// Return the captured contiguous span, including wide-cell continuations.
    /// URI table indices do not encode OSC 8 logical link IDs.
    pub fn span_at_point(
        &self,
        grid: (u16, u16),
        metrics: (f64, f64),
        point: (f64, f64),
    ) -> Option<[u32; 3]> {
        let (x, y) = point;
        let (cw, ch) = metrics;
        if !cw.is_finite()
            || !ch.is_finite()
            || !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || cw <= 0.0
            || ch <= 0.0
        {
            return None;
        }
        let col = (x / cw).floor() as usize;
        let row = (y / ch).floor() as usize;
        if col >= grid.0 as usize || row >= grid.1 as usize {
            return None;
        }
        self.span_at(row * grid.0 as usize + col)
    }

    pub fn target_at(&self, cell: usize) -> Option<&str> {
        let span = self.span_at(cell)?;
        self.targets.get(span[2] as usize).map(String::as_str)
    }

    fn span_at(&self, cell: usize) -> Option<[u32; 3]> {
        let index = self.spans.partition_point(|span| span[1] as usize <= cell);
        let span = self.spans.get(index)?;
        (cell >= span[0] as usize).then_some(*span)
    }

    pub fn validate(&self, cells: usize) -> anyhow::Result<()> {
        anyhow::ensure!(self.spans.len() <= MAX_SPANS, "Too many hyperlink spans");
        anyhow::ensure!(
            self.targets.len() <= self.spans.len(),
            "Unused hyperlink targets"
        );
        let mut bytes = 0usize;
        for target in &self.targets {
            bytes += target.len();
            anyhow::ensure!(
                !target.is_empty()
                    && target.len() <= MAX_URI_BYTES
                    && !target.chars().any(char::is_control),
                "Invalid hyperlink target"
            );
        }
        anyhow::ensure!(bytes <= MAX_TARGET_BYTES, "Hyperlink targets exceed budget");
        let mut end = 0;
        for &[start, next, target] in &self.spans {
            anyhow::ensure!(
                start >= end
                    && start < next
                    && next as usize <= cells
                    && (target as usize) < self.targets.len(),
                "Invalid hyperlink span"
            );
            end = next;
        }
        Ok(())
    }

    pub fn memory_size(&self) -> usize {
        self.targets.capacity() * std::mem::size_of::<String>()
            + self.targets.iter().map(String::capacity).sum::<usize>()
            + self.spans.capacity() * std::mem::size_of::<[u32; 3]>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_lookup_includes_continuations_and_excludes_gaps_and_edges() {
        let links = Hyperlinks {
            targets: vec!["test:wide".into(), "test:wrapped".into()],
            spans: vec![[1, 3, 0], [5, 10, 1]],
        };
        links.validate(12).unwrap();
        for (cell, expected) in [
            (0, None),
            (1, Some("test:wide")),
            (2, Some("test:wide")),
            (3, None),
            (4, None),
            (5, Some("test:wrapped")),
            (9, Some("test:wrapped")),
            (10, None),
            (usize::MAX, None),
        ] {
            assert_eq!(links.target_at(cell), expected);
        }
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    #[test]
    fn rotating_long_targets_bounds_engine_and_cloned_view_storage() {
        let mut capture = Hyperlinks {
            targets: vec!["test:s".into(); 128],
            spans: (0..128).map(|i| [i, i + 1, i]).collect(),
        };
        let mut sent = Hyperlinks::default();
        let mut displayed = Hyperlinks::default();
        let long = "x".repeat(MAX_URI_BYTES);
        for index in 0..512 {
            for target in &mut capture.targets {
                target.clear();
                target.push_str("test:s");
            }
            capture.targets[index % 128].clone_from(&long);
            capture.validate(128).unwrap();
            capture.bound_capacity();
            sent.clone_from(&capture);
            displayed.clone_from(&sent);
            for map in [&capture, &sent, &displayed] {
                assert!(
                    map.targets.iter().map(String::capacity).sum::<usize>() <= MAX_TARGET_BYTES
                );
                assert_eq!(map.target_at(index % 128), Some(long.as_str()));
            }
        }
        sent.clone_from(&Hyperlinks::default());
        displayed.clone_from(&sent);
        assert_eq!(sent.memory_size(), 0);
        assert_eq!(displayed.memory_size(), 0);
    }
}
