use crate::{Cell, Frame};
/// A borrowed viewport. Taking a snapshot does not copy the cell buffer.
pub struct ScreenSnapshot<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    /// OSC 8 targets captured with this frame; empty for cached scroll previews.
    pub hyperlinks: &'a crate::hyperlinks::Hyperlinks,
    /// Opaque token that changes with the map, dimensions or prediction state.
    pub hyperlink_revision: u64,
    /// Bit zero marks a soft wrap to the next row.
    pub row_flags: &'a [u8],
    pub revision: u64,
    /// Changes when text, wide-cell layout, or soft wraps may differ.
    pub text_revision: u64,
}

#[derive(Default)]
pub struct SessionUpdate {
    pub changed: bool,
    pub exited: bool,
    pub directory_changed: bool,
    pub git_changed: bool,
    pub progress_changed: bool,
    pub unread: Option<bool>,
    pub error: Option<String>,
    pub notifications: Vec<String>,
}
