//! Headless owner of PTYs, terminal engines, and the reconnectable live layout.
use crate::{
    persistence::{Save, Worker},
    session::TerminalSession,
};
use gmux_client::{
    Cell, Frame,
    control::{CommandOutput, CommandResult},
    ipc,
    model::{self, CursorStyle, DaemonId, ExecutionState, ExecutionTab, PaneRef, PaneTree, TabRef},
    storage,
    theme::Theme,
    transport::{self, Header, Metadata, Packet},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod commands;
mod event_loop;
mod frontend;
mod network;
mod remote_access;

use commands::{ControlRequest, PendingControl};
pub use event_loop::run;
use frontend::Frontend;

const MAX_PENDING: usize = 32 * 1024 * 1024;
const MAX_NOTIFICATIONS: usize = 64;
const MAX_NOTIFICATION_BYTES: usize = 512 * 1024;

#[derive(Default)]
struct NotificationQueue {
    messages: VecDeque<String>,
    bytes: usize,
}
impl NotificationQueue {
    fn push(&mut self, mut message: String) {
        message.truncate(message.floor_char_boundary(MAX_NOTIFICATION_BYTES));
        // Leave room for JSON escaping alongside a full viewport and Find data.
        while self.messages.len() >= MAX_NOTIFICATIONS
            || self.bytes + message.len() > MAX_NOTIFICATION_BYTES
        {
            self.bytes -= self.messages.pop_front().unwrap().len();
        }
        self.bytes += message.len();
        self.messages.push_back(message);
    }
}

fn poll_timeout(deadline: Option<Instant>, now: Instant) -> i32 {
    deadline.map_or(-1, |deadline| {
        // A positive fraction of a millisecond must still sleep, not spin.
        deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32
    })
}

fn reserve_id_range(last_id: &mut u64, count: u64) -> anyhow::Result<u64> {
    let last = last_id
        .checked_add(count)
        .filter(|id| *id < u64::MAX)
        .ok_or_else(|| anyhow::anyhow!("No IDs remain"))?;
    let first = *last_id + 1;
    *last_id = last;
    Ok(first)
}

fn creation_timestamp(previous: u64, now: SystemTime) -> u64 {
    let micros = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    (micros.min(u64::MAX as u128) as u64).max(previous.saturating_add(1))
}

struct Daemon {
    sessions: HashMap<u64, TerminalSession>,
    id: DaemonId,
    remote_access: remote_access::RemoteAccess,
    state: ExecutionState,
    frontends: HashMap<u64, Frontend>,
    next_frontend: u64,
    revision: u64,
    changed: HashSet<u64>,
    notifications: HashMap<u64, NotificationQueue>,
    errors: HashMap<u64, String>,
    unread: HashMap<u64, u64>,
    alert_sequence: u64,
    quitting: bool,
    snapshots: u64,
    sent_rows: u64,
    started: Instant,
    pending_write: Option<Save>,
    checkpointed: bool,
    git: Option<crate::branch::Service>,
    git_dirty: bool,
}
impl Daemon {
    fn sync_git(&mut self, wake: &ipc::Wake) {
        if !std::mem::take(&mut self.git_dirty) {
            return;
        }
        let wanted: HashMap<_, _> = self
            .frontends
            .values()
            .filter(|frontend| frontend.attached)
            .flat_map(|frontend| &frontend.git_panes)
            .filter_map(|id| {
                self.sessions
                    .get(id)
                    .map(|session| (*id, session.directory().to_path_buf()))
            })
            .collect();
        if wanted.is_empty() {
            self.git = None;
        } else {
            self.git
                .get_or_insert_with(|| crate::branch::Service::new(wake.clone()))
                .sync(wanted);
        }
    }
    fn new() -> anyhow::Result<Self> {
        let mut execution = storage::load::<ExecutionState>("execution.json")?;
        let id = crate::persistence::daemon_id()?;
        let scrollback = std::mem::take(&mut execution.scrollback);
        let last_id = execution
            .tabs
            .iter()
            .flat_map(|tab| std::iter::once(tab.id).chain(tab.layout.ids()))
            .filter(|id| *id < u64::MAX)
            .chain(std::iter::once(execution.last_id))
            .max()
            .unwrap_or(0);
        let saved = execution;
        let mut daemon = Self {
            sessions: HashMap::new(),
            id,
            remote_access: Default::default(),
            state: ExecutionState {
                last_id,
                ..Default::default()
            },
            frontends: HashMap::new(),
            next_frontend: 0,
            revision: 0,
            changed: HashSet::new(),
            notifications: HashMap::new(),
            errors: HashMap::new(),
            unread: HashMap::new(),
            alert_sequence: 0,
            quitting: false,
            snapshots: 0,
            sent_rows: 0,
            started: Instant::now(),
            pending_write: None,
            checkpointed: false,
            git: None,
            git_dirty: false,
        };
        let mut used = HashSet::new();
        let mut history_bytes = 0;
        let mut history_panes = 0;
        for tab in saved.tabs {
            if let Err(e) = tab.validate() {
                eprintln!("Skipping invalid saved tab: {e}");
                continue;
            }
            let ids = tab.layout.ids();
            if used.contains(&tab.id) || ids.iter().any(|id| used.contains(id)) {
                eprintln!("Skipping duplicate saved IDs");
                continue;
            }
            let mut spawned = Vec::new();
            let result = (|| -> anyhow::Result<()> {
                for id in &ids {
                    let directory = tab
                        .directories
                        .get(id)
                        .filter(|p| p.is_dir())
                        .cloned()
                        .unwrap_or_else(model::home);
                    let history = scrollback.get(id).and_then(|name| {
                        if history_panes >= crate::scrollback::MAX_PANES {
                            eprintln!(
                                "Skipping scrollback for pane {id}: restore pane limit reached"
                            );
                            return None;
                        }
                        history_panes += 1;
                        match crate::scrollback::load(
                            *id,
                            name,
                            crate::scrollback::MAX_TOTAL_BYTES - history_bytes,
                        ) {
                            Ok(bytes) => {
                                history_bytes += bytes.len();
                                Some(bytes)
                            }
                            Err(error) => {
                                eprintln!("{error:#}. Starting with empty history");
                                None
                            }
                        }
                    });
                    let session = TerminalSession::spawn(
                        *id,
                        tab.id,
                        directory,
                        CursorStyle::default(),
                        &Theme::adwaita(true),
                        history.as_deref(),
                    )?;
                    if history.is_some()
                        && let Some(name) = scrollback.get(id)
                    {
                        daemon.state.scrollback.insert(*id, name.clone());
                    }
                    daemon.sessions.insert(*id, session);
                    spawned.push(*id);
                }
                Ok(())
            })();
            if let Err(e) = result {
                for id in spawned {
                    daemon.sessions.remove(&id);
                }
                eprintln!("Could not restore tab: {e}");
                continue;
            }
            used.insert(tab.id);
            used.extend(ids);
            daemon.state.tabs.push(tab);
        }
        daemon.state.last_id = daemon
            .state
            .last_id
            .max(used.into_iter().max().unwrap_or(0));
        daemon
            .state
            .scrollback
            .retain(|id, _| daemon.sessions.contains_key(id));
        Ok(daemon)
    }
    fn metadata(session: &TerminalSession, unread_sequence: u64) -> Metadata {
        Metadata {
            id: session.id(),
            directory: session.directory().to_path_buf(),
            title: session.title(),
            size: session.size(),
            exited: session.exited(),
            unread_sequence,
            busy: session.busy(),
            modes: [session.mode(0), session.mode(1004)],
        }
    }
    fn queue_notifications(&mut self, id: u64, messages: impl IntoIterator<Item = String>) {
        let pending = self.notifications.entry(id).or_default();
        for message in messages {
            pending.push(message);
            self.alert_sequence = self.alert_sequence.wrapping_add(1).max(1);
            self.unread.insert(id, self.alert_sequence);
        }
    }
    fn resolve_pane(&self, pane: Option<u64>) -> anyhow::Result<u64> {
        let id = pane
            .or_else(|| (self.sessions.len() == 1).then(|| *self.sessions.keys().next().unwrap()))
            .ok_or_else(|| anyhow::anyhow!("Specify a pane when more than one terminal exists"))?;
        anyhow::ensure!(self.sessions.contains_key(&id), "Pane {id} does not exist");
        Ok(id)
    }
    fn reserve_ids(&mut self, count: u64) -> anyhow::Result<u64> {
        // Closed panes can still appear in queued packets and command replies.
        // Persist the high-water mark across restarts, too.
        reserve_id_range(&mut self.state.last_id, count)
    }
    fn close(&mut self, id: u64) {
        self.git_dirty |= self.git.is_some();
        self.sessions.remove(&id);
        self.state.scrollback.remove(&id);
        self.unread.remove(&id);
        self.notifications.remove(&id);
        self.errors.remove(&id);
        self.changed.remove(&id);
        for frontend in self.frontends.values_mut() {
            frontend.git_panes.remove(&id);
            frontend.git_sent.remove(&id);
            frontend.visible.remove(&id);
            frontend.views.remove(&id);
            frontend.clipboard.remove(&id);
            frontend.errors.remove(&id);
            if frontend
                .paste
                .as_ref()
                .is_some_and(|paste| paste.pane == id)
            {
                frontend.paste = None;
            }
            frontend.screens.remove(&id);
            frontend.sizes.remove(&id);
            frontend.changed.remove(&id);
            frontend.metadata.remove(&id);
            frontend.scroll_serials.remove(&id);
            frontend.sent_scroll_serials.remove(&id);
            frontend.find.remove(&id);
            frontend.selections.remove(&id);
            frontend.selection_serials.remove(&id);
            frontend.sent_selection_serials.remove(&id);
        }
        self.state.tabs.retain_mut(|tab| {
            if !tab.layout.contains(id) {
                return true;
            }
            let Some(layout) = tab.layout.clone().remove(id) else {
                return false;
            };
            tab.layout = layout;
            tab.directories.remove(&id);
            true
        });
    }
    fn refresh_state(&mut self) {
        for tab in &mut self.state.tabs {
            for id in tab.layout.ids() {
                if let Some(session) = self.sessions.get_mut(&id) {
                    session.refresh_directory();
                    tab.directories
                        .insert(id, session.directory().to_path_buf());
                }
            }
        }
    }
    fn execution_snapshot(&mut self) -> ExecutionState {
        self.refresh_state();
        self.state.clone()
    }
    fn execution_state(&self) -> gmux_client::control::CommandReply {
        let mut execution = self.state.clone();
        execution.scrollback.clear();
        gmux_client::control::CommandReply {
            daemon: self.id,
            home_directory: model::home(),
            machine_kind: model::MachineKind::local(),
            hostname: model::hostname().into(),
            revision: self.revision,
            result: CommandOutput::Done { ok: true }.into(),
            execution,
            metadata: self
                .sessions
                .iter()
                .map(|(&id, session)| {
                    Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0))
                })
                .collect(),
        }
    }
    fn state_save(&mut self) -> Save {
        Save::Execution(self.execution_snapshot())
    }
    fn prepare_state_save(&mut self) {
        self.pending_write = Some(self.state_save());
    }

    fn prepare_checkpoint(&mut self) -> anyhow::Result<crate::scrollback::Checkpoint> {
        let mut panes = Vec::new();
        let mut warnings = Vec::new();
        let mut bytes = 0;
        // Layout order makes the aggregate cap deterministic.
        for (index, id) in self
            .state
            .tabs
            .iter()
            .flat_map(|tab| tab.layout.ids())
            .enumerate()
        {
            if index == crate::scrollback::MAX_PANES {
                warnings.push("Further panes omitted: checkpoint pane limit reached".into());
                break;
            }
            let Some(session) = self.sessions.get_mut(&id) else {
                continue;
            };
            session.poll();
            match session.checkpoint() {
                Ok(snapshot) if bytes + snapshot.len() <= crate::scrollback::MAX_TOTAL_BYTES => {
                    bytes += snapshot.len();
                    panes.push((id, snapshot));
                }
                Ok(_) => warnings.push(format!("Pane {id}: checkpoint budget reached")),
                Err(error) => warnings.push(format!("Pane {id}: {error}")),
            }
        }
        crate::scrollback::Checkpoint::new(self.execution_snapshot(), panes, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_ranges_reserve_both_new_ids_and_preserve_the_last_valid_id_after_failure() {
        let mut last = u64::MAX - 3;
        assert_eq!(reserve_id_range(&mut last, 2).unwrap(), u64::MAX - 2);
        assert_eq!(last, u64::MAX - 1);
        assert!(reserve_id_range(&mut last, 1).is_err());
        assert_eq!(last, u64::MAX - 1);

        let mut last = u64::MAX - 2;
        assert!(reserve_id_range(&mut last, 2).is_err());
        assert_eq!(
            last,
            u64::MAX - 2,
            "A failed New must not reserve half its IDs"
        );
        assert_eq!(reserve_id_range(&mut last, 1).unwrap(), u64::MAX - 1);
        assert!(reserve_id_range(&mut last, 1).is_err());
    }

    #[test]
    fn creation_timestamps_increase_when_the_clock_repeats_or_moves_backwards() {
        let now = UNIX_EPOCH + Duration::from_micros(900);
        assert_eq!(creation_timestamp(0, now), 900);
        assert_eq!(creation_timestamp(900, now), 901);
        assert_eq!(
            creation_timestamp(901, now - Duration::from_micros(100)),
            902
        );
    }

    #[test]
    fn notification_queues_keep_recent_messages_within_count_and_byte_limits() {
        let mut queue = NotificationQueue::default();
        for index in 0..100 {
            queue.push(format!("alert-{index}"));
        }
        assert_eq!(queue.messages.len(), MAX_NOTIFICATIONS);
        assert_eq!(queue.messages.front().unwrap(), "alert-36");
        assert_eq!(queue.messages.back().unwrap(), "alert-99");

        let unicode = "a".to_owned() + &"é".repeat(MAX_NOTIFICATION_BYTES);
        queue.push(unicode.clone());
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.bytes, MAX_NOTIFICATION_BYTES - 1);
        assert_eq!(queue.messages[0], unicode[..MAX_NOTIFICATION_BYTES - 1]);
        queue.push("latest".into());
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.bytes, "latest".len());
        assert_eq!(queue.messages[0], "latest");
    }

    #[test]
    fn escaped_alerts_fit_alongside_a_maximum_viewport_and_find_highlights() {
        let mut queue = NotificationQueue::default();
        queue.push("\x01".repeat(MAX_NOTIFICATION_BYTES));
        let packet = Packet {
            header: Header {
                git: None,
                state: None,
                meta: Metadata {
                    id: 1,
                    directory: PathBuf::from("/tmp"),
                    title: "shell".into(),
                    size: (512, 256),
                    exited: false,
                    unread_sequence: 1,
                    busy: false,
                    modes: [false; 2],
                },
                scroll_serial: 0,
                frame: Some(Frame {
                    cols: 512,
                    rows: 256,
                    ..Frame::default()
                }),
                rows: (0..256).collect(),
                notifications: queue.messages.into_iter().collect(),
                error: None,
                find: Some(gmux_client::search::FindState {
                    current: 1,
                    total: gmux_client::viewport::MAX_CELLS,
                    pending: false,
                    highlights: (0..gmux_client::viewport::MAX_CELLS)
                        .map(|i| (i, i + 1, 1))
                        .collect(),
                }),
                selection: None,
                copy: None,
                ..Header::default()
            },
            cells: vec![Cell::default(); gmux_client::viewport::MAX_CELLS],
        };
        packet.encode_into(&mut Vec::new()).unwrap();
    }

    #[test]
    fn poll_deadlines_round_up_positive_waits_and_bound_long_waits() {
        let now = Instant::now();
        assert_eq!(poll_timeout(None, now), -1);
        assert_eq!(poll_timeout(Some(now), now), 0);
        assert_eq!(poll_timeout(Some(now - Duration::from_millis(1)), now), 0);
        for (duration, expected) in [
            (Duration::from_nanos(1), 1),
            (Duration::from_millis(1), 1),
            (Duration::from_micros(1001), 2),
            (Duration::from_millis(i32::MAX as u64 + 1), i32::MAX),
        ] {
            assert_eq!(poll_timeout(Some(now + duration), now), expected);
        }
    }

    #[test]
    fn queued_packets_survive_partial_writes_and_buffer_compaction() {
        let (socket, mut reader) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let size = 4096i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const i32).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let mut frontend = Frontend::new(socket);
        frontend.attached = true;
        let packet = |text| {
            let mut cell = Cell::default();
            cell.text[0] = text;
            Packet {
                header: Header {
                    git: None,
                    state: None,
                    meta: Metadata {
                        id: 1,
                        directory: PathBuf::from("/tmp"),
                        title: "shell".into(),
                        size: (80, 24),
                        exited: false,
                        unread_sequence: 0,
                        busy: false,
                        modes: [false; 2],
                    },
                    scroll_serial: 0,
                    frame: Some(Frame {
                        cols: 80,
                        rows: 24,
                        ..Frame::default()
                    }),
                    rows: (0..24).collect(),
                    notifications: Vec::new(),
                    error: None,
                    find: None,
                    selection: None,
                    copy: None,
                    ..Header::default()
                },
                cells: vec![cell; 80 * 24],
            }
        };
        let mut expected = Vec::new();
        packet(b'a').encode_into(&mut expected).unwrap();
        packet(b'b').encode_into(&mut expected).unwrap();
        frontend.queue(packet(b'a')).unwrap();
        frontend.flush().unwrap();
        assert!(frontend.offset > 0 && frontend.offset < frontend.output.len());
        let mut received = Vec::new();
        let mut buffer = [0; 16384];
        let mut queued_second = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < expected.len() {
            assert!(Instant::now() < deadline, "Timed out draining packets");
            match reader.read(&mut buffer) {
                Ok(n) if n > 0 => received.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now()
                }
                result => panic!("Unexpected socket read: {result:?}"),
            }
            if !queued_second && frontend.offset >= frontend.output.len() / 2 {
                let mut bytes = Vec::new();
                packet(b'b').encode_into(&mut bytes).unwrap();
                frontend.queue_encoded(&bytes).unwrap();
                assert_eq!(frontend.offset, 0, "Consumed bytes should be compacted");
                queued_second = true;
            }
            frontend.flush().unwrap();
        }
        assert!(queued_second);
        assert_eq!(received, expected);
        assert!(frontend.output.is_empty());
    }
}
