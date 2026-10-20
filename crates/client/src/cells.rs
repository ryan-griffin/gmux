//! Terminal viewport values, independent of the terminal engine's native library.

/// Underline codes stored in the low bits of `Cell::decoration`.
/// The byte remains raw so unknown protocol/FFI values are valid Rust values.
pub mod underline {
    pub const NONE: u8 = 0;
    pub const SINGLE: u8 = 1;
    pub const DOUBLE: u8 = 2;
    pub const CURLY: u8 = 3;
    pub const DOTTED: u8 = 4;
    pub const DASHED: u8 = 5;
    pub const STYLES: [u8; 5] = [SINGLE, DOUBLE, CURLY, DOTTED, DASHED];
}

pub mod decoration {
    pub const UNDERLINE_MASK: u8 = 7;
    pub const OVERLINE: u8 = 8;
    pub const UNDERLINE_COLOR: u8 = 16;
}

/// W3C pointer names, with an explicit protocol code for each shape.
/// Unknown codes use the terminal's normal text pointer.
pub mod mouse_shape {
    macro_rules! shapes {
        ($($constant:ident = $code:literal => $name:literal),* $(,)?) => {
            $(pub const $constant: u8 = $code;)*
            pub fn name(shape: u8) -> &'static str {
                match shape {
                    $($constant => $name,)*
                    _ => "text",
                }
            }
        };
    }
    shapes! {
        DEFAULT = 0 => "default",
        CONTEXT_MENU = 1 => "context-menu",
        HELP = 2 => "help",
        POINTER = 3 => "pointer",
        PROGRESS = 4 => "progress",
        WAIT = 5 => "wait",
        CELL = 6 => "cell",
        CROSSHAIR = 7 => "crosshair",
        TEXT = 8 => "text",
        VERTICAL_TEXT = 9 => "vertical-text",
        ALIAS = 10 => "alias",
        COPY = 11 => "copy",
        MOVE = 12 => "move",
        NO_DROP = 13 => "no-drop",
        NOT_ALLOWED = 14 => "not-allowed",
        GRAB = 15 => "grab",
        GRABBING = 16 => "grabbing",
        ALL_SCROLL = 17 => "all-scroll",
        COL_RESIZE = 18 => "col-resize",
        ROW_RESIZE = 19 => "row-resize",
        N_RESIZE = 20 => "n-resize",
        E_RESIZE = 21 => "e-resize",
        S_RESIZE = 22 => "s-resize",
        W_RESIZE = 23 => "w-resize",
        NE_RESIZE = 24 => "ne-resize",
        NW_RESIZE = 25 => "nw-resize",
        SE_RESIZE = 26 => "se-resize",
        SW_RESIZE = 27 => "sw-resize",
        EW_RESIZE = 28 => "ew-resize",
        NS_RESIZE = 29 => "ns-resize",
        NESW_RESIZE = 30 => "nesw-resize",
        NWSE_RESIZE = 31 => "nwse-resize",
        ZOOM_IN = 32 => "zoom-in",
        ZOOM_OUT = 33 => "zoom-out",
    }
}
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub text: [u8; 64],
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub flags: u8,
    /// Underline style in bits 0-2, overline in bit 3, explicit underline color in bit 4.
    pub decoration: u8,
    pub underline_color: [u8; 3],
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
    pub cursor_blink: u8,
    pub mouse_shape: u8,
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
            decoration: 0,
            underline_color: [0; 3],
        }
    }
}
impl Cell {
    pub fn underline(&self) -> u8 {
        self.decoration & decoration::UNDERLINE_MASK
    }
    pub fn overline(&self) -> bool {
        self.decoration & decoration::OVERLINE != 0
    }
    pub fn has_underline_color(&self) -> bool {
        self.decoration & decoration::UNDERLINE_COLOR != 0
    }
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
