use crate::{model::CursorStyle, theme::Theme};
use std::{
    ffi::{CString, c_char, c_void},
    ptr::NonNull,
};

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Cell {
    pub text: [u8; 64],
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub flags: u8,
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
    pub fn text(&self) -> &str {
        let end = self.text.iter().position(|&x| x == 0).unwrap_or(64);
        std::str::from_utf8(&self.text[..end]).unwrap_or("\u{fffd}")
    }
}
#[derive(Default, Clone, Copy)]
#[repr(C)]
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
}
#[derive(Default)]
struct Events {
    replies: Vec<u8>,
    notifications: Vec<String>,
    title_changed: bool,
}
unsafe extern "C" fn event(ud: *mut c_void, kind: i32, data: *const u8, len: usize) {
    // The C engine calls synchronously on the UI thread. Its userdata is a
    // stable Box owned by Engine and lives until after gmux_engine_free.
    let events = unsafe { &mut *ud.cast::<Events>() };
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    match kind {
        0 => events.replies.extend_from_slice(bytes),
        1 => events
            .notifications
            .push(String::from_utf8_lossy(bytes).into_owned()),
        2 => events.title_changed = true,
        _ => {}
    }
}
unsafe extern "C" {
    fn gmux_engine_new(
        ud: *mut c_void,
        cb: unsafe extern "C" fn(*mut c_void, i32, *const u8, usize),
    ) -> *mut c_void;
    fn gmux_engine_free(engine: *mut c_void);
    fn gmux_engine_theme(
        engine: *mut c_void,
        background: *const u8,
        foreground: *const u8,
        cursor: *const u8,
        palette: *const u8,
        dark: bool,
    ) -> i32;
    fn gmux_engine_write(engine: *mut c_void, data: *const u8, len: usize);
    fn gmux_engine_cursor_style(engine: *mut c_void, style: u8) -> i32;
    fn gmux_engine_resize(engine: *mut c_void, cols: u16, rows: u16, cw: u32, ch: u32) -> i32;
    fn gmux_engine_snapshot(
        engine: *mut c_void,
        frame: *mut Frame,
        cells: *mut Cell,
        cap: usize,
    ) -> i32;
    fn gmux_engine_scroll(engine: *mut c_void, delta: isize, bottom: bool);
    fn gmux_engine_expire_render_hold(engine: *mut c_void) -> bool;
    fn gmux_engine_string(engine: *mut c_void, pwd: i32, out: *mut u8, cap: usize) -> usize;
    fn gmux_engine_mode(engine: *mut c_void, mode: i32) -> i32;
    fn gmux_engine_paste(engine: *mut c_void, data: *const u8, len: usize, clipboard: bool) -> i32;
    fn gmux_engine_key(
        engine: *mut c_void,
        name: *const c_char,
        cp: u32,
        mods: u16,
        release: bool,
        text: *const c_char,
        len: usize,
        out: *mut u8,
        cap: usize,
    ) -> usize;
    fn gmux_engine_mouse(
        engine: *mut c_void,
        x: f32,
        y: f32,
        cw: u32,
        ch: u32,
        action: i32,
        button: i32,
        mods: u16,
        pressed: bool,
        out: *mut u8,
        cap: usize,
    ) -> usize;
}

pub struct Engine {
    raw: NonNull<c_void>,
    events: Box<Events>,
    pub frame: Frame,
    pub cells: Vec<Cell>,
    revision: u64,
}
impl Engine {
    pub fn new() -> anyhow::Result<Self> {
        let mut events = Box::<Events>::default();
        let raw =
            NonNull::new(unsafe { gmux_engine_new((&mut *events as *mut Events).cast(), event) })
                .ok_or_else(|| anyhow::anyhow!("Could not initialize libghostty-vt"))?;
        let mut engine = Self {
            raw,
            events,
            frame: Frame::default(),
            cells: vec![Cell::default(); 80 * 24],
            revision: 0,
        };
        engine.set_theme(&Theme::adwaita(true))?;
        engine.set_cursor_style(CursorStyle::default())?;
        Ok(engine)
    }
    pub fn set_cursor_style(&mut self, style: CursorStyle) -> anyhow::Result<()> {
        let result = unsafe { gmux_engine_cursor_style(self.raw.as_ptr(), style as u8) };
        anyhow::ensure!(result == 0, "Ghostty cursor style update failed ({result})");
        Ok(())
    }
    pub fn set_theme(&mut self, theme: &Theme) -> anyhow::Result<()> {
        let result = unsafe {
            gmux_engine_theme(
                self.raw.as_ptr(),
                theme.background.as_ptr(),
                theme.foreground.as_ptr(),
                theme.cursor.as_ptr(),
                theme.palette.as_ptr().cast(),
                theme.dark,
            )
        };
        anyhow::ensure!(result == 0, "Ghostty theme update failed ({result})");
        Ok(())
    }
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { gmux_engine_write(self.raw.as_ptr(), bytes.as_ptr(), bytes.len()) }
    }
    pub fn resize(&mut self, cols: u16, rows: u16, cw: u32, ch: u32) -> anyhow::Result<()> {
        let result = unsafe { gmux_engine_resize(self.raw.as_ptr(), cols, rows, cw, ch) };
        anyhow::ensure!(result == 0, "Ghostty resize failed ({result})");
        self.cells
            .resize(cols as usize * rows as usize, Cell::default());
        Ok(())
    }
    /// Refresh frame metadata and changed rows, returning whether cells changed.
    pub fn snapshot(&mut self) -> anyhow::Result<bool> {
        let result = unsafe {
            gmux_engine_snapshot(
                self.raw.as_ptr(),
                &mut self.frame,
                self.cells.as_mut_ptr(),
                self.cells.len(),
            )
        };
        anyhow::ensure!(result >= 0, "Ghostty render-state update failed");
        if result != 0 {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(result != 0)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn expire_render_hold(&mut self) -> bool {
        unsafe { gmux_engine_expire_render_hold(self.raw.as_ptr()) }
    }
    pub fn scroll(&mut self, delta: isize, bottom: bool) {
        unsafe { gmux_engine_scroll(self.raw.as_ptr(), delta, bottom) }
    }
    pub fn metadata(&self, pwd: bool) -> String {
        let mut out = [0; 8192];
        let len = unsafe {
            gmux_engine_string(self.raw.as_ptr(), pwd as i32, out.as_mut_ptr(), out.len())
        };
        String::from_utf8_lossy(&out[..len]).into_owned()
    }
    pub fn mode(&self, mode: i32) -> bool {
        unsafe { gmux_engine_mode(self.raw.as_ptr(), mode) != 0 }
    }
    pub fn paste(&mut self, text: &str, clipboard: bool) -> anyhow::Result<Vec<u8>> {
        let start = self.events.replies.len();
        let result =
            unsafe { gmux_engine_paste(self.raw.as_ptr(), text.as_ptr(), text.len(), clipboard) };
        anyhow::ensure!(result == 0, "Ghostty paste failed ({result})");
        // Return only this paste's bytes. Device replies already queued by
        // terminal output remain available to the PTY update loop.
        Ok(self.events.replies.split_off(start))
    }
    pub fn take_events(&mut self) -> (Vec<u8>, Vec<String>) {
        (
            std::mem::take(&mut self.events.replies),
            std::mem::take(&mut self.events.notifications),
        )
    }
    pub fn take_title_update(&mut self) -> Option<String> {
        std::mem::take(&mut self.events.title_changed).then(|| self.metadata(false))
    }
    pub fn key(&mut self, name: &str, cp: u32, mods: u16, release: bool, text: &str) -> Vec<u8> {
        let Ok(name) = CString::new(name) else {
            return vec![];
        };
        let mut out = [0; 1024];
        let len = unsafe {
            gmux_engine_key(
                self.raw.as_ptr(),
                name.as_ptr(),
                cp,
                mods,
                release,
                text.as_ptr().cast(),
                text.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        out[..len].to_vec()
    }
    pub fn text(&mut self) -> anyhow::Result<String> {
        self.snapshot()?;
        Ok(self
            .cells
            .chunks(self.frame.cols as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        if cell.flags & 64 != 0 {
                            ""
                        } else if cell.text().is_empty() {
                            " "
                        } else {
                            cell.text()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string())
    }
    pub fn mouse(
        &mut self,
        position: (f32, f32),
        cell: (u32, u32),
        action: i32,
        button: i32,
        mods: u16,
        pressed: bool,
    ) -> Vec<u8> {
        let mut out = [0; 128];
        let len = unsafe {
            gmux_engine_mouse(
                self.raw.as_ptr(),
                position.0,
                position.1,
                cell.0,
                cell.1,
                action,
                button,
                mods,
                pressed,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        out[..len].to_vec()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { gmux_engine_free(self.raw.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_uses_terminal_modes_and_preserves_queued_device_replies() {
        let mut engine = Engine::new().unwrap();
        engine.write(b"\x1b[6n");
        assert_eq!(engine.paste("one\ntwo", false).unwrap(), b"one\rtwo");
        assert_eq!(engine.take_events().0, b"\x1b[1;1R");
        engine.write(b"\x1b[?2004h");
        assert_eq!(
            engine.paste("one\ntwo 界", true).unwrap(),
            "\x1b[200~one\ntwo 界\x1b[201~".as_bytes()
        );
        assert_eq!(
            engine.paste("\x1b[201~\0", true).unwrap(),
            b"\x1b[200~ [201~ \x1b[201~"
        );
        assert!(engine.paste("", true).unwrap().is_empty());
    }
    #[test]
    fn incremental_snapshots_keep_clean_rows_and_follow_resize_and_scrollback() {
        let mut engine = Engine::new().unwrap();
        engine.resize(12, 3, 8, 16).unwrap();
        engine.write(b"one\r\ntwo\r\nthree");
        assert!(engine.snapshot().unwrap());
        assert!(!engine.snapshot().unwrap());
        engine.write(b"\x1b[2;1HTWO");
        assert!(engine.snapshot().unwrap());
        assert_eq!(engine.text().unwrap(), "one\nTWO\nthree");
        engine.resize(20, 3, 8, 16).unwrap();
        assert_eq!(engine.text().unwrap(), "one\nTWO\nthree");
        engine.write(b"\x1b[3;1H\r\nfour\r\nfive");
        assert_eq!(engine.text().unwrap(), "three\nfour\nfive");
        engine.scroll(-2, false);
        assert_eq!(engine.text().unwrap(), "one\nTWO\nthree");
        engine.scroll(0, true);
        assert_eq!(engine.text().unwrap(), "three\nfour\nfive");
    }

    #[test]
    fn synchronized_output_preserves_the_last_complete_frame() {
        let mut engine = Engine::new().unwrap();
        engine.write(b"previous");
        assert_eq!(engine.text().unwrap(), "previous");
        // Capture output before the hold, not merely the most recent snapshot.
        engine.write(b"\r\x1b[2Kcomplete\x1b[?2026h\r\x1b[2Kpartial");
        assert_eq!(engine.text().unwrap(), "complete");
        assert_eq!(engine.text().unwrap(), "complete");
        engine.write(b" frame\x1b[?2026l");
        assert_eq!(engine.text().unwrap(), "partial frame");
        // Back-to-back frames in one read must not freeze on the first frame.
        engine.write(b"\x1b[?2026h\r\x1b[2Knext\x1b[?2026l\x1b[?2026h\r\x1b[2Kunfinished");
        assert_eq!(engine.text().unwrap(), "next");
        engine.resize(100, 30, 8, 16).unwrap();
        assert_eq!(engine.text().unwrap(), "unfinished");
    }

    #[test]
    fn abandoned_synchronized_output_expires_without_more_pty_output() {
        let mut engine = Engine::new().unwrap();
        engine.write(b"before\x1b[?2026h\r\x1b[2Kafter");
        assert_eq!(engine.text().unwrap(), "before");
        assert!(!engine.expire_render_hold());
        std::thread::sleep(std::time::Duration::from_millis(1050));
        assert!(engine.expire_render_hold());
        assert!(!engine.mode(2026));
        assert_eq!(engine.text().unwrap(), "after");
    }

    #[test]
    fn terminal_state_and_device_replies() {
        let mut e = Engine::new().unwrap();
        e.write(b"hello\r\n\x1b[31mred\x1b[0m\x1b[6n");
        assert!(e.text().unwrap().starts_with("hello\nred"));
        assert_eq!(e.take_events().0, b"\x1b[2;4R");
        assert_ne!(e.cells[80].fg, e.cells[0].fg);
        e.write(b"\x1b[?1049halt screen\x1b[?1049l");
        assert!(e.text().unwrap().starts_with("hello\nred"));
    }
    #[test]
    fn notifications_title_and_chunked_utf8() {
        let mut e = Engine::new().unwrap();
        e.write(b"\x1b]2;project\x07\x1b]777;notify;Agent;Ready\x07");
        assert_eq!(e.metadata(false), "project");
        assert_eq!(e.take_events().1, vec!["Ready"]);
        e.write(&[0xc3]);
        e.write(&[0xa9]);
        assert!(e.text().unwrap().contains('é'));
    }
    #[test]
    fn title_updates_include_repeated_and_empty_titles() {
        let mut e = Engine::new().unwrap();
        assert_eq!(e.take_title_update(), None);
        e.write(b"\x1b]2;OpenCode\x07");
        assert_eq!(e.take_title_update().as_deref(), Some("OpenCode"));
        e.write(b"ordinary output");
        assert_eq!(e.take_title_update(), None);
        e.write(b"\x1b]0;OpenCode\x1b\\");
        assert_eq!(e.take_title_update().as_deref(), Some("OpenCode"));
        e.write(b"\x1b]2;\x07");
        assert_eq!(e.take_title_update().as_deref(), Some(""));
    }
    #[test]
    fn keys_respect_terminal_modes() {
        let mut e = Engine::new().unwrap();
        assert_eq!(e.key("space", ' ' as u32, 0, false, " "), b" ");
        assert_eq!(e.key("c", 'c' as u32, 2, false, "c"), vec![3]);
        assert_eq!(e.key("Up", 0, 0, false, ""), b"\x1b[A");
        e.write(b"\x1b[?1h");
        assert_eq!(e.key("Up", 0, 0, false, ""), b"\x1bOA");
        e.write(b"\x1b[?2004h");
        assert!(e.mode(2004));
    }

    #[test]
    fn keypad_keys_respect_application_mode() {
        let mut e = Engine::new().unwrap();
        assert_eq!(e.key("KP_1", '1' as u32, 0, false, "1"), b"1");
        e.write(b"\x1b[?1035l\x1b=");
        assert_eq!(e.key("KP_1", '1' as u32, 0, false, "1"), b"\x1bOq");
        assert_eq!(e.key("KP_Add", '+' as u32, 0, false, "+"), b"\x1bOk");
        assert_eq!(e.key("KP_Enter", '\r' as u32, 0, false, ""), b"\x1bOM");
        e.write(b"\x1b>");
        assert_eq!(e.key("KP_1", '1' as u32, 0, false, "1"), b"1");
        e.write(b"\x1b[>31u");
        assert_eq!(e.key("KP_1", '1' as u32, 0, false, "1"), b"\x1b[57400;;49u");
    }

    #[test]
    fn mouse_reporting_uses_the_terminal_protocol() {
        let mut e = Engine::new().unwrap();
        e.write(b"\x1b[?1000h\x1b[?1006h");
        assert!(e.mode(0));
        assert_eq!(
            e.mouse((15.0, 26.0), (10, 20), 0, 1, 0, true),
            b"\x1b[<0;2;2M"
        );
        assert_eq!(
            e.mouse((15.0, 26.0), (10, 20), 1, 1, 0, true),
            b"\x1b[<0;2;2m"
        );
    }

    #[test]
    fn wide_and_combining_characters_survive_render_state() {
        let mut e = Engine::new().unwrap();
        e.write("你好 e\u{301}".as_bytes());
        assert_eq!(e.text().unwrap(), "你好 e\u{301}");
        assert_ne!(e.cells[1].flags & 64, 0);
        assert_ne!(e.cells[3].flags & 64, 0);
    }

    #[test]
    fn adwaita_colors_update_existing_text_and_color_scheme_reports() {
        let mut e = Engine::new().unwrap();
        let dark = Theme::adwaita(true);
        e.write(b"plain\x1b[34mblue\x1b[0m\x1b[?996n");
        e.snapshot().unwrap();
        assert_eq!(e.frame.background, dark.background);
        assert_eq!(e.frame.foreground, dark.foreground);
        assert_eq!(e.frame.cursor, dark.cursor);
        assert_eq!(e.cells[0].fg, dark.foreground);
        assert_eq!(e.cells[5].fg, dark.palette[4]);
        assert_eq!(e.take_events().0, b"\x1b[?997;1n");
        e.write(b"\x1b[?2031h");
        e.take_events();
        let light = Theme::adwaita(false);
        e.set_theme(&light).unwrap();
        assert_eq!(e.take_events().0, b"\x1b[?997;2n");
        e.snapshot().unwrap();
        assert_eq!(e.frame.background, light.background);
        assert_eq!(e.frame.foreground, light.foreground);
        assert_eq!(e.frame.cursor, light.cursor);
        assert_eq!(e.cells[0].fg, light.foreground);
        assert_eq!(e.cells[5].fg, light.palette[4]);
        assert!(e.text().unwrap().contains("plainblue"));
    }

    #[test]
    fn cursor_defaults_update_live_and_survive_application_overrides_and_resets() {
        let mut e = Engine::new().unwrap();
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Bar as u8);
        for style in CursorStyle::ALL {
            e.set_cursor_style(style).unwrap();
            e.snapshot().unwrap();
            assert_eq!(e.frame.cursor_style, style as u8);
        }
        e.write(b"\x1b[2 q");
        e.set_cursor_style(CursorStyle::Underline).unwrap();
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Block as u8);
        e.write(b"\x1b[0 q");
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Underline as u8);
        e.write(b"\x1b[6 q\x1bc");
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Underline as u8);
        e.write(b"\x1b[?1049h\x1b[0 q");
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Underline as u8);
        e.write(b"\x1b[?1049l\x1b[0 q");
        e.snapshot().unwrap();
        assert_eq!(e.frame.cursor_style, CursorStyle::Underline as u8);
    }

    #[test]
    fn program_color_overrides_survive_theme_changes_and_reset_to_new_defaults() {
        let mut e = Engine::new().unwrap();
        e.write(b"\x1b]10;#abcdef\x07\x1b]11;#123456\x07\x1b]12;#fedcba\x07\x1b]4;4;#456789\x07\x1b[34mblue");
        let light = Theme::adwaita(false);
        e.set_theme(&light).unwrap();
        e.snapshot().unwrap();
        assert_eq!(e.frame.foreground, [0xab, 0xcd, 0xef]);
        assert_eq!(e.frame.background, [0x12, 0x34, 0x56]);
        assert_eq!(e.frame.cursor, [0xfe, 0xdc, 0xba]);
        assert_eq!(e.cells[0].fg, [0x45, 0x67, 0x89]);
        e.write(b"\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b]104;4\x07");
        e.snapshot().unwrap();
        assert_eq!(e.frame.foreground, light.foreground);
        assert_eq!(e.frame.background, light.background);
        assert_eq!(e.frame.cursor, light.cursor);
        assert_eq!(e.cells[0].fg, light.palette[4]);
    }
}
