//! Terminal processes and state, independent of GTK widgets and rendering.

use crate::{
    Cell, Frame,
    engine::Engine,
    ipc,
    model::CursorStyle,
    theme::Theme,
    title::{ForegroundProcess, PaneTitle},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::RefCell,
    collections::VecDeque,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A borrowed viewport. Taking a snapshot does not copy the cell buffer.
pub struct ScreenSnapshot<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    pub revision: u64,
}

#[derive(Default)]
pub struct SessionUpdate {
    pub changed: bool,
    pub directory_changed: bool,
    pub unread: Option<bool>,
    pub error: Option<String>,
    pub notifications: Vec<String>,
}

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

pub struct TerminalSession {
    id: u64,
    engine: Engine,
    pty: Option<Pty>,
    pid: Option<u32>,
    directory: PathBuf,
    size: (u16, u16),
    exited: bool,
    title: PaneTitle,
    title_polled: Instant,
}

impl TerminalSession {
    pub(crate) fn spawn(
        id: u64,
        tab: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
    ) -> anyhow::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command.env("GMUX_TAB", tab.to_string());
        command.env("GMUX_PANE", id.to_string());
        Self::spawn_command(id, directory, cursor_style, theme, command)
    }

    fn spawn_command(
        id: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
        mut command: CommandBuilder,
    ) -> anyhow::Result<Self> {
        // Complete fallible setup before spawning a process we must clean up.
        let mut engine = Engine::new()?;
        engine.set_cursor_style(cursor_style)?;
        engine.set_theme(theme)?;
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
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
            size: (80, 24),
            exited: false,
            title: PaneTitle::default(),
            title_polled: Instant::now(),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    pub fn title(&self) -> String {
        self.title.display(&self.directory, self.pid)
    }

    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(!self.exited, "This terminal's process has exited");
        let pty = self
            .pty
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session is closed"))?;
        let mut pending = pty.pending.borrow_mut();
        anyhow::ensure!(
            pending.len() + bytes.len() <= 1_048_576,
            "Terminal input queue is full"
        );
        pending.extend(bytes);
        Ok(())
    }

    pub fn send_text(&mut self, text: &str, enter: bool) -> anyhow::Result<()> {
        let mut bytes = self.paste(text, false)?;
        if enter {
            bytes.push(b'\r');
        }
        self.send(&bytes)
    }

    pub fn poll(&mut self) -> SessionUpdate {
        let mut update = SessionUpdate {
            changed: self.engine.expire_render_hold(),
            ..SessionUpdate::default()
        };
        if let Some(pty) = &mut self.pty {
            let fd = pty.master.as_raw_fd().unwrap();
            let mut pending = pty.pending.borrow_mut();
            while !pending.is_empty() {
                let bytes = pending.make_contiguous();
                let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
                if n > 0 {
                    pending.drain(..n as usize);
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
        let title_update = self.engine.take_title_update();
        self.refresh_title(title_update);
        if let Some(pty) = &mut self.pty
            && let Some(Ok(Some(_))) = pty.child.as_mut().map(|child| child.try_wait())
        {
            pty.child = None;
            self.exited = true;
        }
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

    pub fn refresh_directory(&mut self) {
        let actual_directory = self
            .foreground_pid()
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok());
        if let Some(directory) = actual_directory {
            self.directory = directory;
        } else {
            // Prefer the live process directory to stale OSC metadata after cd.
            let pwd = self.engine.metadata(true);
            if let Ok((path, _)) = glib::filename_from_uri(&pwd)
                && path.is_dir()
            {
                self.directory = path;
            }
        }
        self.refresh_title(None);
    }

    pub fn busy(&self) -> bool {
        !self.exited
            && self
                .foreground_pid()
                .zip(self.pid)
                .is_some_and(|(fg, pid)| fg != pid)
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) -> anyhow::Result<()> {
        self.engine.set_cursor_style(style)
    }

    pub fn set_theme(&mut self, theme: &Theme) -> anyhow::Result<()> {
        self.engine.set_theme(theme)
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cw: u32,
        ch: u32,
        width: u16,
        height: u16,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(cols > 0 && rows > 0, "Terminal dimensions must be positive");
        self.engine.resize(cols, rows, cw, ch)?;
        self.size = (cols, rows);
        if let Some(pty) = &self.pty {
            pty.master.resize(PtySize {
                cols,
                rows,
                pixel_width: width,
                pixel_height: height,
            })?;
        }
        Ok(())
    }

    pub fn snapshot(&mut self) -> anyhow::Result<ScreenSnapshot<'_>> {
        self.engine.snapshot()?;
        Ok(ScreenSnapshot {
            frame: self.engine.frame,
            cells: &self.engine.cells,
            revision: self.engine.revision(),
        })
    }

    pub fn text(&mut self) -> anyhow::Result<String> {
        self.engine.text()
    }

    pub fn mode(&self, mode: i32) -> bool {
        self.engine.mode(mode)
    }

    pub fn scroll(&mut self, delta: isize, bottom: bool) {
        self.engine.scroll(delta, bottom);
    }

    pub fn paste(&mut self, text: &str, clipboard: bool) -> anyhow::Result<Vec<u8>> {
        self.engine.paste(text, clipboard)
    }

    pub fn key(&mut self, name: &str, cp: u32, mods: u16, release: bool, text: &str) -> Vec<u8> {
        self.engine.key(name, cp, mods, release, text)
    }

    pub fn mouse(
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

    pub fn close(&mut self) {
        self.exited = true;
        self.pty.take();
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
        session.scroll(-100, false);
        assert!(session.text().unwrap().contains("line-01"));
        session.scroll(0, true);
        session.send_text("hello", true).unwrap();
        wait_for(|| notification(&mut session, "input-received"));
        assert!(session.text().unwrap().contains("reattached:hello"));
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
        session.resize(91, 37, 8, 16, 728, 592).unwrap();
        session.send_text("go", true).unwrap();
        wait_for(|| notification(&mut session, "changed-directory"));
        assert_eq!(
            std::fs::read_to_string(directory.0.join("dimensions")).unwrap(),
            "37 91\n"
        );
        session.refresh_directory();
        assert_eq!(session.directory(), directory.0.join("child"));
        let screen = session.snapshot().unwrap();
        assert_eq!((screen.frame.cols, screen.frame.rows), (91, 37));
        assert_eq!(screen.cells.len(), 91 * 37);
    }

    #[test]
    fn closing_or_dropping_session_terminates_its_process() {
        let directory = TestDirectory::new();
        let script = "printf 'alive\\n\\033]9;ready\\007'; IFS= read -r request";
        let mut first = start(1, &directory.0, script);
        wait_for(|| notification(&mut first, "ready"));
        let first_pid = first.pid.unwrap();
        first.close();
        assert!(first.exited());
        assert!(first.send(b"input").is_err());
        assert_eq!(first.text().unwrap(), "alive");
        wait_for(|| !Path::new(&format!("/proc/{first_pid}")).exists());

        let mut second = start(2, &directory.0, script);
        wait_for(|| notification(&mut second, "ready"));
        let second_pid = second.pid.unwrap();
        drop(second);
        wait_for(|| !Path::new(&format!("/proc/{second_pid}")).exists());
    }
}
