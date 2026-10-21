//! Terminal processes and state, independent of GTK widgets and rendering.

use crate::{
    engine::Engine,
    title::{ForegroundProcess, PaneTitle},
};
use gmux_client::{ipc, model::CursorStyle, session::SessionUpdate, theme::Theme};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::RefCell,
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

pub(crate) struct TerminalSession {
    id: u64,
    engine: Engine,
    pty: Option<Pty>,
    pid: Option<u32>,
    directory: PathBuf,
    size: PtySize,
    cell_size: (u32, u32),
    exited: bool,
    title: PaneTitle,
    title_polled: Instant,
}

impl TerminalSession {
    pub(crate) fn progress(&self) -> gmux_client::progress::Progress {
        self.engine.progress()
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
    pub(crate) fn links(&mut self) -> anyhow::Result<serde_json::Value> {
        self.engine.snapshot()?;
        Ok(serde_json::json!({"frame": self.engine.frame, "hyperlinks": self.engine.hyperlinks}))
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
    ) -> anyhow::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command.env("GMUX_TAB", tab.to_string());
        command.env("GMUX_PANE", id.to_string());
        Self::spawn_command(id, directory, cursor_style, theme, command, history)
    }

    fn spawn_command(
        id: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
        mut command: CommandBuilder,
        history: Option<&[u8]>,
    ) -> anyhow::Result<Self> {
        // Complete fallible setup before spawning a process we must clean up.
        let mut engine = Engine::new()?;
        if let Some(history) = history
            && let Err(error) = engine.restore_history(history)
        {
            eprintln!(
                "Could not restore scrollback for pane {id}: {error}. Starting with empty history"
            );
            engine = Engine::new()?;
        }
        engine.set_cursor_style(cursor_style)?;
        engine.set_theme(theme)?;
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
        command.env("GMUX_SOCKET", ipc::socket_path());
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
        anyhow::ensure!(
            pending.len() + bytes.len() + usize::from(enter)
                <= 2 * gmux_client::transport::MAX_PASTE,
            "Terminal input queue is full"
        );
        pending.extend(bytes);
        if enter {
            pending.push_back(b'\r');
        }
        Ok(())
    }

    pub(crate) fn send_text(&mut self, text: &str, enter: bool) -> anyhow::Result<()> {
        let bytes = self.paste(text, false)?;
        self.send_with_enter(&bytes, enter)
    }

    pub(crate) fn poll(&mut self) -> SessionUpdate {
        let mut update = SessionUpdate {
            changed: self.engine.expire_render_hold(),
            ..SessionUpdate::default()
        };
        if let Some(pty) = &mut self.pty {
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
            // Bound work per session so a noisy process cannot block its owner.
            let mut buffer = [0u8; 16384];
            for _ in 0..32 {
                if pty.eof {
                    break;
                }
                let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if n > 0 {
                    self.engine.write(&buffer[..n as usize]);
                    update.changed = true;
                } else if n == 0 {
                    pty.eof = true;
                    break;
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        pty.eof = true;
                    }
                    break;
                }
            }
        }
        let (replies, notifications) = self.engine.take_events();
        update.notifications = notifications;
        if !replies.is_empty() {
            let _ = self.send(&replies);
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
        if let Some(pty) = &mut self.pty
            && let Some(Ok(Some(_))) = pty.child.as_mut().map(|child| child.try_wait())
        {
            pty.child = None;
            self.exited = true;
        }
        if self.exited {
            self.engine.clear_progress();
        }
        update.progress_changed = self.engine.take_progress_changed();
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
            && self
                .foreground_pid()
                .zip(self.pid)
                .is_some_and(|(fg, pid)| fg != pid)
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
        anyhow::ensure!(
            gmux_client::viewport::valid(cols as u64, rows as u64),
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
        self.engine.clear_screen()
    }
    pub(crate) fn mode(&self, mode: i32) -> bool {
        self.engine.mode(mode)
    }

    pub(crate) fn paste(&mut self, text: &str, clipboard: bool) -> anyhow::Result<Vec<u8>> {
        self.engine.paste(text, clipboard)
    }

    pub(crate) fn key(
        &mut self,
        name: &str,
        cp: u32,
        mods: u16,
        release: bool,
        text: &str,
    ) -> Vec<u8> {
        self.engine.key(name, cp, mods, release, text)
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
        if pty.eof && pty.pending.borrow().is_empty() {
            return None;
        }
        Some((
            pty.master.as_raw_fd()?,
            if pty.eof { 0 } else { libc::POLLIN }
                | if pty.pending.borrow().is_empty() {
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
