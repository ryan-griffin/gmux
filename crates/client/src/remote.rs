//! Frontend handles contain cached viewports; terminal engines live in the daemon.
use crate::{incoming, ipc};
use gmux_protocol::{
    Cell, Frame,
    control::InternalRequest,
    model::{CursorStyle, ExecutionState},
    selection::Selection,
    session::{ScreenSnapshot, SessionUpdate},
    theme::Theme,
    transport::{
        self, KeyAction, KeyLayout, Metadata, Packet, PasteOptions, StreamCommand, StreamRequest,
    },
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
    time::{Duration, Instant},
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for tests that do not report a display interval.
#[cfg(test)]
const SCROLL_CADENCE: Duration = transport::DEFAULT_FRAME_INTERVAL;
const CACHED_SCROLL_SETTLE: Duration = Duration::from_millis(100);
const PING: &[u8] = b"{\"method\":\"ping\"}\n";
static NO_HYPERLINKS: gmux_protocol::hyperlinks::Hyperlinks =
    gmux_protocol::hyperlinks::Hyperlinks {
        targets: Vec::new(),
        spans: Vec::new(),
    };

struct Heartbeat {
    interval: Duration,
    next: Instant,
}
impl Heartbeat {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Instant::now() + interval,
        }
    }
    fn remaining(&self) -> Duration {
        self.next.saturating_duration_since(Instant::now())
    }
    fn check(&mut self, writer: &mut impl std::io::Write) -> anyhow::Result<()> {
        let now = Instant::now();
        if now >= self.next {
            writer.write_all(PING)?;
            self.next = now + self.interval;
        }
        Ok(())
    }
}

fn stream_error(error: anyhow::Error) -> String {
    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    }) {
        "Terminal daemon is not responding".into()
    } else {
        format!("Lost connection to the terminal daemon: {error}")
    }
}

pub type SessionHandle = Rc<RefCell<RemoteSession>>;

struct Connection {
    commands: mpsc::SyncSender<Outgoing>,
    pending_bytes: Arc<AtomicUsize>,
    socket: ipc::Stream,
    clipboard_epochs: [ValueCell<u64>; 2],
}
enum Outgoing {
    Close,
    ClipboardReply {
        pane: u64,
        result: gmux_protocol::clipboard::ReadResult,
        contents: Vec<gmux_protocol::clipboard::Representation>,
    },
    Command(Vec<u8>),
    Paste {
        pane: Option<u64>,
        text: String,
        options: PasteOptions,
        user: Option<(u64, u64)>,
    },
}

impl Outgoing {
    fn prepare(request: StreamRequest) -> anyhow::Result<Self> {
        if let StreamCommand::UserPasteText {
            serial,
            epoch,
            text,
        } = request.command
        {
            anyhow::ensure!(text.len() <= transport::MAX_PASTE, "Paste is too large");
            Ok(Self::Paste {
                pane: request.pane,
                text,
                options: PasteOptions::default(),
                user: Some((serial, epoch)),
            })
        } else if let StreamCommand::Paste { text, options } = request.command {
            anyhow::ensure!(
                text.len() <= transport::MAX_PASTE,
                "Paste is too large. The limit is 4 MiB."
            );
            Ok(Self::Paste {
                pane: request.pane,
                text,
                options,
                user: None,
            })
        } else {
            // Reject oversized commands before handing them to the writer. A
            // local validation error must not close the frontend connection.
            let mut encoded = transport::command_bytes(&request)?;
            if let StreamCommand::ClipboardReadData { size, bytes, .. } = request.command {
                anyhow::ensure!(
                    size == bytes.len() && size <= gmux_protocol::clipboard::READ_CHUNK,
                    "Invalid clipboard chunk"
                );
                encoded.extend_from_slice(&bytes);
            }
            Ok(Self::Command(encoded))
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Close => 0,
            Self::ClipboardReply { contents, .. } => {
                contents
                    .iter()
                    .map(|c| c.data.len() + c.mime.len())
                    .sum::<usize>()
                    + 16 * 1024
            }
            Self::Command(bytes) => bytes.len(),
            Self::Paste { text, .. } => text.len(),
        }
    }

    fn write(
        self,
        writer: &mut impl std::io::Write,
        heartbeat: &mut Heartbeat,
    ) -> anyhow::Result<()> {
        match self {
            Self::Close => return Ok(()),
            Self::Command(bytes) => writer.write_all(&bytes)?,
            Self::ClipboardReply {
                pane,
                result,
                contents,
            } => {
                let id = result.id;
                transport::command(
                    writer,
                    &StreamRequest::pane(pane, StreamCommand::ClipboardReadResult { result }),
                )?;
                for c in contents {
                    for bytes in c.data.chunks(gmux_protocol::clipboard::READ_CHUNK) {
                        transport::command(
                            writer,
                            &StreamRequest::pane(
                                pane,
                                StreamCommand::ClipboardReadData {
                                    id,
                                    size: bytes.len(),
                                    bytes: vec![],
                                },
                            ),
                        )?;
                        writer.write_all(bytes)?;
                        heartbeat.check(writer)?;
                    }
                }
                transport::command(
                    writer,
                    &StreamRequest::pane(pane, StreamCommand::ClipboardReadEnd { id }),
                )?;
            }
            Self::Paste {
                pane,
                text,
                options,
                user,
            } => write_paste(writer, pane, &text, options, user, heartbeat)?,
        }
        heartbeat.check(writer)
    }
}

impl Connection {
    fn clipboard_changed(
        &self,
        destination: gmux_protocol::clipboard::Destination,
        epoch: u64,
    ) -> anyhow::Result<()> {
        // Every pane in a window reports through this connection. Serialize a
        // generation once, and allow retries if the outgoing queue was full.
        let sent = &self.clipboard_epochs[destination as usize];
        if sent.get() == epoch {
            return Ok(());
        }
        self.send(StreamRequest::connection(StreamCommand::ClipboardChanged {
            destination,
            epoch,
        }))?;
        sent.set(epoch);
        Ok(())
    }
    fn close(&self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        // Cached viewports may retain this sender after their manager is gone.
        let _ = self.commands.try_send(Outgoing::Close);
    }
    fn send(&self, request: StreamRequest) -> anyhow::Result<()> {
        self.enqueue(Outgoing::prepare(request)?)
    }
    fn enqueue(&self, outgoing: Outgoing) -> anyhow::Result<()> {
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
    user: Option<(u64, u64)>,
    heartbeat: &mut Heartbeat,
) -> anyhow::Result<()> {
    transport::command(
        writer,
        &StreamRequest {
            pane,
            command: if let Some((serial, epoch)) = user {
                StreamCommand::UserPasteBegin {
                    serial,
                    epoch,
                    size: text.len(),
                }
            } else {
                StreamCommand::PasteBegin {
                    size: text.len(),
                    options,
                }
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
        heartbeat.check(writer)?;
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
        self.close();
    }
}

pub struct SessionManager {
    endpoint: ipc::Endpoint,
    daemon: gmux_protocol::model::DaemonId,
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
    frame_interval: Duration,
    frame_interval_sent: bool,
    clipboard_writes: bool,
    pending_reads: HashMap<u64, u64>,
    wake: gmux_platform::unix::ipc::Wake,
    disconnected: bool,
    disconnect_error: Option<anyhow::Error>,
    state: Option<gmux_protocol::control::CommandReply>,
}

impl SessionManager {
    /// Connect to an existing daemon without starting a local process or reading
    /// its execution files. Remote tunnels can supply their own endpoint.
    pub fn connect_at(
        mut endpoint: ipc::Endpoint,
        wake: gmux_platform::unix::ipc::Wake,
    ) -> anyhow::Result<Self> {
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
        wake: gmux_platform::unix::ipc::Wake,
        daemon: gmux_protocol::model::DaemonId,
        socket: impl Into<ipc::Stream>,
    ) -> anyhow::Result<Self> {
        let mut socket = socket.into();
        Self::attach_stream(&mut socket, daemon)?;
        Self::from_attached_stream(endpoint, wake, daemon, socket)
    }
    pub(crate) fn attach_stream(
        socket: &mut ipc::Stream,
        daemon: gmux_protocol::model::DaemonId,
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
        wake: gmux_platform::unix::ipc::Wake,
        daemon: gmux_protocol::model::DaemonId,
        socket: ipc::Stream,
    ) -> anyhow::Result<Self> {
        Self::with_heartbeat(
            endpoint,
            wake,
            daemon,
            socket,
            HEARTBEAT_INTERVAL,
            RESPONSE_TIMEOUT,
        )
    }
    fn with_heartbeat(
        endpoint: ipc::Endpoint,
        wake: gmux_platform::unix::ipc::Wake,
        daemon: gmux_protocol::model::DaemonId,
        socket: ipc::Stream,
        interval: Duration,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        let mut writer = socket.try_clone()?;
        let reader = socket.try_clone()?;
        let (commands, queue) = mpsc::sync_channel::<Outgoing>(128);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let writer_bytes = pending_bytes.clone();
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        let errors = sender.clone();
        let writer_wake = wake.clone();
        std::thread::spawn(move || {
            let mut heartbeat = Heartbeat::new(interval);
            loop {
                let (result, commands_changed) = match queue.recv_timeout(heartbeat.remaining()) {
                    Ok(Outgoing::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(outgoing) => {
                        let bytes = outgoing.bytes();
                        let result = outgoing.write(&mut writer, &mut heartbeat);
                        writer_bytes.fetch_sub(bytes, Ordering::Relaxed);
                        (result, true)
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => (heartbeat.check(&mut writer), false),
                };
                if let Err(error) = result {
                    let _ = errors.send(Err(stream_error(error)));
                    writer_wake.notify();
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    break;
                }
                // Retry state commands after queue backpressure without a timer.
                if commands_changed {
                    writer_wake.notify();
                }
            }
        });
        let reader_wake = wake.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let packet = match Packet::read_message(&mut reader) {
                    Ok(None) => continue,
                    Ok(Some(packet)) => Ok(packet),
                    Err(error) => Err(error),
                }
                .map_err(stream_error);
                let failed = packet.is_err();
                match sender.send(packet) {
                    Ok(()) => (),
                    Err(incoming::SendError::TooLarge) => {
                        // Admission accounts retained storage once. Keep its
                        // rejection ordered after packets already in the queue.
                        if sender
                            .send(Err("Lost connection to the terminal daemon: Viewport packet exceeds the frontend memory budget".into()))
                            .is_ok()
                        {
                            let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
                            reader_wake.notify();
                        }
                        break;
                    }
                    Err(incoming::SendError::Closed) => break,
                }
                if failed {
                    // Queue the cause before shutdown can make the writer fail too.
                    let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
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
                clipboard_epochs: Default::default(),
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
            frame_interval: transport::DEFAULT_FRAME_INTERVAL,
            frame_interval_sent: true,
            clipboard_writes: false,
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
        let reply: gmux_protocol::control::CommandReply =
            serde_json::from_value(self.endpoint.call(InternalRequest::State)?)?;
        self.validate_state(&reply)?;
        self.install_metadata(&reply.metadata, reply.revision);
        Ok(reply.execution)
    }
    pub fn validate_state(
        &self,
        reply: &gmux_protocol::control::CommandReply,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            reply.daemon == self.daemon,
            crate::network::ConnectionRejected("State belongs to a different daemon".into())
        );
        Ok(())
    }
    pub fn daemon_id(&self) -> gmux_protocol::model::DaemonId {
        self.daemon
    }
    pub fn take_state(&mut self) -> Option<gmux_protocol::control::CommandReply> {
        self.state.take()
    }
    pub fn set_focused(&self, active: bool) -> anyhow::Result<()> {
        self.connection
            .send(StreamRequest::connection(StreamCommand::Focused { active }))
    }
    /// Advertise that this frontend can consume unsolicited clipboard writes.
    pub fn set_clipboard_writes(&mut self, enabled: bool) -> anyhow::Result<()> {
        if self.clipboard_writes == enabled {
            return Ok(());
        }
        self.connection
            .send(StreamRequest::connection(StreamCommand::ClipboardWrites {
                enabled,
            }))?;
        self.clipboard_writes = enabled;
        Ok(())
    }
    pub fn set_clipboard_reads(&self, enabled: bool) -> anyhow::Result<()> {
        self.connection
            .send(StreamRequest::connection(StreamCommand::ClipboardReads {
                enabled,
            }))
    }
    pub fn set_frame_interval(&mut self, interval: Duration) {
        let interval = interval.clamp(Duration::from_millis(1), Duration::from_secs(1));
        if self.frame_interval == interval {
            return;
        }
        self.frame_interval = interval;
        self.frame_interval_sent = false;
        for session in self.sessions.values() {
            session.borrow_mut().scroll_cadence = interval;
        }
        let _ = self.subscribe();
        self.wake.notify();
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
                let mut session = session.borrow_mut();
                // A separate control reply can predate queued progress reports.
                // Once attached, the ordered stream owns live progress delivery.
                let progress = session.meta.progress;
                let program_status = session.meta.program_status.take();
                session.meta = meta.clone();
                session.meta.progress = progress;
                session.meta.program_status = program_status;
            } else {
                self.metadata.insert(meta.id, meta.clone());
            }
        }
    }
    pub fn set_visible(&mut self, ids: HashSet<u64>) -> anyhow::Result<()> {
        if self.visible != ids {
            for (id, session) in &self.sessions {
                if !ids.contains(id) {
                    session.borrow_mut().clear_cache();
                }
            }
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
        if !self.frame_interval_sent {
            self.connection
                .send(StreamRequest::connection(StreamCommand::FrameInterval {
                    micros: self.frame_interval.as_micros() as u32,
                }))?;
            self.frame_interval_sent = true;
        }
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
            scroll_sent: ValueCell::new(0),
            scroll_sent_at: ValueCell::new(Instant::now()),
            scroll_acknowledged: 0,
            scroll_cadence: self.frame_interval,
            pending_scroll: ValueCell::new(None),
            find: gmux_protocol::search::FindState::default(),
            selection: None,
            selection_serial: ValueCell::new(0),
            selection_acknowledged: 0,
            selection_origin: None,
            row_flags: vec![0; 24],
            hyperlinks: Default::default(),
            hyperlink_revision: 0,
            cache: gmux_protocol::scroll_cache::RowCache::default(),
            prediction: None,
            scroll_target: None,
            wake: self.wake.clone(),
            scroll_defer_until: ValueCell::new(None),
            scroll_retry_pending: ValueCell::new(false),
        }));
        self.sessions.insert(id, handle.clone());
        Ok(handle)
    }
    pub fn poll(&mut self) -> HashMap<u64, SessionUpdate> {
        self.poll_with_clipboard(|_| {})
    }
    /// Deliver clipboard side effects in wire order, across panes and explicit
    /// copy requests. They are never coalesced into viewport state or replayed.
    pub fn poll_with_clipboard(
        &mut self,
        mut clipboard: impl FnMut(gmux_protocol::clipboard::Write),
    ) -> HashMap<u64, SessionUpdate> {
        self.poll_with_clipboard_events(|event| {
            if let gmux_protocol::clipboard::Event::Write(write) = event {
                clipboard(write);
            }
        })
    }
    pub fn poll_with_clipboard_events(
        &mut self,
        mut clipboard: impl FnMut(gmux_protocol::clipboard::Event),
    ) -> HashMap<u64, SessionUpdate> {
        // A full command queue must not lose a tab's viewport subscription.
        let _ = self.subscribe();
        self.flush_reads();
        for session in self.sessions.values() {
            let session = session.borrow();
            // Writer progress wakes the manager. Until then, a full queue
            // must not keep an expired deadline spinning the UI event loop.
            session.scroll_retry_pending.set(false);
            session.flush_scroll();
        }
        let mut updates: HashMap<u64, SessionUpdate> = HashMap::new();
        for _ in 0..256 {
            match self.receiver.try_recv() {
                Ok(Ok(mut packet)) => {
                    if let Some(write) = packet.header.copy.take() {
                        clipboard(gmux_protocol::clipboard::Event::Write(write));
                        continue;
                    }
                    if let Some(event) = packet.header.clipboard_read.take() {
                        clipboard(gmux_protocol::clipboard::Event::Read {
                            pane: packet.header.meta.id,
                            event,
                        });
                        continue;
                    }
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
                            for &id in self.sessions.keys() {
                                updates.entry(id).or_default().error = Some(error.to_string());
                            }
                            self.disconnect_error = Some(error);
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
                        update.progress_changed |=
                            session.meta.progress != packet.header.meta.progress;
                        update.program_status_changed |=
                            session.meta.program_status != packet.header.meta.program_status;
                        session.meta = packet.header.meta;
                        update.unread = Some(session.unread());
                        let invalidated = packet.header.generation != 0
                            && session.cache.invalidate(packet.header.generation);
                        if invalidated {
                            session.scroll_defer_until.set(None);
                            if session.prediction.take().is_some() {
                                session.revision = session.revision.wrapping_add(1);
                                session.text_revision = session.text_revision.wrapping_add(1);
                                update.changed = true;
                            }
                        }
                        let removed = if packet.header.generation != 0 {
                            session.cache.rebase(packet.header.cache_origin)
                        } else {
                            0
                        };
                        if !invalidated && removed != 0 {
                            if let Some(prediction) = &mut session.prediction {
                                if prediction.frame.scrollback.offset < removed {
                                    session.prediction = None;
                                } else {
                                    prediction.frame.scrollback.offset -= removed;
                                    prediction.frame.scrollback.total =
                                        prediction.frame.scrollback.total.saturating_sub(removed);
                                }
                                session.revision = session.revision.wrapping_add(1);
                                session.text_revision = session.text_revision.wrapping_add(1);
                                update.changed = true;
                            }
                            if let Some(target) = session.scroll_target {
                                let target = target.saturating_sub(removed);
                                session.scroll_target = Some(target);
                                session.pending_scroll.set(Some(target));
                                session.scroll_serial.set(session.scroll_serial.get() + 1);
                                session.scroll_defer_until.set(None);
                            }
                        }
                        if let Some(end) = packet.header.cache_valid_before {
                            session.cache.truncate(end);
                            if session
                                .scroll_target
                                .is_some_and(|row| !session.cache.has_margin(session.frame, row))
                            {
                                session.scroll_defer_until.set(None);
                            }
                            if let Some(prediction) = &session.prediction {
                                let start = prediction.frame.scrollback.offset;
                                if start + prediction.frame.rows as u64 > end {
                                    session.prediction = None;
                                    session.revision = session.revision.wrapping_add(1);
                                    session.text_revision = session.text_revision.wrapping_add(1);
                                    update.changed = true;
                                }
                            }
                        }
                        if let Some(find) = packet.header.find {
                            update.changed |= session.find != find;
                            if (find.total != 0 || find.pending)
                                && session.prediction.take().is_some()
                            {
                                session.scroll_defer_until.set(None);
                                session.revision = session.revision.wrapping_add(1);
                                session.text_revision = session.text_revision.wrapping_add(1);
                                update.changed = true;
                            }
                            session.find = find;
                        }
                        // Preserve the local prediction until the daemon has
                        // processed the latest selection command, including clears.
                        if packet.header.selection_serial >= session.selection_serial.get() {
                            update.changed |= session.selection != packet.header.selection
                                || session.selection_acknowledged != packet.header.selection_serial;
                            session.selection = packet.header.selection;
                            session.selection_origin = packet.header.selection_origin;
                            session.selection_acknowledged = packet.header.selection_serial;
                        }
                        if let Some(frame) = packet.header.frame {
                            let dimensions_changed = (frame.cols, frame.rows)
                                != (session.frame.cols, session.frame.rows);
                            let mut text_changed =
                                dimensions_changed || packet.header.row_shift != 0;
                            session
                                .cells
                                .resize(frame.cols as usize * frame.rows as usize, Cell::default());
                            session.row_flags.resize(frame.rows as usize, 0);
                            let cols = frame.cols as usize;
                            gmux_protocol::scroll_cache::shift(
                                &mut session.cells,
                                cols,
                                packet.header.row_shift,
                            );
                            gmux_protocol::scroll_cache::shift(
                                &mut session.row_flags,
                                1,
                                packet.header.row_shift,
                            );
                            for (index, row) in packet.header.rows.iter().enumerate() {
                                let flags =
                                    packet.header.row_flags.get(index).copied().unwrap_or(0);
                                text_changed |= session.row_flags[*row as usize] != flags;
                                session.row_flags[*row as usize] = flags;
                                let previous = &mut session.cells
                                    [*row as usize * cols..(*row as usize + 1) * cols];
                                let next = &packet.cells[index * cols..(index + 1) * cols];
                                // Text consumers use glyph bytes and wide-cell
                                // layout, not rendition.
                                text_changed = text_changed
                                    || previous.iter().zip(next).any(|(a, b)| !a.text_eq(b));
                                previous.clone_from_slice(next);
                            }
                            if packet.header.hyperlinks.is_some() || dimensions_changed {
                                session.hyperlink_revision =
                                    session.hyperlink_revision.wrapping_add(1);
                            }
                            if let Some(links) = packet.header.hyperlinks {
                                session.hyperlinks = links;
                            } else if dimensions_changed {
                                session.hyperlinks = Default::default();
                            }
                            session.frame = frame;
                            if self.visible.contains(&id)
                                && (packet.header.cache.is_some() || !session.cache.is_empty())
                            {
                                if session.cache.configure(packet.header.generation, frame) {
                                    session.scroll_defer_until.set(None);
                                    if session.prediction.take().is_some() {
                                        text_changed = true;
                                    }
                                }
                                if let Some(cache) = packet.header.cache {
                                    session.cache.insert(
                                        cache.start,
                                        &packet.cells[packet.header.rows.len() * cols..],
                                        &cache.row_flags,
                                    );
                                }
                                // Include unchanged visible rows too. Supplemental
                                // history never substitutes for authoritative deltas.
                                let RemoteSession {
                                    cache,
                                    cells,
                                    row_flags,
                                    ..
                                } = &mut *session;
                                cache.insert(frame.scrollback.offset, cells, row_flags);
                            }
                            session.scroll_acknowledged =
                                session.scroll_acknowledged.max(packet.header.scroll_serial);
                            let mut prediction_changed = false;
                            if !session.scroll_pending() {
                                session.scroll_target = None;
                                if session.prediction.take().is_some() {
                                    text_changed = true;
                                    prediction_changed = true;
                                }
                            } else if let Some(target) = session.scroll_target {
                                prediction_changed = session.predict(target);
                                text_changed |= prediction_changed;
                            }
                            if !packet.header.rows.is_empty()
                                || packet.header.row_shift != 0
                                || prediction_changed
                            {
                                session.revision = session.revision.wrapping_add(1);
                            }
                            if text_changed {
                                session.text_revision = session.text_revision.wrapping_add(1);
                            }
                            update.changed = true;
                        }
                        session.flush_scroll();
                        if packet.header.error.is_some() {
                            update.error = packet.header.error;
                        }
                    }
                }
                Ok(Err(error)) => {
                    self.disconnect_error = Some(anyhow::Error::msg(error));
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
    /// Earliest release time among sessions with a coalesced scroll target,
    /// so the interface can wake and forward scrolling after a fixed cadence
    /// even when viewport replies are delayed.
    pub fn next_scroll(&self) -> Option<Instant> {
        self.sessions
            .values()
            .filter_map(|session| session.borrow().scroll_release())
            .min()
    }
    pub fn disconnect_error(&self) -> Option<&anyhow::Error> {
        self.disconnect_error.as_ref()
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
        self.connection.close();
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
    scroll_sent: ValueCell<u64>,
    scroll_sent_at: ValueCell<Instant>,
    scroll_acknowledged: u64,
    scroll_cadence: Duration,
    pending_scroll: ValueCell<Option<u64>>,
    find: gmux_protocol::search::FindState,
    selection: Option<Selection>,
    selection_serial: ValueCell<u64>,
    selection_acknowledged: u64,
    selection_origin: Option<u64>,
    row_flags: Vec<u8>,
    hyperlinks: gmux_protocol::hyperlinks::Hyperlinks,
    hyperlink_revision: u64,
    cache: gmux_protocol::scroll_cache::RowCache,
    prediction: Option<Prediction>,
    scroll_target: Option<u64>,
    wake: gmux_platform::unix::ipc::Wake,
    scroll_defer_until: ValueCell<Option<Instant>>,
    scroll_retry_pending: ValueCell<bool>,
}
struct Prediction {
    frame: Frame,
    cells: Vec<Cell>,
    row_flags: Vec<u8>,
}
impl RemoteSession {
    pub fn branch(&self) -> &str {
        self.git
            .as_ref()
            .filter(|git| git.directory == self.meta.directory)
            .map_or("", |git| git.branch.as_str())
    }
    pub fn text(&self) -> String {
        let screen = self.snapshot();
        gmux_protocol::cells::viewport_text(screen.frame, screen.cells)
    }
    fn command(&self, command: StreamCommand) -> anyhow::Result<()> {
        // Key releases and appearance updates do not move the viewport. Keep
        // coalescing through them; other commands follow the latest scroll.
        if !matches!(
            command,
            StreamCommand::Key {
                action: KeyAction::Release,
                ..
            } | StreamCommand::Theme { .. }
                | StreamCommand::Style { .. }
        ) {
            self.send_scroll()?;
        }
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
    pub fn progress(&self) -> gmux_protocol::progress::Progress {
        self.meta.progress
    }
    pub fn program_status(&self) -> Option<&gmux_protocol::program_status::Summary> {
        self.meta.program_status.as_ref()
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
    pub fn clipboard_changed(
        &self,
        destination: gmux_protocol::clipboard::Destination,
        epoch: u64,
    ) -> anyhow::Result<()> {
        self.connection.clipboard_changed(destination, epoch)
    }
    pub fn clipboard_command(&self, command: StreamCommand) -> anyhow::Result<()> {
        self.connection
            .send(StreamRequest::pane(self.id(), command))
    }
    pub fn clipboard_reply(
        &self,
        result: gmux_protocol::clipboard::ReadResult,
        contents: Vec<gmux_protocol::clipboard::Representation>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            result.byte_len()? == contents.iter().map(|c| c.data.len()).sum::<usize>()
                && result
                    .contents
                    .iter()
                    .zip(&contents)
                    .all(|(d, c)| d.mime == c.mime && d.len == c.data.len())
                && result.contents.len() == contents.len(),
            "Invalid clipboard reply payload"
        );
        self.connection.enqueue(Outgoing::ClipboardReply {
            pane: self.id(),
            result,
            contents,
        })
    }
    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.send_with_enter(bytes, false)
    }
    pub fn send_with_enter(&self, bytes: &[u8], enter: bool) -> anyhow::Result<()> {
        let mut input = Vec::with_capacity(bytes.len() + usize::from(enter));
        input.extend_from_slice(bytes);
        if enter {
            input.push(b'\r');
        }
        self.command(StreamCommand::Input { bytes: input })
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
                bottom: true,
                ..PasteOptions::default()
            },
        })
    }
    pub fn key(
        &self,
        name: &str,
        layout: KeyLayout,
        mods: u16,
        action: KeyAction,
        text: &str,
    ) -> anyhow::Result<()> {
        self.command(StreamCommand::Key {
            name: name.to_owned(),
            layout,
            mods,
            action,
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
    pub fn find_state(&self) -> &gmux_protocol::search::FindState {
        &self.find
    }
    pub fn select(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        mode: gmux_protocol::transport::SelectionMode,
    ) -> anyhow::Result<()> {
        let serial = self.selection_serial.get() + 1;
        self.command(StreamCommand::Select {
            start,
            end,
            mode,
            serial,
        })?;
        self.selection_serial.set(serial);
        Ok(())
    }
    pub fn copy(&self, formats: gmux_protocol::clipboard::Formats) -> anyhow::Result<()> {
        self.command(StreamCommand::Copy { formats })
    }
    pub fn command_state(&self) -> &gmux_protocol::shell::CommandState {
        &self.meta.command
    }
    pub fn select_output(
        &self,
        point: Option<(u16, u32)>,
        copy: Option<gmux_protocol::clipboard::Formats>,
    ) -> anyhow::Result<()> {
        self.select_output_target(point, false, copy)
    }
    pub fn anchor_output(&self, point: (u16, u32)) -> anyhow::Result<()> {
        self.command(StreamCommand::AnchorOutput { point })
    }
    pub fn select_anchored_output(
        &self,
        copy: Option<gmux_protocol::clipboard::Formats>,
    ) -> anyhow::Result<()> {
        self.select_output_target(None, true, copy)
    }
    fn select_output_target(
        &self,
        point: Option<(u16, u32)>,
        anchored: bool,
        copy: Option<gmux_protocol::clipboard::Formats>,
    ) -> anyhow::Result<()> {
        let serial = self.selection_serial.get() + 1;
        self.command(StreamCommand::SelectOutput {
            point,
            anchored,
            copy,
            serial,
        })?;
        self.selection_serial.set(serial);
        Ok(())
    }
    pub fn prompt(&mut self, previous: bool) -> anyhow::Result<()> {
        self.send_scroll()?;
        let serial = self.scroll_serial.get() + 1;
        self.command(StreamCommand::Prompt { previous, serial })?;
        self.scroll_serial.set(serial);
        self.scroll_sent.set(serial);
        self.scroll_target = None;
        self.prediction = None;
        Ok(())
    }
    pub fn clear_selection(&self) -> anyhow::Result<()> {
        let serial = self.selection_serial.get() + 1;
        self.command(StreamCommand::ClearSelection { serial })?;
        self.selection_serial.set(serial);
        Ok(())
    }
    pub fn selection_pending(&self) -> bool {
        self.selection_acknowledged < self.selection_serial.get()
    }
    pub fn selection_origin(&self) -> Option<u64> {
        (!self.selection_pending())
            .then_some(self.selection_origin)
            .flatten()
    }
    pub fn clear_screen(&mut self) -> anyhow::Result<()> {
        self.command(StreamCommand::ClearScreen)?;
        self.clear_cache();
        Ok(())
    }
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }
    pub fn scroll_to(&mut self, row: u64) {
        self.scroll_target = Some(row);
        let predicted = self.predict(row);
        if predicted {
            self.revision = self.revision.wrapping_add(1);
            self.text_revision = self.text_revision.wrapping_add(1);
        }
        self.scroll_serial.set(self.scroll_serial.get() + 1);
        self.pending_scroll.set(Some(row));
        // Returning to the live bottom needs the daemon's current cursor state,
        // which may be absent from a historical viewport. Skip the cache debounce.
        let at_bottom = row
            >= self
                .frame
                .scrollback
                .total
                .saturating_sub(self.frame.scrollback.len);
        self.scroll_defer_until.set(
            (predicted
                && !at_bottom
                && (self.cache.has_margin(self.frame, row)
                    || self.scroll_acknowledged < self.scroll_sent.get()))
            .then(|| Instant::now() + CACHED_SCROLL_SETTLE),
        );
        self.flush_scroll();
        // The preceding write wake may have already been consumed. Schedule
        // this target's deadline even if no further input or reply arrives.
        if self.pending_scroll.get().is_some() {
            self.wake.notify();
        }
    }
    fn predict(&mut self, row: u64) -> bool {
        // Search highlights are viewport-relative and remain authoritative.
        if self.find.total != 0 || self.find.pending {
            self.prediction = None;
            return false;
        }
        let row = row.min(
            self.frame
                .scrollback
                .total
                .saturating_sub(self.frame.scrollback.len),
        );
        let mut prediction = self.prediction.take().unwrap_or_else(|| Prediction {
            frame: self.frame,
            cells: Vec::new(),
            row_flags: Vec::new(),
        });
        if !self.cache.copy_view(
            row,
            self.frame.rows as usize,
            &mut prediction.cells,
            &mut prediction.row_flags,
        ) {
            // Keep a previously drawn prediction through older replies. An
            // uncached destination waits for authoritative rows.
            if !prediction.cells.is_empty() {
                self.prediction = Some(prediction);
            }
            return false;
        }
        prediction.frame = self.frame;
        let cursor_row = self.frame.scrollback.offset + self.frame.cursor_y as u64;
        prediction.frame.cursor_visible *=
            u8::from(cursor_row >= row && cursor_row < row + self.frame.rows as u64);
        prediction.frame.cursor_y = cursor_row.saturating_sub(row).min(u16::MAX as u64) as u16;
        prediction.frame.scrollback.offset = row;
        self.prediction = Some(prediction);
        true
    }
    fn clear_cache(&mut self) {
        self.cache.clear();
        self.prediction = None;
        self.scroll_defer_until.set(None);
    }
    fn flush_scroll(&self) {
        if self
            .scroll_defer_until
            .get()
            .is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        // A writable command socket does not imply a responsive viewport, so
        // the latest target coalesces while an earlier scroll is in flight.
        // Waiting for every acknowledgement paces scrolling at one update per
        // round-trip and freezes the viewport on remote links, so the coalesced
        // target releases after the scroll cadence instead.
        if self.scroll_acknowledged >= self.scroll_sent.get()
            || self.scroll_sent_at.get().elapsed() >= self.scroll_cadence
        {
            let _ = self.send_scroll();
        }
    }
    fn send_scroll(&self) -> anyhow::Result<()> {
        if let Some(row) = self.pending_scroll.get() {
            let sent = self.connection.send(StreamRequest::pane(
                self.id(),
                StreamCommand::ScrollTo {
                    row,
                    serial: self.scroll_serial.get(),
                },
            ));
            self.scroll_retry_pending.set(sent.is_err());
            sent?;
            self.scroll_sent.set(self.scroll_serial.get());
            self.scroll_sent_at.set(Instant::now());
            self.pending_scroll.set(None);
            self.scroll_defer_until.set(None);
        }
        Ok(())
    }
    /// When a coalesced scroll target would release without another event,
    /// unless a fresh acknowledgement clears the wait first.
    fn scroll_release(&self) -> Option<Instant> {
        self.pending_scroll.get()?;
        if self.scroll_retry_pending.get() {
            return None;
        }
        if let Some(at) = self.scroll_defer_until.get() {
            return Some(at);
        }
        if self.scroll_acknowledged >= self.scroll_sent.get() {
            return None;
        }
        Some(self.scroll_sent_at.get() + self.scroll_cadence)
    }
    pub fn scroll_pending(&self) -> bool {
        self.scroll_acknowledged < self.scroll_serial.get()
    }
    pub fn scroll_cache_bytes(&self) -> usize {
        self.cache.retained_bytes()
    }
    pub fn set_cursor_style(&self, style: CursorStyle, blink: bool) -> anyhow::Result<()> {
        self.command(StreamCommand::Style { style, blink })
    }
    pub fn set_theme(&mut self, theme: &Theme) -> anyhow::Result<()> {
        self.command(StreamCommand::Theme { theme: *theme })?;
        self.clear_cache();
        Ok(())
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
        if (cols, rows) != (self.frame.cols, self.frame.rows) {
            self.clear_cache();
        }
        Ok(())
    }
    pub fn snapshot(&self) -> ScreenSnapshot<'_> {
        // Cached rows carry no URI targets. Include prediction in the displayed
        // revision so every entry/exit refreshes hover, even without a new map.
        let hyperlink_revision = self.hyperlink_revision.wrapping_mul(2);
        if let Some(prediction) = &self.prediction {
            return ScreenSnapshot {
                frame: prediction.frame,
                cells: &prediction.cells,
                row_flags: &prediction.row_flags,
                hyperlinks: &NO_HYPERLINKS,
                hyperlink_revision: hyperlink_revision | 1,
                revision: self.revision,
                text_revision: self.text_revision,
            };
        }
        ScreenSnapshot {
            frame: self.frame,
            cells: &self.cells,
            row_flags: &self.row_flags,
            hyperlinks: &self.hyperlinks,
            hyperlink_revision,
            revision: self.revision,
            text_revision: self.text_revision,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_results_stream_as_one_reserved_operation_with_raw_chunks() {
        use gmux_protocol::clipboard::{ReadDescriptor, ReadResult, ReadStatus, Representation};
        use std::io::{BufRead, Read};
        let data = (0..=255).cycle().take(40_000).collect::<Vec<u8>>();
        let outgoing = Outgoing::ClipboardReply {
            pane: 7,
            result: ReadResult {
                id: 42,
                status: ReadStatus::Success,
                epoch: 2,
                available: vec![],
                contents: vec![ReadDescriptor {
                    mime: "application/octet-stream".into(),
                    len: data.len(),
                }],
            },
            contents: vec![Representation {
                mime: "application/octet-stream".into(),
                data: data.clone(),
            }],
        };
        assert!(outgoing.bytes() >= data.len());
        let mut encoded = Vec::new();
        outgoing
            .write(&mut encoded, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
            .unwrap();
        let mut reader = std::io::Cursor::new(encoded);
        let mut received = Vec::new();
        let mut methods = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let header: serde_json::Value = serde_json::from_str(&line).unwrap();
            methods.push(header["method"].as_str().unwrap().to_owned());
            if header["method"] == "clipboard-read-data" {
                let len = header["size"].as_u64().unwrap() as usize;
                assert!(len <= gmux_protocol::clipboard::READ_CHUNK);
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).unwrap();
                received.extend(bytes);
            }
        }
        assert_eq!(methods.first().unwrap(), "clipboard-read-result");
        assert_eq!(methods.last().unwrap(), "clipboard-read-end");
        assert_eq!(received, data);
    }

    fn layout(unshifted: u32) -> gmux_protocol::transport::KeyLayout {
        gmux_protocol::transport::KeyLayout {
            unshifted,
            ..Default::default()
        }
    }
    use super::*;
    use gmux_protocol::cells::{decoration, underline};
    use std::os::unix::net::UnixStream;

    #[test]
    fn heartbeat_replies_keep_idle_streams_alive_without_waking_the_ui() {
        use std::{
            io::{BufRead, Write},
            os::fd::AsRawFd,
        };
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        let mut manager = SessionManager::with_heartbeat(
            ipc::Endpoint::local(),
            wake.clone(),
            gmux_platform::unix::identity::daemon_id().unwrap(),
            socket.into(),
            Duration::from_millis(10),
            Duration::from_millis(300),
        )
        .unwrap();
        let cached = manager.connection.clone();
        let (ready, done) = mpsc::channel();
        let (release, hold) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            for _ in 0..40 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.as_bytes(), PING);
                peer.write_all(transport::HEARTBEAT_REPLY).unwrap();
            }
            ready.send(()).unwrap();
            let _ = hold.recv();
        });
        done.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(manager.poll().is_empty());
        assert!(!manager.disconnected());
        let mut event = libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut event, 1, 0) }, 0);
        drop(manager);
        let deadline = Instant::now() + Duration::from_secs(2);
        // The old viewport can retain its connection without retaining a worker.
        loop {
            if matches!(
                cached.commands.try_send(Outgoing::Close),
                Err(mpsc::TrySendError::Disconnected(_))
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Closing the manager retained its heartbeat worker"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn an_open_socket_without_daemon_replies_times_out() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut manager = SessionManager::with_heartbeat(
            ipc::Endpoint::local(),
            gmux_platform::unix::ipc::Wake::new().unwrap(),
            gmux_platform::unix::identity::daemon_id().unwrap(),
            socket.into(),
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !manager.disconnected() {
            manager.poll();
            assert!(
                Instant::now() < deadline,
                "An unresponsive daemon stayed connected"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            manager
                .disconnect_error()
                .unwrap()
                .to_string()
                .contains("not responding")
        );
    }

    #[test]
    fn heartbeat_probes_can_be_interleaved_with_paste_chunks() {
        let text = "a界".repeat(transport::PASTE_CHUNK);
        let mut bytes = Vec::new();
        let mut heartbeat = Heartbeat::new(Duration::ZERO);
        Outgoing::prepare(request(json!({"method":"paste","pane":7,"text":text})))
            .unwrap()
            .write(&mut bytes, &mut heartbeat)
            .unwrap();
        let commands: Vec<Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(commands.iter().any(|command| command["method"] == "ping"));
        let paste: Vec<_> = commands
            .iter()
            .filter(|command| command["method"] != "ping")
            .collect();
        assert_eq!(paste.first().unwrap()["method"], "paste-begin");
        assert_eq!(paste.last().unwrap()["method"], "paste-end");
        let received: String = paste
            .iter()
            .filter_map(|command| command["text"].as_str())
            .collect();
        assert_eq!(received, text);
    }

    fn request(value: Value) -> StreamRequest {
        serde_json::from_value(value).unwrap()
    }
    fn wire(outgoing: Outgoing) -> Value {
        let mut bytes = Vec::new();
        outgoing
            .write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    #[test]
    fn large_pastes_use_bounded_utf8_chunks_and_preserve_operation_options() {
        let text = "a界\n\0".repeat(transport::MAX_PASTE / 6);
        let mut bytes = Vec::new();
        Outgoing::prepare(request(json!({"method":"paste", "pane":7, "text":text, "clipboard":true, "enter":true, "bottom":true}))).unwrap().write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL)).unwrap();
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
            clipboard_epochs: Default::default(),
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
            clipboard_epochs: Default::default(),
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
            clipboard_epochs: Default::default(),
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
    use gmux_protocol::transport::Header;
    use std::path::PathBuf;

    fn manager() -> (SessionManager, incoming::Sender, SessionHandle) {
        let (socket, _) = UnixStream::pair().unwrap();
        let (commands, _) = mpsc::sync_channel(1);
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
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
                command: Default::default(),
                progress: Default::default(),
                program_status: None,
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
            scroll_sent: ValueCell::new(0),
            scroll_sent_at: ValueCell::new(Instant::now()),
            scroll_acknowledged: 0,
            scroll_cadence: SCROLL_CADENCE,
            pending_scroll: ValueCell::new(None),
            find: gmux_protocol::search::FindState::default(),
            selection: None,
            selection_serial: ValueCell::new(0),
            selection_acknowledged: 0,
            selection_origin: None,
            row_flags: vec![0; 2],
            hyperlinks: Default::default(),
            hyperlink_revision: 0,
            cache: gmux_protocol::scroll_cache::RowCache::default(),
            prediction: None,
            scroll_target: None,
            wake: wake.clone(),
            scroll_defer_until: ValueCell::new(None),
            scroll_retry_pending: ValueCell::new(false),
        }));
        (
            SessionManager {
                endpoint: ipc::Endpoint::local(),
                daemon: gmux_platform::unix::identity::daemon_id().unwrap(),
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
                frame_interval: transport::DEFAULT_FRAME_INTERVAL,
                frame_interval_sent: true,
                clipboard_writes: false,
                pending_reads: HashMap::new(),
                wake,
                disconnected: false,
                disconnect_error: None,
                state: None,
            },
            sender,
            session,
        )
    }

    #[test]
    fn clipboard_generations_coalesce_across_panes_and_retry_after_queue_pressure() {
        use gmux_protocol::clipboard::Destination::{Clipboard, Primary};
        let (mut manager, _, first) = manager();
        manager.metadata.insert(
            2,
            Metadata {
                id: 2,
                ..first.borrow().meta.clone()
            },
        );
        let second = manager.attach(2, &Theme::adwaita(true)).unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        first.borrow_mut().connection = connection.clone();
        second.borrow_mut().connection = connection;
        for index in 0..128 {
            let session = if index % 2 == 0 { &first } else { &second };
            session.borrow().clipboard_changed(Clipboard, 1).unwrap();
        }
        assert!(second.borrow().clipboard_changed(Clipboard, 2).is_err());
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"clipboard-changed","destination":"clipboard","epoch":1})
        );
        second.borrow().clipboard_changed(Clipboard, 2).unwrap();
        assert!(first.borrow().clipboard_changed(Primary, 1).is_err());
        assert_eq!(wire(receiver.try_recv().unwrap())["epoch"], 2);
        first.borrow().clipboard_changed(Primary, 1).unwrap();
        second.borrow().clipboard_changed(Primary, 1).unwrap();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"clipboard-changed","destination":"primary","epoch":1})
        );
        assert!(receiver.try_recv().is_err());

        // A replacement connection must advertise its own initial generation.
        let (commands, receiver) = mpsc::sync_channel(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        second.borrow_mut().connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        second.borrow().clipboard_changed(Clipboard, 2).unwrap();
        assert_eq!(wire(receiver.try_recv().unwrap())["epoch"], 2);
    }

    #[test]
    fn clipboard_capability_changes_coalesce_and_failed_advertisements_can_retry() {
        let (mut manager, _, _) = manager();
        let (commands, receiver) = mpsc::sync_channel(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        manager.connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.set_clipboard_writes(false).unwrap();
        assert!(receiver.try_recv().is_err());
        for _ in 0..10 {
            manager.set_clipboard_writes(true).unwrap();
        }
        assert!(manager.set_clipboard_writes(false).is_err());
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"clipboard-writes","enabled":true})
        );
        manager.set_clipboard_writes(false).unwrap();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"clipboard-writes","enabled":false})
        );
        manager.set_clipboard_writes(false).unwrap();
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn clipboard_packets_keep_cross_pane_wire_order_without_mutating_selection_or_replay() {
        use gmux_protocol::clipboard::{Destination, Write};
        let (mut manager, sender, handle) = manager();
        handle.borrow_mut().selection = Some(Selection::linear(0, 1));
        for (pane, destination, text) in [
            (1, Destination::Clipboard, Some("first 界🙂")),
            (2, Destination::Primary, Some("")),
            (1, Destination::Clipboard, None),
            (2, Destination::Clipboard, Some("last")),
        ] {
            let content = text.map(|text| {
                let mut offer: gmux_protocol::clipboard::Offer =
                    gmux_protocol::clipboard::Content {
                        text: text.into(),
                        html: (text == "last").then(|| "<div>last</div>".into()),
                    }
                    .into();
                if text == "last" {
                    offer.0.push(gmux_protocol::clipboard::Representation {
                        mime: "image/png".into(),
                        data: include_bytes!("../../../scripts/fixtures/clipboard.png").to_vec(),
                    });
                }
                offer
            });
            let packet = Packet {
                header: Header {
                    meta: Metadata {
                        id: pane,
                        ..Metadata::default()
                    },
                    copy: Some(Write {
                        destination,
                        content,
                    }),
                    ..Header::default()
                },
                cells: vec![],
            };
            // Include actual encoding, decoding, and retained-byte admission.
            let mut bytes = vec![];
            packet.encode_into(&mut bytes).unwrap();
            sender
                .send(Ok(Packet::read(&mut &bytes[..]).unwrap()))
                .unwrap();
        }
        let mut writes = vec![];
        assert!(
            manager
                .poll_with_clipboard(|write| writes.push(write))
                .is_empty()
        );
        assert_eq!(
            writes
                .iter()
                .map(|w| (
                    w.destination,
                    w.content.as_ref().and_then(|c| c.plain_text())
                ))
                .collect::<Vec<_>>(),
            [
                (Destination::Clipboard, Some("first 界🙂")),
                (Destination::Primary, Some("")),
                (Destination::Clipboard, None),
                (Destination::Clipboard, Some("last")),
            ]
        );
        assert_eq!(
            writes[3].content.as_ref().unwrap().get("text/html"),
            Some(b"<div>last</div>".as_slice())
        );
        assert_eq!(
            writes[3].content.as_ref().unwrap().get("image/png"),
            Some(include_bytes!("../../../scripts/fixtures/clipboard.png").as_slice())
        );
        assert_eq!(handle.borrow().selection, Some(Selection::linear(0, 1)));
        manager.poll_with_clipboard(|_| panic!("replayed clipboard write"));
    }

    #[test]
    fn link_only_frames_and_late_scroll_replies_keep_displayed_targets_atomic() {
        use gmux_protocol::hyperlinks::Hyperlinks;
        let (mut first, sender, session) = manager();
        let (mut second, other_sender, other) = manager();
        let send = |sender: &incoming::Sender, handle: &SessionHandle, serial, uri: &str| {
            let mut frame = handle.borrow().frame;
            frame.scrollback.offset = serial;
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: handle.borrow().meta.clone(),
                        frame: Some(frame),
                        scroll_serial: serial,
                        hyperlinks: Some(Hyperlinks {
                            targets: vec![uri.into()],
                            spans: vec![[0, 2, 0]],
                        }),
                        ..Header::default()
                    },
                    cells: vec![],
                }))
                .unwrap();
        };
        send(&sender, &session, 2, "test:newer-scroll");
        send(&other_sender, &other, 7, "test:other-client");
        first.poll();
        second.poll();
        // No hover request can race: even an earlier scroll reply's targets
        // describe exactly the viewport installed from that packet.
        send(&sender, &session, 1, "test:late-scroll");
        assert!(first.poll()[&1].changed);
        let screen = session.borrow();
        assert_eq!(screen.snapshot().frame.scrollback.offset, 1);
        assert_eq!(
            screen.snapshot().hyperlinks.target_at(1),
            Some("test:late-scroll")
        );
        assert_eq!(screen.snapshot().revision, 0);
        assert_eq!(screen.snapshot().text_revision, 0);
        assert_eq!(
            other.borrow().snapshot().hyperlinks.target_at(1),
            Some("test:other-client")
        );
        drop(screen);
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: session.borrow().meta.clone(),
                    frame: Some(session.borrow().frame),
                    hyperlinks: Some(Hyperlinks::default()),
                    ..Header::default()
                },
                cells: vec![],
            }))
            .unwrap();
        first.poll();
        assert_eq!(session.borrow().snapshot().hyperlinks.memory_size(), 0);
        send(&sender, &session, 3, "test:before-resize");
        first.poll();
        let link_revision = session.borrow().snapshot().hyperlink_revision;
        let mut resized = session.borrow().frame;
        resized.cols = 3;
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: session.borrow().meta.clone(),
                    frame: Some(resized),
                    ..Header::default()
                },
                cells: vec![],
            }))
            .unwrap();
        assert!(first.poll()[&1].changed);
        {
            let screen = session.borrow();
            let snapshot = screen.snapshot();
            assert_eq!(snapshot.frame.cols, 3);
            assert_eq!(snapshot.hyperlinks.memory_size(), 0);
            assert_ne!(snapshot.hyperlink_revision, link_revision);
            assert_eq!(snapshot.text_revision, 1);
            assert_eq!(snapshot.revision, 0);
        }
        let (_, _, fresh) = manager();
        assert_eq!(fresh.borrow().snapshot().hyperlinks.target_at(0), None);
    }

    #[test]
    fn cached_predictions_hide_links_until_the_canonical_view_is_displayed() {
        use gmux_protocol::hyperlinks::Hyperlinks;
        let (mut manager, sender, handle) = manager();
        manager.visible.insert(1);
        let (commands, _receiver) = mpsc::sync_channel(32);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        handle.borrow_mut().connection = connection;
        let frame = Frame {
            cols: 2,
            rows: 2,
            scrollback: gmux_protocol::Scrollback {
                total: 20,
                offset: 4,
                len: 2,
            },
            ..Frame::default()
        };
        let header = |offset, serial| Header {
            frame: Some(Frame {
                scrollback: gmux_protocol::Scrollback {
                    offset,
                    ..frame.scrollback
                },
                ..frame
            }),
            scroll_serial: serial,
            generation: 1,
            ..Header::default()
        };
        let row = |value| {
            let mut cell = Cell::default();
            cell.set_text_bytes(&[value]).unwrap();
            vec![cell.clone(), cell]
        };
        let links = |uri: &str| {
            Some(Hyperlinks {
                targets: vec![uri.into()],
                spans: vec![[0, 4, 0]],
            })
        };
        let cache = || {
            Some(transport::CachedRows {
                start: 0,
                row_flags: vec![0; 10],
            })
        };
        let send = |mut header: Header, cells| {
            header.meta = handle.borrow().meta.clone();
            sender.send(Ok(Packet { header, cells })).unwrap();
        };
        let initial = || [row(b'e'), row(b'f'), (b'a'..=b'j').flat_map(row).collect()].concat();
        send(
            Header {
                rows: vec![0, 1],
                cache: cache(),
                hyperlinks: links("test:initial"),
                ..header(4, 0)
            },
            initial(),
        );
        manager.poll();
        let authoritative_revision = handle.borrow().snapshot().hyperlink_revision;
        handle.borrow_mut().scroll_to(5);
        let predicted_revision = handle.borrow().snapshot().hyperlink_revision;
        assert_ne!(predicted_revision, authoritative_revision);
        assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);

        // Older canonical deltas and map-only replies cannot lend targets to
        // the retained prediction, including shifted rows with new targets.
        send(
            Header {
                row_shift: -1,
                rows: vec![0],
                hyperlinks: links("test:shifted"),
                ..header(3, 0)
            },
            row(b'd'),
        );
        manager.poll();
        let cells = handle.borrow().snapshot().cells.to_vec();
        send(
            Header {
                hyperlinks: links("test:map-only"),
                ..header(3, 0)
            },
            vec![],
        );
        manager.poll();
        {
            let session = handle.borrow();
            let screen = session.snapshot();
            assert_eq!(screen.frame.scrollback.offset, 5);
            assert!(screen.cells == cells);
            assert_eq!(screen.hyperlinks.memory_size(), 0);
            assert_eq!(session.frame.scrollback.offset, 3);
            assert_eq!(session.cells[0].text(), "d");
            assert_eq!(session.hyperlinks.target_at(0), Some("test:map-only"));
        }
        send(
            Header {
                rows: vec![0, 1],
                hyperlinks: links("test:matched"),
                ..header(5, 1)
            },
            [row(b'f'), row(b'g')].concat(),
        );
        manager.poll();
        assert!(handle.borrow().prediction.is_none());
        assert_eq!(
            handle.borrow().snapshot().hyperlinks.target_at(0),
            Some("test:matched")
        );
        assert_ne!(
            handle.borrow().snapshot().hyperlink_revision,
            predicted_revision
        );

        // Completion without a new map still changes displayed map identity.
        handle.borrow_mut().scroll_to(4);
        send(
            Header {
                rows: vec![0, 1],
                hyperlinks: links("test:four"),
                ..header(4, 1)
            },
            [row(b'e'), row(b'f')].concat(),
        );
        manager.poll();
        assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
        let before_ack = handle.borrow().snapshot().hyperlink_revision;
        send(header(4, 2), vec![]);
        manager.poll();
        let canonical_revision = handle.borrow().snapshot().hyperlink_revision;
        assert_ne!(canonical_revision, before_ack);
        assert_eq!(
            handle.borrow().snapshot().hyperlinks.target_at(0),
            Some("test:four")
        );

        // Metadata-only generation and truncation invalidation restore the
        // canonical frame/map together; neither needs a map-bearing reply.
        handle.borrow_mut().scroll_to(3);
        assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
        send(
            Header {
                generation: 2,
                ..Header::default()
            },
            vec![],
        );
        manager.poll();
        assert_eq!(
            handle.borrow().snapshot().hyperlink_revision,
            canonical_revision
        );
        assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 4);
        assert_eq!(
            handle.borrow().snapshot().hyperlinks.target_at(0),
            Some("test:four")
        );
        for resize in [false, true] {
            send(
                Header {
                    generation: 2,
                    cache: cache(),
                    ..header(4, 3)
                },
                (b'a'..=b'j').flat_map(row).collect(),
            );
            manager.poll();
            handle.borrow_mut().scroll_to(3);
            assert!(handle.borrow().prediction.is_some());
            let predicted_revision = handle.borrow().snapshot().hyperlink_revision;
            assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
            if resize {
                handle.borrow_mut().resize(3, 2, 0, 0, 0, 0).unwrap();
            } else {
                send(
                    Header {
                        generation: 2,
                        cache_valid_before: Some(4),
                        ..Header::default()
                    },
                    vec![],
                );
                manager.poll();
            }
            let session = handle.borrow();
            let screen = session.snapshot();
            assert!(session.prediction.is_none());
            assert_ne!(screen.hyperlink_revision, predicted_revision);
            assert_eq!(screen.frame.scrollback.offset, 4);
            assert_eq!(screen.hyperlinks.target_at(0), Some("test:four"));
        }
        send(
            Header {
                generation: 3,
                frame: Some(Frame { cols: 3, ..frame }),
                rows: vec![0, 1],
                ..header(4, 4)
            },
            vec![Cell::default(); 6],
        );
        manager.poll();
        assert_eq!(handle.borrow().snapshot().frame.cols, 3);
        assert_eq!(handle.borrow().snapshot().hyperlinks.memory_size(), 0);
    }

    #[test]
    fn streamed_oversized_packet_disconnects_after_applying_earlier_packets() {
        use std::{
            io::Write,
            time::{Duration, Instant},
        };
        let (socket, mut daemon_socket) = UnixStream::pair().unwrap();
        daemon_socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut manager = SessionManager::from_attached_stream(
            ipc::Endpoint::local(),
            gmux_platform::unix::ipc::Wake::new().unwrap(),
            gmux_platform::unix::identity::daemon_id().unwrap(),
            socket.into(),
        )
        .unwrap();
        let meta = Metadata {
            id: 1,
            title: "original".into(),
            ..Metadata::default()
        };
        manager.install_metadata(std::slice::from_ref(&meta), 1);
        let session = manager.attach(1, &Theme::adwaita(true)).unwrap();
        let mut packet = Packet {
            header: Header {
                meta,
                notifications: vec!["earlier notification".into()],
                ..Header::default()
            },
            cells: vec![],
        };
        packet.header.meta.title = "before oversized packet".into();
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        // Many empty strings fit on the wire, but their retained vector alone
        // exceeds the frontend budget after deserialization.
        packet.header.notifications =
            vec![String::new(); incoming::MAX_BYTES / std::mem::size_of::<String>() + 1];
        packet.header.meta.title = "oversized packet".into();
        packet.encode_into(&mut bytes).unwrap();
        packet.header.notifications.clear();
        packet.header.meta.title = "after oversized packet".into();
        packet.encode_into(&mut bytes).unwrap();
        drop(packet);
        daemon_socket.write_all(&bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut notifications = Vec::new();
        while !manager.disconnected() {
            for update in manager.poll().into_values() {
                notifications.extend(update.notifications);
            }
            assert!(
                Instant::now() < deadline,
                "reader did not report oversized packet"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            manager
                .disconnect_error()
                .unwrap()
                .to_string()
                .contains("memory budget")
        );
        assert_eq!(session.borrow().meta.title, "before oversized packet");
        assert_eq!(notifications, ["earlier notification"]);
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
        local_session.borrow_mut().selection = Some(Selection::linear(1, 3));
        send(&local_sender, "/tmp", "local-main");
        send(&remote_sender, "/tmp", "remote-main");
        let updates = local.poll();
        assert!(updates[&1].git_changed);
        assert!(!updates[&1].changed && !updates[&1].directory_changed);
        assert_eq!(
            local_session.borrow().selection,
            Some(Selection::linear(1, 3))
        );
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
            clipboard_epochs: Default::default(),
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
        receiver
            .try_recv()
            .unwrap()
            .write(&mut bytes, &mut Heartbeat::new(HEARTBEAT_INTERVAL))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"method":"git-subscribe","panes":[1]})
        );
    }

    #[test]
    fn progress_and_program_status_changes_preserve_viewport_and_ordered_metadata() {
        let (mut manager, sender, session) = manager();
        // Metadata must also leave a populated history cache and its active
        // prediction alone, rather than only preserving an empty live viewport.
        manager.visible.insert(1);
        let (commands, _outgoing) = mpsc::sync_channel(8);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        {
            let mut session = session.borrow_mut();
            session.connection = connection;
            session.frame.scrollback = gmux_protocol::Scrollback {
                total: 20,
                offset: 4,
                len: 2,
            };
            let frame = session.frame;
            session.cache.configure(1, frame);
            session.cache.insert(
                0,
                &std::array::from_fn::<_, 20, _>(|_| Cell::default()),
                &[0; 10],
            );
            session.scroll_to(3);
        }
        let before = session.borrow();
        let revision = before.snapshot().revision;
        let text_revision = before.snapshot().text_revision;
        let frame = before.snapshot().frame;
        let cache_bytes = before.cache.retained_bytes();
        drop(before);
        let mut meta = session.borrow().meta.clone();
        for state in [
            gmux_protocol::progress::ProgressState::Determinate,
            gmux_protocol::progress::ProgressState::Error,
            gmux_protocol::progress::ProgressState::Paused,
            gmux_protocol::progress::ProgressState::Indeterminate,
            gmux_protocol::progress::ProgressState::Inactive,
        ] {
            meta.progress.apply(state, Some(42));
            meta.program_status = Some(serde_json::from_value(serde_json::json!({
                "count": 1, "record": {"id":"worker", "state":"blocked", "kind":"question",
                    "progress":42, "app":"cargo", "title":"Tests", "message": format!("{state:?}")}
            })).unwrap());
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: meta.clone(),
                        ..Header::default()
                    },
                    cells: vec![],
                }))
                .unwrap();
            let updates = manager.poll();
            assert!(updates[&1].progress_changed);
            assert!(updates[&1].program_status_changed);
            assert!(!updates[&1].changed);
            let borrowed = session.borrow();
            assert_eq!(borrowed.progress(), meta.progress);
            assert_eq!(borrowed.snapshot().revision, revision);
            assert_eq!(borrowed.snapshot().text_revision, text_revision);
            assert!(borrowed.snapshot().frame == frame);
            assert!(borrowed.prediction.is_some());
            assert_eq!(borrowed.cache.retained_bytes(), cache_bytes);
            assert!(
                borrowed.snapshot().cells == std::array::from_fn::<_, 4, _>(|_| Cell::default())
            );
            assert!(!borrowed.unread());
            assert!(!borrowed.busy());
            drop(borrowed);
            let mut stale = meta.clone();
            stale.progress = Default::default();
            stale.program_status = None;
            manager.install_metadata(&[stale], 1);
            assert_eq!(session.borrow().progress(), meta.progress);
            assert_eq!(
                session.borrow().program_status(),
                meta.program_status.as_ref()
            );
        }
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
                    notifications: vec!["ready".into()],
                    ..Header::default()
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
        assert!(session.snapshot().cells == std::array::from_fn::<_, 4, _>(|_| Cell::default()));
        assert!(manager.poll().is_empty());
    }

    #[test]
    fn state_from_another_daemon_stops_recovery_without_replacing_the_local_view() {
        let (mut manager, sender, session) = manager();
        let packet = |daemon| Packet {
            header: Header {
                state: Some(gmux_protocol::control::CommandReply {
                    home_directory: "/home/test".into(),
                    machine_kind: gmux_protocol::model::MachineKind::Unknown,
                    hostname: "test-host".into(),
                    daemon,
                    revision: 1,
                    result: gmux_protocol::control::CommandOutput::Done { ok: true }.into(),
                    execution: ExecutionState::default(),
                    metadata: vec![],
                }),
                meta: session.borrow().meta.clone(),
                ..Header::default()
            },
            cells: vec![],
        };
        sender.send(Ok(packet(manager.daemon))).unwrap();
        assert!(manager.poll().is_empty());
        assert_eq!(manager.take_state().unwrap().daemon, manager.daemon);
        sender
            .send(Ok(packet(
                gmux_platform::unix::identity::daemon_id().unwrap(),
            )))
            .unwrap();
        let mut connection = crate::connection::Connection::from_session(
            manager,
            gmux_platform::unix::ipc::Wake::new().unwrap(),
        );
        connection.set_auto_reconnect(true);
        let update = connection.poll();
        assert!(matches!(
            connection.status(),
            crate::connection::Status::Disconnected { error: Some(_) }
        ));
        assert!(connection.next_retry().is_none());
        assert!(update.state.is_none());
        assert!(
            update.panes[&1]
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
                        meta,
                        ..Header::default()
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
                        meta,
                        notifications: notifications.into_iter().map(str::to_owned).collect(),
                        ..Header::default()
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
            cell.set_text_bytes(&[text]).unwrap();
            frame.cursor_y = row;
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: session.borrow().meta.clone(),
                        frame: Some(frame),
                        rows: vec![row],
                        ..Header::default()
                    },
                    cells: vec![cell.clone(), cell],
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
        selection: Option<Selection>,
    ) {
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: session.borrow().meta.clone(),
                    frame: Some(frame),
                    rows,
                    selection,
                    ..Header::default()
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
        cells[0].set_text("界").unwrap();
        cells[1].flags = 64;
        cells[2]
            .set_text(&format!("e{}", "\u{301}".repeat(64)))
            .unwrap();
        send_frame(&sender, &session, frame, vec![0, 1], cells.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 1);
        for cell in &mut cells {
            cell.fg = [1, 2, 3];
            cell.bg = [4, 5, 6];
            cell.flags |= 63;
            cell.decoration =
                underline::DASHED | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
            cell.underline_color = [8, 9, 10];
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
        row[0].set_text("e\u{301}").unwrap();
        row[1].set_text("X").unwrap();
        send_frame(&sender, &session, frame, vec![1], row.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 1);
        assert!(
            session.borrow().snapshot().cells[..2]
                == std::array::from_fn::<_, 2, _>(|_| Cell::default())
        );
        // The same concatenated text can occupy different cells. Its caret and
        // geometry mappings must still be rebuilt.
        row[0].set_text("").unwrap();
        row[0].set_text("e").unwrap();
        row[1].set_text("\u{301}X").unwrap();
        send_frame(&sender, &session, frame, vec![1], row.clone(), None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 2);
        row[1].flags = 64;
        send_frame(&sender, &session, frame, vec![1], row, None);
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 3);
    }

    #[test]
    fn selection_replies_wait_for_the_latest_range_or_clear() {
        use gmux_protocol::transport::SelectionMode;
        let (mut manager, sender, session) = manager();
        let (commands, _outgoing) = mpsc::sync_channel(16);
        let (socket, _peer) = UnixStream::pair().unwrap();
        session.borrow_mut().connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        let send = |serial, selection| {
            sender
                .send(Ok(transport::Packet {
                    header: Header {
                        meta: session.borrow().meta.clone(),
                        selection,
                        selection_serial: serial,
                        ..Header::default()
                    },
                    cells: Vec::new(),
                }))
                .unwrap();
        };
        session
            .borrow()
            .select((0, 0), (1, 0), SelectionMode::Word)
            .unwrap();
        send(0, Some(Selection::linear(2, 3)));
        manager.poll();
        assert!(session.borrow().selection_pending());
        assert_eq!(session.borrow().selection(), None);
        send(1, Some(Selection::linear(0, 1)));
        assert!(manager.poll()[&1].changed);
        assert!(!session.borrow().selection_pending());
        assert_eq!(session.borrow().selection(), Some(Selection::linear(0, 1)));
        session.borrow().clear_selection().unwrap();
        send(1, Some(Selection::linear(2, 3)));
        manager.poll();
        assert!(session.borrow().selection_pending());
        assert_eq!(session.borrow().selection(), Some(Selection::linear(0, 1)));
        send(2, None);
        assert!(manager.poll()[&1].changed);
        assert!(!session.borrow().selection_pending());
        assert_eq!(session.borrow().selection(), None);
    }

    #[test]
    fn geometry_and_positions_survive_frames_without_row_deltas() {
        let (mut manager, sender, session) = manager();
        let frame = Frame {
            cursor_x: 1,
            cursor_y: 1,
            ..session.borrow().frame
        };
        send_frame(
            &sender,
            &session,
            frame,
            vec![],
            vec![],
            Some(Selection::linear(1, 3)),
        );
        assert!(manager.poll()[&1].changed);
        assert_eq!(session.borrow().snapshot().text_revision, 0);
        assert_eq!(session.borrow().snapshot().frame.cursor_y, 1);
        assert_eq!(session.borrow().selection(), Some(Selection::linear(1, 3)));
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
    fn output_and_pruning_preserve_predictions_until_their_rows_change() {
        let (mut manager, sender, handle) = manager();
        manager.visible.insert(1);
        let (commands, _outgoing) = mpsc::sync_channel(32);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        handle.borrow_mut().connection = connection;
        let frame = Frame {
            cols: 2,
            rows: 2,
            scrollback: gmux_protocol::Scrollback {
                total: 20,
                offset: 4,
                len: 2,
            },
            ..Frame::default()
        };
        let row = |value| {
            let mut cell = Cell::default();
            cell.set_text_bytes(&[value]).unwrap();
            [cell.clone(), cell]
        };
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: handle.borrow().meta.clone(),
                    frame: Some(frame),
                    generation: 1,
                    rows: vec![0, 1],
                    cache: Some(transport::CachedRows {
                        start: 0,
                        row_flags: vec![0; 10],
                    }),
                    ..Header::default()
                },
                cells: row(b'e')
                    .into_iter()
                    .chain(row(b'f'))
                    .chain((b'a'..=b'j').flat_map(row))
                    .collect(),
            }))
            .unwrap();
        manager.poll();
        handle.borrow_mut().scroll_to(3);
        let send = |origin, valid, frame| {
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: handle.borrow().meta.clone(),
                        generation: 1,
                        cache_origin: origin,
                        cache_valid_before: Some(valid),
                        frame,
                        ..Header::default()
                    },
                    cells: vec![],
                }))
                .unwrap()
        };
        send(0, 8, None);
        manager.poll();
        assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
        send(
            2,
            6,
            Some(Frame {
                scrollback: gmux_protocol::Scrollback {
                    total: 18,
                    offset: 2,
                    len: 2,
                },
                ..frame
            }),
        );
        manager.poll();
        assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 1);
        assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
        // Copy and selection metadata omit cache state and must not reset its origin.
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: handle.borrow().meta.clone(),
                    ..Header::default()
                },
                cells: vec![],
            }))
            .unwrap();
        manager.poll();
        assert_eq!(handle.borrow().snapshot().cells[0].text(), "d");
        send(2, 2, None);
        assert!(manager.poll()[&1].changed);
        assert!(handle.borrow().prediction.is_none());
        handle.borrow_mut().clear_cache();
        handle.borrow_mut().scroll_to(10);
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: handle.borrow().meta.clone(),
                    generation: 2,
                    cache_origin: 100,
                    frame: Some(frame),
                    ..Header::default()
                },
                cells: vec![],
            }))
            .unwrap();
        manager.poll();
        assert_eq!(
            handle.borrow().scroll_target,
            Some(10),
            "A new generation's origin cannot rebase a target from an unknown cache"
        );
    }

    #[test]
    fn cached_edge_requests_coalesce_during_refill_but_misses_keep_pipelining() {
        let (mut manager, _sender, handle) = manager();
        let (commands, outgoing) = mpsc::sync_channel(8);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut session = handle.borrow_mut();
        session.connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        session.frame.scrollback = gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        };
        let frame = session.frame;
        session.cache.configure(1, frame);
        session.cache.insert(
            4,
            &std::array::from_fn::<_, 6, _>(|_| Cell::default()),
            &[0; 3],
        );
        session.scroll_to(4);
        assert_eq!(wire(outgoing.try_recv().unwrap())["row"], 4);
        session.scroll_to(5);
        assert_eq!(session.snapshot().frame.scrollback.offset, 5);
        assert!(outgoing.try_recv().is_err());
        assert!(session.scroll_defer_until.get().is_some());
        session.scroll_to(10);
        assert!(session.scroll_defer_until.get().is_none());
        session
            .scroll_sent_at
            .set(Instant::now() - session.scroll_cadence);
        drop(session);
        manager.poll();
        assert_eq!(wire(outgoing.try_recv().unwrap())["row"], 10);
        assert!(handle.borrow().scroll_pending());
    }

    #[test]
    fn returning_to_the_live_bottom_does_not_debounce_cursor_synchronization() {
        let (_manager, _sender, handle) = manager();
        let (commands, outgoing) = mpsc::sync_channel(8);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut session = handle.borrow_mut();
        session.connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        session.frame.scrollback = gmux_protocol::Scrollback {
            total: 20,
            offset: 4,
            len: 2,
        };
        session.frame.cursor_visible = 0;
        let frame = session.frame;
        session.cache.configure(1, frame);
        session.cache.insert(
            10,
            &std::array::from_fn::<_, 20, _>(|_| Cell::default()),
            &[0; 10],
        );
        session.scroll_to(16);
        assert_eq!(session.snapshot().frame.scrollback.offset, 16);
        assert!(session.scroll_defer_until.get().is_some());
        assert!(outgoing.try_recv().is_err());
        session.scroll_to(18);
        assert_eq!(session.snapshot().frame.scrollback.offset, 18);
        assert!(
            session.scroll_defer_until.get().is_none(),
            "The live cursor must not wait for cache settle"
        );
        assert_eq!(
            wire(outgoing.try_recv().unwrap()),
            json!({"method":"scroll-to", "pane":1, "row":18, "serial":2})
        );
    }

    #[test]
    fn display_interval_changes_update_scroll_pacing_and_retry_after_backpressure() {
        let (mut manager, _sender, handle) = manager();
        let (commands, outgoing) = mpsc::sync_channel(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        handle.borrow_mut().connection = connection;
        manager.set_focused(true).unwrap();
        manager.set_frame_interval(Duration::from_micros(4167));
        assert_eq!(handle.borrow().scroll_cadence, Duration::from_micros(4167));
        assert_eq!(wire(outgoing.try_recv().unwrap())["method"], "focused");
        manager.poll();
        assert_eq!(
            wire(outgoing.try_recv().unwrap()),
            json!({"method":"frame-interval", "micros":4167})
        );
        assert!(manager.frame_interval_sent);
    }

    #[test]
    fn cached_scrolling_is_immediate_and_reconciles_ordered_deltas_and_invalidations() {
        let (mut manager, sender, handle) = manager();
        manager.visible.insert(1);
        let (commands, receiver) = mpsc::sync_channel(32);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        handle.borrow_mut().connection = connection;
        let row = |value: u8| {
            let mut cell = Cell::default();
            cell.set_text_bytes(&[value]).unwrap();
            cell.decoration = underline::CURLY;
            vec![cell.clone(), cell]
        };
        let frame = Frame {
            cols: 2,
            rows: 2,
            scrollback: gmux_protocol::Scrollback {
                total: 20,
                offset: 4,
                len: 2,
            },
            cursor_visible: 1,
            cursor_y: 1,
            ..Frame::default()
        };
        let send = |offset, serial, generation, shift, rows, cells, cache| {
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: handle.borrow().meta.clone(),
                        frame: Some(Frame {
                            scrollback: gmux_protocol::Scrollback {
                                offset,
                                ..frame.scrollback
                            },
                            ..frame
                        }),
                        scroll_serial: serial,
                        generation,
                        row_shift: shift,
                        rows,
                        cache,
                        ..Header::default()
                    },
                    cells,
                }))
                .unwrap();
        };
        let mut initial = [row(b'e'), row(b'f')].concat();
        initial.extend((b'a'..=b'j').flat_map(row));
        send(
            4,
            0,
            1,
            0,
            vec![0, 1],
            initial,
            Some(transport::CachedRows {
                start: 0,
                row_flags: vec![0; 10],
            }),
        );
        manager.poll();
        for target in [3, 6, 2, 5] {
            handle.borrow_mut().scroll_to(target);
            let session = handle.borrow();
            let screen = session.snapshot();
            assert_eq!(screen.frame.scrollback.offset, target);
            assert_eq!(
                &screen.cells[0].text().as_bytes()[0..1],
                &[b'a' + target as u8]
            );
            assert_eq!(screen.cells[0].decoration, underline::CURLY);
            assert!(session.scroll_pending());
        }
        assert!(
            receiver.try_recv().is_err(),
            "Cached reversals should not stream intermediate targets"
        );
        assert!(manager.next_scroll().is_some());
        handle
            .borrow()
            .scroll_defer_until
            .set(Some(Instant::now() - Duration::from_millis(1)));
        manager.poll();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":5,"serial":4})
        );
        // This old viewport shifts the canonical grid to [d,e]. It must
        // not shift the locally predicted [f,g] or lose the delta dependency.
        send(3, 1, 1, -1, vec![0], row(b'd'), None);
        manager.poll();
        assert_eq!(handle.borrow().snapshot().frame.scrollback.offset, 5);
        assert_eq!(handle.borrow().snapshot().cells[0].text(), "f");
        send(
            5,
            4,
            1,
            0,
            vec![0, 1],
            [row(b'f'), row(b'g')].concat(),
            None,
        );
        manager.poll();
        assert!(!handle.borrow().scroll_pending());
        assert!(handle.borrow().prediction.is_none());
        assert_eq!(handle.borrow().text(), "ff\ngg");
        handle.borrow_mut().scroll_to(4);
        assert_eq!(handle.borrow().snapshot().cells[0].text(), "e");
        handle
            .borrow()
            .copy(gmux_protocol::clipboard::Formats::Text)
            .unwrap();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":4,"serial":5})
        );
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "copy");
        sender
            .send(Ok(Packet {
                header: Header {
                    meta: handle.borrow().meta.clone(),
                    generation: 2,
                    ..Header::default()
                },
                cells: Vec::new(),
            }))
            .unwrap();
        assert!(manager.poll()[&1].changed);
        assert!(
            handle.borrow().prediction.is_none(),
            "Metadata-only invalidation must discard predicted rows"
        );
        // New output may trim/reflow history even if total and size are
        // unchanged. A new generation must discard every old row.
        send(
            4,
            5,
            2,
            0,
            vec![0, 1],
            [row(b'x'), row(b'y')].concat(),
            None,
        );
        manager.poll();
        handle.borrow_mut().scroll_to(2);
        assert!(handle.borrow().prediction.is_none());
        assert_eq!(handle.borrow().text(), "xx\nyy");
        manager.set_visible(HashSet::new()).unwrap();
        assert!(handle.borrow().cache.is_empty());
    }

    #[test]
    fn key_actions_survive_the_frontend_command_queue() {
        let (_, _, session) = manager();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(8);
        session.borrow_mut().connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        for (action, expected) in [
            (KeyAction::Press, "press"),
            (KeyAction::Repeat, "repeat"),
            (KeyAction::Release, "release"),
        ] {
            session
                .borrow()
                .key(
                    "exclam",
                    KeyLayout {
                        physical: 2,
                        unshifted: 49,
                        consumed_mods: 1,
                    },
                    33,
                    action,
                    "!",
                )
                .unwrap();
            let request = wire(receiver.try_recv().unwrap());
            assert_eq!(
                request,
                json!({"method":"key","pane":1,"name":"exclam","physical":2,"unshifted":49,"consumed_mods":1,"mods":33,"action":expected,"text":"!"})
            );
            assert!(
                matches!(StreamRequest::decode(&serde_json::to_vec(&request).unwrap()).command,
                StreamCommand::Key { action: actual, .. } if actual == action)
            );
        }
    }

    #[test]
    fn rapid_scroll_events_pipeline_before_viewport_replies_arrive() {
        let (mut manager, sender, session) = manager();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(128);
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        // The full cadence is longer than any test runtime, so the release in
        // the dedicated cadence test is the only deadline path.
        session.borrow_mut().scroll_cadence = Duration::from_secs(600);

        session.borrow_mut().scroll_to(10);
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":10,"serial":1})
        );
        let reply = |serial, frame| {
            sender
                .send(Ok(Packet {
                    header: Header {
                        meta: session.borrow().meta.clone(),
                        frame,
                        scroll_serial: serial,
                        ..Header::default()
                    },
                    cells: vec![],
                }))
                .unwrap();
        };
        // A responsive viewport acknowledges each scroll before the next wheel
        // event arrives: scrolling streams at input rate instead of pacing at
        // one update per round-trip.
        for (serial, index) in (1..).zip(0..120) {
            let row = if index % 2 == 0 { 80 } else { 20 };
            reply(serial, Some(session.borrow().frame));
            manager.poll();
            session.borrow_mut().scroll_to(row);
            session
                .borrow_mut()
                .set_theme(&Theme::adwaita(true))
                .unwrap();
            session
                .borrow()
                .set_cursor_style(CursorStyle::Block, false)
                .unwrap();
            session
                .borrow()
                .key("a", layout(b'a' as u32), 0, KeyAction::Release, "")
                .unwrap();
            manager.poll();
            let commands: Vec<_> = receiver.try_iter().map(wire).collect();
            assert_eq!(commands.len(), 4);
            assert_eq!(
                commands[0],
                json!({"method":"scroll-to","pane":1,"row":row,"serial":serial + 1})
            );
            assert_eq!(commands[1]["method"], "theme");
            assert_eq!(commands[2]["method"], "style");
            assert_eq!(commands[3]["method"], "key");
        }
        assert!(session.borrow().scroll_pending());
        assert!(manager.next_scroll().is_none());
        assert!(receiver.try_recv().is_err());

        // Replays and older serials never acknowledge the latest target, and
        // acknowledgements must not re-send settled scrolls.
        let mut cell = Cell::default();
        cell.set_text("x").unwrap();
        send_frame(
            &sender,
            &session,
            session.borrow().frame,
            vec![0],
            vec![cell.clone(), cell],
            None,
        );
        manager.poll();
        assert!(session.borrow().scroll_pending());
        assert_eq!(session.borrow().snapshot().cells[0].text(), "x");
        assert!(manager.next_scroll().is_none());

        let mut frame = session.borrow().frame;
        frame.scrollback.offset = 20;
        reply(121, Some(frame));
        assert!(manager.poll()[&1].changed);
        assert!(!session.borrow().scroll_pending());
        assert_eq!(session.borrow().snapshot().frame.scrollback.offset, 20);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_coalesced_scroll_target_releases_at_the_cadence_without_responses() {
        let (mut manager, _sender, session) = manager();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(8);
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;

        session.borrow_mut().scroll_to(10);
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":10,"serial":1})
        );
        // While the scroll is outstanding, further wheel events only coalesce
        // into the latest target...
        manager.wake.drain();
        session.borrow_mut().scroll_to(20);
        session.borrow_mut().scroll_to(30);
        {
            use std::os::fd::AsRawFd;
            let mut event = libc::pollfd {
                fd: manager.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut event, 1, 0) },
                1,
                "A deferred target must wake the interface to schedule its deadline"
            );
        }
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            manager.next_scroll(),
            Some(session.borrow().scroll_sent_at.get() + SCROLL_CADENCE)
        );
        // ...and release after the cadence expires with no response in sight,
        // so a slow viewport can never freeze scrolling on repeated targets.
        session
            .borrow_mut()
            .scroll_sent_at
            .set(Instant::now() - SCROLL_CADENCE);
        manager.poll();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":30,"serial":3})
        );
        assert!(session.borrow().scroll_pending());
        assert!(manager.next_scroll().is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn queue_backpressure_suspends_expired_scroll_deadlines_until_progress() {
        let (mut manager, _sender, session) = manager();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            socket: socket.into(),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        session.borrow_mut().scroll_to(10);
        session.borrow_mut().scroll_to(20);
        session
            .borrow()
            .scroll_sent_at
            .set(Instant::now() - SCROLL_CADENCE);
        manager.poll();
        assert!(session.borrow().scroll_pending());
        assert!(
            manager.next_scroll().is_none(),
            "A full queue must not arm an expired timer"
        );
        assert_eq!(wire(receiver.try_recv().unwrap())["row"], 10);
        // The writer wakes the manager after releasing a queue slot.
        manager.wake.notify();
        manager.poll();
        assert_eq!(wire(receiver.try_recv().unwrap())["row"], 20);
        assert!(manager.next_scroll().is_none());
    }

    #[test]
    fn commands_follow_the_latest_scroll_target_under_backpressure() {
        let (mut manager, _sender, session) = manager();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (commands, receiver) = mpsc::sync_channel(2);
        let connection = Rc::new(Connection {
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        let mut meta = session.borrow().meta.clone();
        meta.id = 2;
        manager.metadata.insert(2, meta);
        let other = manager.attach(2, &Theme::adwaita(true)).unwrap();

        // Fill the command queue and coalesce wheel events into the latest
        // target. Other panes and queued commands keep their order.
        other.borrow_mut().scroll_to(30);
        session
            .borrow()
            .key("a", layout(b'a' as u32), 0, KeyAction::Press, "a")
            .unwrap();
        session.borrow_mut().scroll_to(10);
        session.borrow_mut().scroll_to(20);
        assert!(session.borrow().scroll_pending());
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":2,"row":30,"serial":1})
        );
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "key");
        // The next command releases the coalesced target ahead of itself so
        // viewport-dependent input never overtakes older scrolls.
        session
            .borrow()
            .key("b", layout(b'b' as u32), 0, KeyAction::Press, "b")
            .unwrap();
        let commands: Vec<_> = receiver.try_iter().take(2).map(wire).collect();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands[0],
            json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
        );
        assert_eq!(commands[1]["method"], "key");
        assert!(receiver.try_recv().is_err());
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
            clipboard_epochs: Default::default(),
            commands,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            socket: socket.into(),
        });
        manager.connection = connection.clone();
        session.borrow_mut().connection = connection;
        session.borrow_mut().scroll_to(10);
        session.borrow_mut().scroll_to(20);
        assert!(session.borrow().scroll_pending());
        assert_eq!(wire(receiver.try_recv().unwrap())["method"], "input");
        manager.poll();
        assert_eq!(
            wire(receiver.try_recv().unwrap()),
            json!({"method":"scroll-to","pane":1,"row":20,"serial":2})
        );
        for serial in [1, 2] {
            let mut frame = session.borrow().frame;
            frame.scrollback = gmux_protocol::Scrollback {
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
                        ..Header::default()
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
            clipboard_epochs: Default::default(),
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
                        meta,
                        ..Header::default()
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
            clipboard_epochs: Default::default(),
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
