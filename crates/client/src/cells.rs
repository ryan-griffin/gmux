//! Terminal viewport values, independent of the terminal engine's native library.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub text: [u8; 64],
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub flags: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scrollback {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: u8,
    pub cursor_style: u8,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub cursor: [u8; 3],
    pub scrollback: Scrollback,
}
impl Default for Cell {
    fn default() -> Self {
        Self {
            text: [0; 64],
            fg: [0; 3],
            bg: [0; 3],
            flags: 0,
        }
    }
}
impl Cell {
    /// Compare stored glyphs and wide-cell layout, ignoring rendition.
    pub fn text_eq(&self, other: &Self) -> bool {
        self.text == other.text && (self.flags ^ other.flags) & (64 | 128) == 0
    }
    pub fn text(&self) -> &str {
        let end = self.text.iter().position(|&x| x == 0).unwrap_or(64);
        std::str::from_utf8(&self.text[..end]).unwrap_or("\u{fffd}")
    }
}
/// Plain text for the displayed grid, with wide-cell continuations omitted.
pub fn viewport_text(frame: Frame, cells: &[Cell]) -> String {
    let mut text = String::with_capacity(cells.len());
    for (index, row) in cells.chunks(frame.cols as usize).enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let start = text.len();
        for cell in row {
            if cell.flags & 64 != 0 {
                continue;
            }
            let glyph = cell.text();
            text.push_str(if glyph.is_empty() { " " } else { glyph });
        }
        text.truncate(start + text[start..].trim_end().len());
    }
    text.truncate(text.trim_end().len());
    text
}
