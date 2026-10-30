//! Daemon policy for supplemental history, independent of client retention.
use gmux_protocol::{Frame, transport::cache_row_limit};

// Bound extraction work even for short text. The protocol separately reserves
// enough wire bytes for maximum-length graphemes in every transmitted cell.
const MAX_CELLS: usize = 48 * 1024;

/// Prefer two screens on either side of the viewport.
pub(super) fn window(frame: Frame) -> std::ops::Range<u64> {
    let cols = frame.cols as usize;
    if cols == 0 {
        return 0..0;
    }
    let count = (MAX_CELLS / cols)
        .min(cache_row_limit(cols))
        .min(frame.rows as usize * 5) as u64;
    if count < frame.rows as u64 || frame.scrollback.total <= frame.rows as u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_cover_the_view_and_bound_extraction_at_every_supported_size() {
        for cols in 1..=gmux_protocol::viewport::MAX_AXIS {
            for rows in [1, 24, 1000.min(gmux_protocol::viewport::MAX_CELLS / cols)] {
                for offset in [0, 2000, 10_000 - rows] {
                    let frame = Frame {
                        cols: cols as u16,
                        rows: rows as u16,
                        scrollback: gmux_protocol::Scrollback {
                            total: 10_000,
                            offset: offset as u64,
                            len: rows as u64,
                        },
                        ..Frame::default()
                    };
                    let window = window(frame);
                    if window.is_empty() {
                        assert!(MAX_CELLS / cols < rows || cache_row_limit(cols) < rows);
                    } else {
                        let count = (window.end - window.start) as usize;
                        assert!(count <= rows * 5 && count <= cache_row_limit(cols));
                        assert!(count * cols <= MAX_CELLS);
                        assert!(window.start <= offset as u64);
                        assert!(window.end >= (offset + rows) as u64);
                        assert!(window.end <= frame.scrollback.total);
                    }
                }
            }
        }
    }
}
