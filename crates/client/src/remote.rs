//! Frontend handles contain cached viewports; terminal engines live in the daemon.
use crate::{incoming, ipc, runtime};
use gmux_protocol::{
    control::InternalRequest,
    model::{CursorStyle, ExecutionSnapshot},
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
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
mod viewport;
use viewport::Viewport;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::compat::TokioAsyncReadCompatExt;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for tests that do not report a display interval.
#[cfg(test)]
const SCROLL_CADENCE: Duration = transport::DEFAULT_FRAME_INTERVAL;
const PING: &[u8] = b"{\"method\":\"ping\"}\n";
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
    async fn check(&mut self, writer: &mut (impl AsyncWrite + Unpin)) -> anyhow::Result<()> {
        let now = Instant::now();
        if now >= self.next {
            writer.write_all(PING).await?;
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

fn outgoing_budget(control: bool) -> Arc<tokio::sync::Semaphore> {
    use std::sync::OnceLock;
    static BULK: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    static CONTROL: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let (budget, bytes) = if control {
        (&CONTROL, 4 * 1024 * 1024)
    } else {
        (&BULK, 32 * 1024 * 1024)
    };
    budget
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(bytes)))
        .clone()
}

struct Connection {
    commands: tokio::sync::mpsc::Sender<Queued>,
    task: Option<tokio::task::AbortHandle>,
    pending_bytes: Arc<AtomicUsize>,
    socket: ipc::Stream,
    clipboard_epochs: [ValueCell<u64>; 2],
}
struct Queued {
    outgoing: Outgoing,
    reservation: Reservation,
}
struct Reservation {
    bytes: usize,
    pending: Arc<AtomicUsize>,
    _shared: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.pending.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

enum Outgoing {
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

    fn is_control(&self) -> bool {
        matches!(self, Self::Command(bytes) if bytes.capacity() + size_of::<Queued>() <= 32 * 1024)
    }
    fn retained_bytes(&self) -> usize {
        size_of::<Queued>()
            + match self {
                Self::Command(bytes) => bytes.capacity(),
                Self::Paste { text, .. } => text.capacity(),
                Self::ClipboardReply {
                    result, contents, ..
                } => {
                    // Include the retained result metadata as well as payload capacity.
                    result.contents.capacity()
                        * size_of::<gmux_protocol::clipboard::ReadDescriptor>()
                        + result.available.capacity() * size_of::<String>()
                        + result.available.iter().map(String::capacity).sum::<usize>()
                        + contents.capacity()
                            * size_of::<gmux_protocol::clipboard::Representation>()
                        + contents
                            .iter()
                            .map(|c| c.data.capacity() + c.mime.capacity())
                            .sum::<usize>()
                        + result
                            .contents
                            .iter()
                            .map(|r| r.mime.capacity())
                            .sum::<usize>()
                }
            }
    }

    #[cfg(test)]
    fn write(self, writer: &mut Vec<u8>, heartbeat: &mut Heartbeat) -> anyhow::Result<()> {
        runtime::block_on(self.write_async(writer, heartbeat))
    }
    async fn write_async(
        self,
        writer: &mut (impl AsyncWrite + Unpin),
        heartbeat: &mut Heartbeat,
    ) -> anyhow::Result<()> {
        match self {
            Self::Command(bytes) => writer.write_all(&bytes).await?,
            Self::ClipboardReply {
                pane,
                result,
                contents,
            } => {
                let id = result.id;
                write_command(
                    writer,
                    &StreamRequest::pane(pane, StreamCommand::ClipboardReadResult { result }),
                )
                .await?;
                for c in contents {
                    for bytes in c.data.chunks(gmux_protocol::clipboard::READ_CHUNK) {
                        write_command(
                            writer,
                            &StreamRequest::pane(
                                pane,
                                StreamCommand::ClipboardReadData {
                                    id,
                                    size: bytes.len(),
                                    bytes: vec![],
                                },
                            ),
                        )
                        .await?;
                        writer.write_all(bytes).await?;
                        heartbeat.check(writer).await?;
                    }
                }
                write_command(
                    writer,
                    &StreamRequest::pane(pane, StreamCommand::ClipboardReadEnd { id }),
                )
                .await?;
            }
            Self::Paste {
                pane,
                text,
                options,
                user,
            } => write_paste(writer, pane, &text, options, user, heartbeat).await?,
        }
        heartbeat.check(writer).await
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
        if let Some(task) = &self.task {
            task.abort();
        }
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
    fn send(&self, request: StreamRequest) -> anyhow::Result<()> {
        self.enqueue(Outgoing::prepare(request)?)
    }
    fn enqueue(&self, outgoing: Outgoing) -> anyhow::Result<()> {
        let budget = outgoing_budget(outgoing.is_control());
        self.enqueue_with_budget(outgoing, budget)
    }
    fn enqueue_with_budget(
        &self,
        outgoing: Outgoing,
        budget: Arc<tokio::sync::Semaphore>,
    ) -> anyhow::Result<()> {
        let bytes = outgoing.bytes();
        // Bulk input cannot consume the allowance for keys, control commands,
        // or clipboard read chunks. Rejection leaves ownership with this call
        // and is reported to the caller, just like the local queue ceiling.
        let retained = outgoing.retained_bytes();
        let reserved_slots = (self.commands.max_capacity() / 8).min(8);
        anyhow::ensure!(
            outgoing.is_control() || self.commands.capacity() > reserved_slots,
            "Terminal bulk input queue is full"
        );
        let shared = budget
            .try_acquire_many_owned(retained.try_into()?)
            .map_err(|_| anyhow::anyhow!("Process terminal input budget is full"))?;
        let local_limit =
            2 * transport::MAX_PASTE + if outgoing.is_control() { 64 * 1024 } else { 0 };
        self.pending_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                (pending + bytes <= local_limit).then_some(pending + bytes)
            })
            .map_err(|_| anyhow::anyhow!("Terminal input queue is full"))?;
        let queued = Queued {
            outgoing,
            reservation: Reservation {
                bytes,
                pending: self.pending_bytes.clone(),
                _shared: shared,
            },
        };
        self.commands
            .try_send(queued)
            .map_err(|error| anyhow::anyhow!("Could not queue daemon command: {error}"))?;
        Ok(())
    }
}

async fn write_command(
    writer: &mut (impl AsyncWrite + Unpin),
    request: &StreamRequest,
) -> anyhow::Result<()> {
    writer
        .write_all(&transport::command_bytes(request)?)
        .await?;
    Ok(())
}

async fn write_paste(
    writer: &mut (impl AsyncWrite + Unpin),
    pane: Option<u64>,
    text: &str,
    options: PasteOptions,
    user: Option<(u64, u64)>,
    heartbeat: &mut Heartbeat,
) -> anyhow::Result<()> {
    write_command(
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
    )
    .await?;
    let mut start = 0;
    while start < text.len() {
        let end = text.floor_char_boundary((start + transport::PASTE_CHUNK).min(text.len()));
        write_command(
            writer,
            &StreamRequest {
                pane,
                command: StreamCommand::PasteData {
                    text: text[start..end].to_owned(),
                },
            },
        )
        .await?;
        start = end;
        heartbeat.check(writer).await?;
    }
    write_command(
        writer,
        &StreamRequest {
            pane,
            command: StreamCommand::PasteEnd,
        },
    )
    .await
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
    focused: ValueCell<bool>,
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

async fn read_packets(
    reader: impl tokio::io::AsyncRead + Unpin,
    sender: &incoming::Sender,
    wake: &gmux_platform::unix::ipc::Wake,
    timeout: Duration,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut reader = tokio::io::BufReader::new(reader);
    loop {
        // Read the fixed prefix before admission. Idle/partial prefixes and
        // heartbeat acknowledgements need no allocation or decode reservation.
        // BufReader retains at most its fixed 8 KiB outside the packet budget.
        let mut prefix = [0; 4];
        runtime::deadline(Instant::now() + timeout, async {
            reader.read_exact(&mut prefix).await?;
            Ok(())
        })
        .await?;
        if transport::Record::is_heartbeat(prefix)? {
            continue;
        }
        let deadline = Instant::now() + timeout;
        let mut framing = [0; transport::RECORD_PREFIX_BYTES];
        framing[..4].copy_from_slice(&prefix);
        runtime::deadline(deadline, async {
            reader.read_exact(&mut framing[4..]).await?;
            Ok(())
        })
        .await?;
        let record = transport::Record::decode(framing)?;
        let reservation = sender.reserve_decode(record.decode_bytes()).await?;
        let (packet, reservation, retained) = runtime::deadline(Instant::now() + timeout, async {
            // read_buf initializes only bytes received. Avoid clearing large
            // header allocations immediately before the socket overwrites them.
            let mut bytes = Vec::with_capacity(record.header_size());
            let mut header_reader = (&mut reader).take(record.header_size() as u64);
            while bytes.len() < record.header_size() {
                if header_reader.read_buf(&mut bytes).await? == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                }
            }
            // The blocking closure owns the credit. Cancellation cannot release
            // it while an uninterruptible parse still holds decoded allocations.
            let parse_on_worker = bytes.len() > 8 * 1024
                || (matches!(
                    record.kind(),
                    transport::RecordKind::Clipboard | transport::RecordKind::ClipboardRead
                ) && record.decode_bytes() > 64 * 1024);
            let (decoded, reservation) = if parse_on_worker {
                runtime::blocking(move || (record.parse_header(&bytes), reservation)).await
            } else {
                (record.parse_header(&bytes), reservation)
            };
            let packet =
                Packet::read_body_async(&mut (&mut reader).compat(), record, decoded?).await?;
            let (result, packet, reservation) = if record.decode_bytes() > 64 * 1024 {
                runtime::blocking(move || (record.validate_packet(&packet), packet, reservation))
                    .await
            } else {
                (record.validate_packet(&packet), packet, reservation)
            };
            Ok((packet, reservation, result?))
        })
        .await?;
        sender.send_decoded(packet, reservation, retained).await?;
        wake.notify();
    }
}

async fn write_commands(
    mut writer: impl AsyncWrite + Unpin,
    queue: &mut tokio::sync::mpsc::Receiver<Queued>,
    wake: &gmux_platform::unix::ipc::Wake,
    interval: Duration,
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut heartbeat = Heartbeat::new(interval);
    loop {
        match tokio::time::timeout(heartbeat.remaining(), queue.recv()).await {
            Ok(None) => return Ok(()),
            Ok(Some(Queued {
                outgoing,
                reservation,
            })) => {
                runtime::deadline(
                    Instant::now() + timeout,
                    outgoing.write_async(&mut writer, &mut heartbeat),
                )
                .await?;
                drop(reservation);
                wake.notify();
            }
            Err(_) => {
                runtime::deadline(Instant::now() + timeout, heartbeat.check(&mut writer)).await?
            }
        }
    }
}

impl SessionManager {
    /// Connect to an existing daemon without starting a local process or reading
    /// its execution files. Remote tunnels can supply their own endpoint.
    pub fn connect_at(
        mut endpoint: ipc::Endpoint,
        wake: gmux_platform::unix::ipc::Wake,
    ) -> anyhow::Result<Self> {
        let hello = endpoint.call(InternalRequest::<u64, u64>::Hello)?;
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
        runtime::block_on(async {
            let mut socket = crate::io::Socket::from_std(socket.try_clone()?)?;
            let result = runtime::deadline(
                Instant::now() + gmux_protocol::network::TIMEOUT,
                write_command(
                    &mut socket.split().1,
                    &StreamRequest::connection(StreamCommand::Attach {
                        version: transport::VERSION,
                        daemon,
                        remote: false,
                    }),
                ),
            )
            .await;
            socket.into_std()?;
            result
        })
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
        let socket_copy = socket.try_clone()?;
        let (commands, mut queue) = tokio::sync::mpsc::channel::<Queued>(128);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = incoming::channel(incoming::MAX_BYTES, 64);
        let task_wake = wake.clone();
        let task = runtime::spawn(async move {
            let result = async {
                let mut socket = crate::io::Socket::from_std(socket_copy)?;
                let result = {
                    let (reader, writer) = socket.split();
                    tokio::select! {
                        result = read_packets(reader, &sender, &task_wake, timeout) => result,
                        result = write_commands(writer, &mut queue, &task_wake, interval, timeout) => result,
                    }
                };
                // Cached views can retain the UI's descriptor after a failure.
                socket.shutdown();
                result
            }.await;
            // Release queued data before waiting to report an error to the UI.
            drop(queue);
            if let Err(error) = result {
                let message = if error.downcast_ref::<incoming::SendError>()
                    == Some(&incoming::SendError::TooLarge)
                {
                    "Lost connection to the terminal daemon: Viewport packet exceeds the frontend memory budget".into()
                } else {
                    stream_error(error)
                };
                let _ = sender.send_async(Err(message)).await;
                task_wake.notify();
            }
        });
        Ok(Self {
            endpoint,
            daemon,
            connection: Rc::new(Connection {
                clipboard_epochs: Default::default(),
                commands,
                task: Some(task.abort_handle()),
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
            focused: ValueCell::new(false),
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
    pub fn execution_state(&mut self) -> anyhow::Result<ExecutionSnapshot> {
        let reply: gmux_protocol::control::CommandReply =
            serde_json::from_value(self.endpoint.call(InternalRequest::<u64, u64>::State)?)?;
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
        self.focused.set(active);
        for session in self.sessions.values() {
            session.borrow().viewport.set_focused(active);
        }
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
            session.borrow_mut().viewport.set_cadence(interval);
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
                session.viewport.install_metadata(meta.clone());
            } else {
                self.metadata.insert(meta.id, meta.clone());
            }
        }
    }
    pub fn set_visible(&mut self, ids: HashSet<u64>) -> anyhow::Result<()> {
        if self.visible != ids {
            for (id, session) in &self.sessions {
                if !ids.contains(id) {
                    session.borrow_mut().viewport.clear_cache();
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
        let sequence = session.viewport.mark_read();
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
            viewport: Viewport::new(meta, theme, self.frame_interval, self.focused.get()),
            wake: self.wake.clone(),
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
        clipboard: impl FnMut(gmux_protocol::clipboard::Event),
    ) -> HashMap<u64, SessionUpdate> {
        let mut budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 256);
        self.poll_with_budget(&mut budget, clipboard)
    }
    /// Apply ordered packets within the caller's event-loop budget. A connection
    /// yields after 16 packets even when the shared budget has time left.
    pub fn poll_with_budget(
        &mut self,
        budget: &mut gmux_platform::work::Budget,
        mut clipboard: impl FnMut(gmux_protocol::clipboard::Event),
    ) -> HashMap<u64, SessionUpdate> {
        // Reserve the first packet before maintenance. Even a large set of
        // scroll targets must not spend every turn's time before applying data.
        let first = budget.take();
        // A full command queue must not lose a tab's viewport subscription.
        let _ = self.subscribe();
        self.flush_reads();
        for session in self.sessions.values() {
            let session = session.borrow();
            // Writer progress wakes the manager. Until then, a full queue
            // must not keep an expired deadline spinning the UI event loop.
            let scroll = session.viewport.retry_scroll(Instant::now());
            let _ = session.enqueue_scroll(scroll);
        }
        let mut updates: HashMap<u64, SessionUpdate> = HashMap::new();
        for index in 0..16 {
            if !(if index == 0 { first } else { budget.take() }) {
                break;
            }
            let (received, _reservation) = match self.receiver.try_recv_reserved() {
                Ok((item, reservation)) => (Ok(item), Some(reservation)),
                Err(error) => (Err(error), None),
            };
            match received {
                Ok(Ok(packet)) => {
                    let packet = match packet {
                        Packet::Clipboard(write) => {
                            clipboard(gmux_protocol::clipboard::Event::Write(write));
                            continue;
                        }
                        Packet::ClipboardRead(read) => {
                            clipboard(gmux_protocol::clipboard::Event::Read {
                                pane: read.pane,
                                event: read.event,
                            });
                            continue;
                        }
                        Packet::Git(git) => {
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
                        Packet::Execution(state) => {
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
                        Packet::Pane(update) => update,
                    };
                    let id = packet.header.meta.id;
                    if let Some(session) = self.sessions.get(&id) {
                        let mut session = session.borrow_mut();
                        let changes = session.viewport.apply(
                            packet,
                            self.visible.contains(&id),
                            Instant::now(),
                        );
                        merge_presentation(updates.entry(id).or_default(), changes.presentation);
                        let _ = session.enqueue_scroll(changes.scroll);
                    } else if packet.header.meta.exited || !packet.header.notifications.is_empty() {
                        let update = updates.entry(id).or_default();
                        update.exited |= packet.header.meta.exited;
                        update.notifications.extend(packet.header.notifications);
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
        if !self.disconnected && self.receiver.pending() {
            self.wake.notify();
        }
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
            .filter_map(|session| session.borrow().viewport.scroll_release())
            .min()
    }
    pub fn disconnect_error(&self) -> Option<&anyhow::Error> {
        self.disconnect_error.as_ref()
    }
    pub fn close(&mut self, id: u64) {
        if let Some(session) = self.sessions.remove(&id) {
            session.borrow_mut().viewport.clear_cache();
        }
        self.metadata.remove(&id);
        self.git.remove(&id);
        self.git_subscription_pending |= self.git_panes.remove(&id);
        self.pending_reads.remove(&id);
    }
    pub fn clear(&mut self) {
        for session in self.sessions.values() {
            let mut session = session.borrow_mut();
            session.git = None;
            session.viewport.clear_cache();
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
    viewport: Viewport,
    git: Option<transport::GitMetadata>,
    wake: gmux_platform::unix::ipc::Wake,
}
impl RemoteSession {
    pub fn branch(&self) -> &str {
        self.git
            .as_ref()
            .filter(|git| git.directory == self.viewport.meta().directory)
            .map_or("", |git| git.branch.as_str())
    }
    pub fn text(&self) -> String {
        let screen = self.snapshot();
        gmux_protocol::cells::viewport_text(screen.frame, screen.cells)
    }
    fn command(&self, command: StreamCommand) -> anyhow::Result<()> {
        if Viewport::command_needs_scroll(&command) {
            self.send_scroll()?;
        }
        self.connection
            .send(StreamRequest::pane(self.id(), command))
    }
    pub fn id(&self) -> u64 {
        self.viewport.meta().id
    }
    pub fn directory(&self) -> &Path {
        &self.viewport.meta().directory
    }
    pub fn size(&self) -> (u16, u16) {
        self.viewport.meta().size
    }
    pub fn exited(&self) -> bool {
        self.viewport.meta().exited
    }
    pub fn unread(&self) -> bool {
        self.viewport.unread()
    }
    pub fn progress(&self) -> gmux_protocol::progress::Progress {
        self.viewport.meta().progress
    }
    pub fn program_status(&self) -> Option<&gmux_protocol::program_status::Summary> {
        self.viewport.meta().program_status.as_ref()
    }
    pub fn busy(&self) -> bool {
        self.viewport.meta().busy
    }
    pub fn title(&self) -> &str {
        &self.viewport.meta().title
    }
    pub fn mode(&self, mode: i32) -> bool {
        match mode {
            0 => self.viewport.meta().modes[0],
            1004 => self.viewport.meta().modes[1],
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
        self.viewport.find_state()
    }
    pub fn select(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        mode: gmux_protocol::transport::SelectionMode,
    ) -> anyhow::Result<()> {
        let serial = self.viewport.next_selection_serial();
        self.command(StreamCommand::Select {
            start,
            end,
            mode,
            serial,
        })?;
        self.viewport.selection_enqueued(serial);
        Ok(())
    }
    pub fn copy(&self, formats: gmux_protocol::clipboard::Formats) -> anyhow::Result<()> {
        self.command(StreamCommand::Copy { formats })
    }
    pub fn command_state(&self) -> &gmux_protocol::shell::CommandState {
        &self.viewport.meta().command
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
        let serial = self.viewport.next_selection_serial();
        self.command(StreamCommand::SelectOutput {
            point,
            anchored,
            copy,
            serial,
        })?;
        self.viewport.selection_enqueued(serial);
        Ok(())
    }
    pub fn prompt(&mut self, previous: bool) -> anyhow::Result<()> {
        self.send_scroll()?;
        let serial = self.viewport.next_prompt_serial();
        self.command(StreamCommand::Prompt { previous, serial })?;
        self.viewport.prompt_enqueued(serial);
        Ok(())
    }
    pub fn clear_selection(&self) -> anyhow::Result<()> {
        let serial = self.viewport.next_selection_serial();
        self.command(StreamCommand::ClearSelection { serial })?;
        self.viewport.selection_enqueued(serial);
        Ok(())
    }
    pub fn selection_pending(&self) -> bool {
        self.viewport.selection_pending()
    }
    pub fn selection_origin(&self) -> Option<u64> {
        self.viewport.selection_origin()
    }
    pub fn clear_screen(&mut self) -> anyhow::Result<()> {
        self.command(StreamCommand::ClearScreen)?;
        self.viewport.clear_cache();
        Ok(())
    }
    pub fn selection(&self) -> Option<Selection> {
        self.viewport.selection()
    }
    pub fn scroll_to(&mut self, row: u64) {
        let changes = self.viewport.scroll_to(row, Instant::now());
        let _ = self.enqueue_scroll(changes.scroll);
        // Arrange a main-loop turn to schedule a deferred deadline, even if the
        // preceding writer wake has already been consumed.
        if self.viewport.forced_scroll().is_some() {
            self.wake.notify();
        }
    }
    fn enqueue_scroll(&self, scroll: Option<viewport::ScrollCommand>) -> anyhow::Result<()> {
        if let Some(scroll) = scroll {
            let result = self
                .connection
                .send(StreamRequest::pane(self.id(), scroll.command()));
            self.viewport.scroll_enqueued(
                scroll,
                Instant::now(),
                if result.is_ok() {
                    viewport::EnqueueOutcome::Accepted
                } else {
                    viewport::EnqueueOutcome::Rejected
                },
            );
            result?;
        }
        Ok(())
    }
    fn send_scroll(&self) -> anyhow::Result<()> {
        self.enqueue_scroll(self.viewport.forced_scroll())
    }
    pub fn scroll_pending(&self) -> bool {
        self.viewport.scroll_pending()
    }
    pub fn scroll_cache_bytes(&self) -> usize {
        self.viewport.scroll_cache_bytes()
    }
    pub fn set_cursor_style(&self, style: CursorStyle, blink: bool) -> anyhow::Result<()> {
        self.command(StreamCommand::Style { style, blink })
    }
    pub fn set_theme(&mut self, theme: &Theme) -> anyhow::Result<()> {
        self.command(StreamCommand::Theme { theme: *theme })?;
        self.viewport.clear_cache();
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
        self.viewport.resized(cols, rows);
        Ok(())
    }
    pub fn snapshot(&self) -> ScreenSnapshot<'_> {
        self.viewport.snapshot()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod admission_benchmark;
#[cfg(test)]
mod admission_tests;

fn merge_presentation(current: &mut SessionUpdate, next: SessionUpdate) {
    current.changed |= next.changed;
    current.exited |= next.exited;
    current.directory_changed |= next.directory_changed;
    current.git_changed |= next.git_changed;
    current.progress_changed |= next.progress_changed;
    current.program_status_changed |= next.program_status_changed;
    current.unread = next.unread.or(current.unread);
    if next.error.is_some() {
        current.error = next.error;
    }
    current.notifications.extend(next.notifications);
}
