//! Terminal processes and state, independent of GTK widgets and rendering.

use crate::{
    engine::Engine,
    title::{ForegroundProcess, PaneTitle},
};
use gmux_protocol::{
    model::CursorStyle,
    session::SessionUpdate,
    theme::Theme,
    transport::{KeyAction, KeyLayout},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Pty {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    pending: RefCell<VecDeque<u8>>,
    eof: bool,
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(fg) = self.master.process_group_leader().filter(|pid| *pid > 1) {
                unsafe {
                    libc::kill(-fg, libc::SIGHUP);
                }
            }
            let _ = child.kill();
            // Reap asynchronously so closing a session does not block its owner.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct TerminalMemory {
    pane: u64,
    memory: crate::engine::MemoryUsage,
    compression: crate::compression::CompressionStats,
    scheduled: bool,
    program_status_bytes: usize,
}

pub(crate) struct TerminalSession {
    id: u64,
    engine: Engine,
    compression: crate::compression::IdleCompression,
    pty: Option<Pty>,
    pid: Option<u32>,
    directory: PathBuf,
    size: PtySize,
    cell_size: (u32, u32),
    exited: bool,
    title: PaneTitle,
    title_polled: Instant,
    output_tail: Vec<u8>,
    read_responding: bool,
    read_reservation: Cell<usize>,
    clipboard_queued: Cell<usize>,
    after_response: RefCell<Vec<u8>>,
    delayed_resize: Option<(u16, u16, u32, u32, u16, u16)>,
}

impl TerminalSession {
    pub(crate) fn set_scrollback_limits(
        &mut self,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> anyhow::Result<()> {
        self.engine.set_scrollback_limits(limits)
    }
    pub(crate) fn compression_deadline(&self) -> Option<Instant> {
        self.compression.deadline
    }

    pub(crate) fn refresh_compression(&mut self, now: Instant) -> Option<Instant> {
        match self.engine.compression_activity() {
            Ok(activity) => self.compression.observe(activity, now),
            Err(_) => self.compression.deadline = None,
        }
        self.compression_deadline()
    }

    pub(crate) fn compress(&mut self) {
        let started = Instant::now();
        let result = self.engine.compress();
        self.compression
            .finish(result.as_ref().ok().copied(), started, Instant::now());
        if let Err(error) = result {
            eprintln!(
                "Could not compress scrollback for pane {}: {error}",
                self.id
            );
        }
    }

    pub(crate) fn memory_usage(&self) -> anyhow::Result<TerminalMemory> {
        Ok(TerminalMemory {
            pane: self.id,
            memory: self.engine.memory_usage()?,
            compression: self.compression.stats,
            scheduled: self.compression.deadline.is_some(),
            program_status_bytes: self.engine.program_status().memory_bytes(),
        })
    }

    pub(crate) fn progress(&self) -> gmux_protocol::progress::Progress {
        self.engine.progress()
    }
    pub(crate) fn program_status(&self) -> Option<&gmux_protocol::program_status::Summary> {
        self.engine.program_status().summary()
    }
    pub(crate) fn program_status_records(&self) -> Vec<&gmux_protocol::program_status::Record> {
        self.engine.program_status().records()
    }
    pub(crate) fn retain_exited(&self) -> bool {
        self.engine.program_status().has_result()
    }
    pub(crate) fn acknowledge_program_status(&mut self) -> bool {
        self.engine.program_status_mut().acknowledge();
        self.engine.program_status_mut().take_changed()
    }
    pub(crate) fn progress_deadline(&self) -> Option<Instant> {
        self.engine.progress_deadline()
    }
    pub(crate) fn use_view_appearance(
        &mut self,
        view: &crate::engine::EngineView,
    ) -> anyhow::Result<()> {
        self.engine.use_view_appearance(view)
    }
    pub(crate) fn links(&mut self) -> anyhow::Result<gmux_protocol::control::PaneLinks> {
        self.engine.snapshot()?;
        Ok(gmux_protocol::control::PaneLinks {
            frame: self.engine.frame,
            hyperlinks: self.engine.hyperlinks.clone(),
        })
    }
    pub(crate) fn new_view(&self) -> anyhow::Result<crate::engine::EngineView> {
        self.engine.new_view()
    }
    pub(crate) fn with_view<T>(
        &mut self,
        view: &mut crate::engine::EngineView,
        f: impl FnOnce(&mut Engine) -> T,
    ) -> T {
        self.engine.with_view(view, f)
    }
    pub(crate) fn spawn(
        id: u64,
        tab: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
        history: Option<&[u8]>,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> anyhow::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(&shell);
        crate::shell::configure(&mut command, Path::new(&shell));
        command.arg("-l");
        command.env("GMUX_TAB", tab.to_string());
        command.env("GMUX_PANE", id.to_string());
        Self::spawn_command(id, directory, cursor_style, theme, command, history, limits)
    }

    fn spawn_command(
        id: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
        mut command: CommandBuilder,
        history: Option<&[u8]>,
        limits: gmux_protocol::retention::ScrollbackLimits,
    ) -> anyhow::Result<Self> {
        // Complete fallible setup before spawning a process we must clean up.
        let mut engine = Engine::new()?;
        engine.set_scrollback_limits(limits)?;
        if let Some(history) = history
            && let Err(error) = engine.restore_history(history)
        {
            eprintln!(
                "Could not restore scrollback for pane {id}: {error}. Starting with empty history"
            );
            engine = Engine::new()?;
            engine.set_scrollback_limits(limits)?;
        }
        engine.set_cursor_style(cursor_style)?;
        engine.set_theme(theme)?;
        let mut compression = crate::compression::IdleCompression::default();
        compression.observe(engine.compression_activity()?, Instant::now());
        let (cols, rows) = engine.dimensions();
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("PTY has no file descriptor"))?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        anyhow::ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "Could not set nonblocking PTY"
        );
        command.cwd(&directory);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "gmux");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        command.env("GMUX_SOCKET", gmux_platform::unix::ipc::socket_path());
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            command.env(
                "PATH",
                format!(
                    "{}:{}",
                    parent.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        }
        let child = pair.slave.spawn_command(command)?;
        let pid = child.process_id();
        drop(pair.slave);
        Ok(Self {
            id,
            engine,
            compression,
            pty: Some(Pty {
                master: pair.master,
                child: Some(child),
                pending: RefCell::default(),
                eof: false,
            }),
            pid,
            directory,
            size: PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            },
            cell_size: (0, 0),
            exited: false,
            title: PaneTitle::default(),
            title_polled: Instant::now(),
            output_tail: Vec::new(),
            read_responding: false,
            read_reservation: Cell::new(0),
            clipboard_queued: Cell::new(0),
            after_response: RefCell::default(),
            delayed_resize: None,
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn size(&self) -> (u16, u16) {
        (self.size.cols, self.size.rows)
    }

    pub(crate) fn exited(&self) -> bool {
        self.exited
    }

    pub(crate) fn title(&self) -> String {
        self.title.display(&self.directory, self.pid)
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.send_with_enter(bytes, false)
    }

    pub(crate) fn send_with_enter(&self, bytes: &[u8], enter: bool) -> anyhow::Result<()> {
        anyhow::ensure!(!self.exited, "This terminal's process has exited");
        let pty = self
            .pty
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session is closed"))?;
        let mut pending = pty.pending.borrow_mut();
        let mut after = self.after_response.borrow_mut();
        anyhow::ensure!(
            pending.len()
                + after.len()
                + self.read_reservation.get()
                + bytes.len()
                + usize::from(enter)
                <= 2 * gmux_protocol::transport::MAX_PASTE - 64 * 1024,
            "Terminal input queue is full"
        );
        if self.read_responding {
            after.extend(bytes);
            if enter {
                after.push(b'\r');
            }
        } else {
            pending.extend(bytes);
            if enter {
                pending.push_back(b'\r');
            }
        }
        Ok(())
    }

    pub(crate) fn send_text(&mut self, text: &str, enter: bool) -> anyhow::Result<()> {
        let bytes = self.paste(text)?;
        self.send_with_enter(&bytes, enter)
    }

    pub(crate) fn clipboard_queue_budget(&self) -> usize {
        self.clipboard_queued.get()
    }
    pub(crate) fn read_pending(&self) -> u64 {
        self.engine.read_pending()
    }
    pub(crate) fn set_read_enabled(&mut self, enabled: bool) {
        self.engine.set_read_enabled(
            enabled && !self.exited && self.pty.as_ref().is_some_and(|p| p.child.is_some()),
        );
    }
    pub(crate) fn take_read_request(&mut self) -> Option<gmux_protocol::clipboard::ReadRequest> {
        self.engine.take_read_request()
    }
    pub(crate) fn clear_grants(&mut self) {
        self.engine.clear_grants();
    }
    pub(crate) fn has_output_tail(&self) -> bool {
        !self.output_tail.is_empty() && self.read_pending() == 0
    }
    pub(crate) fn complete_read(
        &mut self,
        result: &gmux_protocol::clipboard::ReadResult,
        data: &[u8],
        reservation: usize,
    ) -> anyhow::Result<()> {
        use gmux_protocol::clipboard::ReadStatus;
        if result.status == ReadStatus::Success {
            anyhow::ensure!(
                self.clipboard_queued.get() == 0,
                "A previous clipboard reply is queued"
            );
            let pty = self
                .pty
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Session is closed"))?;
            anyhow::ensure!(
                pty.pending.borrow().len() + self.after_response.borrow().len() + reservation
                    <= 2 * gmux_protocol::transport::MAX_PASTE - 64 * 1024,
                "Terminal input is busy"
            );
        }
        let bytes = self.engine.complete_read(result, data)?;
        if !bytes.is_empty()
            && let Some(pty) = &self.pty
        {
            anyhow::ensure!(
                pty.pending.borrow().len() + bytes.len() <= 2 * gmux_protocol::transport::MAX_PASTE,
                "Terminal reply queue is full"
            );
            pty.pending.borrow_mut().extend(bytes);
        }
        if result.status == ReadStatus::Success {
            self.read_responding = true;
            self.read_reservation.set(reservation);
            self.clipboard_queued.set(reservation);
        }
        Ok(())
    }
    pub(crate) fn step_read(&mut self) -> anyhow::Result<bool> {
        if self.exited || self.read_pending() == 0 {
            return Ok(true);
        }
        let (done, bytes) = self.engine.step_read()?;
        let reserved = self.read_reservation.get();
        anyhow::ensure!(
            bytes.len() <= reserved,
            "Native clipboard response exceeded reservation"
        );
        self.read_reservation.set(reserved - bytes.len());
        if let Some(pty) = &self.pty {
            pty.pending.borrow_mut().extend(bytes);
        }
        if done {
            self.read_responding = false;
            self.read_reservation.set(0);
            if let Some(pty) = &self.pty {
                pty.pending
                    .borrow_mut()
                    .extend(self.after_response.borrow_mut().drain(..));
            }
        }
        Ok(done)
    }
    pub(crate) fn abort_read(&mut self) {
        // A failed encoder may have emitted an incomplete OSC. Close the PTY
        // rather than allowing unrelated bytes to continue inside that frame.
        self.pty.take();
        self.engine.discard_read();
        self.engine.clear_grants();
        self.exited = true;
        self.read_responding = false;
        self.read_reservation.set(0);
        self.clipboard_queued.set(0);
        self.after_response.borrow_mut().clear();
        self.output_tail.clear();
    }
    pub(crate) fn user_paste(
        &mut self,
        destination: gmux_protocol::clipboard::Destination,
        mimes: &[String],
    ) -> anyhow::Result<()> {
        let bytes = self.engine.user_paste(destination, mimes)?;
        if let Err(error) = self.send(&bytes) {
            self.engine.clear_grants();
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn poll_with_clipboard(
        &mut self,
        enabled: bool,
    ) -> (SessionUpdate, Vec<gmux_protocol::clipboard::Write>) {
        self.engine.set_clipboard_enabled(enabled);
        let update = self.poll();
        self.engine.set_clipboard_enabled(false);
        (update, self.engine.take_clipboard_writes())
    }
    pub(crate) fn poll(&mut self) -> SessionUpdate {
        let mut update = SessionUpdate {
            changed: self.engine.expire_render_hold(),
            ..SessionUpdate::default()
        };
        if self.read_pending() == 0 {
            if let Some((cols, rows, cw, ch, width, height)) = self.delayed_resize.take() {
                let _ = self.resize(cols, rows, cw, ch, width, height);
            }
            if !self.output_tail.is_empty() {
                let (changed, consumed) = self.engine.feed(&self.output_tail);
                update.changed |= changed;
                self.output_tail.drain(..consumed);
            }
        }
        let mut read_budget_exhausted = false;
        if let Some(pty) = &mut self.pty {
            if matches!(
                pty.child.as_mut().map(|child| child.try_wait()),
                Some(Ok(Some(_)))
            ) {
                pty.child = None;
            }
            if pty.child.is_none() {
                self.engine.set_read_enabled(false);
                if self.engine.read_pending() != 0 {
                    // Reaping must cancel even when an unread tail is parked
                    // behind the request. Drain that tail without desktop effects.
                    self.engine.discard_read();
                    self.engine.clear_grants();
                    self.read_responding = false;
                    self.read_reservation.set(0);
                    self.clipboard_queued.set(0);
                    self.after_response.borrow_mut().clear();
                    pty.pending.borrow_mut().clear();
                }
            }
            let fd = pty.master.as_raw_fd().unwrap();
            let mut pending = pty.pending.borrow_mut();
            let mut budget = 512 * 1024;
            while !pending.is_empty() && budget > 0 {
                let (bytes, _) = pending.as_slices();
                let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len().min(budget)) };
                if n > 0 {
                    pending.drain(..n as usize);
                    budget -= n as usize;
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        update.error = Some(error.to_string());
                        pending.clear();
                    }
                    break;
                }
            }
            if pending.is_empty() && !self.read_responding {
                self.clipboard_queued.set(0);
            }
            // Bound work per session so a noisy process cannot block its owner.
            let mut buffer = [0u8; 16384];
            read_budget_exhausted = !pty.eof;
            for _ in 0..32 {
                if pty.eof
                    || self.engine.read_pending() != 0
                    || !self.output_tail.is_empty()
                    || pending.len() > 2 * gmux_protocol::transport::MAX_PASTE - 512 * 1024
                {
                    read_budget_exhausted = !self.output_tail.is_empty();
                    break;
                }
                let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if n > 0 {
                    let (changed, consumed) = self.engine.feed(&buffer[..n as usize]);
                    update.changed |= changed;
                    self.output_tail
                        .extend_from_slice(&buffer[consumed..n as usize]);
                } else if n == 0 {
                    pty.eof = true;
                    read_budget_exhausted = false;
                    break;
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        pty.eof = true;
                    }
                    read_budget_exhausted = false;
                    break;
                }
            }
        }
        let (replies, notifications) = self.engine.take_events();
        update.notifications = notifications;
        if !replies.is_empty()
            && let Err(error) = self.send(&replies)
        {
            update.error = Some(error.to_string());
        }
        update.changed |= self.engine.poll_find();
        if self.engine.take_directory_update() {
            let previous = self.directory.clone();
            self.refresh_directory();
            update.directory_changed = self.directory != previous;
            update.changed |= update.directory_changed;
        }
        let title_update = self.engine.take_title_update();
        self.refresh_title(title_update);
        if let Some(pty) = &mut self.pty {
            // Drain final output in bounded batches before publishing exit. A
            // final done/error report may follow more than one read budget.
            // Do not wait for EOF, which background descendants can delay.
            // Reaping before reading ensures this drain follows known exit;
            // no extra poll is needed after WouldBlock in a detached session.
            self.exited |= pty.child.is_none() && !read_budget_exhausted;
        }
        if self.exited {
            self.engine.discard_read();
            self.engine.clear_grants();
            self.read_responding = false;
            self.read_reservation.set(0);
            self.clipboard_queued.set(0);
            self.after_response.borrow_mut().clear();
            if let Some(pty) = &self.pty {
                pty.pending.borrow_mut().clear();
            }
            self.engine.clear_progress();
            self.engine.program_status_mut().end_execution();
        }
        update.progress_changed = self.engine.take_progress_changed();
        update.program_status_changed = self.engine.program_status_mut().take_changed();
        update.exited = self.exited;
        update
    }

    fn foreground_pid(&self) -> Option<u32> {
        self.pty
            .as_ref()
            .and_then(|pty| pty.master.process_group_leader())
            .filter(|pid| *pid > 0)
            .map(|pid| pid as u32)
            .or(self.pid)
    }

    fn refresh_title(&mut self, title_update: Option<String>) {
        if self.exited {
            return;
        }
        let pid = self.foreground_pid();
        let now = Instant::now();
        if pid != self.title.foreground_pid()
            || title_update.is_some()
            || now.duration_since(self.title_polled) >= Duration::from_millis(250)
        {
            self.title
                .update(pid.and_then(ForegroundProcess::read), title_update);
            self.title_polled = now;
        }
    }

    pub(crate) fn refresh_directory(&mut self) {
        if self.exited {
            return;
        }
        let actual_directory = self
            .foreground_pid()
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok());
        if let Some(directory) = actual_directory {
            self.directory = directory;
        } else {
            // Prefer the live process directory to stale OSC metadata after cd.
            let pwd = self.engine.metadata(true);
            if let Some(path) = directory_from_uri(&pwd)
                && path.is_dir()
            {
                self.directory = path;
            }
        }
        self.refresh_title(None);
    }

    pub(crate) fn busy(&self) -> bool {
        !self.exited
            && (self.engine.semantic_busy()
                || self
                    .foreground_pid()
                    .zip(self.pid)
                    .is_some_and(|(fg, pid)| fg != pid))
    }

    pub(crate) fn command_state(&self) -> gmux_protocol::shell::CommandState {
        self.engine.command_state()
    }

    pub(crate) fn command_output(
        &mut self,
        point: Option<(u16, u32)>,
        formats: gmux_protocol::clipboard::Formats,
    ) -> anyhow::Result<gmux_protocol::clipboard::Content> {
        self.engine.select_output(point)?;
        self.engine.copy_content(formats)
    }

    /// Apply changed grid, cell, or pixel dimensions; return whether a resize ran.
    pub(crate) fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cw: u32,
        ch: u32,
        width: u16,
        height: u16,
    ) -> anyhow::Result<bool> {
        if self.read_pending() != 0 {
            self.delayed_resize = Some((cols, rows, cw, ch, width, height));
            return Ok(false);
        }

        anyhow::ensure!(
            gmux_protocol::viewport::valid(cols as u64, rows as u64),
            "Invalid terminal size"
        );
        let size = PtySize {
            cols,
            rows,
            pixel_width: width,
            pixel_height: height,
        };
        if self.size == size && self.cell_size == (cw, ch) {
            return Ok(false);
        }
        self.engine.resize(cols, rows, cw, ch)?;
        if let Some(pty) = &self.pty {
            pty.master.resize(size)?;
        }
        self.size = size;
        self.cell_size = (cw, ch);
        Ok(true)
    }

    pub(crate) fn checkpoint(&self) -> anyhow::Result<Vec<u8>> {
        self.engine.checkpoint()
    }

    pub(crate) fn text(&mut self) -> anyhow::Result<String> {
        self.engine.text()
    }

    pub(crate) fn clear_screen(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.read_pending() == 0, "Clipboard read is pending");
        self.engine.clear_screen()
    }
    pub(crate) fn mode(&self, mode: i32) -> bool {
        self.engine.mode(mode)
    }

    pub(crate) fn paste(&mut self, text: &str) -> anyhow::Result<Vec<u8>> {
        self.engine.paste(text)
    }

    pub(crate) fn key(
        &mut self,
        name: &str,
        layout: KeyLayout,
        mods: u16,
        action: KeyAction,
        text: &str,
    ) -> Vec<u8> {
        self.engine.key(name, layout, mods, action, text)
    }

    pub(crate) fn mouse(
        &mut self,
        position: (f32, f32),
        metrics: (u32, u32),
        action: i32,
        button: i32,
        mods: u16,
        pressed: bool,
    ) -> Vec<u8> {
        self.engine
            .mouse(position, metrics, action, button, mods, pressed)
    }

    pub(crate) fn poll_fd(&self) -> Option<(libc::c_int, libc::c_short)> {
        let pty = self.pty.as_ref()?;
        if (pty.eof || self.read_pending() != 0) && pty.pending.borrow().is_empty() {
            return None;
        }
        Some((
            pty.master.as_raw_fd()?,
            if pty.eof || self.read_pending() != 0 {
                0
            } else {
                libc::POLLIN
            } | if pty.pending.borrow().is_empty() {
                0
            } else {
                libc::POLLOUT
            },
        ))
    }

    pub(crate) fn render_held(&self) -> bool {
        self.engine.mode(2026)
    }
}

/// Decode OSC 7 file URIs without a desktop-library dependency. Paths may
/// contain arbitrary Unix bytes; malformed escapes and NUL are invalid.
fn directory_from_uri(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut path = uri.strip_prefix("file:")?;
    if let Some(authority) = path.strip_prefix("//") {
        path = &authority[authority.find('/')?..];
    }
    if !path.starts_with('/') || path.contains(['?', '#']) {
        return None;
    }
    let mut bytes = path.bytes();
    let mut decoded = Vec::with_capacity(path.len());
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        decoded.push(byte);
    }
    Some(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(test)]
mod uri_tests {
    use super::directory_from_uri;

    #[test]
    fn osc_directories_decode_paths_and_reject_malformed_uris() {
        for uri in [
            "file:/tmp/a%20b",
            "file:///tmp/a%20b",
            "file://host/tmp/a%20b",
        ] {
            assert_eq!(directory_from_uri(uri).unwrap().to_str(), Some("/tmp/a b"));
        }
        for uri in [
            "https://host/tmp",
            "file:relative",
            "file://host",
            "file:/tmp/%",
            "file:/tmp/%GG",
            "file:/tmp/%00",
            "file:/tmp/#fragment",
        ] {
            assert!(directory_from_uri(uri).is_none(), "{uri}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gmux-session-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn start(id: u64, directory: &Path, script: &str) -> TerminalSession {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        TerminalSession::spawn_command(
            id,
            directory.to_path_buf(),
            CursorStyle::default(),
            &Theme::adwaita(true),
            command,
            None,
            Default::default(),
        )
        .unwrap()
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for session state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn notification(session: &mut TerminalSession, text: &str) -> bool {
        let update = session.poll();
        assert!(update.error.is_none(), "{:?}", update.error);
        update.notifications.iter().any(|message| message == text)
    }

    #[test]
    fn exit_cancels_a_deferred_read_before_draining_its_parked_tail() {
        let directory = TestDirectory::new();
        let mut session = start(1, &directory.0, "printf '\\033]52;c;?\\007TAIL'; sleep 0.1");
        session.set_read_enabled(true);
        wait_for(|| {
            session.poll();
            session.read_pending() != 0
        });
        assert!(!session.output_tail.is_empty());
        assert!(session.take_read_request().is_some());
        wait_for(|| session.poll().exited);
        assert_eq!(session.read_pending(), 0);
        assert!(session.text().unwrap().contains("TAIL"));
    }

    #[test]
    fn user_paste_waits_for_the_complete_clipboard_response_transaction() {
        use gmux_protocol::clipboard::{Destination, ReadDescriptor, ReadResult, ReadStatus};
        let directory = TestDirectory::new();
        let mut session = start(1, &directory.0, "sleep 5");
        session.engine.write(b"\x1b[?5522h");
        session.set_read_enabled(true);
        session.engine.feed(b"\x1b]52;c;?\x07");
        let request = session.take_read_request().unwrap();
        let data = vec![b'x'; 128 * 1024];
        session
            .complete_read(
                &ReadResult {
                    id: request.id,
                    status: ReadStatus::Success,
                    epoch: 1,
                    available: vec![],
                    contents: vec![ReadDescriptor {
                        mime: "text/plain".into(),
                        len: data.len(),
                    }],
                },
                &data,
                1024 * 1024,
            )
            .unwrap();
        assert!(!session.step_read().unwrap());
        let prefix = session.pty.as_ref().unwrap().pending.borrow().len();
        session
            .user_paste(Destination::Clipboard, &["text/plain".into()])
            .unwrap();
        assert!(!session.after_response.borrow().is_empty());
        assert_eq!(session.pty.as_ref().unwrap().pending.borrow().len(), prefix);
        while !session.step_read().unwrap() {}
        assert!(session.after_response.borrow().is_empty());
        let bytes: Vec<_> = session
            .pty
            .as_ref()
            .unwrap()
            .pending
            .borrow()
            .iter()
            .copied()
            .collect();
        let end = bytes.iter().position(|&b| b == 7).unwrap();
        assert!(bytes[..end].starts_with(b"\x1b]52;c;"));
        assert!(bytes[end + 1..].starts_with(b"\x1b]5522;"));
        assert!(bytes[end + 1..].windows(4).any(|s| s == b":pw="));
    }

    #[test]
    fn progress_is_cleared_on_process_exit() {
        let directory = TestDirectory::new();
        let mut session = start(
            1,
            &directory.0,
            "stty -echo; printf '\\033]9;4;1;42\\007'; IFS= read -r line",
        );
        wait_for(|| {
            session.poll();
            session.progress().active()
        });
        session.send_text("exit", true).unwrap();
        wait_for(|| session.poll().exited);
        assert_eq!(session.progress(), Default::default());
        assert!(session.progress_deadline().is_none());
    }

    #[test]
    fn process_exit_drops_transients_and_keeps_results_until_interaction() {
        let directory = TestDirectory::new();
        let mut session = start(
            1,
            &directory.0,
            "stty -echo; printf '\\033]7501;state=blocked:id=worker\\007'; IFS= read -r line; head -c 700000 /dev/zero | tr '\\000' x; printf '\\033]7501;state=done:id=result:msg=SGVsbG8=\\007'",
        );
        wait_for(|| {
            session.poll();
            session.program_status().is_some_and(|s| s.count == 1)
        });
        session.send_text("exit", true).unwrap();
        wait_for(|| session.poll().exited);
        assert!(session.retain_exited());
        assert!(session.poll_fd().is_none());
        assert_eq!(session.program_status().unwrap().record.id, "result");
        assert_eq!(session.program_status().unwrap().count, 1);
        assert!(session.acknowledge_program_status());
        assert!(!session.retain_exited());
        assert!(session.program_status().is_none());
    }

    #[test]
    fn headless_session_retains_scrollback_and_accepts_later_input() {
        let directory = TestDirectory::new();
        let mut session = start(
            1,
            &directory.0,
            r#"
            stty -echo
            printf '\033]9;ready\007'
            IFS= read -r request
            i=1
            while [ $i -le 40 ]; do printf 'line-%02d\n' $i; i=$((i + 1)); done
            printf '\033]2;Detached worker\007\033]9;finished\007'
            IFS= read -r request
            printf 'reattached:%s\n\033]9;input-received\007' "$request"
            IFS= read -r request
        "#,
        );
        wait_for(|| notification(&mut session, "ready"));
        session.resize(40, 3, 8, 16, 320, 48).unwrap();
        session.send_text("go", true).unwrap();
        wait_for(|| notification(&mut session, "finished"));
        assert!(!session.exited());
        assert_eq!(session.size(), (40, 3));
        assert_eq!(session.title(), "Detached worker");
        assert!(session.text().unwrap().contains("line-40"));
        session.engine.scroll(-100, false);
        assert!(session.text().unwrap().contains("line-01"));
        session.engine.scroll(0, true);
        session.send_text("hello", true).unwrap();
        wait_for(|| notification(&mut session, "input-received"));
        assert!(session.text().unwrap().contains("reattached:hello"));
    }

    #[test]
    fn directory_messages_refresh_live_cwd_in_the_same_pty_update() {
        let directory = TestDirectory::new();
        let subdir = directory.0.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let mut session = start(
            1,
            &directory.0,
            r#"
            stty -echo
            printf '\033]9;ready\007'
            IFS= read -r request
            cd subdir
            printf '\033]7;file://localhost%s\007\033]9;changed\007' "$PWD"
            IFS= read -r request
            printf '\033]7;file://localhost/tmp/stale\007\033]9;stale\007'
            IFS= read -r request
        "#,
        );
        wait_for(|| notification(&mut session, "ready"));
        session.send_text("go", true).unwrap();
        let mut changed = false;
        wait_for(|| {
            let update = session.poll();
            if update.directory_changed {
                assert_eq!(session.directory(), subdir);
                changed = true;
            }
            if update
                .notifications
                .iter()
                .any(|message| message == "changed")
            {
                assert!(changed);
                assert_eq!(session.directory(), subdir);
                true
            } else {
                false
            }
        });
        session.send_text("go", true).unwrap();
        wait_for(|| notification(&mut session, "stale"));
        assert_eq!(
            session.directory(),
            subdir,
            "Live process cwd must override stale OSC metadata"
        );
    }
    #[test]
    fn headless_poll_delivers_terminal_replies_to_the_process() {
        let directory = TestDirectory::new();
        let mut session = start(
            1,
            &directory.0,
            r"
            stty raw -echo
            printf '\033[6n'
            dd bs=1 count=6 status=none > reply
            printf '\033]9;reply-received\007'
            dd bs=1 count=1 status=none > /dev/null
        ",
        );
        wait_for(|| notification(&mut session, "reply-received"));
        assert_eq!(
            std::fs::read(directory.0.join("reply")).unwrap(),
            b"\x1b[1;1R"
        );
    }

    #[test]
    fn wrapped_input_queues_preserve_bytes_through_partial_pty_writes() {
        let directory = TestDirectory::new();
        let mut session = start(
            1,
            &directory.0,
            r"
            stty raw -echo
            printf '\033]9;ready\007'
            dd bs=65536 count=1 iflag=fullblock status=none > input
            printf '\033]9;input-received\007'
            dd bs=1 count=1 status=none > /dev/null
        ",
        );
        wait_for(|| notification(&mut session, "ready"));
        let bytes: Vec<_> = (0..65536).map(|i| (i % 251) as u8).collect();
        {
            let mut pending = session.pty.as_ref().unwrap().pending.borrow_mut();
            *pending = VecDeque::with_capacity(bytes.len());
            pending.extend(std::iter::repeat_n(0, 32768));
            pending.extend(&bytes[..16384]);
            pending.drain(..32768);
        }
        session.send(&bytes[16384..]).unwrap();
        {
            let pending = session.pty.as_ref().unwrap().pending.borrow();
            let (first, second) = pending.as_slices();
            assert!(!first.is_empty() && !second.is_empty());
        }
        wait_for(|| notification(&mut session, "input-received"));
        assert_eq!(std::fs::read(directory.0.join("input")).unwrap(), bytes);
    }

    #[test]
    fn headless_resize_and_directory_tracking_reach_the_process() {
        let directory = TestDirectory::new();
        std::fs::create_dir(directory.0.join("child")).unwrap();
        let mut session = start(
            1,
            &directory.0,
            r"
            stty -echo
            printf '\033]9;ready\007'
            IFS= read -r request
            stty size > dimensions
            cd child
            printf '\033]7;file:///\007\033]9;changed-directory\007'
            IFS= read -r request
        ",
        );
        wait_for(|| notification(&mut session, "ready"));
        assert!(session.resize(91, 37, 8, 16, 728, 592).unwrap());
        assert!(!session.resize(91, 37, 8, 16, 728, 592).unwrap());
        // Pixel and cell dimensions still reach the terminal when the grid
        // has not changed, as happens with zoom or fractional scaling.
        assert!(session.resize(91, 37, 9, 18, 819, 666).unwrap());
        let size = session.pty.as_ref().unwrap().master.get_size().unwrap();
        assert_eq!((size.pixel_width, size.pixel_height), (819, 666));
        session.send_text("go", true).unwrap();
        wait_for(|| notification(&mut session, "changed-directory"));
        assert_eq!(
            std::fs::read_to_string(directory.0.join("dimensions")).unwrap(),
            "37 91\n"
        );
        session.refresh_directory();
        assert_eq!(session.directory(), directory.0.join("child"));
        session.engine.snapshot().unwrap();
        assert_eq!(
            (session.engine.frame.cols, session.engine.frame.rows),
            (91, 37)
        );
        assert_eq!(session.engine.cells.len(), 91 * 37);
    }

    #[test]
    fn dropping_session_terminates_its_process() {
        let directory = TestDirectory::new();
        let script = "printf 'alive\\n\\033]9;ready\\007'; IFS= read -r request";
        let mut session = start(1, &directory.0, script);
        wait_for(|| notification(&mut session, "ready"));
        let pid = session.pid.unwrap();
        drop(session);
        wait_for(|| !Path::new(&format!("/proc/{pid}")).exists());
    }
}
