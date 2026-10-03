//! Frontend handles contain cached viewports; terminal engines live in the daemon.
use crate::{
    Cell, Frame, ipc,
    model::{CursorStyle, Session},
    session::{ScreenSnapshot, SessionUpdate},
    theme::Theme,
    transport::{self, Metadata, Packet},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::BufReader,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

pub type SessionHandle = Rc<RefCell<RemoteSession>>;

struct Connection {
    commands: mpsc::SyncSender<Value>,
    socket: UnixStream,
}

impl Connection {
    fn send(&self, request: Value) -> anyhow::Result<()> {
        self.commands
            .try_send(request)
            .map_err(|e| anyhow::anyhow!("Could not queue daemon command: {e}"))
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

pub struct SessionManager {
    connection: Rc<Connection>,
    receiver: mpsc::Receiver<Result<Packet, String>>,
    sessions: HashMap<u64, SessionHandle>,
    visible: HashSet<u64>,
    subscription_pending: bool,
    pending_reads: HashMap<u64, u64>,
    wake: ipc::Wake,
    disconnected: bool,
}

pub fn ensure_daemon() -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut child: Option<std::process::Child> = None;
    loop {
        if let Ok(value) = ipc::call(json!({"method":"_hello"})) {
            anyhow::ensure!(
                value["version"] == transport::VERSION,
                "Incompatible gmux daemon. Stop it with gmux quit before upgrading."
            );
            if !value["quitting"].as_bool().unwrap_or(false) {
                if let Some(mut child) = child.take() {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                return Ok(());
            }
        } else if child.is_none() {
            let exe = std::env::current_exe()?.with_file_name("gmux-daemon");
            anyhow::ensure!(
                exe.is_file(),
                "Missing {}. Build both binaries with cargo build --bins.",
                exe.display()
            );
            std::fs::create_dir_all(crate::model::config_dir())?;
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::model::config_dir().join("daemon.log"))?;
            let mut command = std::process::Command::new(exe);
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log);
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            child = Some(command.spawn()?);
        }
        if let Some(process) = child.as_mut()
            && let Some(status) = process.try_wait()?
        {
            anyhow::ensure!(
                status.success(),
                "Daemon exited with {status}. See daemon.log in {}",
                crate::model::config_dir().display()
            );
            // Another launcher may have won the lock while its daemon was still starting.
            child = None;
        }
        anyhow::ensure!(Instant::now() < deadline, "Timed out starting daemon");
        std::thread::sleep(Duration::from_millis(25));
    }
}

impl SessionManager {
    pub fn connect(wake: ipc::Wake) -> anyhow::Result<Self> {
        ensure_daemon()?;
        let socket = UnixStream::connect(ipc::companion_path(".stream"))?;
        let mut writer = socket.try_clone()?;
        transport::command(
            &mut writer,
            &json!({"method":"attach","version":transport::VERSION}),
        )?;
        let reader = socket.try_clone()?;
        let (commands, queue) = mpsc::sync_channel(128);
        let (sender, receiver) = mpsc::sync_channel(64);
        let errors = sender.clone();
        let writer_wake = wake.clone();
        std::thread::spawn(move || {
            while let Ok(request) = queue.recv() {
                if let Err(error) = transport::command(&mut writer, &request) {
                    let _ = errors.send(Err(error.to_string()));
                    writer_wake.notify();
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    break;
                }
                // Retry state commands after queue backpressure without a timer.
                writer_wake.notify();
            }
        });
        let reader_wake = wake.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let packet = Packet::read(&mut reader)
                    .map_err(|e| format!("Lost connection to the terminal daemon: {e}"));
                let failed = packet.is_err();
                if sender.send(packet).is_err() {
                    break;
                }
                reader_wake.notify();
                if failed {
                    break;
                }
            }
        });
        Ok(Self {
            connection: Rc::new(Connection { commands, socket }),
            receiver,
            sessions: HashMap::new(),
            visible: HashSet::new(),
            subscription_pending: false,
            pending_reads: HashMap::new(),
            wake,
            disconnected: false,
        })
    }
    pub fn saved_session(&self) -> anyhow::Result<Session> {
        Ok(serde_json::from_value(
            ipc::call(json!({"method":"_state"}))?["session"].take(),
        )?)
    }
    pub fn save(&self, session: &Session) -> anyhow::Result<()> {
        ipc::call(json!({"method":"_save","session":session}))?;
        Ok(())
    }
    pub fn shutdown(&self) -> anyhow::Result<()> {
        ipc::call(json!({"method":"quit"}))?;
        Ok(())
    }
    pub fn set_visible(&mut self, ids: HashSet<u64>) -> anyhow::Result<()> {
        if self.visible != ids {
            self.visible = ids;
            self.subscription_pending = true;
        }
        self.subscribe()
    }
    fn subscribe(&mut self) -> anyhow::Result<()> {
        if self.subscription_pending {
            self.connection
                .send(json!({"method":"subscribe","panes":self.visible}))?;
            self.subscription_pending = false;
        }
        Ok(())
    }
    pub fn mark_read(&mut self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let mut session = session.borrow_mut();
        let sequence = session.meta.unread;
        session.read_through = session.read_through.max(sequence);
        drop(session);
        self.pending_reads.insert(id, sequence);
        self.flush_reads();
    }
    fn flush_reads(&mut self) {
        self.pending_reads.retain(|id, sequence| {
            self.connection
                .send(json!({"method":"mark-read","pane":id,"through":sequence}))
                .is_err()
        });
    }
    pub fn spawn(
        &mut self,
        id: u64,
        tab: u64,
        directory: PathBuf,
        cursor_style: CursorStyle,
        theme: &Theme,
    ) -> anyhow::Result<SessionHandle> {
        let meta = serde_json::from_value(ipc::call(json!({"method":"_spawn","pane":id,"tab":tab,"directory":directory,"style":cursor_style,"theme":theme}))?["meta"].take())?;
        let handle = Rc::new(RefCell::new(RemoteSession {
            connection: self.connection.clone(),
            meta,
            frame: Frame {
                cols: 80,
                rows: 24,
                background: theme.background,
                foreground: theme.foreground,
                cursor: theme.cursor,
                ..Frame::default()
            },
            cells: vec![
                Cell {
                    fg: theme.foreground,
                    bg: theme.background,
                    ..Cell::default()
                };
                80 * 24
            ],
            revision: 0,
            read_through: 0,
        }));
        self.sessions.insert(id, handle.clone());
        Ok(handle)
    }
    pub fn poll(&mut self) -> HashMap<u64, SessionUpdate> {
        // A full command queue must not lose a tab's viewport subscription.
        let _ = self.subscribe();
        self.flush_reads();
        let mut updates: HashMap<u64, SessionUpdate> = HashMap::new();
        for _ in 0..256 {
            match self.receiver.try_recv() {
                Ok(Ok(packet)) => {
                    let id = packet.header.meta.id;
                    if let Some(session) = self.sessions.get(&id) {
                        let mut session = session.borrow_mut();
                        let update = updates.entry(id).or_default();
                        update.directory_changed |=
                            session.meta.directory != packet.header.meta.directory;
                        session.meta = packet.header.meta;
                        update.unread = Some(session.unread());
                        if let Some(frame) = packet.header.frame {
                            session
                                .cells
                                .resize(frame.cols as usize * frame.rows as usize, Cell::default());
                            let cols = frame.cols as usize;
                            for (index, row) in packet.header.rows.iter().enumerate() {
                                session.cells[*row as usize * cols..(*row as usize + 1) * cols]
                                    .copy_from_slice(
                                        &packet.cells[index * cols..(index + 1) * cols],
                                    );
                            }
                            session.frame = frame;
                            if !packet.header.rows.is_empty() {
                                session.revision = session.revision.wrapping_add(1);
                            }
                            update.changed = true;
                        }
                        update.notifications.extend(packet.header.notifications);
                        if packet.header.error.is_some() {
                            update.error = packet.header.error;
                        }
                    }
                }
                Ok(Err(error)) => {
                    eprintln!("{error}");
                    self.disconnected = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => return updates,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    return updates;
                }
            }
        }
        self.wake.notify();
        updates
    }
    pub fn disconnected(&self) -> bool {
        self.disconnected
    }
    pub fn close(&mut self, id: u64) {
        if let Err(error) = ipc::call(json!({"method":"_close","pane":id})) {
            eprintln!("Could not close pane: {error}");
        }
        self.sessions.remove(&id);
        self.pending_reads.remove(&id);
    }
    pub fn clear(&mut self) {
        self.sessions.clear();
        let _ = self.connection.socket.shutdown(std::net::Shutdown::Both);
    }
}
impl Drop for SessionManager {
    fn drop(&mut self) {
        self.clear();
    }
}

pub struct RemoteSession {
    connection: Rc<Connection>,
    meta: Metadata,
    frame: Frame,
    cells: Vec<Cell>,
    revision: u64,
    read_through: u64,
}
impl RemoteSession {
    fn command(&self, mut request: Value) -> anyhow::Result<()> {
        request["pane"] = json!(self.id());
        self.connection.send(request)
    }
    pub fn id(&self) -> u64 {
        self.meta.id
    }
    pub fn directory(&self) -> &Path {
        &self.meta.directory
    }
    pub fn size(&self) -> (u16, u16) {
        self.meta.size
    }
    pub fn exited(&self) -> bool {
        self.meta.exited
    }
    pub fn unread(&self) -> bool {
        self.meta.unread > self.read_through
    }
    pub fn busy(&self) -> bool {
        ipc::call(json!({"method":"_metadata","pane":self.id()}))
            .ok()
            .and_then(|value| value["meta"]["busy"].as_bool())
            .unwrap_or(self.meta.busy)
    }
    pub fn title(&self) -> &str {
        &self.meta.title
    }
    pub fn mode(&self, mode: i32) -> bool {
        match mode {
            0 => self.meta.modes[0],
            1004 => self.meta.modes[1],
            _ => false,
        }
    }
    pub fn refresh_directory(&mut self) {
        if let Ok(mut value) = ipc::call(json!({"method":"_metadata","pane":self.id()}))
            && let Ok(meta) = serde_json::from_value(value["meta"].take())
        {
            self.meta = meta;
        }
    }
    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.command(json!({"method":"input","bytes":bytes}))
    }
    pub fn send_text(&self, text: &str, enter: bool) -> anyhow::Result<()> {
        self.command(json!({"method":"paste","text":text,"enter":enter,"clipboard":false}))
    }
    pub fn paste(&self, text: &str) -> anyhow::Result<()> {
        self.command(
            json!({"method":"paste","text":text,"enter":false,"clipboard":true,"bottom":true}),
        )
    }
    pub fn key(
        &self,
        name: &str,
        cp: u32,
        mods: u16,
        release: bool,
        text: &str,
    ) -> anyhow::Result<()> {
        self.command(
            json!({"method":"key","name":name,"cp":cp,"mods":mods,"release":release,"text":text}),
        )
    }
    pub fn mouse(
        &self,
        position: (f32, f32),
        metrics: (u32, u32),
        action: i32,
        button: i32,
        mods: u16,
        pressed: bool,
    ) -> anyhow::Result<()> {
        self.command(json!({"method":"mouse","position":position,"metrics":metrics,"action":action,"button":button,"mods":mods,"pressed":pressed}))
    }
    pub fn scroll(&self, delta: isize, bottom: bool) {
        if let Err(e) = self.command(json!({"method":"scroll","delta":delta,"bottom":bottom})) {
            eprintln!("{e}");
        }
    }
    pub fn set_cursor_style(&self, style: CursorStyle) -> anyhow::Result<()> {
        self.command(json!({"method":"style","style":style}))
    }
    pub fn set_theme(&self, theme: &Theme) -> anyhow::Result<()> {
        self.command(json!({"method":"theme","theme":theme}))
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
        self.command(json!({"method":"resize","cols":cols,"rows":rows,"cw":cw,"ch":ch,"width":width,"height":height}))?;
        Ok(())
    }
    pub fn snapshot(&self) -> ScreenSnapshot<'_> {
        ScreenSnapshot {
            frame: self.frame,
            cells: &self.cells,
            revision: self.revision,
        }
    }
    pub fn text(&self) -> anyhow::Result<String> {
        Ok(
            ipc::call(json!({"method":"_read","pane":self.id()}))?["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Header;

    fn manager() -> (
        SessionManager,
        mpsc::Sender<Result<Packet, String>>,
        SessionHandle,
    ) {
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, _) = mpsc::sync_channel(1);
        let connection = Rc::new(Connection { commands, socket });
        let (sender, receiver) = mpsc::channel();
        let session = Rc::new(RefCell::new(RemoteSession {
            connection: connection.clone(),
            meta: Metadata {
                id: 1,
                directory: PathBuf::from("/tmp"),
                title: "shell".into(),
                size: (2, 2),
                exited: false,
                unread: 0,
                busy: false,
                modes: [false; 2],
            },
            frame: Frame {
                cols: 2,
                rows: 2,
                ..Frame::default()
            },
            cells: vec![Cell::default(); 4],
            revision: 0,
            read_through: 0,
        }));
        (
            SessionManager {
                connection,
                receiver,
                sessions: HashMap::from([(1, session.clone())]),
                visible: HashSet::new(),
                subscription_pending: false,
                pending_reads: HashMap::new(),
                wake: ipc::Wake::new().unwrap(),
                disconnected: false,
            },
            sender,
            session,
        )
    }

    #[test]
    fn idle_and_metadata_updates_do_not_repaint_or_change_cells() {
        let (mut manager, sender, session) = manager();
        assert!(manager.poll().is_empty());
        let mut meta = session.borrow().meta.clone();
        meta.directory = PathBuf::from("/tmp/project");
        meta.title = "worker".into();
        sender
            .send(Ok(Packet {
                header: Header {
                    meta,
                    frame: None,
                    rows: vec![],
                    notifications: vec!["ready".into()],
                    error: None,
                },
                cells: vec![],
            }))
            .unwrap();
        let updates = manager.poll();
        assert_eq!(updates.len(), 1);
        let update = &updates[&1];
        assert!(!update.changed);
        assert!(update.directory_changed);
        assert_eq!(update.notifications, ["ready"]);
        let session = session.borrow();
        assert_eq!(session.title(), "worker");
        assert_eq!(session.directory(), Path::new("/tmp/project"));
        assert_eq!(session.snapshot().revision, 0);
        assert!(session.snapshot().cells == [Cell::default(); 4]);
        assert!(manager.poll().is_empty());
    }

    #[test]
    fn queued_rows_coalesce_without_losing_cells_or_cursor_changes() {
        let (mut manager, sender, session) = manager();
        let mut frame = session.borrow().frame;
        for (row, text) in [(0, b'a'), (1, b'b')] {
            let mut cell = Cell::default();
            cell.text[0] = text;
            frame.cursor_y = row;
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: session.borrow().meta.clone(),
                        frame: Some(frame),
                        rows: vec![row],
                        notifications: vec![],
                        error: None,
                    },
                    cells: vec![cell; 2],
                }))
                .unwrap();
        }
        let updates = manager.poll();
        assert_eq!(updates.len(), 1);
        assert!(updates[&1].changed);
        assert!(!updates[&1].directory_changed);
        let session = session.borrow();
        let screen = session.snapshot();
        assert_eq!(screen.revision, 2);
        assert_eq!(screen.frame.cursor_y, 1);
        assert_eq!(
            screen.cells.iter().map(Cell::text).collect::<Vec<_>>(),
            ["a", "a", "b", "b"]
        );
    }
    #[test]
    fn viewport_subscription_retries_after_command_backpressure() {
        let (mut manager, _sender, _session) = manager();
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        commands.send(json!({"method":"input"})).unwrap();
        manager.connection = Rc::new(Connection { commands, socket });
        assert!(manager.set_visible(HashSet::from([1])).is_err());
        assert_eq!(receiver.try_recv().unwrap()["method"], "input");
        assert!(manager.poll().is_empty());
        assert_eq!(
            receiver.try_recv().unwrap(),
            json!({"method":"subscribe","panes":[1]})
        );
        assert!(manager.poll().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn acknowledging_an_alert_ignores_old_packets_and_keeps_new_alerts() {
        let (mut manager, sender, session) = manager();
        session.borrow_mut().meta.unread = 7;
        assert!(session.borrow().unread());
        manager.mark_read(1);
        assert!(!session.borrow().unread());
        for (sequence, expected) in [(7, false), (0, false), (8, true)] {
            let mut meta = session.borrow().meta.clone();
            meta.unread = sequence;
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta,
                        frame: None,
                        rows: vec![],
                        notifications: vec![],
                        error: None,
                    },
                    cells: vec![],
                }))
                .unwrap();
            assert_eq!(manager.poll()[&1].unread, Some(expected));
            assert_eq!(session.borrow().unread(), expected);
        }
    }

    #[test]
    fn read_acknowledgements_retry_after_command_backpressure() {
        let (mut manager, _sender, session) = manager();
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        commands.send(json!({"method":"input"})).unwrap();
        manager.connection = Rc::new(Connection { commands, socket });
        session.borrow_mut().meta.unread = 7;
        manager.mark_read(1);
        assert!(!session.borrow().unread());
        assert_eq!(receiver.try_recv().unwrap()["method"], "input");
        manager.poll();
        assert_eq!(
            receiver.try_recv().unwrap(),
            json!({"method":"mark-read","pane":1,"through":7})
        );
        manager.poll();
        assert!(receiver.try_recv().is_err());
    }
}
