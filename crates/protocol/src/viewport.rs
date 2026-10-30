//! Shared limits for terminal grids and their streamed cell buffers.

pub const MAX_AXIS: usize = 1000;
pub const MAX_CELLS: usize = 131_072;

pub fn valid(cols: u64, rows: u64) -> bool {
    (1..=MAX_AXIS as u64).contains(&cols)
        && (1..=MAX_AXIS as u64).contains(&rows)
        && cols * rows <= MAX_CELLS as u64
}

pub fn constrain(cols: usize, rows: usize) -> (u16, u16) {
    let cols = cols.clamp(1, MAX_AXIS);
    let rows = rows.clamp(1, MAX_AXIS).min(MAX_CELLS / cols);
    (cols as u16, rows as u16)
}

/// Reuse overlapping rows before applying a packet's changed rows.
pub fn shift_rows<T>(data: &mut [T], cols: usize, rows: i16) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids_remain_positive_and_fit_the_daemon_and_packet_limits() {
        for (cols, rows) in [(0, 0), (80, 24), (1000, 1000), (usize::MAX, usize::MAX)] {
            let constrained = constrain(cols, rows);
            assert!(valid(constrained.0 as u64, constrained.1 as u64));
        }
        assert_eq!(constrain(80, 24), (80, 24));
        assert_eq!(constrain(1000, 1000), (1000, 131));
        assert!(!valid(0, 24));
        assert!(!valid(1000, 1000));
        assert!(!valid(u64::MAX, u64::MAX));
    }
}
