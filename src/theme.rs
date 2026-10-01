// Adwaita and Adwaita Dark, from the theme collection distributed with Ghostty:
// https://github.com/mbadolato/iTerm2-Color-Schemes/tree/master/ghostty
#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub cursor: [u8; 3],
    pub selection_background: [u8; 3],
    pub selection_foreground: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

const fn rgb(hex: u32) -> [u8; 3] {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]
}

impl Theme {
    pub fn adwaita(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                background: rgb(0x1c1c1f),
                foreground: rgb(0xffffff),
                cursor: rgb(0xffffff),
                selection_background: rgb(0xffffff),
                selection_foreground: rgb(0x1c1c1f),
                palette: [
                    rgb(0x241f31),
                    rgb(0xc01c28),
                    rgb(0x2ec27e),
                    rgb(0xf5c211),
                    rgb(0x1e78e4),
                    rgb(0x9841bb),
                    rgb(0x0ab9dc),
                    rgb(0xc0bfbc),
                    rgb(0x5e5c64),
                    rgb(0xed333b),
                    rgb(0x57e389),
                    rgb(0xf8e45c),
                    rgb(0x51a1ff),
                    rgb(0xc061cb),
                    rgb(0x4fd2fd),
                    rgb(0xf6f5f4),
                ],
            }
        } else {
            Self {
                dark,
                background: rgb(0xffffff),
                foreground: rgb(0x1d1d20),
                cursor: rgb(0x1d1d20),
                selection_background: rgb(0x1d1d20),
                selection_foreground: rgb(0xffffff),
                palette: [
                    rgb(0x1d1d20),
                    rgb(0xc01c28),
                    rgb(0x26a269),
                    rgb(0xa2734c),
                    rgb(0x12488b),
                    rgb(0xa347ba),
                    rgb(0x2aa1b3),
                    rgb(0xc2c2c2),
                    rgb(0x5d5d5d),
                    rgb(0xf66151),
                    rgb(0x33d17a),
                    rgb(0xe9ad0c),
                    rgb(0x2a7bde),
                    rgb(0xc061cb),
                    rgb(0x33c7de),
                    rgb(0xffffff),
                ],
            }
        }
    }
}
