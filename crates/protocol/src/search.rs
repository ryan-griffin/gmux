//! Shared Find values and query limits.

/// Maximum query input in UTF-8 bytes before lowercase normalization.
pub const MAX_QUERY_BYTES: usize = 4096;

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
