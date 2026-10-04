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
    cell::{Cell as ValueCell, RefCell},
    collections::{HashMap, HashSet},
    io::BufReader,
    os::unix::net::UnixStream,
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub type SessionHandle = Rc<RefCell<RemoteSession>>;

struct Connection {
    commands: mpsc::SyncSender<Outgoing>,
    pending_bytes: Arc<AtomicUsize>,
    socket: UnixStream,
}
struct Outgoing {
    request: Value,
    bytes: usize,
}

impl Connection {
    fn send(&self, request: Value) -> anyhow::Result<()> {
        let bytes = if request["method"] == "paste" {
            request["text"].as_str().map_or(0, str::len)
        } else {
            0
        };
        anyhow::ensure!(
            bytes <= transport::MAX_PASTE,
            "Paste is too large. The limit is 4 MiB."
        );
        self.pending_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                (pending + bytes <= 2 * transport::MAX_PASTE).then_some(pending + bytes)
            })
            .map_err(|_| anyhow::anyhow!("Terminal input queue is full"))?;
        if let Err(error) = self.commands.try_send(Outgoing { request, bytes }) {
            self.pending_bytes.fetch_sub(bytes, Ordering::Relaxed);
            anyhow::bail!("Could not queue daemon command: {error}");
        }
        Ok(())
    }
}

fn write_request(writer: &mut impl std::io::Write, mut request: Value) -> anyhow::Result<()> {
    if request["method"] != "paste" {
        return transport::command(writer, &request);
    }
    let Value::String(text) = request["text"].take() else {
        anyhow::bail!("Paste requires text");
    };
    request["method"] = json!("paste-begin");
    request["size"] = json!(text.len());
    transport::command(writer, &request)?;
    let pane = request["pane"].clone();
    let mut start = 0;
    while start < text.len() {
        let end = text.floor_char_boundary((start + transport::PASTE_CHUNK).min(text.len()));
        transport::command(
            writer,
            &json!({"method":"paste-data", "pane":pane, "text":&text[start..end]}),
        )?;
        start = end;
    }
    transport::command(writer, &json!({"method":"paste-end", "pane":pane}))
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
    metadata: HashMap<u64, Metadata>,
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
            // Surface recovery errors to the launcher before a failing daemon
            // can hide their details in its log.
            let settings = crate::model::load::<crate::model::Settings>("settings.json")?;
            let _: crate::model::WorkspaceCatalog = crate::model::load("workspaces.json")?;
            if settings.restore {
                let _: Session = crate::model::load("session.json")?;
            }
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
        let (commands, queue) = mpsc::sync_channel::<Outgoing>(128);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let writer_bytes = pending_bytes.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        let errors = sender.clone();
        let writer_wake = wake.clone();
        std::thread::spawn(move || {
            while let Ok(outgoing) = queue.recv() {
                let result = write_request(&mut writer, outgoing.request);
                writer_bytes.fetch_sub(outgoing.bytes, Ordering::Relaxed);
                if let Err(error) = result {
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
            connection: Rc::new(Connection {
                commands,
                pending_bytes,
                socket,
            }),
            receiver,
            sessions: HashMap::new(),
            metadata: HashMap::new(),
            visible: HashSet::new(),
            subscription_pending: false,
            pending_reads: HashMap::new(),
            wake,
            disconnected: false,
        })
    }
    pub fn saved_session(&mut self) -> anyhow::Result<Session> {
        let mut value = ipc::call(json!({"method":"_state"}))?;
        self.install_metadata(serde_json::from_value(value["metadata"].take())?);
        Ok(serde_json::from_value(value["session"].take())?)
    }
    pub fn install_metadata(&mut self, metadata: Vec<Metadata>) {
        self.metadata.clear();
        for meta in metadata {
            if let Some(session) = self.sessions.get(&meta.id) {
                session.borrow_mut().meta = meta.clone();
            } else {
                self.metadata.insert(meta.id, meta);
            }
        }
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
    pub fn attach(&mut self, id: u64, theme: &Theme) -> anyhow::Result<SessionHandle> {
        if let Some(handle) = self.sessions.get(&id) {
            return Ok(handle.clone());
        }
        let meta = self
            .metadata
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Missing daemon metadata for pane {id}"))?;
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
            scroll_serial: ValueCell::new(0),
            scroll_acknowledged: 0,
            pending_scroll: ValueCell::new(None),
            find: crate::search::FindState::default(),
            selection: None,
        }));
        self.sessions.insert(id, handle.clone());
        Ok(handle)
    }
    pub fn poll(&mut self) -> HashMap<u64, SessionUpdate> {
        // A full command queue must not lose a tab's viewport subscription.
        let _ = self.subscribe();
        self.flush_reads();
        for session in self.sessions.values() {
            session.borrow().flush_scroll();
        }
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
                        if let Some(find) = packet.header.find {
                            update.changed |= session.find != find;
                            session.find = find;
                        }
                        update.changed |= session.selection != packet.header.selection;
                        session.selection = packet.header.selection;
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
                            session.scroll_acknowledged = packet.header.scroll_serial;
                            if !packet.header.rows.is_empty() {
                                session.revision = session.revision.wrapping_add(1);
                            }
                            update.changed = true;
                        }
                        update.notifications.extend(packet.header.notifications);
                        if packet.header.copy.is_some() {
                            update.copy = packet.header.copy;
                        }
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
        self.sessions.remove(&id);
        self.metadata.remove(&id);
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
    scroll_serial: ValueCell<u64>,
    scroll_acknowledged: u64,
    pending_scroll: ValueCell<Option<u64>>,
    find: crate::search::FindState,
    selection: Option<(u64, u64)>,
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
        self.meta.busy
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
    pub fn find(&self, query: &str) -> anyhow::Result<()> {
        self.command(json!({"method":"find","query":query}))
    }
    pub fn find_advance(&self, previous: bool) -> anyhow::Result<()> {
        self.command(json!({"method":"find-advance","previous":previous}))
    }
    pub fn find_state(&self) -> &crate::search::FindState {
        &self.find
    }
    pub fn select(&self, start: (u16, u32), end: (u16, u32)) -> anyhow::Result<()> {
        self.command(json!({"method":"select","start":start,"end":end}))
    }
    pub fn copy(&self) -> anyhow::Result<()> {
        self.command(json!({"method":"copy"}))
    }
    pub fn clear_selection(&self) -> anyhow::Result<()> {
        self.command(json!({"method":"clear-selection"}))
    }
    pub fn selection(&self) -> Option<(u64, u64)> {
        self.selection
    }
    pub fn scroll(&self, delta: isize, bottom: bool) {
        if let Err(e) = self.command(json!({"method":"scroll","delta":delta,"bottom":bottom})) {
            eprintln!("{e}");
        }
    }
    pub fn scroll_to(&self, row: u64) {
        self.scroll_serial.set(self.scroll_serial.get() + 1);
        self.pending_scroll.set(Some(row));
        self.flush_scroll();
    }
    fn flush_scroll(&self) {
        if let Some(row) = self.pending_scroll.get()
            && self
                .command(json!({"method":"scroll-to","row":row,"serial":self.scroll_serial.get()}))
                .is_ok()
        {
            self.pending_scroll.set(None);
        }
    }
    pub fn scroll_pending(&self) -> bool {
        self.scroll_acknowledged < self.scroll_serial.get()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_pastes_use_bounded_utf8_chunks_and_preserve_operation_options() {
        let text = "a界\n\0".repeat(transport::MAX_PASTE / 6);
        let mut bytes = Vec::new();
        write_request(&mut bytes, json!({"method":"paste", "pane":7, "text":text, "clipboard":true, "enter":true, "bottom":true})).unwrap();
        let commands: Vec<Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                assert!(line.len() < 100_000);
                serde_json::from_slice(line).unwrap()
            })
            .collect();
        assert_eq!(commands[0]["method"], "paste-begin");
        assert_eq!(commands[0]["size"], text.len());
        assert_eq!(commands[0]["clipboard"], true);
        assert_eq!(commands[0]["enter"], true);
        assert_eq!(commands.last().unwrap()["method"], "paste-end");
        let mut received = String::new();
        for command in &commands[1..commands.len() - 1] {
            assert_eq!(command["method"], "paste-data");
            assert_eq!(command["pane"], 7);
            let chunk = command["text"].as_str().unwrap();
            assert!(chunk.len() <= transport::PASTE_CHUNK);
            received.push_str(chunk);
        }
        assert_eq!(received, text);
    }
    #[test]
    fn paste_queue_limits_count_bytes_and_failed_enqueues_release_the_reservation() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(2);
        let connection = Connection {
            commands,
            socket,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        };
        let paste = || json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)});
        connection.send(paste()).unwrap();
        connection.send(paste()).unwrap();
        assert!(connection.send(paste()).is_err());
        assert_eq!(
            connection.pending_bytes.load(Ordering::Relaxed),
            2 * transport::MAX_PASTE
        );
        assert!(
            connection
                .send(json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE + 1)}))
                .is_err()
        );
        assert!(
            connection
                .send(json!({"method":"paste","text":""}))
                .is_err()
        );
        assert_eq!(
            connection.pending_bytes.load(Ordering::Relaxed),
            2 * transport::MAX_PASTE
        );
        assert!(receiver.try_recv().is_ok());
    }
    use crate::transport::Header;
    use std::path::PathBuf;

    fn manager() -> (
        SessionManager,
        mpsc::Sender<Result<Packet, String>>,
        SessionHandle,
    ) {
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, _) = mpsc::sync_channel(1);
        let connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket,
        });
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
            scroll_serial: ValueCell::new(0),
            scroll_acknowledged: 0,
            pending_scroll: ValueCell::new(None),
            find: crate::search::FindState::default(),
            selection: None,
        }));
        (
            SessionManager {
                connection,
                receiver,
                sessions: HashMap::from([(1, session.clone())]),
                metadata: HashMap::new(),
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
                    scroll_serial: 0,
                    rows: vec![],
                    notifications: vec!["ready".into()],
                    error: None,
                    find: None,
                    selection: None,
                    copy: None,
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
                        scroll_serial: 0,
                        rows: vec![row],
                        notifications: vec![],
                        error: None,
                        find: None,
                        selection: None,
                        copy: None,
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
    fn scroll_requests_coalesce_under_backpressure_and_ignore_stale_acknowledgements() {
        let (mut manager, sender, session) = manager();
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        commands
            .send(Outgoing {
                request: json!({"method":"input"}),
                bytes: 0,
            })
            .unwrap();
        let connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket,
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        session.borrow().scroll_to(10);
        session.borrow().scroll_to(20);
        assert!(session.borrow().scroll_pending());
        assert_eq!(receiver.try_recv().unwrap().request["method"], "input");
        manager.poll();
        assert_eq!(
            receiver.try_recv().unwrap().request,
            json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
        );
        for serial in [1, 2] {
            let mut frame = session.borrow().frame;
            frame.scrollback = crate::Scrollback {
                total: 100,
                offset: serial * 10,
                len: 2,
            };
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: session.borrow().meta.clone(),
                        frame: Some(frame),
                        scroll_serial: serial,
                        rows: vec![],
                        notifications: vec![],
                        error: None,
                        find: None,
                        selection: None,
                        copy: None,
                    },
                    cells: vec![],
                }))
                .unwrap();
            assert!(manager.poll()[&1].changed);
            assert_eq!(session.borrow().scroll_pending(), serial < 2);
            assert_eq!(session.borrow().snapshot().revision, 0);
            assert_eq!(
                session.borrow().snapshot().frame.scrollback.offset,
                serial * 10
            );
        }
        manager.poll();
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn viewport_subscription_retries_after_command_backpressure() {
        let (mut manager, _sender, _session) = manager();
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        commands
            .send(Outgoing {
                request: json!({"method":"input"}),
                bytes: 0,
            })
            .unwrap();
        manager.connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket,
        });
        assert!(manager.set_visible(HashSet::from([1])).is_err());
        assert_eq!(receiver.try_recv().unwrap().request["method"], "input");
        assert!(manager.poll().is_empty());
        assert_eq!(
            receiver.try_recv().unwrap().request,
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
                        scroll_serial: 0,
                        rows: vec![],
                        notifications: vec![],
                        error: None,
                        find: None,
                        selection: None,
                        copy: None,
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
        commands
            .send(Outgoing {
                request: json!({"method":"input"}),
                bytes: 0,
            })
            .unwrap();
        manager.connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket,
        });
        session.borrow_mut().meta.unread = 7;
        manager.mark_read(1);
        assert!(!session.borrow().unread());
        assert_eq!(receiver.try_recv().unwrap().request["method"], "input");
        manager.poll();
        assert_eq!(
            receiver.try_recv().unwrap().request,
            json!({"method":"mark-read","pane":1,"through":7})
        );
        manager.poll();
        assert!(receiver.try_recv().is_err());
    }
}
