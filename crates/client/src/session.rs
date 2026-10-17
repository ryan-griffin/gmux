use crate::{Cell, Frame};
/// A borrowed viewport. Taking a snapshot does not copy the cell buffer.
pub struct ScreenSnapshot<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    pub revision: u64,
    /// Changes when text or wide-cell layout may differ.
    pub text_revision: u64,
}

#[derive(Default)]
pub struct SessionUpdate {
    pub changed: bool,
    pub exited: bool,
    pub directory_changed: bool,
    pub git_changed: bool,
    pub unread: Option<bool>,
    pub error: Option<String>,
    pub notifications: Vec<String>,
    pub copy: Option<String>,
}
