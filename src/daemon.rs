//! Headless owner of PTYs, terminal engines, and the reconnectable live layout.
use crate::{
    Cell, Frame,
    control::{Command, CommandOutput, CommandResult},
    ipc,
    model::{self, Layout, SavedTab, Session, Settings, TabFilter, WorkspaceCatalog},
    persistence::{Save, Worker},
    session::TerminalSession,
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
    time::{Duration, Instant},
};

mod commands;
mod event_loop;
mod frontend;

use commands::{ControlRequest, PendingControl, PreparedSave};
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

struct Daemon {
    sessions: HashMap<u64, TerminalSession>,
    state: Session,
    frontends: HashMap<u64, Frontend>,
    next_frontend: u64,
    revision: u64,
    changed: HashSet<u64>,
    notifications: HashMap<u64, NotificationQueue>,
    errors: HashMap<u64, String>,
    unread: HashMap<u64, u64>,
    alert_sequence: u64,
    last_id: u64,
    quitting: bool,
    snapshots: u64,
    sent_rows: u64,
    started: Instant,
    pending_write: Option<PreparedSave>,
    checkpointed: bool,
}
impl Daemon {
    fn new() -> anyhow::Result<Self> {
        let settings = storage::load::<Settings>("settings.json")?;
        let saved = if settings.restore {
            storage::load::<Session>("session.json")?
        } else {
            Session::default()
        };
        let mut daemon = Self {
            sessions: HashMap::new(),
            state: Session {
                width: saved.width,
                height: saved.height,
                maximized: saved.maximized,
                filter: saved.filter,
                active: saved.active,
                ..Session::default()
            },
            frontends: HashMap::new(),
            next_frontend: 0,
            revision: 0,
            changed: HashSet::new(),
            notifications: HashMap::new(),
            errors: HashMap::new(),
            unread: HashMap::new(),
            alert_sequence: 0,
            last_id: 0,
            quitting: false,
            snapshots: 0,
            sent_rows: 0,
            started: Instant::now(),
            pending_write: None,
            checkpointed: false,
        };
        let catalog = storage::load_catalog()?;
        if let TabFilter::Workspace(id) = daemon.state.filter
            && !catalog
                .workspaces
                .iter()
                .any(|workspace| workspace.id == id)
        {
            daemon.state.filter = TabFilter::All;
        }
        let mut used: HashSet<_> = catalog.workspaces.iter().map(|w| w.id).collect();
        let mut history_bytes = 0;
        let mut history_panes = 0;
        for mut tab in saved.tabs {
            if let Err(e) = tab.validate() {
                eprintln!("Skipping invalid saved tab: {e}");
                continue;
            }
            tab.workspace = tab.workspace.filter(|id| {
                catalog
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == *id)
            });
            let ids = tab.layout.ids();
            if !ids.contains(&tab.active) {
                tab.active = ids[0];
            }
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
                    let history = saved.scrollback.get(id).and_then(|name| {
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
                        settings.cursor_style,
                        &Theme::adwaita(settings.appearance != "light"),
                        history.as_deref(),
                    )?;
                    if history.is_some()
                        && let Some(name) = saved.scrollback.get(id)
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
        daemon.last_id = used.into_iter().max().unwrap_or(0);
        daemon
            .state
            .scrollback
            .retain(|id, _| daemon.sessions.contains_key(id));
        daemon.select_visible();
        Ok(daemon)
    }
    fn metadata(session: &TerminalSession, unread: u64) -> Metadata {
        Metadata {
            id: session.id(),
            directory: session.directory().to_path_buf(),
            title: session.title(),
            size: session.size(),
            exited: session.exited(),
            unread,
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
            .or_else(|| {
                self.state
                    .tabs
                    .iter()
                    .find(|t| t.id == self.state.active)
                    .map(|t| t.active)
            })
            .ok_or_else(|| anyhow::anyhow!("No active terminal"))?;
        anyhow::ensure!(self.sessions.contains_key(&id), "Pane {id} does not exist");
        Ok(id)
    }
    fn reserve_ids(&mut self, count: u64) -> anyhow::Result<u64> {
        // Closed panes can still appear in queued packets and command replies.
        // Keep their IDs reserved until this daemon stops, across all ID types.
        self.last_id = storage::load_catalog()?
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .chain(std::iter::once(self.last_id))
            .max()
            .unwrap_or(0);
        reserve_id_range(&mut self.last_id, count)
    }
    fn select_visible(&mut self) {
        if !self
            .state
            .tabs
            .iter()
            .any(|tab| tab.id == self.state.active && self.state.filter.matches(tab.workspace))
        {
            self.state.active = self
                .state
                .tabs
                .iter()
                .find(|tab| self.state.filter.matches(tab.workspace))
                .map(|tab| tab.id)
                .unwrap_or(0);
        }
    }
    fn focus_tab(&mut self, id: u64) -> anyhow::Result<()> {
        let tab = self
            .state
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .ok_or_else(|| anyhow::anyhow!("Tab {id} does not exist"))?;
        if !self.state.filter.matches(tab.workspace) {
            self.state.filter = TabFilter::All;
        }
        self.state.active = id;
        Ok(())
    }
    fn close(&mut self, id: u64) {
        let active_index = self
            .state
            .tabs
            .iter()
            .filter(|tab| self.state.filter.matches(tab.workspace))
            .position(|tab| tab.id == self.state.active);
        self.sessions.remove(&id);
        self.state.scrollback.remove(&id);
        self.unread.remove(&id);
        self.notifications.remove(&id);
        self.errors.remove(&id);
        self.changed.remove(&id);
        for frontend in self.frontends.values_mut() {
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
        }
        self.state.tabs.retain_mut(|tab| {
            if !tab.layout.contains(id) {
                return true;
            }
            let Some(layout) = tab.layout.clone().remove(id) else {
                return false;
            };
            if tab.active == id {
                let old = tab.layout.ids();
                let index = old.iter().position(|pane| *pane == id).unwrap_or(0);
                let remaining = layout.ids();
                tab.active = remaining[index.min(remaining.len() - 1)];
            }
            tab.layout = layout;
            tab.directories.remove(&id);
            true
        });
        if !self
            .state
            .tabs
            .iter()
            .any(|tab| tab.id == self.state.active)
        {
            let visible: Vec<_> = self
                .state
                .tabs
                .iter()
                .filter(|tab| self.state.filter.matches(tab.workspace))
                .map(|tab| tab.id)
                .collect();
            self.state.active = visible
                .get(
                    active_index
                        .unwrap_or(0)
                        .min(visible.len().saturating_sub(1)),
                )
                .copied()
                .unwrap_or(0);
        }
        self.select_visible();
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
    fn shared_state(&self) -> anyhow::Result<crate::control::CommandReply> {
        Ok(crate::control::CommandReply {
            revision: self.revision,
            result: CommandOutput::Done { ok: true }.into(),
            session: self.state.clone(),
            catalog: storage::load_catalog()?,
            metadata: self
                .sessions
                .iter()
                .map(|(&id, s)| Self::metadata(s, self.unread.get(&id).copied().unwrap_or(0)))
                .collect(),
        })
    }
    fn session_snapshot(&mut self) -> Session {
        self.refresh_state();
        self.state.clone()
    }
    fn prepare_session_save(&mut self) {
        self.pending_write = Some(PreparedSave::Session(self.session_snapshot()));
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
        crate::scrollback::Checkpoint::new(self.session_snapshot(), panes, warnings)
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
                state: None,
                meta: Metadata {
                    id: 1,
                    directory: PathBuf::from("/tmp"),
                    title: "shell".into(),
                    size: (512, 256),
                    exited: false,
                    unread: 1,
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
                find: Some(crate::search::FindState {
                    current: 1,
                    total: crate::viewport::MAX_CELLS,
                    pending: false,
                    highlights: (0..crate::viewport::MAX_CELLS)
                        .map(|i| (i, i + 1, 1))
                        .collect(),
                }),
                selection: None,
                copy: None,
            },
            cells: vec![Cell::default(); crate::viewport::MAX_CELLS],
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
                    state: None,
                    meta: Metadata {
                        id: 1,
                        directory: PathBuf::from("/tmp"),
                        title: "shell".into(),
                        size: (80, 24),
                        exited: false,
                        unread: 0,
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
