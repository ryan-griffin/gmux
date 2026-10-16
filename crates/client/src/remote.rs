//! Frontend handles contain cached viewports; terminal engines live in the daemon.
use crate::{
    Cell, Frame,
    control::InternalRequest,
    incoming, ipc,
    model::{CursorStyle, ExecutionState},
    session::{ScreenSnapshot, SessionUpdate},
    theme::Theme,
    transport::{self, Metadata, Packet, PasteOptions, StreamCommand, StreamRequest},
};
#[cfg(test)]
use serde_json::{Value, json};
use std::{
    cell::{Cell as ValueCell, RefCell},
    collections::{HashMap, HashSet},
    io::BufReader,
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

pub type SessionHandle = Rc<RefCell<RemoteSession>>;

struct Connection {
    commands: mpsc::SyncSender<Outgoing>,
    pending_bytes: Arc<AtomicUsize>,
    socket: ipc::Stream,
}
enum Outgoing {
    Command(Vec<u8>),
    Paste {
        pane: Option<u64>,
        text: String,
        options: PasteOptions,
    },
}

impl Outgoing {
    fn prepare(request: StreamRequest) -> anyhow::Result<Self> {
        if let StreamCommand::Paste { text, options } = request.command {
            anyhow::ensure!(
                text.len() <= transport::MAX_PASTE,
                "Paste is too large. The limit is 4 MiB."
            );
            Ok(Self::Paste {
                pane: request.pane,
                text,
                options,
            })
        } else {
            // Reject oversized commands before handing them to the writer. A
            // local validation error must not close the frontend connection.
            Ok(Self::Command(transport::command_bytes(&request)?))
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Command(bytes) => bytes.len(),
            Self::Paste { text, .. } => text.len(),
        }
    }

    fn write(self, writer: &mut impl std::io::Write) -> anyhow::Result<()> {
        match self {
            Self::Command(bytes) => Ok(writer.write_all(&bytes)?),
            Self::Paste {
                pane,
                text,
                options,
            } => write_paste(writer, pane, &text, options),
        }
    }
}

impl Connection {
    fn send(&self, request: StreamRequest) -> anyhow::Result<()> {
        let outgoing = Outgoing::prepare(request)?;
        let bytes = outgoing.bytes();
        self.pending_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                (pending + bytes <= 2 * transport::MAX_PASTE).then_some(pending + bytes)
            })
            .map_err(|_| anyhow::anyhow!("Terminal input queue is full"))?;
        if let Err(error) = self.commands.try_send(outgoing) {
            self.pending_bytes.fetch_sub(bytes, Ordering::Relaxed);
            anyhow::bail!("Could not queue daemon command: {error}");
        }
        Ok(())
    }
}

fn write_paste(
    writer: &mut impl std::io::Write,
    pane: Option<u64>,
    text: &str,
    options: PasteOptions,
) -> anyhow::Result<()> {
    transport::command(
        writer,
        &StreamRequest {
            pane,
            command: StreamCommand::PasteBegin {
                size: text.len(),
                options,
            },
        },
    )?;
    let mut start = 0;
    while start < text.len() {
        let end = text.floor_char_boundary((start + transport::PASTE_CHUNK).min(text.len()));
        transport::command(
            writer,
            &StreamRequest {
                pane,
                command: StreamCommand::PasteData {
                    text: text[start..end].to_owned(),
                },
            },
        )?;
        start = end;
    }
    transport::command(
        writer,
        &StreamRequest {
            pane,
            command: StreamCommand::PasteEnd,
        },
    )
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

pub struct SessionManager {
    endpoint: ipc::Endpoint,
    daemon: crate::model::DaemonId,
    connection: Rc<Connection>,
    receiver: incoming::Receiver,
    sessions: HashMap<u64, SessionHandle>,
    metadata: HashMap<u64, Metadata>,
    metadata_revision: u64,
    git: HashMap<u64, transport::GitMetadata>,
    git_panes: HashSet<u64>,
    git_subscription_pending: bool,
    visible: HashSet<u64>,
    subscription_pending: bool,
    pending_reads: HashMap<u64, u64>,
    wake: ipc::Wake,
    disconnected: bool,
    disconnect_error: Option<String>,
    state: Option<crate::control::CommandReply>,
}

impl SessionManager {
    /// Connect to an existing daemon without starting a local process or reading
    /// its execution files. Remote tunnels can supply their own endpoint.
    pub fn connect_at(mut endpoint: ipc::Endpoint, wake: ipc::Wake) -> anyhow::Result<Self> {
        let hello = endpoint.call(InternalRequest::Hello)?;
        anyhow::ensure!(
            hello["version"] == transport::VERSION,
            "Incompatible gmux daemon"
        );
        let daemon = serde_json::from_value(hello["daemon"].clone())?;
        endpoint.pin(daemon);
        let socket = endpoint.connect_stream()?;
        Self::from_stream(endpoint, wake, daemon, socket)
    }
    pub fn endpoint(&self) -> &ipc::Endpoint {
        &self.endpoint
    }
    pub fn from_stream(
        endpoint: ipc::Endpoint,
        wake: ipc::Wake,
        daemon: crate::model::DaemonId,
        socket: impl Into<ipc::Stream>,
    ) -> anyhow::Result<Self> {
        let mut socket = socket.into();
        Self::attach_stream(&mut socket, daemon)?;
        Self::from_attached_stream(endpoint, wake, daemon, socket)
    }
    pub(crate) fn attach_stream(
        socket: &mut ipc::Stream,
        daemon: crate::model::DaemonId,
    ) -> anyhow::Result<()> {
        transport::command(
            socket,
            &StreamRequest::connection(StreamCommand::Attach {
                version: transport::VERSION,
                daemon,
                remote: false,
            }),
        )
    }
    pub(crate) fn from_attached_stream(
        endpoint: ipc::Endpoint,
        wake: ipc::Wake,
        daemon: crate::model::DaemonId,
        socket: ipc::Stream,
    ) -> anyhow::Result<Self> {
        let mut writer = socket.try_clone()?;
        let reader = socket.try_clone()?;
        let (commands, queue) = mpsc::sync_channel::<Outgoing>(128);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let writer_bytes = pending_bytes.clone();
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        let errors = sender.clone();
        let writer_wake = wake.clone();
        std::thread::spawn(move || {
            while let Ok(outgoing) = queue.recv() {
                let bytes = outgoing.bytes();
                let result = outgoing.write(&mut writer);
                writer_bytes.fetch_sub(bytes, Ordering::Relaxed);
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
                    .and_then(|packet| {
                        anyhow::ensure!(
                            incoming::packet_size(&packet) <= incoming::MAX_BYTES,
                            "Viewport packet exceeds the frontend memory budget"
                        );
                        Ok(packet)
                    })
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
            endpoint,
            daemon,
            connection: Rc::new(Connection {
                commands,
                pending_bytes,
                socket,
            }),
            receiver,
            sessions: HashMap::new(),
            metadata: HashMap::new(),
            metadata_revision: 0,
            git: HashMap::new(),
            git_panes: HashSet::new(),
            git_subscription_pending: false,
            visible: HashSet::new(),
            subscription_pending: false,
            pending_reads: HashMap::new(),
            wake,
            disconnected: false,
            disconnect_error: None,
            state: None,
        })
    }
    pub fn set_endpoint(&mut self, endpoint: ipc::Endpoint) {
        self.endpoint = endpoint;
    }
    pub fn execution_state(&mut self) -> anyhow::Result<ExecutionState> {
        let reply: crate::control::CommandReply =
            serde_json::from_value(self.endpoint.call(InternalRequest::State)?)?;
        self.validate_state(&reply)?;
        self.install_metadata(&reply.metadata, reply.revision);
        Ok(reply.execution)
    }
    pub fn validate_state(&self, reply: &crate::control::CommandReply) -> anyhow::Result<()> {
        anyhow::ensure!(
            reply.daemon == self.daemon,
            "State belongs to a different daemon"
        );
        Ok(())
    }
    pub fn daemon_id(&self) -> crate::model::DaemonId {
        self.daemon
    }
    pub fn take_state(&mut self) -> Option<crate::control::CommandReply> {
        self.state.take()
    }
    pub fn set_focused(&self, active: bool) -> anyhow::Result<()> {
        self.connection
            .send(StreamRequest::connection(StreamCommand::Focused { active }))
    }
    pub fn install_metadata(&mut self, metadata: &[Metadata], revision: u64) {
        // Control replies can arrive before older queued stream snapshots.
        // Those snapshots must not remove metadata for newly created panes.
        if revision < self.metadata_revision {
            return;
        }
        self.metadata_revision = revision;
        self.metadata.clear();
        for meta in metadata {
            if let Some(session) = self.sessions.get(&meta.id) {
                session.borrow_mut().meta = meta.clone();
            } else {
                self.metadata.insert(meta.id, meta.clone());
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
    pub fn is_visible(&self, id: u64) -> bool {
        self.visible.contains(&id)
    }
    /// Request derived repository metadata independently of viewport visibility.
    pub fn set_git_panes(&mut self, panes: HashSet<u64>) -> anyhow::Result<()> {
        if self.git_panes != panes {
            self.git.retain(|id, _| panes.contains(id));
            for (&id, session) in &self.sessions {
                if !panes.contains(&id) {
                    session.borrow_mut().git = None;
                }
            }
            self.git_panes = panes;
            self.git_subscription_pending = true;
        }
        self.subscribe()
    }
    fn subscribe(&mut self) -> anyhow::Result<()> {
        if self.subscription_pending {
            self.connection
                .send(StreamRequest::connection(StreamCommand::Subscribe {
                    panes: self.visible.clone(),
                }))?;
            self.subscription_pending = false;
        }
        if self.git_subscription_pending {
            self.connection
                .send(StreamRequest::connection(StreamCommand::GitSubscribe {
                    panes: self.git_panes.clone(),
                }))?;
            self.git_subscription_pending = false;
        }
        Ok(())
    }
    pub fn mark_read(&mut self, id: u64) {
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let mut session = session.borrow_mut();
        let sequence = session.meta.unread_sequence;
        session.read_through = session.read_through.max(sequence);
        drop(session);
        self.pending_reads.insert(id, sequence);
        self.flush_reads();
    }
    fn flush_reads(&mut self) {
        self.pending_reads.retain(|id, sequence| {
            self.connection
                .send(StreamRequest::pane(
                    *id,
                    StreamCommand::MarkRead { through: *sequence },
                ))
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
            git: self.git.remove(&id),
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
            text_revision: 0,
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
                Ok(Ok(mut packet)) => {
                    if let Some(git) = packet.header.git.take() {
                        if self.git_panes.contains(&git.id) {
                            if let Some(session) = self.sessions.get(&git.id) {
                                let mut session = session.borrow_mut();
                                if session.git.as_ref() != Some(&git) {
                                    updates.entry(git.id).or_default().git_changed = true;
                                    session.git = Some(git);
                                }
                            } else {
                                self.git.insert(git.id, git);
                            }
                        }
                        continue;
                    }
                    if let Some(state) = packet.header.state.take() {
                        if let Err(error) = self.validate_state(&state) {
                            self.disconnected = true;
                            self.disconnect_error = Some(error.to_string());
                            for &id in self.sessions.keys() {
                                updates.entry(id).or_default().error = Some(error.to_string());
                            }
                            let _ = self.connection.socket.shutdown(std::net::Shutdown::Both);
                            break;
                        }
                        if state.revision >= self.metadata_revision {
                            self.state = Some(state);
                        }
                        continue;
                    }
                    let id = packet.header.meta.id;
                    if packet.header.meta.exited || !packet.header.notifications.is_empty() {
                        let update = updates.entry(id).or_default();
                        update.exited |= packet.header.meta.exited;
                        update.notifications.extend(packet.header.notifications);
                    }
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
                            let mut text_changed = (frame.cols, frame.rows)
                                != (session.frame.cols, session.frame.rows);
                            session
                                .cells
                                .resize(frame.cols as usize * frame.rows as usize, Cell::default());
                            let cols = frame.cols as usize;
                            for (index, row) in packet.header.rows.iter().enumerate() {
                                let previous = &mut session.cells
                                    [*row as usize * cols..(*row as usize + 1) * cols];
                                let next = &packet.cells[index * cols..(index + 1) * cols];
                                // Accessibility only uses glyph bytes and the
                                // wide-cell continuation flag, not rendition.
                                text_changed = text_changed
                                    || previous.iter().zip(next).any(|(a, b)| !a.text_eq(b));
                                previous.copy_from_slice(next);
                            }
                            session.frame = frame;
                            session.scroll_acknowledged = packet.header.scroll_serial;
                            if !packet.header.rows.is_empty() {
                                session.revision = session.revision.wrapping_add(1);
                            }
                            if text_changed {
                                session.text_revision = session.text_revision.wrapping_add(1);
                            }
                            update.changed = true;
                        }
                        if packet.header.copy.is_some() {
                            update.copy = packet.header.copy;
                        }
                        if packet.header.error.is_some() {
                            update.error = packet.header.error;
                        }
                    }
                }
                Ok(Err(error)) => {
                    self.disconnect_error = Some(error);
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
    pub fn disconnect_reason(&self) -> Option<&str> {
        self.disconnect_error.as_deref()
    }
    pub fn close(&mut self, id: u64) {
        self.sessions.remove(&id);
        self.metadata.remove(&id);
        self.git.remove(&id);
        self.git_subscription_pending |= self.git_panes.remove(&id);
        self.pending_reads.remove(&id);
    }
    pub fn clear(&mut self) {
        for session in self.sessions.values() {
            session.borrow_mut().git = None;
        }
        self.git.clear();
        self.git_panes.clear();
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
    git: Option<transport::GitMetadata>,
    frame: Frame,
    cells: Vec<Cell>,
    revision: u64,
    text_revision: u64,
    read_through: u64,
    scroll_serial: ValueCell<u64>,
    scroll_acknowledged: u64,
    pending_scroll: ValueCell<Option<u64>>,
    find: crate::search::FindState,
    selection: Option<(u64, u64)>,
}
impl RemoteSession {
    pub fn branch(&self) -> &str {
        self.git
            .as_ref()
            .filter(|git| git.directory == self.meta.directory)
            .map_or("", |git| git.branch.as_str())
    }
    pub fn text(&self) -> String {
        crate::cells::viewport_text(self.frame, &self.cells)
    }
    fn command(&self, command: StreamCommand) -> anyhow::Result<()> {
        self.connection
            .send(StreamRequest::pane(self.id(), command))
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
        self.meta.unread_sequence > self.read_through
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
        self.command(StreamCommand::Input {
            bytes: bytes.to_vec(),
        })
    }
    pub fn send_text(&self, text: &str, enter: bool) -> anyhow::Result<()> {
        self.command(StreamCommand::Paste {
            text: text.to_owned(),
            options: PasteOptions {
                enter,
                ..PasteOptions::default()
            },
        })
    }
    pub fn paste(&self, text: &str) -> anyhow::Result<()> {
        self.command(StreamCommand::Paste {
            text: text.to_owned(),
            options: PasteOptions {
                clipboard: true,
                bottom: true,
                ..PasteOptions::default()
            },
        })
    }
    pub fn key(
        &self,
        name: &str,
        cp: u32,
        mods: u16,
        release: bool,
        text: &str,
    ) -> anyhow::Result<()> {
        self.command(StreamCommand::Key {
            name: name.to_owned(),
            cp,
            mods,
            release,
            text: text.to_owned(),
        })
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
        self.command(StreamCommand::Mouse {
            position,
            metrics,
            action,
            button,
            mods,
            pressed,
        })
    }
    pub fn find(&self, query: &str) -> anyhow::Result<()> {
        self.command(StreamCommand::Find {
            query: query.to_owned(),
        })
    }
    pub fn find_advance(&self, previous: bool) -> anyhow::Result<()> {
        self.command(StreamCommand::FindAdvance { previous })
    }
    pub fn find_state(&self) -> &crate::search::FindState {
        &self.find
    }
    pub fn select(&self, start: (u16, u32), end: (u16, u32)) -> anyhow::Result<()> {
        self.command(StreamCommand::Select { start, end })
    }
    pub fn copy(&self) -> anyhow::Result<()> {
        self.command(StreamCommand::Copy)
    }
    pub fn clear_selection(&self) -> anyhow::Result<()> {
        self.command(StreamCommand::ClearSelection)
    }
    pub fn clear_screen(&self) -> anyhow::Result<()> {
        self.command(StreamCommand::ClearScreen)
    }
    pub fn selection(&self) -> Option<(u64, u64)> {
        self.selection
    }
    pub fn scroll(&self, delta: isize, bottom: bool) {
        if let Err(e) = self.command(StreamCommand::Scroll { delta, bottom }) {
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
                .command(StreamCommand::ScrollTo {
                    row,
                    serial: self.scroll_serial.get(),
                })
                .is_ok()
        {
            self.pending_scroll.set(None);
        }
    }
    pub fn scroll_pending(&self) -> bool {
        self.scroll_acknowledged < self.scroll_serial.get()
    }
    pub fn set_cursor_style(&self, style: CursorStyle) -> anyhow::Result<()> {
        self.command(StreamCommand::Style { style })
    }
    pub fn set_theme(&self, theme: &Theme) -> anyhow::Result<()> {
        self.command(StreamCommand::Theme { theme: *theme })
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
        self.command(StreamCommand::Resize {
            cols,
            rows,
            cw,
            ch,
            width,
            height,
        })?;
        Ok(())
    }
    pub fn snapshot(&self) -> ScreenSnapshot<'_> {
        ScreenSnapshot {
            frame: self.frame,
            cells: &self.cells,
            revision: self.revision,
            text_revision: self.text_revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    fn request(value: Value) -> StreamRequest {
        serde_json::from_value(value).unwrap()
    }
    fn wire(outgoing: Outgoing) -> Value {
        let mut bytes = Vec::new();
        outgoing.write(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    #[test]
    fn large_pastes_use_bounded_utf8_chunks_and_preserve_operation_options() {
        let text = "a界\n\0".repeat(transport::MAX_PASTE / 6);
        let mut bytes = Vec::new();
        Outgoing::prepare(request(json!({"method":"paste", "pane":7, "text":text, "clipboard":true, "enter":true, "bottom":true}))).unwrap().write(&mut bytes).unwrap();
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
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        };
        let paste = || request(json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)}));
        connection.send(paste()).unwrap();
        connection.send(paste()).unwrap();
        assert!(connection.send(paste()).is_err());
        assert_eq!(
            connection.pending_bytes.load(Ordering::Relaxed),
            2 * transport::MAX_PASTE
        );
        assert!(
            connection
                .send(request(
                    json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE + 1)})
                ))
                .is_err()
        );
        assert!(
            connection
                .send(request(json!({"method":"paste","text":""})))
                .is_err()
        );
        assert_eq!(
            connection.pending_bytes.load(Ordering::Relaxed),
            2 * transport::MAX_PASTE
        );
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn oversized_commands_fail_before_enqueue_and_leave_later_input_usable() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(2);
        let connection = Connection {
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        };
        for command in [
            json!({"method":"find","query":"\u{1}".repeat(200_000)}),
            json!({"method":"input","bytes":vec![255u8; 300_000]}),
        ] {
            assert!(
                connection
                    .send(request(command))
                    .unwrap_err()
                    .to_string()
                    .contains("too large")
            );
            assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), 0);
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
        connection
            .send(request(json!({"method":"input","bytes":[97]})))
            .unwrap();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"input","bytes":[97]})
        );
    }

    #[test]
    fn ordinary_commands_share_the_outgoing_byte_budget_with_pastes() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(16);
        let connection = Connection {
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        };
        connection
            .send(request(
                json!({"method":"paste","text":"x".repeat(transport::MAX_PASTE)}),
            ))
            .unwrap();
        let input = || request(json!({"method":"input","bytes":vec![255u8; 250_000]}));
        for _ in 0..4 {
            connection.send(input()).unwrap();
        }
        let before = connection.pending_bytes.load(Ordering::Relaxed);
        assert!(before <= 2 * transport::MAX_PASTE);
        assert!(connection.send(input()).is_err());
        assert_eq!(connection.pending_bytes.load(Ordering::Relaxed), before);
        assert_eq!(receiver.try_iter().count(), 5);
    }
    use crate::transport::Header;
    use std::path::PathBuf;

    fn manager() -> (SessionManager, incoming::Sender, SessionHandle) {
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, _) = mpsc::sync_channel(1);
        let connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        let session = Rc::new(RefCell::new(RemoteSession {
            connection: connection.clone(),
            git: None,
            meta: Metadata {
                id: 1,
                directory: PathBuf::from("/tmp"),
                title: "shell".into(),
                size: (2, 2),
                exited: false,
                unread_sequence: 0,
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
            text_revision: 0,
            read_through: 0,
            scroll_serial: ValueCell::new(0),
            scroll_acknowledged: 0,
            pending_scroll: ValueCell::new(None),
            find: crate::search::FindState::default(),
            selection: None,
        }));
        (
            SessionManager {
                endpoint: ipc::Endpoint::local(),
                daemon: crate::model::DaemonId::generate().unwrap(),
                connection,
                receiver,
                sessions: HashMap::from([(1, session.clone())]),
                metadata: HashMap::new(),
                metadata_revision: 0,
                git: HashMap::new(),
                git_panes: HashSet::new(),
                git_subscription_pending: false,
                visible: HashSet::new(),
                subscription_pending: false,
                pending_reads: HashMap::new(),
                wake: ipc::Wake::new().unwrap(),
                disconnected: false,
                disconnect_error: None,
                state: None,
            },
            sender,
            session,
        )
    }

    #[test]
    fn older_stream_state_cannot_replace_metadata_from_a_completed_command() {
        let (mut manager, sender, handle) = manager();
        let mut current = handle.borrow().meta.clone();
        current.title = "newer state".into();
        let mut added = current.clone();
        added.id = 2;
        manager.install_metadata(&[current.clone(), added], 2);
        current.title = "older stream state".into();
        manager.install_metadata(&[current.clone()], 1);
        let state = serde_json::from_value(serde_json::json!({
            "daemon":manager.daemon,"home_directory":"/tmp","machine_kind":"unknown","hostname":"host", "revision":1,
            "result":{"ok":true},"execution":{"last_id":1,"tabs":[],"scrollback":{}},"metadata":[current]
        })).unwrap();
        sender
            .send(Ok(Packet {
                header: Header {
                    state: Some(state),
                    ..Header::default()
                },
                cells: Vec::new(),
            }))
            .unwrap();
        manager.poll();
        assert!(
            manager.take_state().is_none(),
            "An older stream snapshot was published"
        );
        assert_eq!(handle.borrow().meta.title, "newer state");
        assert!(
            manager.attach(2, &Theme::adwaita(true)).is_ok(),
            "A new pane's metadata was lost"
        );
    }

    #[test]
    fn git_metadata_is_connection_scoped_ignores_old_directories_and_does_not_repaint() {
        let (mut local, local_sender, local_session) = manager();
        let (mut remote, remote_sender, remote_session) = manager();
        let send = |sender: &incoming::Sender, directory: &str, branch: &str| {
            sender
                .send(Ok(Packet {
                    header: Header {
                        git: Some(transport::GitMetadata {
                            id: 1,
                            directory: directory.into(),
                            branch: branch.into(),
                        }),
                        ..Header::default()
                    },
                    cells: Vec::new(),
                }))
                .unwrap();
        };
        for manager in [&mut local, &mut remote] {
            manager.git_panes.insert(1);
        }
        local_session.borrow_mut().selection = Some((1, 3));
        send(&local_sender, "/tmp", "local-main");
        send(&remote_sender, "/tmp", "remote-main");
        let updates = local.poll();
        assert!(updates[&1].git_changed);
        assert!(!updates[&1].changed && !updates[&1].directory_changed);
        assert_eq!(local_session.borrow().selection, Some((1, 3)));
        remote.poll();
        assert_eq!(local_session.borrow().branch(), "local-main");
        assert_eq!(remote_session.borrow().branch(), "remote-main");
        assert_eq!(local_session.borrow().revision, 0);
        assert_eq!(local_session.borrow().text_revision, 0);

        // A delayed completion from the prior CWD cannot label the current pane.
        remote_session.borrow_mut().meta.directory = "/tmp/other".into();
        assert_eq!(remote_session.borrow().branch(), "");
        send(&remote_sender, "/tmp", "old-directory");
        remote.poll();
        assert_eq!(remote_session.borrow().branch(), "");
        send(&remote_sender, "/tmp/other", "current-directory");
        remote.poll();
        assert_eq!(remote_session.borrow().branch(), "current-directory");
        let _ = remote.set_git_panes(HashSet::new());
        send(&remote_sender, "/tmp/other", "late-unsubscribed");
        assert!(remote.poll().is_empty());
        assert_eq!(remote_session.borrow().branch(), "");
        assert_eq!(local_session.borrow().branch(), "local-main");
        local.clear();
        assert_eq!(local_session.borrow().branch(), "");
    }

    #[test]
    fn git_subscriptions_retry_after_command_backpressure() {
        let (mut manager, _, _) = manager();
        let (commands, receiver) = mpsc::sync_channel(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        manager.connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager
            .connection
            .send(StreamRequest::connection(StreamCommand::Focused {
                active: true,
            }))
            .unwrap();
        assert!(manager.set_git_panes(HashSet::from([1])).is_err());
        assert!(manager.git_subscription_pending);
        let _ = receiver.try_recv().unwrap();
        manager.poll();
        assert!(!manager.git_subscription_pending);
        let mut bytes = Vec::new();
        receiver.try_recv().unwrap().write(&mut bytes).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"method":"git-subscribe","panes":[1]})
        );
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
                    git: None,
                    state: None,
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
    fn state_from_another_daemon_disconnects_without_replacing_the_local_view() {
        let (mut manager, sender, session) = manager();
        let packet = |daemon| Packet {
            header: Header {
                git: None,
                state: Some(crate::control::CommandReply {
                    home_directory: "/home/test".into(),
                    machine_kind: crate::model::MachineKind::Unknown,
                    hostname: "test-host".into(),
                    daemon,
                    revision: 1,
                    result: crate::control::CommandOutput::Done { ok: true }.into(),
                    execution: ExecutionState::default(),
                    metadata: vec![],
                }),
                meta: session.borrow().meta.clone(),
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
        };
        sender.send(Ok(packet(manager.daemon))).unwrap();
        assert!(manager.poll().is_empty());
        assert_eq!(manager.take_state().unwrap().daemon, manager.daemon);
        sender
            .send(Ok(packet(crate::model::DaemonId::generate().unwrap())))
            .unwrap();
        let updates = manager.poll();
        assert!(manager.disconnected);
        assert!(manager.take_state().is_none());
        assert!(
            updates[&1]
                .error
                .as_ref()
                .unwrap()
                .contains("different daemon")
        );
    }

    #[test]
    fn exits_arrive_for_unattached_panes_and_survive_update_coalescing() {
        let (mut manager, sender, session) = manager();
        for (id, exited) in [(2, false), (2, true), (1, true), (1, false)] {
            let mut meta = session.borrow().meta.clone();
            meta.id = id;
            meta.exited = exited;
            sender
                .send(Ok(Packet {
                    header: Header {
                        git: None,
                        state: None,
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
        }
        let updates = manager.poll();
        assert_eq!(updates.len(), 2);
        assert!(updates[&1].exited);
        assert!(updates[&2].exited);
        assert!(!manager.sessions.contains_key(&2));
        assert!(!manager.metadata.contains_key(&2));
        assert!(manager.poll().is_empty());
    }

    #[test]
    fn unattached_notifications_arrive_in_order_without_empty_metadata_updates() {
        let (mut manager, sender, session) = manager();
        for (notifications, exited) in [
            (vec![], false),
            (vec!["first"], false),
            (vec!["second"], true),
        ] {
            let mut meta = session.borrow().meta.clone();
            meta.id = 2;
            meta.exited = exited;
            let empty = notifications.is_empty();
            sender
                .send(Ok(Packet {
                    header: Header {
                        git: None,
                        state: None,
                        meta,
                        frame: None,
                        scroll_serial: 0,
                        rows: vec![],
                        notifications: notifications.into_iter().map(str::to_owned).collect(),
                        error: None,
                        find: None,
                        selection: None,
                        copy: None,
                    },
                    cells: vec![],
                }))
                .unwrap();
            if empty {
                assert!(manager.poll().is_empty());
            }
        }
        let updates = manager.poll();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[&2].notifications, ["first", "second"]);
        assert!(updates[&2].exited);
        assert!(!manager.sessions.contains_key(&2));
        assert!(!manager.metadata.contains_key(&2));
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
                        git: None,
                        state: None,
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
        assert_eq!(screen.text_revision, 2);
        assert_eq!(screen.frame.cursor_y, 1);
        assert_eq!(
            screen.cells.iter().map(Cell::text).collect::<Vec<_>>(),
            ["a", "a", "b", "b"]
        );
    }

    fn send_frame(
        sender: &incoming::Sender,
        session: &SessionHandle,
        frame: Frame,
        rows: Vec<u16>,
        cells: Vec<Cell>,
        selection: Option<(u64, u64)>,
    ) {
        sender
            .send(Ok(Packet {
                header: Header {
                    git: None,
                    state: None,
                    meta: session.borrow().meta.clone(),
                    frame: Some(frame),
                    scroll_serial: 0,
                    rows,
                    notifications: vec![],
                    error: None,
                    find: None,
                    selection,
                    copy: None,
                },
                cells,
            }))
            .unwrap();
    }

    #[test]
    fn color_and_style_rows_preserve_text_revision_and_still_repaint() {
        let (mut manager, sender, session) = manager();
        let frame = session.borrow().frame;
        let mut cells = session.borrow().cells.clone();
        cells[0].text[..3].copy_from_slice("界".as_bytes());
        cells[1].flags = 64;
        cells[2].text[..3].copy_from_slice("e\u{301}".as_bytes());
        send_frame(&sender, &session, frame, vec![0, 1], cells.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 1);
        for cell in &mut cells {
            cell.fg = [1, 2, 3];
            cell.bg = [4, 5, 6];
            cell.flags |= 63;
        }
        send_frame(&sender, &session, frame, vec![0, 1], cells.clone(), None);
        assert!(manager.poll()[&1].changed);
        let session = session.borrow();
        let screen = session.snapshot();
        assert_eq!(screen.revision, 2);
        assert_eq!(screen.text_revision, 1);
        assert!(screen.cells == cells);
    }

    #[test]
    fn single_row_text_and_wide_layout_updates_invalidate_accessible_text() {
        let (mut manager, sender, session) = manager();
        let frame = session.borrow().frame;
        let mut row = vec![Cell::default(); 2];
        row[0].text[..3].copy_from_slice("e\u{301}".as_bytes());
        row[1].text[0] = b'X';
        send_frame(&sender, &session, frame, vec![1], row.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 1);
        assert!(session.borrow().snapshot().cells[..2] == [Cell::default(); 2]);
        // The same concatenated text can occupy different cells. Its caret and
        // geometry mappings must still be rebuilt.
        row[0].text = [0; 64];
        row[0].text[0] = b'e';
        row[1].text[..3].copy_from_slice("\u{301}X".as_bytes());
        send_frame(&sender, &session, frame, vec![1], row.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 2);
        row[1].flags = 64;
        send_frame(&sender, &session, frame, vec![1], row, None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 3);
    }

    #[test]
    fn geometry_and_positions_survive_frames_without_row_deltas() {
        let (mut manager, sender, session) = manager();
        let frame = Frame {
            cursor_x: 1,
            cursor_y: 1,
            ..session.borrow().frame
        };
        send_frame(&sender, &session, frame, vec![], vec![], Some((1, 3)));
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 0);
        assert_eq!(session.borrow().snapshot().frame.cursor_y, 1);
        assert_eq!(session.borrow().selection(), Some((1, 3)));
        let frame = Frame {
            cols: 4,
            rows: 1,
            ..frame
        };
        send_frame(&sender, &session, frame, vec![], vec![], None);
        assert!(manager.poll()[&1].changed);
        let session = session.borrow();
        let screen = session.snapshot();
        assert_eq!(screen.revision, 0);
        assert_eq!(screen.text_revision, 1);
        assert_eq!((screen.frame.cols, screen.frame.rows), (4, 1));
        assert_eq!(screen.cells.len(), 4);
    }
    #[test]
    fn scroll_requests_coalesce_under_backpressure_and_ignore_stale_acknowledgements() {
        let (mut manager, sender, session) = manager();
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        commands
            .send(Outgoing::prepare(request(json!({"method":"input","bytes":[]}))).unwrap())
            .unwrap();
        let connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        session.borrow().scroll_to(10);
        session.borrow().scroll_to(20);
        assert!(session.borrow().scroll_pending());
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
        manager.poll();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
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
                        git: None,
                        state: None,
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
            .send(Outgoing::prepare(request(json!({"method":"input","bytes":[]}))).unwrap())
            .unwrap();
        manager.connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        assert!(manager.set_visible(HashSet::from([1])).is_err());
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
        assert!(manager.poll().is_empty());
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"subscribe","panes":[1]})
        );
        assert!(manager.poll().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn acknowledging_an_alert_ignores_old_packets_and_keeps_new_alerts() {
        let (mut manager, sender, session) = manager();
        session.borrow_mut().meta.unread_sequence = 7;
        assert!(session.borrow().unread());
        manager.mark_read(1);
        assert!(!session.borrow().unread());
        for (sequence, expected) in [(7, false), (0, false), (8, true)] {
            let mut meta = session.borrow().meta.clone();
            meta.unread_sequence = sequence;
            sender
                .send(Ok(Packet {
                    header: Header {
                        git: None,
                        state: None,
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
            .send(Outgoing::prepare(request(json!({"method":"input","bytes":[]}))).unwrap())
            .unwrap();
        manager.connection = Rc::new(Connection {
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        session.borrow_mut().meta.unread_sequence = 7;
        manager.mark_read(1);
        assert!(!session.borrow().unread());
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
        manager.poll();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"mark-read","pane":1,"through":7})
        );
        manager.poll();
        assert!(receiver.try_recv().is_err());
    }
}
