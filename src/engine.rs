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

// Accept only the pinned cell formatter's rendition operations. Validate the
// whole UTF-8 stream before touching the fresh terminal, including controls in
// damaged cell/grapheme data. Never accept OSC, modes, cursor movement or an
// unfinished parser sequence just because it came from a validated checkpoint.
fn validate_history_replay(bytes: &[u8]) -> anyhow::Result<()> {
    let mut text = std::str::from_utf8(bytes)?;
    while !text.is_empty() {
        if let Some(parameters) = text.strip_prefix("\x1b[") {
            let end = parameters.find('m').filter(|&end| end <= 18);
            let end = end.ok_or_else(|| anyhow::anyhow!("Invalid history rendition"))?;
            let parameters = &parameters[..end];
            let valid = match parameters {
                "0" | "1" | "2" | "3" | "4" | "5" | "7" | "8" | "9" | "53" | "4:2" | "4:3"
                | "4:4" | "4:5" => true,
                _ => {
                    let mut parts = parameters.split(';');
                    let color_value = |s: &str| {
                        !s.is_empty()
                            && s.bytes().all(|byte| byte.is_ascii_digit())
                            && s.parse::<u8>().is_ok()
                    };
                    matches!(parts.next(), Some("38" | "48" | "58"))
                        && match parts.next() {
                            Some("5") => parts.next().is_some_and(color_value),
                            Some("2") => (0..3).all(|_| parts.next().is_some_and(color_value)),
                            _ => false,
                        }
                        && parts.next().is_none()
                }
            };
            anyhow::ensure!(valid, "Invalid history rendition");
            text = &text[end + 3..];
        } else if let Some(rest) = text.strip_prefix("\r\n") {
            text = rest;
        } else {
            let character = text.chars().next().unwrap();
            anyhow::ensure!(!character.is_control(), "Invalid history text");
            text = &text[character.len_utf8()..];
        }
    }
    Ok(())
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
    /// Compare stored glyphs and wide-cell continuation, ignoring rendition.
    pub fn text_eq(&self, other: &Self) -> bool {
        self.text == other.text && (self.flags ^ other.flags) & 64 == 0
    }
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

/// Presentation state for one client, independent of the PTY and terminal modes.
pub(crate) struct EngineView {
    raw: NonNull<GmuxView>,
    held_frame: Frame,
    held_cells: Vec<Cell>,
    find: crate::search::HistorySearch,
    find_row: Vec<Cell>,
}
impl EngineView {
    pub(crate) fn find_pending(&self) -> bool {
        self.find.pending || self.find.dirty
    }
    pub(crate) fn invalidate(&mut self) {
        self.find.invalidate();
    }
    pub(crate) fn restart_find(&mut self) {
        self.find.restart();
    }
}
/// Plain text for the displayed grid, with wide-cell continuations omitted.
pub(crate) fn viewport_text(frame: Frame, cells: &[Cell]) -> String {
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
impl Drop for EngineView {
    fn drop(&mut self) {
        unsafe { gmux_engine_view_free(self.raw.as_ptr()) }
    }
}
impl Engine {
    pub(crate) fn new_view(&self) -> anyhow::Result<EngineView> {
        Ok(EngineView {
            raw: NonNull::new(unsafe { gmux_engine_view_new(self.raw.as_ptr()) })
                .ok_or_else(|| anyhow::anyhow!("Could not create terminal view"))?,
            find: crate::search::HistorySearch::default(),
            find_row: Vec::new(),
            held_frame: Frame::default(),
            held_cells: Vec::new(),
        })
    }
    pub(crate) fn with_view<T>(
        &mut self,
        view: &mut EngineView,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        unsafe { gmux_engine_view_enter(self.raw.as_ptr(), view.raw.as_ptr()) }
        self.swap_view(view);
        let held = self.mode(2026);
        if held {
            view.held_cells.resize(self.cells.len(), Cell::default());
            std::mem::swap(&mut self.cells, &mut view.held_cells);
            std::mem::swap(&mut self.frame, &mut view.held_frame);
        }
        let result = f(self);
        if held {
            std::mem::swap(&mut self.cells, &mut view.held_cells);
            std::mem::swap(&mut self.frame, &mut view.held_frame);
        }
        self.swap_view(view);
        unsafe { gmux_engine_view_leave(self.raw.as_ptr(), view.raw.as_ptr()) }
        result
    }
    fn swap_view(&mut self, view: &mut EngineView) {
        std::mem::swap(&mut self.find, &mut view.find);
        std::mem::swap(&mut self.find_row, &mut view.find_row);
    }
    pub(crate) fn dimensions(&self) -> (u16, u16) {
        self.size
    }
    pub(crate) fn checkpoint(&self) -> anyhow::Result<Vec<u8>> {
        let mut len = 0;
        let result =
            unsafe { gmux_engine_checkpoint(self.raw.as_ptr(), std::ptr::null_mut(), 0, &mut len) };
        anyhow::ensure!(
            result >= 0,
            "Ghostty snapshot could not capture terminal state"
        );
        anyhow::ensure!(
            len <= crate::scrollback::MAX_PANE_BYTES,
            "Terminal snapshot exceeds 8 MiB"
        );
        let mut bytes = vec![0; len];
        let result = unsafe {
            gmux_engine_checkpoint(self.raw.as_ptr(), bytes.as_mut_ptr(), bytes.len(), &mut len)
        };
        anyhow::ensure!(
            result == 0 && len == bytes.len(),
            "Ghostty snapshot encoding failed"
        );
        Ok(bytes)
    }

    pub(crate) fn restore_history(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes.len() <= crate::scrollback::MAX_PANE_BYTES,
            "Snapshot exceeds 8 MiB"
        );
        // Stream one decode/format pass into bounded storage. The callback is
        // synchronous and borrows this Vec only until the C function returns.
        unsafe extern "C" fn collect(ud: *mut c_void, data: *const u8, len: usize) -> bool {
            let text = unsafe { &mut *ud.cast::<Vec<u8>>() };
            if len > crate::scrollback::MAX_PANE_BYTES - text.len()
                || text.try_reserve(len).is_err()
            {
                return false;
            }
            text.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
            true
        }
        let (mut cols, mut rows) = (0, 0);
        let mut text = Vec::<u8>::new();
        let result = unsafe {
            gmux_checkpoint_history(
                bytes.as_ptr(),
                bytes.len(),
                Some(collect),
                (&mut text as *mut Vec<u8>).cast(),
                &mut cols,
                &mut rows,
            )
        };
        anyhow::ensure!(
            result == 0,
            "Invalid Ghostty snapshot or history exceeds restore budget"
        );
        anyhow::ensure!(
            crate::viewport::valid(cols as u64, rows as u64),
            "Invalid snapshot dimensions"
        );
        validate_history_replay(&text)?;
        // Include the clean ending in the replay budget. Reset before CRLF so
        // a saved background cannot fill the new prompt's row during scrolling.
        if !text.is_empty() {
            let ending = b"\x1b[0m\r\n";
            anyhow::ensure!(
                text.len() <= crate::scrollback::MAX_PANE_BYTES - ending.len(),
                "History exceeds restore budget"
            );
            text.try_reserve(ending.len())?;
            text.extend_from_slice(ending);
        }
        self.resize(cols, rows, 0, 0)?;
        self.write(&text);
        Ok(())
    }

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
    pub fn clear_screen(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            unsafe { gmux_engine_clear_screen(self.raw.as_ptr()) } == 0,
            "Clear Screen is unavailable while a full-screen application is using the terminal"
        );
        self.find.restart();
        Ok(())
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
        Ok(viewport_text(self.frame, &self.cells))
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
    fn clear_screen_erases_history_selection_and_search_without_resetting_session_state() {
        let mut engine = Engine::new().unwrap();
        engine.resize(20, 3, 0, 0).unwrap();
        for _ in 0..12 {
            engine.write("old 你好 e\u{301}\r\n".as_bytes());
        }
        engine.write(b"\x1b]2;Keep title\x07\x1b]7;file:///tmp\x07\x1b[?2004h\x1b[31m");
        engine.scroll_to(0);
        engine.snapshot().unwrap();
        assert!(engine.frame.scrollback.total > 3);
        engine.select((0, 0), (2, 0)).unwrap();
        engine.find("old");
        while engine.find_pending() {
            engine.poll_find();
        }
        assert_eq!(engine.find_state().total, 12);

        engine.clear_screen().unwrap();
        engine.snapshot().unwrap();
        assert_eq!(engine.frame.scrollback.total, 3);
        assert_eq!(engine.frame.scrollback.offset, 0);
        assert_eq!((engine.frame.cursor_x, engine.frame.cursor_y), (0, 0));
        assert_eq!(engine.text().unwrap(), "");
        assert!(engine.selection().is_none());
        while engine.find_pending() {
            engine.poll_find();
        }
        assert_eq!(engine.find_state().total, 0);
        assert!(engine.mode(2004));
        assert_eq!(engine.metadata(false), "Keep title");
        assert_eq!(engine.metadata(true), "file:///tmp");
        let mut restored = Engine::new().unwrap();
        restored
            .restore_history(&engine.checkpoint().unwrap())
            .unwrap();
        assert_eq!(restored.text().unwrap(), "");

        engine.write(b"old\x1b[6n");
        engine.snapshot().unwrap();
        assert_eq!(engine.text().unwrap(), "old");
        assert_eq!(engine.cells[0].fg, Theme::adwaita(true).palette[1]);
        assert_eq!(engine.take_events().0, b"\x1b[1;4R");
        while engine.find_pending() {
            engine.poll_find();
        }
        assert_eq!(engine.find_state().total, 1);
    }

    #[test]
    fn clear_screen_waits_for_partial_vt_and_utf8_before_erasing() {
        for (prefix, suffix) in [
            (b"\x1b]2;Partial title".as_slice(), b"\x07NEW".as_slice()),
            (b"\x1b[38;2;1;2;".as_slice(), b"3mNEW".as_slice()),
            (b"\xf0\x9f".as_slice(), b"\xa6\x80NEW".as_slice()),
        ] {
            let mut engine = Engine::new().unwrap();
            engine.write(b"OLD\r\n");
            engine.write(prefix);
            engine.clear_screen().unwrap();
            engine.write(suffix);
            assert_eq!(engine.text().unwrap(), "NEW");
            assert_eq!(engine.take_events().0, b"");
        }
    }

    #[test]
    fn clear_screen_ends_render_hold_and_clears_every_client_selection_and_scroll_anchor() {
        let mut engine = Engine::new().unwrap();
        engine.resize(20, 3, 0, 0).unwrap();
        engine.write(b"old\r\nold\r\nold\r\nold\r\nold");
        let mut first = engine.new_view().unwrap();
        let mut second = engine.new_view().unwrap();
        for view in [&mut first, &mut second] {
            engine.with_view(view, |engine| {
                engine.scroll_to(0);
                engine.select((0, 0), (2, 0)).unwrap();
                engine.snapshot().unwrap();
            });
        }
        engine.write(b"\x1b[?2026hHELD");
        engine.clear_screen().unwrap();
        assert!(!engine.mode(2026));
        for view in [&mut first, &mut second] {
            engine.with_view(view, |engine| {
                assert_eq!(engine.text().unwrap(), "");
                assert!(engine.selection().is_none());
                assert_eq!(engine.frame.scrollback.total, 3);
                assert_eq!(engine.frame.scrollback.offset, 0);
            });
        }
    }

    #[test]
    fn clear_screen_preserves_alternate_screen_applications() {
        let mut engine = Engine::new().unwrap();
        engine.write(b"PRIMARY\x1b[?1049h\x1b[HALTERNATE");
        assert!(engine.clear_screen().is_err());
        assert_eq!(engine.text().unwrap(), "ALTERNATE");
        engine.write(b"\x1b[?1049l");
        assert_eq!(engine.text().unwrap(), "PRIMARY");
    }

    #[test]
    fn client_views_share_output_but_keep_scroll_selection_and_search() {
        let mut engine = Engine::new().unwrap();
        engine.resize(30, 3, 8, 16).unwrap();
        engine.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let mut first = engine.new_view().unwrap();
        let mut second = engine.new_view().unwrap();
        engine.with_view(&mut first, |engine| {
            engine.scroll_to(0);
            engine.select((0, 0), (2, 0)).unwrap();
            engine.find("one");
            while engine.find_pending() {
                engine.poll_find();
            }
            assert!(engine.text().unwrap().starts_with("one"));
            assert_eq!(engine.copy().unwrap(), "one");
        });
        engine.with_view(&mut second, |engine| {
            assert!(engine.text().unwrap().contains("five"));
            assert!(engine.selection().is_none());
            assert_eq!(engine.find_state().total, 0);
        });
        engine.write(b"\r\nsix");
        first.invalidate();
        second.invalidate();
        // A normal snapshot can consume Ghostty's dirty flags before either client.
        assert!(engine.text().unwrap().contains("six"));
        engine.with_view(&mut second, |engine| {
            assert!(engine.text().unwrap().contains("six"));
        });
        engine.with_view(&mut first, |engine| {
            assert!(engine.text().unwrap().starts_with("one"));
            assert_eq!(engine.copy().unwrap(), "one");
            while engine.find_pending() {
                engine.poll_find();
            }
            assert_eq!(engine.find_state().total, 1);
        });
        engine.resize(20, 4, 8, 16).unwrap();
        engine.with_view(&mut first, |engine| {
            assert_eq!(engine.copy().unwrap(), "one")
        });
        // Tracked view references can be freed after their terminal disappears.
        drop(engine);
        drop(first);
        drop(second);
    }

    #[test]
    fn synchronized_output_holds_each_clients_own_viewport() {
        let mut engine = Engine::new().unwrap();
        engine.resize(20, 3, 8, 16).unwrap();
        engine.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let mut first = engine.new_view().unwrap();
        let mut second = engine.new_view().unwrap();
        let earlier = engine.with_view(&mut first, |engine| {
            engine.scroll_to(0);
            engine.text().unwrap()
        });
        let later = engine.with_view(&mut second, |engine| engine.text().unwrap());
        engine.write(b"\x1b[?2026h\r\x1b[2Kchanged");
        let mut attached_during_hold = engine.new_view().unwrap();
        engine.with_view(&mut attached_during_hold, |engine| {
            assert_eq!(engine.text().unwrap(), later)
        });
        for _ in 0..2 {
            engine.with_view(&mut first, |engine| {
                assert_eq!(engine.text().unwrap(), earlier)
            });
            engine.with_view(&mut second, |engine| {
                assert_eq!(engine.text().unwrap(), later)
            });
        }
        engine.write(b"\x1b[?2026l");
        engine.with_view(&mut second, |engine| {
            assert!(engine.text().unwrap().contains("changed"))
        });
        engine.with_view(&mut first, |engine| {
            assert_eq!(engine.text().unwrap(), earlier)
        });
    }

    fn rendered_history(engine: &mut Engine) -> Vec<(String, [u8; 3], [u8; 3], u8)> {
        engine.snapshot().unwrap();
        let total = engine.frame.scrollback.total;
        let mut result = Vec::new();
        for row in 0..total {
            engine.scroll_to(row);
            engine.snapshot().unwrap();
            let local = row - engine.frame.scrollback.offset;
            let start = local as usize * engine.frame.cols as usize;
            for cell in &engine.cells[start..start + engine.frame.cols as usize] {
                // VT represents a background-only blank as a literal space.
                // Include those cells in the exact rendition comparison too.
                if cell.flags & 64 == 0 {
                    let text = if cell.text().is_empty() && cell.bg != engine.frame.background {
                        " "
                    } else {
                        cell.text()
                    };
                    if !text.is_empty() {
                        result.push((text.to_owned(), cell.fg, cell.bg, cell.flags));
                    }
                }
            }
        }
        result
    }

    #[test]
    fn checkpoints_restore_cell_colors_attributes_and_unicode_across_reflow() {
        let mut old = Engine::new().unwrap();
        old.resize(12, 3, 0, 0).unwrap();
        old.set_theme(&Theme::adwaita(true)).unwrap();
        old.write(
            concat!(
                "\x1b[31;44mINDEX\x1b[38;5;201;48;5;22mEXTENDED\x1b[0m\r\n",
                "\x1b[38;2;12;34;56;48;2;78;90;123;1;3;4;9m你好 e\u{301} 🦀 wrapped",
                "\x1b[0m plain\r\n\r\n",
                "\x1b[7;2;8mATTR\x1b[0m\r\n",
                // No SGR reset before capture, with a background on the last row.
                "\x1b[38;2;111;112;113;48;2;21;22;23mFINAL"
            )
            .as_bytes(),
        );
        let snapshot = old.checkpoint().unwrap();
        let expected = rendered_history(&mut old);
        let mut fresh = Engine::new().unwrap();
        fresh.restore_history(&snapshot).unwrap();
        fresh.set_theme(&Theme::adwaita(true)).unwrap();
        for cols in [12, 19, 9] {
            fresh.resize(cols, 3, 0, 0).unwrap();
            assert_eq!(rendered_history(&mut fresh), expected, "{cols} columns");
            fresh
                .select((0, 0), (cols - 1, fresh.frame.scrollback.total as u32 - 1))
                .unwrap();
            assert!(
                fresh
                    .copy()
                    .unwrap()
                    .contains("你好 e\u{301} 🦀 wrapped plain\n\nATTR\nFINAL")
            );
            fresh.find("你好 e\u{301} 🦀 wrapped");
            while fresh.find_pending() {
                fresh.poll_find();
            }
            assert_eq!(fresh.find_state().total, 1);
        }
        fresh.scroll(0, true);
        fresh.write(b"Z");
        fresh.snapshot().unwrap();
        let new = fresh.cells.iter().find(|cell| cell.text() == "Z").unwrap();
        assert_eq!(
            (new.fg, new.bg, new.flags),
            (
                Theme::adwaita(true).foreground,
                Theme::adwaita(true).background,
                0
            )
        );
        assert!(fresh.events.replies.is_empty());
        assert!(fresh.events.notifications.is_empty());
    }

    #[test]
    fn restored_indices_and_defaults_follow_current_theme_and_discard_saved_palette() {
        let mut old = Engine::new().unwrap();
        old.set_theme(&Theme::adwaita(true)).unwrap();
        old.write(b"\x1b]4;1;rgb:01/02/03\x1b\\\x1b]10;rgb:04/05/06\x1b\\\x1b[31mI\x1b[0mD\x1b[38;2;7;8;9mR");
        let mut fresh = Engine::new().unwrap();
        fresh.restore_history(&old.checkpoint().unwrap()).unwrap();
        for dark in [false, true] {
            let theme = Theme::adwaita(dark);
            fresh.set_theme(&theme).unwrap();
            fresh.snapshot().unwrap();
            assert_eq!(fresh.cells[0].fg, theme.palette[1]);
            assert_eq!(fresh.cells[1].fg, theme.foreground);
            assert_eq!(fresh.cells[2].fg, [7, 8, 9]);
            assert_eq!(fresh.cells[0].bg, theme.background);
        }
    }

    #[test]
    fn restored_unwritten_gaps_have_default_rendition() {
        let mut old = Engine::new().unwrap();
        old.resize(12, 3, 0, 0).unwrap();
        old.set_theme(&Theme::adwaita(true)).unwrap();
        old.write("\x1b[31;44mA\x1b[0m\x1b[4CB\x1b[32;45mC\x1b[0m\x1b[2CD\r\n\r\n\x1b[31;44m你好 e\u{301} 🦀\x1b[0m\x1b[2CZ wrapped\r\n".as_bytes());
        let mut fresh = Engine::new().unwrap();
        fresh.restore_history(&old.checkpoint().unwrap()).unwrap();
        fresh.set_theme(&Theme::adwaita(true)).unwrap();
        old.scroll_to(0);
        fresh.scroll_to(0);
        old.snapshot().unwrap();
        fresh.snapshot().unwrap();
        for x in 0..10 {
            assert_eq!(
                (fresh.cells[x].fg, fresh.cells[x].bg, fresh.cells[x].flags),
                (old.cells[x].fg, old.cells[x].bg, old.cells[x].flags),
                "column {x}"
            );
        }
        for cols in [12, 19] {
            fresh.resize(cols, 3, 0, 0).unwrap();
            fresh.snapshot().unwrap();
            fresh
                .select((0, 0), (cols - 1, fresh.frame.scrollback.total as u32 - 1))
                .unwrap();
            assert_eq!(
                fresh.copy().unwrap(),
                "A    BC  D\n\n你好 e\u{301} 🦀  Z wrapped"
            );
        }
    }

    #[test]
    fn history_replay_accepts_only_complete_utf8_crlf_and_cell_rendition() {
        for valid in [
            "你好 e\u{301} 🦀\r\n",
            "\x1b[0m\x1b[4:3m\x1b[58;2;255;255;255m",
            "\x1b[38;5;201m\x1b[48;2;1;2;3m",
        ] {
            validate_history_replay(valid.as_bytes()).unwrap();
        }
        for invalid in [
            b"\x1b]4;1;rgb:01/02/03\x1b\\".as_slice(),
            b"\x1b[?1049h",
            b"\x1b[2J",
            b"\x1b[1;2H",
            b"\x1b[",
            b"\x1b[0m\xf0\x9f",
            b"\x1b[38;2;256;0;0m",
            b"\x1b[38;2;1;2m",
            b"\x1b[38;5;1;2m",
            b"\x1b[9999999999999999999m",
            b"\x1b[0\x07m",
            b"\x1b[+1m",
            b"\x1b[38;5;+1m",
            b"\n",
            b"\r",
            b"\t",
            b"\x07",
            b"\xc2\x9b31m",
        ] {
            assert!(validate_history_replay(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn styled_history_expansion_is_bounded_before_installation() {
        let mut old = Engine::new().unwrap();
        old.resize(1000, 131, 0, 0).unwrap();
        let line = concat!(
            "\x1b[1;2;3;5;7;8;9;53m\x1b[4:3m\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m\x1b[58;2;13;14;15mA",
            "\x1b[4:5m\x1b[38;2;7;8;9m\x1b[48;2;10;11;12m\x1b[58;2;16;17;18mB"
        )
        .repeat(500);
        for _ in 0..131 {
            old.write(line.as_bytes());
            old.write(b"\r\n");
        }
        // A compact style table and cell grid can expand beyond the replay
        // budget when every cell switches styles. The valid file stays small.
        let snapshot = old.checkpoint().unwrap();
        let mut fresh = Engine::new().unwrap();
        assert!(fresh.restore_history(&snapshot).is_err());
        assert_eq!(fresh.dimensions(), (80, 24));
        fresh.write(b"fresh");
        assert_eq!(fresh.text().unwrap(), "fresh");
        assert!(
            fresh
                .restore_history(&vec![0; crate::scrollback::MAX_PANE_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn crc_valid_snapshots_with_control_cells_or_excessive_page_dimensions_are_rejected() {
        let mut old = Engine::new().unwrap();
        old.resize(2, 1, 0, 0).unwrap();
        old.write(b"X");
        let snapshot = old.checkpoint().unwrap();
        // Pinned v1 envelope is 10 bytes; records have tag/length/CRC32C.
        // Locate the first PAGE, whose 20-byte header precedes this fixture's
        // empty style/hyperlink tables and one row of bare one-byte cells.
        let mut record = 10;
        while u16::from_le_bytes(snapshot[record..record + 2].try_into().unwrap()) != 3 {
            let len = u32::from_le_bytes(snapshot[record + 2..record + 6].try_into().unwrap());
            record += 10 + len as usize;
        }
        let len = u32::from_le_bytes(snapshot[record + 2..record + 6].try_into().unwrap()) as usize;
        let page = record + 10;
        assert_eq!(&snapshot[page + 4..page + 8], &[0; 4]);
        assert_eq!((snapshot[page + 20] >> 4) & 3, 0);
        assert_eq!(snapshot[page + 23], b'X');
        for (offset, replacement) in [
            (page + 23, vec![0x07]),
            (page + 23, vec![0x1b]),
            (page + 23, vec![0x9b]),
            (page, vec![0xff; 4]),
        ] {
            let mut damaged = snapshot.clone();
            damaged[offset..offset + replacement.len()].copy_from_slice(&replacement);
            let mut crc = !0u32;
            for byte in damaged[record..record + 6]
                .iter()
                .chain(&damaged[page..page + len])
            {
                crc ^= *byte as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(crc & 1));
                }
            }
            damaged[record + 6..record + 10].copy_from_slice(&(!crc).to_le_bytes());
            let mut fresh = Engine::new().unwrap();
            assert!(
                fresh.restore_history(&damaged).is_err(),
                "offset {offset}, {replacement:?}"
            );
            assert_eq!(fresh.dimensions(), (80, 24));
            fresh.write(b"fresh");
            assert_eq!(fresh.text().unwrap(), "fresh");
            assert!(fresh.events.replies.is_empty());
            assert!(fresh.events.notifications.is_empty());
        }
    }

    #[test]
    fn checkpoints_restore_unicode_and_reflow_wrapped_history() {
        let mut old = Engine::new().unwrap();
        old.resize(12, 3, 0, 0).unwrap();
        let line = "before 你好 e\u{301} 🦀 after a long wrapped line";
        old.write(format!("{line}\r\nsecond hard line\r\nthird\r\nfourth\r\n").as_bytes());
        let snapshot = old.checkpoint().unwrap();
        let mut fresh = Engine::new().unwrap();
        fresh.restore_history(&snapshot).unwrap();
        assert_eq!(fresh.dimensions(), (12, 3));
        for cols in [12, 19] {
            fresh.resize(cols, 3, 0, 0).unwrap();
            fresh.find(line);
            while fresh.find_pending() {
                fresh.poll_find();
            }
            assert_eq!(fresh.find_state().total, 1);
            fresh.snapshot().unwrap();
            let total = fresh.frame.scrollback.total;
            fresh.select((0, 0), (cols - 1, total as u32 - 1)).unwrap();
            let text = fresh.copy().unwrap();
            assert!(text.contains(line), "{text:?}");
            assert!(text.contains("second hard line\nthird\nfourth"), "{text:?}");
        }
    }

    #[test]
    fn restoring_history_discards_alternate_screen_modes_and_unfinished_input() {
        for unfinished in [
            b"\x1b[".as_slice(),
            b"\x1b]2;unfinished",
            b"\xf0\x9f",
            b"\x1bPqunfinished",
            b"\x1b_Ga=T;unfinished",
        ] {
            let mut old = Engine::new().unwrap();
            old.write(b"\x1b]2;old title\x07\x1b]8;;https://example.com\x07primary history\x1b]8;;\x07\r\n\x1b[?1h\x1b[?1004h\x1b[?2004h\x1b[?1000h\x1b[?1049halternate content\x1b[>31u");
            old.write(unfinished);
            let mut fresh = Engine::new().unwrap();
            fresh.restore_history(&old.checkpoint().unwrap()).unwrap();
            assert!(!fresh.mode(0));
            assert!(!fresh.mode(1004));
            assert_eq!(fresh.key("Up", 0, 0, false, ""), b"\x1b[A");
            assert_eq!(fresh.paste("input", false).unwrap(), b"input");
            assert_eq!(fresh.metadata(false), "");
            fresh.write(b"fresh shell\r\n");
            let text = fresh.text().unwrap();
            assert!(
                text.contains("primary history") && text.contains("fresh shell"),
                "{text:?}"
            );
            assert!(!text.contains("alternate content") && !text.contains("unfinished"));
        }
    }

    #[test]
    fn checkpoint_continuation_budget_recovers_after_the_parser_returns_to_ground() {
        let mut old = Engine::new().unwrap();
        old.write(b"primary history\r\n\x1b]2;");
        old.write(&vec![b'a'; 64 * 1024 + 1]);
        assert!(
            old.checkpoint().is_err(),
            "An untracked continuation must never become a partial snapshot"
        );
        old.write(b"\x07");
        let snapshot = old.checkpoint().unwrap();
        let mut fresh = Engine::new().unwrap();
        fresh.restore_history(&snapshot).unwrap();
        fresh.write(b"fresh shell");
        assert!(fresh.text().unwrap().contains("fresh shell"));
    }

    #[test]
    fn damaged_ghostty_snapshots_never_install_partial_history() {
        let mut old = Engine::new().unwrap();
        old.write(b"saved history");
        let snapshot = old.checkpoint().unwrap();
        let mut damaged = snapshot.clone();
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0xff;
        let mut trailing = snapshot.clone();
        trailing.push(0);
        for bytes in [
            &snapshot[..0],
            &snapshot[..32],
            &snapshot[..snapshot.len() / 2],
            &snapshot[..snapshot.len() - 1],
            &damaged,
            &trailing,
        ] {
            let mut fresh = Engine::new().unwrap();
            assert!(fresh.restore_history(bytes).is_err());
            fresh.write(b"fresh");
            assert_eq!(fresh.text().unwrap(), "fresh");
        }
    }
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
