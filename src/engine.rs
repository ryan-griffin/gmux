use crate::{model::CursorStyle, theme::Theme};
use std::{
    ffi::{CString, c_void},
    ptr::NonNull,
};

// Generated declarations and compile-time layout checks for our C shim.
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/ghostty_bridge.rs"));
}
use ffi::*;
pub use ffi::{Cell, Frame, Scrollback};

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
#[derive(Default)]
struct Events {
    replies: Vec<u8>,
    notifications: Vec<String>,
    title_changed: bool,
    directory_changed: bool,
}
unsafe extern "C" fn event(ud: *mut c_void, kind: i32, data: *const u8, len: usize) {
    // The C engine calls synchronously on the thread owning the session.
    // Its userdata is a stable Box owned by Engine, valid until gmux_engine_free.
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
        3 => events.directory_changed = true,
        _ => {}
    }
}

pub struct Engine {
    raw: NonNull<GmuxEngine>,
    events: Box<Events>,
    pub frame: Frame,
    pub cells: Vec<Cell>,
    size: (u16, u16),
    revision: u64,
    find: crate::search::HistorySearch,
    find_row: Vec<Cell>,
}
impl Engine {
    pub fn new() -> anyhow::Result<Self> {
        let mut events = Box::<Events>::default();
        let raw = NonNull::new(unsafe {
            gmux_engine_new((&mut *events as *mut Events).cast(), Some(event))
        })
        .ok_or_else(|| anyhow::anyhow!("Could not initialize libghostty-vt"))?;
        let mut engine = Self {
            raw,
            events,
            frame: Frame::default(),
            cells: vec![Cell::default(); 80 * 24],
            size: (80, 24),
            revision: 0,
            find: crate::search::HistorySearch::default(),
            find_row: Vec::new(),
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
        self.find.invalidate();
    }
    pub fn resize(&mut self, cols: u16, rows: u16, cw: u32, ch: u32) -> anyhow::Result<()> {
        let result = unsafe { gmux_engine_resize(self.raw.as_ptr(), cols, rows, cw, ch) };
        anyhow::ensure!(result == 0, "Ghostty resize failed ({result})");
        if self.size != (cols, rows) {
            self.find.invalidate();
            self.find.pending = false;
            self.cells
                .resize(cols as usize * rows as usize, Cell::default());
            self.size = (cols, rows);
        }
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
    pub fn find(&mut self, query: &str) {
        self.find.set_query(query);
    }
    pub fn find_pending(&self) -> bool {
        self.find.pending || self.find.dirty
    }
    pub fn poll_find(&mut self) -> bool {
        if !self.find_pending() {
            return false;
        }
        if !self.find.pending {
            let (mut cols, mut rows) = (0, 0);
            if unsafe { gmux_engine_history_size(self.raw.as_ptr(), &mut cols, &mut rows) } != 0
                || cols == 0
            {
                return false;
            }
            self.find.begin(rows as u64, cols as usize);
            self.find_row.resize(cols as usize, Cell::default());
            if rows == 0 {
                self.find.finish();
            }
        }
        for _ in 0..64 {
            if !self.find.pending {
                break;
            }
            let mut wrapped = false;
            if unsafe {
                gmux_engine_history_row(
                    self.raw.as_ptr(),
                    self.find.next as u32,
                    self.find_row.as_mut_ptr(),
                    self.find.cols,
                    &mut wrapped,
                )
            } != 0
            {
                self.find.finish();
                self.find.invalidate();
                break;
            }
            self.find.feed(&self.find_row, wrapped);
        }
        true
    }
    pub fn find_state(&self) -> crate::search::FindState {
        self.find
            .state(self.frame.scrollback.offset, self.cells.len())
    }
    pub fn find_advance(&mut self, previous: bool) {
        if let Some(row) = self.find.advance(previous) {
            let offset = self.frame.scrollback.offset;
            if row < offset || row >= offset + self.frame.rows as u64 {
                self.scroll_to(row);
            }
        }
    }
    pub fn select(&mut self, start: (u16, u32), end: (u16, u32)) -> anyhow::Result<()> {
        let result =
            unsafe { gmux_engine_select(self.raw.as_ptr(), start.0, start.1, end.0, end.1) };
        anyhow::ensure!(result == 0, "Selection is outside terminal history");
        Ok(())
    }
    pub fn clear_selection(&mut self) {
        unsafe { gmux_engine_clear_selection(self.raw.as_ptr()) }
    }
    pub fn selection(&self) -> Option<(u64, u64)> {
        let (mut start, mut end) = (0, 0);
        (unsafe { gmux_engine_selection(self.raw.as_ptr(), &mut start, &mut end) } == 0)
            .then_some((start, end))
    }
    pub fn copy(&self) -> anyhow::Result<String> {
        let mut written = 0;
        let result =
            unsafe { gmux_engine_copy(self.raw.as_ptr(), std::ptr::null_mut(), 0, &mut written) };
        anyhow::ensure!(
            result >= 0 && written <= crate::transport::MAX_PASTE,
            "Selection is too large to copy"
        );
        let mut bytes = vec![0; written];
        let result = unsafe {
            gmux_engine_copy(
                self.raw.as_ptr(),
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut written,
            )
        };
        anyhow::ensure!(
            result == 0 && written <= bytes.len(),
            "Could not copy terminal selection"
        );
        bytes.truncate(written);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
    pub fn scroll_to(&mut self, row: u64) {
        unsafe { gmux_engine_scroll_to(self.raw.as_ptr(), row) }
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
    pub fn take_directory_update(&mut self) -> bool {
        std::mem::take(&mut self.events.directory_changed)
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
        let mut text = String::with_capacity(self.cells.len());
        for (index, row) in self.cells.chunks(self.frame.cols as usize).enumerate() {
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
        Ok(text)
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
    fn history_find_preserves_unicode_matching_and_selection_across_scrolling() {
        let mut engine = Engine::new().unwrap();
        engine.resize(30, 3, 8, 16).unwrap();
        engine.write("needle É e\u{301} İ\r\n".as_bytes());
        for i in 0..200 {
            engine.write(format!("line-{i:03}\r\n").as_bytes());
        }
        for query in ["needle", "é", "e\u{301}", "i\u{307}"] {
            engine.find(query);
            for _ in 0..100 {
                if !engine.find_pending() {
                    break;
                }
                engine.poll_find();
            }
            assert!(!engine.find_pending());
            assert_eq!(engine.find_state().total, 1, "{query}");
        }
        engine.find("needle");
        engine.poll_find();
        assert!(
            engine.find_pending(),
            "History scans must yield between batches"
        );
        while engine.find_pending() {
            engine.poll_find();
        }
        engine.snapshot().unwrap();
        assert!(engine.frame.scrollback.offset > 0);
        engine.find_advance(false);
        assert!(engine.text().unwrap().starts_with("needle"));
        engine.select((0, 0), (7, 1)).unwrap();
        let copied = engine.copy().unwrap();
        assert!(copied.starts_with("needle É"));
        assert!(copied.contains("line-000"));
        engine.scroll(0, true);
        engine.write(b"another line\r\n");
        assert_eq!(engine.copy().unwrap(), copied);
        engine.clear_selection();
        assert_eq!(engine.copy().unwrap(), "");
        engine.select((11, 201), (0, 0)).unwrap();
        let copied = engine.copy().unwrap();
        assert!(copied.starts_with("needle É"));
        assert!(copied.ends_with("another line"));
        assert!(copied.contains("line-199"));
        engine.resize(10, 3, 8, 16).unwrap();
        assert_eq!(
            engine.copy().unwrap(),
            copied,
            "Reflow must retain selection endpoints"
        );
    }

    #[test]
    fn cell_metric_updates_preserve_completed_and_pending_history_searches() {
        let mut engine = Engine::new().unwrap();
        engine.resize(30, 3, 8, 16).unwrap();
        for i in 0..200 {
            engine.write(format!("needle {i}\r\n").as_bytes());
        }
        engine.find("needle");
        engine.poll_find();
        assert!(engine.find.pending);
        let next = engine.find.next;
        engine.resize(30, 3, 9, 18).unwrap();
        assert!(engine.find.pending);
        assert_eq!(engine.find.next, next);
        while engine.find_pending() {
            engine.poll_find();
        }
        assert_eq!(engine.find_state().total, 200);
        engine.resize(30, 3, 10, 20).unwrap();
        assert!(!engine.find_pending());
        assert_eq!(engine.find_state().total, 200);
        engine.resize(15, 3, 10, 20).unwrap();
        assert!(engine.find_pending(), "Grid reflow must restart the search");
    }
    #[test]
    fn reading_text_preserves_blank_rows_indentation_and_unicode() {
        let mut engine = Engine::new().unwrap();
        engine.resize(20, 7, 8, 16).unwrap();
        engine.write("\r\n  界é   \r\n\r\n second  \r\n\r\n".as_bytes());
        assert_eq!(engine.text().unwrap(), "\n  界é\n\n second");
    }
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
    fn narrowing_without_reflow_clears_split_wide_characters_before_editing() {
        // Ghostty #14509: a clipped wide head used to retain a missing tail,
        // so editing the last column could write past the row's cells.
        for screen in ["primary-no-wrap", "alternate", "inactive-alternate"] {
            for edit in [b"\x1b[@".as_slice(), b"\x1b[P", b"\x1b[X", b""] {
                let mut engine = Engine::new().unwrap();
                engine.resize(6, 3, 8, 16).unwrap();
                if screen == "primary-no-wrap" {
                    engine.write(b"\x1b[?7l");
                } else {
                    engine.write(b"\x1b[?47h");
                }
                engine.write("界ab一\r\nKEEP".as_bytes());
                assert_eq!(engine.text().unwrap(), "界ab一\nKEEP");
                if screen == "inactive-alternate" {
                    engine.write(b"\x1b[?47l");
                }
                engine.resize(5, 3, 8, 16).unwrap();
                if screen == "inactive-alternate" {
                    engine.write(b"\x1b[?47h");
                }
                assert_eq!(engine.text().unwrap(), "界ab\nKEEP", "{screen}");
                assert_eq!(engine.cells[4].text(), "", "Clipped head on {screen}");
                assert_eq!(engine.cells[4].flags & 64, 0);
                assert_ne!(engine.cells[1].flags & 64, 0, "Intact wide tail");
                engine.write(b"\x1b[1;5H");
                engine.write(edit);
                engine.write(b"X");
                assert_eq!(engine.text().unwrap(), "界abX\nKEEP", "{screen}: {edit:?}");
                engine.resize(6, 3, 8, 16).unwrap();
                assert_eq!(engine.text().unwrap(), "界abX\nKEEP", "{screen}");
                assert_eq!(engine.cells[5].text(), "", "Clipped tail must stay cleared");
                assert_eq!(engine.cells[5].flags & 64, 0);
            }
        }
    }

    #[test]
    fn same_grid_resize_updates_size_reports_and_releases_synchronized_output() {
        // Ghostty #14482: unchanged rows/columns still have resize effects.
        let mut engine = Engine::new().unwrap();
        engine.resize(12, 3, 8, 16).unwrap();
        engine.write(b"before\x1b[?2048h");
        engine.take_events();
        for (width, height) in [(9, 18), (9, 18), (10, 20)] {
            engine.write(b"\x1b[?2026h\r\x1b[2Kafter");
            assert!(engine.mode(2026));
            engine.resize(12, 3, width, height).unwrap();
            assert!(!engine.mode(2026));
            assert_eq!(engine.text().unwrap(), "after");
            assert_eq!((engine.frame.cols, engine.frame.rows), (12, 3));
            assert_eq!(
                engine.take_events().0,
                format!("\x1b[48;3;12;{};{}t", 3 * height, 12 * width).as_bytes()
            );
            engine.write(b"\r\x1b[2Kbefore");
            assert_eq!(engine.text().unwrap(), "before");
        }
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
    fn scrollbar_rows_round_trip_and_follow_output_and_screen_changes() {
        let mut engine = Engine::new().unwrap();
        engine.resize(12, 3, 8, 16).unwrap();
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 3,
                offset: 0,
                len: 3
            }
        );
        engine.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 5,
                offset: 2,
                len: 3
            }
        );
        engine.scroll_to(1);
        assert_eq!(engine.text().unwrap(), "two\nthree\nfour");
        assert_eq!(engine.frame.scrollback.offset, 1);
        engine.write(b"\r\nsix");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 6,
                offset: 1,
                len: 3
            }
        );
        engine.scroll_to(u64::MAX);
        assert_eq!(engine.text().unwrap(), "four\nfive\nsix");
        assert_eq!(engine.frame.scrollback.offset, 3);
        engine.write(b"\x1b[?1049halternate");
        engine.scroll_to(100);
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 3,
                offset: 0,
                len: 3
            }
        );
        engine.write(b"\x1b[?1049l");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 6,
                offset: 3,
                len: 3
            }
        );
        engine.resize(12, 4, 8, 16).unwrap();
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 6,
                offset: 2,
                len: 4
            }
        );
        engine.write(b"\x1b[3J");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 4,
                offset: 0,
                len: 4
            }
        );
    }

    #[test]
    fn synchronized_output_holds_scrollback_metadata_with_the_visible_frame() {
        let mut engine = Engine::new().unwrap();
        engine.resize(12, 3, 8, 16).unwrap();
        engine.write(b"one\r\ntwo\r\nthree\x1b[?2026h\r\nfour\r\nfive");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 3,
                offset: 0,
                len: 3
            }
        );
        engine.write(b"\x1b[?2026l");
        engine.snapshot().unwrap();
        assert_eq!(
            engine.frame.scrollback,
            Scrollback {
                total: 5,
                offset: 2,
                len: 3
            }
        );
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
    fn directory_events_wait_for_complete_sequences_and_include_clears() {
        let mut e = Engine::new().unwrap();
        assert!(!e.take_directory_update());
        e.write(b"\x1b]7;file://localhost/tmp/project");
        assert!(!e.take_directory_update());
        e.write(b"\x07");
        assert!(e.take_directory_update());
        assert_eq!(e.metadata(true), "file://localhost/tmp/project");
        assert!(!e.take_directory_update());
        e.write(b"ordinary output");
        assert!(!e.take_directory_update());
        e.write(b"\x1b]7;\x1b\\");
        assert!(e.take_directory_update());
        assert_eq!(e.metadata(true), "");
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
    fn extended_keyboard_reports_modifier_and_lock_keys() {
        let mut engine = Engine::new().unwrap();
        assert!(engine.key("Shift_L", 0, 0, false, "").is_empty());
        assert!(engine.key("Control_R", 0, 0, true, "").is_empty());
        assert_eq!(engine.key("Menu", 0, 2, false, ""), b"\x1b[29;5~");
        assert_eq!(engine.key("Menu", 0, 1, false, ""), b"\x1b[29;2~");
        engine.write(b"\x1b[>31u");
        assert_eq!(engine.key("Shift_L", 0, 1, false, ""), b"\x1b[57441;2u");
        assert_eq!(engine.key("Shift_L", 0, 0, true, ""), b"\x1b[57441;1:3u");
        assert_eq!(engine.key("Control_R", 0, 2, false, ""), b"\x1b[57448;5u");
        assert_eq!(engine.key("Alt_L", 0, 4, false, ""), b"\x1b[57443;3u");
        assert_eq!(engine.key("Super_R", 0, 8, false, ""), b"\x1b[57450;9u");
        for (name, code) in [
            ("Caps_Lock", 57358),
            ("Scroll_Lock", 57359),
            ("Num_Lock", 57360),
            ("Print", 57361),
            ("Pause", 57362),
        ] {
            assert_eq!(
                engine.key(name, 0, 0, false, ""),
                format!("\x1b[{code}u").as_bytes()
            );
        }
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

    #[test]
    fn full_reset_restores_the_latest_configured_palette() {
        // Ghostty #14480: RIS must discard OSC 4 overrides, including the
        // extended palette, while retaining the application's defaults.
        let mut engine = Engine::new().unwrap();
        engine.write(b"\x1b[38;5;200mE");
        engine.snapshot().unwrap();
        let extended_default = engine.cells[0].fg;
        engine.write(b"\x1b]4;2;#ffff00;200;#112233\x07\r\x1b[32mG\x1b[38;5;200mE");
        engine.snapshot().unwrap();
        assert_eq!(engine.cells[0].fg, [0xff, 0xff, 0x00]);
        assert_eq!(engine.cells[1].fg, [0x11, 0x22, 0x33]);
        let mut theme = Theme::adwaita(false);
        theme.palette[2] = [0xa1, 0xb2, 0xc3];
        engine.set_theme(&theme).unwrap();
        engine.snapshot().unwrap();
        assert_eq!(engine.cells[0].fg, [0xff, 0xff, 0x00]);
        assert_eq!(engine.cells[1].fg, [0x11, 0x22, 0x33]);
        engine.write(b"\x1bc\x1b[32mG\x1b[38;5;200mE");
        assert_eq!(engine.text().unwrap(), "GE");
        assert_eq!(engine.cells[0].fg, theme.palette[2]);
        assert_eq!(engine.cells[1].fg, extended_default);
        assert_eq!(engine.frame.foreground, theme.foreground);
        assert_eq!(engine.frame.background, theme.background);
    }
}
