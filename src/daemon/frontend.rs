//! Frontend socket buffering, terminal input, and viewport updates.
use super::*;
use gmux_protocol::transport::{KeyAction, PasteOptions, StreamCommand, StreamRequest};

type TerminalSize = (u16, u16, u32, u32, u16, u16);
const READ_CHUNK: usize = 16 * 1024;
const MAX_INPUT_PER_VISIT: usize = 512 * 1024;
const MAX_REQUESTS_PER_VISIT: usize = 8;
const MAX_COMMAND: usize = 1_048_577;

#[derive(Default)]
struct SentScreen {
    frame: Frame,
    cells: Vec<Cell>,
    row_flags: Vec<u8>,
    source_revision: Option<u64>,
    generation: u64,
    history_generation: u64,
    cache_origin: u64,
    cached: std::ops::Range<u64>,
    cache_active: bool,
    prefetch_at: Option<Instant>,
    hyperlinks: gmux_protocol::hyperlinks::Hyperlinks,
    cache_bytes: usize,
    last_used: Option<Instant>,
}
struct SentSelection {
    range: Option<gmux_protocol::selection::Selection>,
    serial: u64,
}
/// One frontend's ownership of a pane. Metadata-only panes never allocate a
/// presentation, native view, or screen baseline.
#[derive(Default)]
struct PaneViewState {
    metadata: Option<Metadata>,
    presentation: Option<Box<PanePresentation>>,
    size: Option<TerminalSize>,
    error: Option<String>,
    git_sent: Option<Box<transport::GitMetadata>>,
    git_subscribed: bool,
    visible: bool,
    changed: bool,
    metadata_changed: bool,
    terminal_changed: bool,
}

struct PanePresentation {
    view: crate::engine::EngineView,
    screen: Option<Box<SentScreen>>,
    find: Option<gmux_protocol::search::FindStatus>,
    selection: Option<SentSelection>,
    scroll_serial: u64,
    sent_scroll_serial: Option<u64>,
    selection_serial: u64,
}

impl PanePresentation {
    fn new(view: crate::engine::EngineView) -> Self {
        Self {
            view,
            screen: None,
            find: None,
            selection: None,
            scroll_serial: 0,
            sent_scroll_serial: None,
            selection_serial: 0,
        }
    }
}

impl PaneViewState {
    fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.changed |= visible;
        if let Some(presentation) = &mut self.presentation {
            // Reattachment always needs a full viewport. Keep per-view state.
            presentation.screen = None;
            presentation.view.set_active(visible);
        }
    }

    fn presentation(
        &mut self,
        session: &TerminalSession,
        resources: &std::rc::Rc<crate::engine::ViewResources>,
    ) -> anyhow::Result<&mut PanePresentation> {
        if self.presentation.is_none() {
            let mut view = session.new_view_in(resources)?;
            view.set_active(self.visible);
            self.presentation = Some(Box::new(PanePresentation::new(view)));
        }
        self.presentation.as_ref().unwrap().view.check_resources()?;
        Ok(self.presentation.as_mut().unwrap())
    }

    fn screen(&self) -> Option<&SentScreen> {
        self.presentation.as_ref()?.screen.as_deref()
    }

    fn find_pending(&self) -> bool {
        self.presentation
            .as_ref()
            .map(|p| &p.view)
            .is_some_and(crate::engine::EngineView::find_pending)
    }

    fn pending(&self) -> bool {
        self.changed
            || self.metadata_changed
            || self.terminal_changed
            || self.error.is_some()
            || self.find_pending()
    }

    fn invalidate_cache(&mut self) {
        if let Some(screen) = self.presentation.as_mut().and_then(|p| p.screen.as_mut()) {
            screen.release_cache();
        }
    }
}

pub(super) struct Frontend {
    pub(super) socket: UnixStream,
    panes: HashMap<u64, PaneViewState>,
    pub(super) input: Vec<u8>,
    read_buffer: Vec<u8>,
    read_offset: usize,
    pub(super) output: resources::Output,
    pub(super) attached: bool,
    pub(super) remote: bool,
    git_revision: Option<u64>,
    pub(super) discarding: bool,
    pub(super) paste: Option<Paste>,
    clipboard_writes: bool,
    pub(super) clipboard_reads: bool,
    pub(super) clipboard_epochs: [u64; 2],
    pub(super) paste_serial: u64,
    binary: Option<StreamRequest>,
    pub(super) binary_deadline: Option<Instant>,
    pub(super) state_revision: Option<u64>,
    pub(super) focused: bool,
    frame_interval: Duration,
    next_frame: Instant,
    frame_panes: VecDeque<u64>,
    frame_started: Option<Instant>,
    frame_activity: bool,
    frame_git_revision: Option<u64>,
}
pub(super) struct Paste {
    pub(super) pane: u64,
    pub(super) size: usize,
    pub(super) text: String,
    pub(super) options: PasteOptions,
    user: Option<(u64, u64)>,
}
impl Frontend {
    pub(super) fn is_visible(&self, pane: u64) -> bool {
        self.panes.get(&pane).is_some_and(|pane| pane.visible)
    }

    pub(super) fn view(&self, pane: u64) -> Option<&crate::engine::EngineView> {
        self.panes
            .get(&pane)?
            .presentation
            .as_ref()
            .map(|p| &p.view)
    }

    pub(super) fn view_mut(&mut self, pane: u64) -> Option<&mut crate::engine::EngineView> {
        self.panes
            .get_mut(&pane)?
            .presentation
            .as_mut()
            .map(|p| &mut p.view)
    }

    pub(super) fn git_panes(&self) -> impl Iterator<Item = u64> + '_ {
        self.panes
            .iter()
            .filter_map(|(&id, pane)| pane.git_subscribed.then_some(id))
    }

    pub(super) fn has_git_subscriptions(&self) -> bool {
        self.panes.values().any(|pane| pane.git_subscribed)
    }

    pub(super) fn pane_error(&mut self, pane: u64, error: String) {
        self.panes.entry(pane).or_default().error = Some(error);
    }

    /// Dropping the owner releases native views and every pane-local cache and
    /// subscription. Connection-level transfers need separate cancellation.
    pub(super) fn close_pane(&mut self, pane: u64) {
        self.panes.remove(&pane);
        // Release bucket capacity too once the last pane closes. Avoid keeping
        // a frontend's largest layout allocation for an empty connection.
        if self.panes.is_empty() {
            self.panes = HashMap::new();
        }
        if self.paste.as_ref().is_some_and(|paste| paste.pane == pane) {
            self.paste = None;
        }
    }

    pub(super) fn retained_output(&self) -> usize {
        self.output.capacity()
    }
    pub(super) fn retained_screens(&self) -> usize {
        self.panes
            .values()
            .filter_map(PaneViewState::screen)
            .map(|screen| screen.cache_bytes)
            .sum()
    }
    pub(super) fn retained_views(&self) -> usize {
        self.panes
            .values()
            .filter_map(|p| p.presentation.as_ref())
            .map(|p| {
                size_of::<PanePresentation>() - size_of::<crate::engine::EngineView>()
                    + p.view.memory_size()
            })
            .sum()
    }
    fn queue_clipboard(
        &mut self,
        pane: u64,
        write: gmux_protocol::clipboard::Write,
    ) -> anyhow::Result<()> {
        self.queue(Packet {
            header: Header {
                meta: Metadata {
                    id: pane,
                    ..Metadata::default()
                },
                copy: Some(write),
                ..Header::default()
            },
            cells: Vec::new(),
        })
    }
    pub(super) fn new(socket: UnixStream, budget: &resources::OutputBudget) -> Self {
        Self {
            socket,
            input: Vec::new(),
            read_buffer: Vec::new(),
            read_offset: 0,
            output: resources::Output::new(budget),
            attached: false,
            remote: false,
            panes: HashMap::new(),
            git_revision: None,
            discarding: false,
            paste: None,
            clipboard_writes: false,
            clipboard_reads: false,
            clipboard_epochs: [0; 2],
            paste_serial: 0,
            binary: None,
            binary_deadline: None,
            state_revision: None,
            focused: false,
            frame_interval: transport::DEFAULT_FRAME_INTERVAL,
            next_frame: Instant::now(),
            frame_panes: VecDeque::new(),
            frame_started: None,
            frame_activity: false,
            frame_git_revision: None,
        }
    }
    pub(super) fn queue(&mut self, packet: Packet) -> anyhow::Result<()> {
        self.output.queue(packet)
    }
    pub(super) fn queue_encoded(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.output.queue_encoded(bytes)
    }
    pub(super) fn flush(&mut self) -> anyhow::Result<()> {
        self.output.flush(&mut self.socket)
    }
    pub(super) fn input_pending(&self) -> bool {
        self.read_offset < self.read_buffer.len()
    }
    pub(super) fn requests(&mut self) -> anyhow::Result<Vec<StreamRequest>> {
        anyhow::ensure!(!self.output.closed(), "Frontend output budget exhausted");
        // Keep the decoded batch small even for empty or invalid JSON lines.
        // The unread suffix survives the request limit and schedules another turn.
        let mut requests = Vec::new();
        let mut budget = MAX_INPUT_PER_VISIT;
        let mut reads = 0;
        while budget > 0 && requests.len() < MAX_REQUESTS_PER_VISIT {
            if !self.input_pending() {
                if reads == 32 {
                    break;
                }
                reads += 1;
                self.read_offset = 0;
                self.read_buffer.resize(READ_CHUNK.min(budget), 0);
                let result = self.socket.read(&mut self.read_buffer);
                if result.is_err() {
                    self.read_buffer.clear();
                }
                match result {
                    Ok(0) => anyhow::bail!("Frontend disconnected"),
                    Ok(n) => self.read_buffer.truncate(n),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if let Some(mut request) = self.binary.take() {
                let StreamCommand::ClipboardReadData { size, bytes, .. } = &mut request.command
                else {
                    unreachable!()
                };
                let n = (*size - bytes.len())
                    .min(budget)
                    .min(self.read_buffer.len() - self.read_offset);
                bytes.extend_from_slice(&self.read_buffer[self.read_offset..self.read_offset + n]);
                self.read_offset += n;
                budget -= n;
                if bytes.len() == *size {
                    self.binary_deadline = None;
                    requests.push(request);
                } else {
                    self.binary = Some(request);
                }
                continue;
            }
            let end = self.read_buffer.len().min(self.read_offset + budget);
            for part in self.read_buffer[self.read_offset..end].split_inclusive(|c| *c == b'\n') {
                if requests.len() == MAX_REQUESTS_PER_VISIT {
                    break;
                }
                self.read_offset += part.len();
                budget -= part.len();
                let complete = part.ends_with(b"\n");
                if self.discarding {
                    self.discarding = !complete;
                    continue;
                }
                if self.input.len() + part.len() > MAX_COMMAND {
                    self.input.clear();
                    self.discarding = !complete;
                    requests.push(StreamRequest::invalid("Frontend command is too large"));
                    continue;
                }
                self.input.extend_from_slice(part);
                if complete {
                    if self.input.len() > gmux_protocol::clipboard::MAX_HEADER_BYTES {
                        #[derive(serde::Deserialize)]
                        struct Method {
                            method: Option<String>,
                        }
                        let method = serde_json::from_slice::<Method>(&self.input)
                            .ok()
                            .and_then(|m| m.method);
                        anyhow::ensure!(
                            !matches!(
                                method.as_deref(),
                                Some(
                                    "clipboard-read-result" | "user-paste" | "clipboard-read-data"
                                )
                            ),
                            "Clipboard metadata is too large"
                        );
                    }
                    let request = StreamRequest::decode(&self.input);
                    self.input.clear();
                    if let StreamCommand::ClipboardReadData { size, .. } = &request.command {
                        anyhow::ensure!(
                            (1..=gmux_protocol::clipboard::READ_CHUNK).contains(size),
                            "Invalid clipboard chunk length"
                        );
                        self.binary = Some(request);
                        self.binary_deadline =
                            Some(Instant::now() + gmux_protocol::clipboard::TRANSFER_TIMEOUT);
                        break;
                    }
                    requests.push(request);
                }
            }
        }
        Ok(requests)
    }
}

impl SentScreen {
    fn memory_size(&self) -> usize {
        self.cells.capacity() * size_of::<Cell>()
            + self.cells.iter().map(Cell::heap_bytes).sum::<usize>()
            + self.row_flags.capacity()
            + self.hyperlinks.memory_size()
    }
    fn release_cache(&mut self) {
        self.cells = Vec::new();
        self.row_flags = Vec::new();
        self.hyperlinks = Default::default();
        self.cache_bytes = 0;
        self.source_revision = None;
        self.prefetch_at = None;
        // Keep generation and cache_active until the next full snapshot sends
        // an invalidation. EngineView owns viewport, selection and appearance.
    }
}

fn trim_screen_caches(current: &mut Frontend, others: &mut HashMap<u64, Frontend>, limit: usize) {
    let mut total = current.retained_screens()
        + others
            .values()
            .map(Frontend::retained_screens)
            .sum::<usize>();
    while total > limit {
        let victim = std::iter::once((None, &*current))
            .chain(others.iter().map(|(&id, frontend)| (Some(id), frontend)))
            .flat_map(|(id, frontend)| {
                frontend
                    .panes
                    .iter()
                    .filter_map(|(pane, state)| state.screen().map(|screen| (pane, screen)))
                    .filter(|(_, screen)| screen.cache_bytes != 0)
                    .map(move |(&pane, screen)| {
                        (
                            (
                                !frontend.output.backlogged(),
                                frontend.focused,
                                screen.last_used,
                                id,
                                pane,
                            ),
                            id,
                            pane,
                        )
                    })
            })
            .min_by_key(|(rank, _, _)| *rank);
        let Some((_, id, pane)) = victim else { break };
        let frontend = match id {
            Some(id) => others.get_mut(&id).unwrap(),
            None => &mut *current,
        };
        let state = frontend.panes.get_mut(&pane).unwrap();
        total -= state.screen().unwrap().cache_bytes;
        state.invalidate_cache();
    }
}

fn size_owner<'a>(
    pane: u64,
    frontends: impl Iterator<Item = (u64, &'a Frontend)>,
) -> Option<&'a Frontend> {
    frontends
        .filter(|(_, frontend)| {
            frontend.attached
                && frontend
                    .panes
                    .get(&pane)
                    .is_some_and(|pane| pane.visible && pane.size.is_some())
        })
        // Without a focused subscriber, keep a stable owner to avoid contention.
        .min_by_key(|(id, frontend)| (!frontend.focused, *id))
        .map(|(_, frontend)| frontend)
}
#[cfg(test)]
fn requested_size<'a>(
    pane: u64,
    frontends: impl Iterator<Item = (u64, &'a Frontend)>,
) -> Option<TerminalSize> {
    size_owner(pane, frontends).map(|frontend| frontend.panes[&pane].size.unwrap())
}

impl Daemon {
    pub(super) fn admit_frontend(&mut self, socket: UnixStream) -> anyhow::Result<bool> {
        if self.frontends.len() >= resources::MAX_FRONTENDS {
            return Ok(false);
        }
        socket.set_nonblocking(true)?;
        self.next_frontend += 1;
        self.frontends.insert(
            self.next_frontend,
            Frontend::new(socket, &self.output_budget),
        );
        Ok(true)
    }
    pub(super) fn clipboard_recipient(&self, pane: u64) -> Option<u64> {
        self.frontends
            .iter()
            .filter(|(_, frontend)| {
                frontend.attached && frontend.clipboard_writes && frontend.is_visible(pane)
            })
            .min_by_key(|(id, frontend)| (!frontend.focused, **id))
            .map(|(&id, _)| id)
    }
    pub(super) fn deliver_clipboard(
        &mut self,
        recipient: u64,
        pane: u64,
        writes: Vec<gmux_protocol::clipboard::Write>,
    ) {
        let Some(frontend) = self.frontends.get_mut(&recipient) else {
            return;
        };
        for write in writes {
            if let Err(error) = frontend.queue_clipboard(pane, write) {
                eprintln!("Detaching frontend: {error}");
                self.frontends.remove(&recipient);
                self.git_dirty = true;
                self.resize_sessions(None);
                break;
            }
        }
    }
    pub(super) fn resize_sessions(&mut self, current: Option<(u64, &Frontend)>) {
        let panes: Vec<_> = self.sessions.keys().copied().collect();
        for pane in panes {
            self.resize_session(pane, current);
        }
    }

    fn resize_session(&mut self, pane: u64, current: Option<(u64, &Frontend)>) {
        // Input processing temporarily removes its frontend from the map.
        let Some(owner) = size_owner(
            pane,
            self.frontends
                .iter()
                .map(|(&id, frontend)| (id, frontend))
                .chain(current),
        ) else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&pane) else {
            return;
        };
        if let Some(view) = owner.view(pane)
            && let Err(error) = session.use_view_appearance(view)
        {
            self.errors.insert(pane, error.to_string());
            self.changed.insert(pane);
        }
        let (cols, rows, cw, ch, width, height) = owner.panes[&pane].size.unwrap();
        match session.resize(cols, rows, cw, ch, width, height) {
            Ok(true) => {
                self.changed.insert(pane);
            }
            Ok(false) => {}
            Err(error) => {
                self.errors.insert(pane, error.to_string());
                self.changed.insert(pane);
            }
        }
    }

    pub(super) fn flush_frontends_budgeted(&mut self, budget: &mut gmux_platform::work::Budget) {
        let before = self.frontends.len();
        self.frontends
            .retain(|_, frontend| !frontend.output.closed());
        let mut ids: Vec<_> = self.frontends.keys().copied().collect();
        ids.sort_unstable();
        let start = ids.partition_point(|id| *id <= self.flush_cursor);
        let count = ids.len();
        ids.rotate_left(if start == count { 0 } else { start });
        for id in ids {
            if !budget.take() {
                break;
            }
            self.flush_cursor = id;
            if self.frontends.get_mut(&id).unwrap().flush().is_err() {
                self.frontends.remove(&id);
            }
        }
        if self.frontends.len() != before {
            self.git_dirty = true;
            self.resize_sessions(None);
        }
    }

    pub(super) fn input(
        &mut self,
        frontend_id: u64,
        frontend: &mut Frontend,
        request: StreamRequest,
    ) -> anyhow::Result<()> {
        let StreamRequest { pane, command } = request;
        match command {
            StreamCommand::Attach {
                version,
                daemon,
                remote,
            } => {
                anyhow::ensure!(!frontend.attached, "Frontend already attached");
                anyhow::ensure!(
                    version == transport::VERSION,
                    "Incompatible frontend protocol"
                );
                anyhow::ensure!(daemon == self.id, "Frontend attached to a different daemon");
                frontend.attached = true;
                frontend.remote = remote;
                return Ok(());
            }
            StreamCommand::Invalid { message, .. } => {
                frontend.paste = None;
                anyhow::bail!("{message}");
            }
            _ => {}
        }
        anyhow::ensure!(frontend.attached, "Frontend must attach first");
        if let StreamCommand::Ping = command {
            return frontend.queue_encoded(transport::HEARTBEAT_REPLY);
        }
        if let StreamCommand::FrameInterval { micros } = command {
            anyhow::ensure!(
                (1_000..=1_000_000).contains(&micros),
                "Invalid frame interval"
            );
            frontend.frame_interval = Duration::from_micros(micros as u64);
            frontend.next_frame = Instant::now();
            return Ok(());
        }
        if let StreamCommand::ClipboardWrites { enabled } = command {
            frontend.clipboard_writes = enabled;
            return Ok(());
        }
        if let StreamCommand::ClipboardReads { enabled } = command {
            frontend.clipboard_reads = enabled;
            if !enabled {
                self.cancel_frontend_reads(frontend_id);
            }
            return Ok(());
        }
        if let StreamCommand::ClipboardChanged { destination, epoch } = command {
            let previous = &mut frontend.clipboard_epochs[destination as usize];
            anyhow::ensure!(epoch >= *previous, "Stale clipboard generation");
            if epoch == *previous {
                return Ok(());
            }
            *previous = epoch;
            self.clipboard_changed(frontend_id, destination);
            return Ok(());
        }
        if let StreamCommand::Focused { active } = command {
            // Keep the last focused client as size owner while another app has focus.
            if active {
                frontend.focused = true;
                for other in self.frontends.values_mut() {
                    other.focused = false;
                }
                self.resize_sessions(Some((frontend_id, frontend)));
            }
            return Ok(());
        }
        if let StreamCommand::Subscribe { panes } = command {
            for (&id, state) in &mut frontend.panes {
                state.set_visible(panes.contains(&id));
            }
            for id in panes {
                // Do not retain subscriptions to closed or unknown panes.
                if self.sessions.contains_key(&id) {
                    frontend.panes.entry(id).or_default().set_visible(true);
                }
            }
            self.resize_sessions(Some((frontend_id, frontend)));
            return Ok(());
        }
        if let StreamCommand::GitSubscribe { mut panes } = command {
            panes.retain(|id| self.sessions.contains_key(id));
            for (&id, state) in &mut frontend.panes {
                state.git_subscribed = panes.contains(&id);
                if !state.git_subscribed {
                    state.git_sent = None;
                }
            }
            for id in panes {
                frontend.panes.entry(id).or_default().git_subscribed = true;
            }
            frontend.git_revision = None;
            self.git_dirty = true;
            return Ok(());
        }
        let target = if frontend.remote && pane.is_none() {
            Err(anyhow::anyhow!(
                "Remote pane commands require an explicit pane"
            ))
        } else {
            self.resolve_pane(pane)
        };
        let id = match target {
            Ok(id) => id,
            Err(error) => {
                frontend.paste = None;
                return Err(error);
            }
        };
        if self.clipboard_input(frontend_id, frontend, id, &command)? {
            return Ok(());
        }
        if let StreamCommand::Resize {
            cols,
            rows,
            cw,
            ch,
            width,
            height,
        } = command
        {
            anyhow::ensure!(
                gmux_protocol::viewport::valid(cols.into(), rows.into()),
                "Invalid terminal size"
            );
            frontend.panes.entry(id).or_default().size = Some((cols, rows, cw, ch, width, height));
            self.resize_session(id, Some((frontend_id, frontend)));
            return Ok(());
        }
        match command {
            StreamCommand::PasteBegin { size, options } => {
                frontend.paste = None;
                anyhow::ensure!(
                    size <= transport::MAX_PASTE,
                    "Paste is too large. The limit is 4 MiB."
                );
                frontend.paste = Some(Paste {
                    pane: id,
                    size,
                    text: String::with_capacity(size),
                    options,
                    user: None,
                });
                return Ok(());
            }
            StreamCommand::UserPasteBegin {
                serial,
                epoch,
                size,
            } => {
                anyhow::ensure!(
                    frontend.clipboard_reads
                        && frontend.is_visible(id)
                        && size <= transport::MAX_PASTE,
                    "Invalid desktop paste transfer"
                );
                frontend.paste = Some(Paste {
                    pane: id,
                    size,
                    text: String::with_capacity(size),
                    options: Default::default(),
                    user: Some((serial, epoch)),
                });
                return Ok(());
            }
            StreamCommand::PasteData { text } => {
                let mut paste = frontend
                    .paste
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("No paste is in progress"))?;
                anyhow::ensure!(
                    paste.pane == id
                        && text.len() <= transport::PASTE_CHUNK
                        && paste.text.len() + text.len() <= paste.size,
                    "Invalid paste chunk"
                );
                paste.text.push_str(&text);
                frontend.paste = Some(paste);
                return Ok(());
            }
            StreamCommand::PasteEnd => {
                let paste = frontend
                    .paste
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("No paste is in progress"))?;
                anyhow::ensure!(
                    paste.pane == id && paste.text.len() == paste.size,
                    "Incomplete paste"
                );
                return self.input(
                    frontend_id,
                    frontend,
                    StreamRequest::pane(
                        id,
                        if let Some((serial, epoch)) = paste.user {
                            StreamCommand::UserPasteText {
                                serial,
                                epoch,
                                text: paste.text,
                            }
                        } else {
                            StreamCommand::Paste {
                                text: paste.text,
                                options: paste.options,
                            }
                        },
                    ),
                );
            }
            StreamCommand::MarkRead { through } => {
                if self
                    .unread
                    .get(&id)
                    .is_some_and(|&sequence| sequence <= through)
                {
                    self.unread.remove(&id);
                    self.changed.insert(id);
                }
                return Ok(());
            }
            StreamCommand::ClearScreen => {
                self.sessions.get_mut(&id).unwrap().clear_screen()?;
                for client in std::iter::once(&mut *frontend).chain(self.frontends.values_mut()) {
                    if let Some(view) = client.view_mut(id) {
                        view.restart_find();
                    }
                }
                self.changed.insert(id);
                return Ok(());
            }
            _ => {}
        }
        let session = self.sessions.get_mut(&id).unwrap();
        // Only an interaction delivered by a viewing frontend acknowledges a
        // result. Focus/read synchronization and CLI sends do not consume it.
        let interaction = match &command {
            StreamCommand::Key { action, .. } => *action != KeyAction::Release,
            StreamCommand::Input { bytes } => !bytes.is_empty(),
            StreamCommand::Paste { text, .. } => !text.is_empty(),
            _ => false,
        };
        if interaction && frontend.is_visible(id) && session.acknowledge_program_status() {
            self.metadata_changed.insert(id);
        }
        if interaction && session.exited() {
            return Ok(());
        }
        let pane = frontend.panes.entry(id).or_default();
        let presentation = pane.presentation(session, &self.view_resources)?;
        if matches!(command, StreamCommand::Find { .. }) {
            // A new query can finish in one batch with identical counts but
            // different highlights. Its scalar summary cannot detect that.
            presentation.find = None;
        }
        let view = &mut presentation.view;
        let appearance_changed = matches!(
            command,
            StreamCommand::Theme { .. } | StreamCommand::Style { .. }
        );
        let mut copy = None;
        match command {
            StreamCommand::Input { bytes } => session.send(&bytes)?,
            StreamCommand::Paste { text, options } => {
                anyhow::ensure!(
                    text.len() <= transport::MAX_PASTE,
                    "Paste is too large. The limit is 4 MiB."
                );
                let mut bytes = session.paste(&text)?;
                if options.enter {
                    bytes.push(b'\r');
                }
                if options.bottom {
                    session.with_view(view, |engine| engine.scroll(0, true));
                }
                session.send(&bytes)?;
            }
            StreamCommand::Key {
                name,
                layout,
                mods,
                action,
                text,
            } => {
                let bytes = session.key(&name, layout, mods, action, &text);
                if !bytes.is_empty() {
                    if action != KeyAction::Release {
                        session.with_view(view, |engine| engine.scroll(0, true));
                    }
                    session.send(&bytes)?;
                }
            }
            StreamCommand::Mouse {
                position,
                metrics,
                action,
                button,
                mods,
                pressed,
            } => {
                let bytes = session.mouse(position, metrics, action, button, mods, pressed);
                if !bytes.is_empty() {
                    session.send(&bytes)?;
                }
            }
            StreamCommand::Theme { theme } => {
                view.set_theme(&theme);
                if let Some(screen) = presentation.screen.as_mut() {
                    screen.source_revision = None;
                }
            }
            StreamCommand::Style { style, blink } => {
                view.set_cursor_style(style)?;
                view.set_cursor_blink(blink)?;
            }
            StreamCommand::Scroll { delta, bottom } => {
                session.with_view(view, |engine| engine.scroll(delta, bottom));
            }
            StreamCommand::ScrollTo { row, serial } => {
                let previous = &mut presentation.scroll_serial;
                if serial >= *previous {
                    session.with_view(view, |engine| engine.scroll_to(row));
                    *previous = serial;
                }
            }
            StreamCommand::Prompt { previous, serial } => {
                let old = &mut presentation.scroll_serial;
                if serial >= *old {
                    *old = serial;
                    // A missing target is harmless, but still acknowledge the
                    // viewport serial so the client can drop its prediction.
                    let _ = session.with_view(view, |engine| engine.prompt(previous));
                }
            }
            command => session.with_view(view, |engine| -> anyhow::Result<()> {
                // Search navigation uses this client's current viewport metadata.
                if matches!(command, StreamCommand::FindAdvance { .. }) {
                    engine.snapshot()?;
                }
                match command {
                    StreamCommand::Copy { formats } => {
                        anyhow::ensure!(
                            engine.selection().is_some(),
                            "Select terminal text to copy it"
                        );
                        copy = Some(engine.copy_content(formats)?);
                    }
                    StreamCommand::SelectOutput {
                        point,
                        anchored,
                        copy: should_copy,
                        serial,
                    } => {
                        presentation.selection_serial = serial;
                        if anchored {
                            engine.select_anchored_output()?;
                        } else {
                            engine.select_output(point)?;
                        }
                        if let Some(formats) = should_copy {
                            copy = Some(engine.copy_content(formats)?);
                        }
                    }
                    StreamCommand::AnchorOutput { point } => engine.anchor_output(point)?,
                    StreamCommand::Find { query } => engine.find(&query),
                    StreamCommand::FindAdvance { previous } => engine.find_advance(previous),
                    StreamCommand::Select {
                        start,
                        end,
                        mode,
                        serial,
                    } => {
                        presentation.selection_serial = serial;
                        engine.select(start, end, mode)?
                    }
                    StreamCommand::ClearSelection { serial } => {
                        presentation.selection_serial = serial;
                        engine.clear_selection();
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })?,
        }
        pane.changed = true;
        if let Some(content) = copy {
            frontend.queue_clipboard(
                id,
                gmux_protocol::clipboard::Write {
                    destination: gmux_protocol::clipboard::Destination::Clipboard,
                    content: Some(content.into()),
                },
            )?;
        }
        if appearance_changed {
            self.resize_session(id, Some((frontend_id, frontend)));
        }
        Ok(())
    }
    pub(super) fn next_update(&self) -> Option<Instant> {
        self.frontends
            .values()
            .filter(|frontend| frontend.attached)
            .filter_map(|frontend| {
                if !frontend.frame_panes.is_empty() {
                    return Some(Instant::now());
                }
                let pending = !self.changed.is_empty()
                    || !self.metadata_changed.is_empty()
                    || !self.notifications.is_empty()
                    || !self.errors.is_empty()
                    || frontend.panes.values().any(PaneViewState::pending)
                    || frontend.state_revision != Some(self.revision)
                    || (frontend.has_git_subscriptions()
                        && frontend.git_revision
                            != self.git.as_ref().map(crate::branch::Service::revision));
                let prefetch = frontend
                    .panes
                    .values()
                    .filter_map(PaneViewState::screen)
                    .filter_map(|screen| screen.prefetch_at)
                    .min()
                    .map(|at| at.max(frontend.next_frame));
                pending
                    .then_some(frontend.next_frame)
                    .into_iter()
                    .chain(prefetch)
                    .min()
            })
            .min()
    }
    /// Publish each connection's due updates without coupling display intervals.
    #[cfg(test)]
    pub(super) fn updates(&mut self, metadata_due: bool) {
        let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), usize::MAX);
        self.updates_budgeted(metadata_due, &mut budget);
    }
    pub(super) fn updates_budgeted(
        &mut self,
        metadata_due: bool,
        budget: &mut gmux_platform::work::Budget,
    ) {
        let before = self.frontends.len();
        let mut ids: Vec<_> = self.frontends.keys().copied().collect();
        ids.sort_unstable();
        let recipient = ids
            .iter()
            .copied()
            .filter(|id| {
                let frontend = &self.frontends[id];
                frontend.attached
                    && (metadata_due
                        || !frontend.frame_panes.is_empty()
                        || Instant::now() >= frontend.next_frame)
            })
            .min_by_key(|id| (!self.frontends[id].focused, *id));
        let start = ids.partition_point(|id| *id <= self.update_cursor);
        let count = ids.len();
        ids.rotate_left(if start == count { 0 } else { start });
        // Every client receives the same execution revision. Serialize each transport snapshot once per broadcast.
        let mut state = None;
        // Deliver each alert once, preferring a focused client. All clients receive unread state.
        let mut notifications = std::mem::take(&mut self.notifications);
        for frontend in self
            .frontends
            .values_mut()
            .filter(|frontend| frontend.attached)
        {
            for &id in &self.changed {
                frontend.panes.entry(id).or_default().terminal_changed = true;
            }
            if metadata_due {
                for &id in self.sessions.keys() {
                    frontend.panes.entry(id).or_default().metadata_changed = true;
                }
            }
            for &id in &self.metadata_changed {
                frontend.panes.entry(id).or_default().metadata_changed = true;
            }
            for (&id, error) in &self.errors {
                frontend.pane_error(id, error.clone());
            }
        }
        for id in ids {
            if budget.expired() {
                break;
            }
            let mut frontend = self.frontends.remove(&id).unwrap();
            if !frontend.attached
                || (frontend.frame_panes.is_empty()
                    && !metadata_due
                    && Instant::now() < frontend.next_frame)
            {
                self.frontends.insert(id, frontend);
                continue;
            }
            self.update_cursor = id;
            let result = (|| {
                if frontend.state_revision != Some(self.revision) {
                    let bytes = state
                        .get_or_insert_with(|| self.state_packet())
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    frontend.queue_encoded(bytes)?;
                    frontend.state_revision = Some(self.revision);
                    for &id in self.sessions.keys() {
                        if !frontend.panes.contains_key(&id) {
                            frontend.panes.entry(id).or_default().metadata_changed = true;
                        }
                    }
                }
                let mut none = HashMap::new();
                self.update_frontend_budgeted(
                    &mut frontend,
                    if Some(id) == recipient {
                        &mut notifications
                    } else {
                        &mut none
                    },
                    budget,
                )
            })();
            match result {
                Ok(()) => {
                    self.frontends.insert(id, frontend);
                }
                Err(error) => eprintln!("Detaching frontend: {error}"),
            }
        }
        notifications.retain(|id, _| self.sessions.contains_key(id));
        self.notifications = notifications;
        self.changed.clear();
        self.metadata_changed.clear();
        self.errors.clear();
        if self.frontends.len() != before {
            self.git_dirty = true;
            // Keep handoff changes pending even if survivors were already sent a frame.
            self.resize_sessions(None);
        }
        // Another stream's enqueue may have evicted a frontend already visited
        // by this broadcast. Release its view state and socket this turn.
        self.flush_frontends_budgeted(budget);
    }
    fn state_packet(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Packet {
            header: Header {
                state: Some(self.execution_reply()),
                meta: Metadata {
                    id: 0,
                    directory: PathBuf::new(),
                    title: String::new(),
                    size: (80, 24),
                    exited: false,
                    unread_sequence: 0,
                    busy: false,
                    command: Default::default(),
                    progress: Default::default(),
                    program_status: None,
                    modes: [false; 2],
                },
                ..Header::default()
            },
            cells: Vec::new(),
        }
        .encode_into(&mut bytes)?;
        Ok(bytes)
    }
    #[cfg(test)]
    fn update_frontend(
        &mut self,
        frontend: &mut Frontend,
        metadata_due: bool,
        notifications: &mut HashMap<u64, NotificationQueue>,
    ) -> anyhow::Result<()> {
        if metadata_due {
            for &id in self.sessions.keys() {
                frontend.panes.entry(id).or_default().metadata_changed = true;
            }
        }
        let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), usize::MAX);
        self.update_frontend_budgeted(frontend, notifications, &mut budget)
    }
    fn update_frontend_budgeted(
        &mut self,
        frontend: &mut Frontend,
        notifications: &mut HashMap<u64, NotificationQueue>,
        budget: &mut gmux_platform::work::Budget,
    ) -> anyhow::Result<()> {
        let mut frame_activity = false;
        if frontend.frame_panes.is_empty() {
            let mut ids: Vec<_> = self
                .sessions
                .keys()
                .chain(frontend.panes.keys())
                .copied()
                .collect();
            ids.sort_unstable();
            ids.dedup();
            frontend.frame_panes.extend(ids);
            frontend.frame_git_revision = self.git.as_ref().map(crate::branch::Service::revision);
            frontend.frame_started = Some(Instant::now());
        }
        while !frontend.frame_panes.is_empty() && budget.take() {
            let id = frontend.frame_panes.pop_front().unwrap();
            let Some(session) = self.sessions.get_mut(&id) else {
                if let Some(pane) = frontend.panes.get_mut(&id) {
                    pane.changed = false;
                    pane.terminal_changed = false;
                    pane.metadata_changed = false;
                    pane.error = None;
                }
                continue;
            };
            if let Some(git) = &self.git {
                let pane = frontend.panes.entry(id).or_default();
                if pane.git_subscribed
                    && let Some(meta) = git.metadata(id)
                    && pane.git_sent.as_deref() != Some(&meta)
                {
                    pane.git_sent = Some(Box::new(meta.clone()));
                    frontend.output.queue(Packet {
                        header: Header {
                            git: Some(meta),
                            ..Header::default()
                        },
                        cells: Vec::new(),
                    })?;
                }
            }
            let notify: Vec<_> = notifications
                .remove(&id)
                .unwrap_or_default()
                .messages
                .into_iter()
                .collect();
            let pane = frontend.panes.entry(id).or_default();
            let error = pane.error.take().or_else(|| self.errors.get(&id).cloned());
            let terminal_changed = std::mem::take(&mut pane.terminal_changed);
            let requested = std::mem::take(&mut pane.changed);
            let metadata_changed = std::mem::take(&mut pane.metadata_changed);
            let changed = terminal_changed
                || requested
                || pane.screen().is_some_and(|screen| {
                    screen.prefetch_at.is_some_and(|at| at <= Instant::now())
                })
                || pane.find_pending();
            let screen = pane.visible && (changed || pane.screen().is_none());
            if !metadata_changed
                && !changed
                && !screen
                && notify.is_empty()
                && error.is_none()
                && pane.metadata.is_some()
            {
                continue;
            }
            let meta = Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0));
            if !screen
                && !changed
                && notify.is_empty()
                && error.is_none()
                && pane.metadata.as_ref() == Some(&meta)
            {
                continue;
            }
            let mut rows = Vec::new();
            let mut cells = Vec::new();
            let mut row_flags = Vec::new();
            let mut row_shift = 0;
            let mut generation = 0;
            let mut cache_origin = 0;
            let mut cache_valid_before = None;
            let mut cache = None;
            let mut cache_invalidated = false;
            let mut hyperlinks = None;
            let (frame, find, selection, selection_origin) = if screen
                || pane.presentation.is_some()
            {
                let presentation = pane.presentation(session, &self.view_resources)?;
                let view = &mut presentation.view;
                if terminal_changed {
                    view.invalidate();
                }
                session.with_view(view, |engine| -> anyhow::Result<_> {
                    // Search scans bounded batches even when its visible state
                    // is unchanged. Keep those batches paced instead of spinning.
                    frame_activity |= engine.find_pending();
                    if engine.poll_find() && !engine.find_pending() {
                        // A rescan can move highlights without changing visible
                        // cells or counts, including when it fits in one batch.
                        presentation.find = None;
                    }
                    if screen {
                        // Repeated output can dirty and extract rows without
                        // changing their final cells. Coalesce that work too.
                        frame_activity |= engine.snapshot()?;
                    }
                    let frame = if screen {
                        let frame = engine.frame;
                        let cells_snapshot = &engine.cells;
                        self.snapshots += 1;
                        let cols = frame.cols as usize;
                        let old = presentation.screen.get_or_insert_with(Default::default);
                        let full = old.frame.cols != frame.cols
                            || old.frame.rows != frame.rows
                            || old.cells.len() != cells_snapshot.len();
                        let same_content = old.source_revision == Some(engine.revision) && !full;
                        let reset = full
                            || old.source_revision.is_none()
                            || old.history_generation != engine.history.generation;
                        let removed = if reset {
                            0
                        } else {
                            engine.history.origin.saturating_sub(old.cache_origin)
                        };
                        if reset {
                            cache_invalidated = old.cache_active;
                            old.generation = old.generation.wrapping_add(1).max(1);
                            old.cached = 0..0;
                        } else if !same_content || removed != 0 {
                            let valid = old
                                .frame
                                .scrollback
                                .total
                                .saturating_sub(old.frame.rows as u64)
                                .saturating_sub(removed)
                                .min(frame.scrollback.total.saturating_sub(frame.rows as u64));
                            cache_valid_before = old.cache_active.then_some(valid);
                            old.cached = old.cached.start.saturating_sub(removed)
                                ..old.cached.end.saturating_sub(removed).min(valid);
                            if old.cached.start >= old.cached.end {
                                old.cached = 0..0;
                            }
                        }
                        if !same_content {
                            old.prefetch_at = (!engine.mode(2026)
                                && !super::prefetch::window(frame).is_empty())
                            .then(|| Instant::now() + Duration::from_millis(100));
                        }
                        old.source_revision = Some(engine.revision);
                        old.history_generation = engine.history.generation;
                        old.cache_origin = engine.history.origin;
                        generation = old.generation;
                        cache_origin = old.cache_origin;
                        if full {
                            old.cells.resize(cells_snapshot.len(), Cell::default());
                            old.row_flags.resize(frame.rows as usize, 0);
                        }
                        let delta = frame.scrollback.offset as i128
                            - old.frame.scrollback.offset.saturating_sub(removed) as i128;
                        if !reset && delta != 0 && delta.unsigned_abs() < frame.rows as u128 {
                            row_shift = delta as i16;
                            gmux_protocol::viewport::shift_rows(&mut old.cells, cols, row_shift);
                            gmux_protocol::viewport::shift_rows(&mut old.row_flags, 1, row_shift);
                        }
                        for (row, data) in cells_snapshot.chunks_exact(cols).enumerate() {
                            let previous = &mut old.cells[row * cols..(row + 1) * cols];
                            let flags = engine.row_flags[row];
                            let exposed = (row_shift > 0
                                && row >= frame.rows as usize - row_shift as usize)
                                || (row_shift < 0 && row < row_shift.unsigned_abs() as usize);
                            if full || exposed || *previous != *data || old.row_flags[row] != flags
                            {
                                rows.push(row as u16);
                                row_flags.push(flags);
                                old.row_flags[row] = flags;
                                cells.extend_from_slice(data);
                                previous.clone_from_slice(data);
                            }
                        }
                        self.sent_rows += rows.len() as u64;
                        // A client can discard every row while detached; attachment always resends a full viewport.
                        let serial = presentation.scroll_serial;
                        // Immutable history remains useful during output. Only
                        // prefetch live screen rows after output has settled.
                        if !engine.mode(2026) {
                            let quiet = old.prefetch_at.is_none_or(|at| at <= Instant::now());
                            let mut wanted = super::prefetch::window(frame);
                            let history_end =
                                frame.scrollback.total.saturating_sub(frame.rows as u64);
                            if !quiet {
                                wanted.end = wanted.end.min(history_end);
                            }
                            if wanted.start > wanted.end {
                                wanted.start = wanted.end;
                            }
                            let margin = frame.rows as u64 / 2;
                            let near = frame
                                .scrollback
                                .offset
                                .saturating_sub(margin)
                                .max(wanted.start)
                                ..(frame.scrollback.offset + frame.rows as u64 + margin)
                                    .min(wanted.end);
                            if !wanted.is_empty()
                                && (old.cached.start > near.start || old.cached.end < near.end)
                            {
                                let missing = if old.cached.start <= wanted.start
                                    && old.cached.end >= wanted.start
                                {
                                    old.cached.end..wanted.end
                                } else if old.cached.end >= wanted.end
                                    && old.cached.start <= wanted.end
                                {
                                    wanted.start..old.cached.start
                                } else {
                                    wanted.clone()
                                };
                                if !missing.is_empty() {
                                    let (extra, flags) = engine.cache_rows(missing.clone())?;
                                    cells.extend(extra);
                                    cache = Some(transport::CachedRows {
                                        start: missing.start,
                                        row_flags: flags,
                                    });
                                    old.cache_active = true;
                                }
                                old.cached = wanted.clone();
                            }
                            if quiet || wanted.end < history_end {
                                old.prefetch_at = None;
                            }
                        }
                        if old.hyperlinks != engine.hyperlinks || full {
                            old.hyperlinks.clone_from(&engine.hyperlinks);
                            hyperlinks = Some(engine.hyperlinks.clone());
                        }
                        let send = hyperlinks.is_some()
                            || full
                            || !rows.is_empty()
                            || old.frame != frame
                            || cache.is_some()
                            || removed != 0
                            || presentation.sent_scroll_serial != Some(serial);
                        old.frame = frame;
                        old.cache_bytes = old.memory_size();
                        old.last_used = Some(Instant::now());
                        send.then_some(frame)
                    } else {
                        None
                    };
                    Ok((
                        frame,
                        engine.find_state(),
                        engine.selection(),
                        engine.selection_origin(),
                    ))
                })?
            } else {
                (
                    None,
                    gmux_protocol::search::FindState::default(),
                    None,
                    None,
                )
            };
            let presentation = pane.presentation.as_mut();
            // Highlights travel with viewport changes and completed rescans.
            // Retain only the scalar summary, not another highlight vector.
            let find_summary = find.status();
            let find_changed = (frame.is_some() && find.total != 0)
                || presentation.as_ref().map_or(pane.metadata.is_none(), |p| {
                    p.find.as_ref() != Some(&find_summary)
                });
            let selection_serial = presentation.as_ref().map_or(0, |p| p.selection_serial);
            let previous = presentation.as_ref().and_then(|p| p.selection.as_ref());
            let selection_changed =
                previous.map_or(pane.metadata.is_none(), |sent| sent.range != selection);
            let selection_ack_changed = previous.map_or(pane.metadata.is_none(), |sent| {
                sent.serial != selection_serial
            });
            if frame.is_none()
                && !cache_invalidated
                && cache_valid_before.is_none()
                && !find_changed
                && !selection_changed
                && !selection_ack_changed
                && notify.is_empty()
                && error.is_none()
                && pane.metadata.as_ref() == Some(&meta)
            {
                continue;
            }
            let scroll_serial = if let Some(presentation) = presentation {
                presentation.find = Some(find_summary);
                presentation.selection = Some(SentSelection {
                    range: selection,
                    serial: selection_serial,
                });
                presentation.sent_scroll_serial = Some(presentation.scroll_serial);
                presentation.scroll_serial
            } else {
                0
            };
            let status_changed = pane.metadata.as_ref().is_some_and(|previous| {
                previous.progress != meta.progress || previous.program_status != meta.program_status
            });
            pane.metadata = Some(meta.clone());
            let viewport_changed = screen && (frame.is_some() || find_changed || selection_changed);
            if screen {
                trim_screen_caches(frontend, &mut self.frontends, resources::MAX_SCREEN_CACHES);
            }
            frontend.queue(Packet {
                header: Header {
                    git: None,
                    state: None,
                    meta,
                    frame,
                    hyperlinks,
                    scroll_serial,
                    generation,
                    cache_origin,
                    cache_valid_before,
                    row_shift,
                    cache,
                    rows,
                    row_flags: if row_flags.iter().any(|flags| *flags != 0) {
                        row_flags
                    } else {
                        Vec::new()
                    },
                    notifications: notify,
                    error,
                    find: find_changed.then_some(find),
                    selection,
                    selection_serial,
                    selection_origin,
                    copy: None,
                    clipboard_read: None,
                },
                cells,
            })?;
            frame_activity |= viewport_changed || status_changed;
        }
        frontend.frame_activity |= frame_activity;
        if frontend.frame_panes.is_empty() {
            if std::mem::take(&mut frontend.frame_activity) {
                frontend.next_frame = frontend.frame_started.unwrap() + frontend.frame_interval;
            }
            frontend.frame_started = None;
            frontend.git_revision = frontend.frame_git_revision;
        }
        frontend.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_frontends_rotate_and_keep_unvisited_dirty_panes() {
        let mut daemon = Daemon::empty_for_test();
        for id in 1..=3 {
            daemon.sessions.insert(id, session(id));
            daemon.changed.insert(id);
        }
        let mut peers = Vec::new();
        for id in 1..=2 {
            let (mut frontend, peer) = connection();
            frontend.attached = true;
            frontend.state_revision = Some(0);
            daemon.frontends.insert(id, frontend);
            peers.push(peer);
        }
        for turn in 1..=6 {
            let mut budget = gmux_platform::work::Budget::new(Duration::from_secs(60), 1);
            daemon.updates_budgeted(false, &mut budget);
            let serviced = daemon
                .frontends
                .values()
                .flat_map(|f| f.panes.values())
                .filter(|p| p.metadata.is_some())
                .count();
            assert_eq!(serviced, turn);
            assert!(daemon.changed.is_empty());
            if turn < 6 {
                assert!(daemon.next_update().is_some_and(|at| at <= Instant::now()));
            }
            if turn == 3 {
                // A pane already visited in this pass changes again.
                daemon.changed.insert(1);
            }
            if turn == 2 {
                assert!(
                    daemon
                        .frontends
                        .values()
                        .all(|f| f.panes[&1].metadata.is_some())
                );
                assert!(
                    daemon
                        .frontends
                        .values()
                        .all(|f| f.panes[&3].terminal_changed)
                );
            }
        }
        assert!(
            daemon
                .frontends
                .values()
                .all(|f| f.panes[&1].terminal_changed)
        );
        assert!(daemon.next_update().is_some());
        daemon.updates(false);
        assert!(daemon.next_update().is_none());
    }

    pub(super) fn session(id: u64) -> TerminalSession {
        TerminalSession::spawn(
            id,
            id,
            PathBuf::from("/tmp"),
            CursorStyle::default(),
            &Theme::adwaita(true),
            None,
            Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn deferred_pane_updates_keep_each_frontends_serials_errors_and_dirty_state() {
        let mut daemon = Daemon::empty_for_test();
        let mut session = session(1);
        let mut seed = session.new_view().unwrap();
        session.with_view(&mut seed, |engine| {
            engine.feed(b"first row\r\nsecond row\r\n");
        });
        daemon.sessions.insert(1, session);
        let (mut fast, fast_peer) = connection();
        let (mut slow, slow_peer) = connection();
        fast_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        slow_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut fast_peer = std::io::BufReader::new(fast_peer);
        let mut slow_peer = std::io::BufReader::new(slow_peer);
        for frontend in [&mut fast, &mut slow] {
            frontend.attached = true;
            frontend.state_revision = Some(0);
            daemon
                .input(
                    1,
                    frontend,
                    StreamRequest::pane(
                        1,
                        StreamCommand::Subscribe {
                            panes: HashSet::from([1]),
                        },
                    ),
                )
                .unwrap();
        }
        daemon.frontends = HashMap::from([(1, fast), (2, slow)]);
        daemon.updates(true);
        for peer in [&mut fast_peer, &mut slow_peer] {
            assert!(Packet::read(peer).unwrap().header.frame.is_some());
        }
        let mut slow = daemon.frontends.remove(&2).unwrap();
        slow.next_frame = Instant::now() + Duration::from_secs(60);
        slow.focused = true;
        daemon
            .input(
                2,
                &mut slow,
                StreamRequest::pane(1, StreamCommand::ScrollTo { row: 0, serial: 7 }),
            )
            .unwrap();
        daemon
            .input(
                2,
                &mut slow,
                StreamRequest::pane(
                    1,
                    StreamCommand::Select {
                        start: (0, 0),
                        end: (4, 0),
                        mode: Default::default(),
                        serial: 9,
                    },
                ),
            )
            .unwrap();
        daemon.frontends.insert(2, slow);
        daemon.frontends.get_mut(&1).unwrap().next_frame = Instant::now();
        daemon.changed.insert(1);
        daemon.metadata_changed.insert(1);
        daemon.errors.insert(1, "pending error".into());
        daemon.queue_notifications(1, ["paced-alert".to_string()]);
        daemon.updates(false);
        let fast = Packet::read(&mut fast_peer).unwrap().header;
        assert_eq!(fast.scroll_serial, 0);
        assert_eq!(fast.selection_serial, 0);
        assert!(fast.selection.is_none());
        assert_eq!(fast.error.as_deref(), Some("pending error"));
        assert_eq!(fast.notifications, ["paced-alert"]);
        assert!(daemon.changed.is_empty() && daemon.errors.is_empty());
        // Another broadcast may clear shared dirty state before the slow frame is due.
        daemon.updates(false);
        daemon.frontends.get_mut(&2).unwrap().next_frame = Instant::now();
        daemon.updates(false);
        let slow = Packet::read(&mut slow_peer).unwrap().header;
        assert_eq!(slow.scroll_serial, 7);
        assert_eq!(slow.selection_serial, 9);
        assert!(slow.selection.is_some());
        assert_eq!(slow.error.as_deref(), Some("pending error"));
    }

    #[test]
    fn closing_a_pane_releases_visible_retained_and_metadata_only_owners() {
        let mut daemon = Daemon::empty_for_test();
        daemon.sessions.insert(1, session(1));
        let mut peers = Vec::new();
        for id in 1..=3 {
            let (mut frontend, peer) = connection();
            frontend.attached = true;
            frontend.state_revision = Some(0);
            if id != 3 {
                daemon
                    .input(
                        id,
                        &mut frontend,
                        StreamRequest::pane(
                            1,
                            StreamCommand::Subscribe {
                                panes: HashSet::from([1]),
                            },
                        ),
                    )
                    .unwrap();
            }
            daemon
                .update_frontend(&mut frontend, true, &mut HashMap::new())
                .unwrap();
            if id == 2 {
                daemon
                    .input(
                        id,
                        &mut frontend,
                        StreamRequest::pane(
                            1,
                            StreamCommand::Subscribe {
                                panes: HashSet::new(),
                            },
                        ),
                    )
                    .unwrap();
                assert!(frontend.view(1).is_some());
                assert_eq!(frontend.retained_screens(), 0);
            } else if id == 3 {
                assert!(frontend.view(1).is_none());
                assert_eq!(frontend.retained_screens(), 0);
            } else {
                assert!(frontend.retained_screens() > 0);
            }
            daemon
                .input(
                    id,
                    &mut frontend,
                    StreamRequest::pane(
                        1,
                        StreamCommand::GitSubscribe {
                            panes: HashSet::from([1]),
                        },
                    ),
                )
                .unwrap();
            daemon
                .input(
                    id,
                    &mut frontend,
                    StreamRequest::pane(
                        1,
                        StreamCommand::PasteBegin {
                            size: 100,
                            options: Default::default(),
                        },
                    ),
                )
                .unwrap();
            frontend.pane_error(1, "undelivered".into());
            daemon.frontends.insert(id, frontend);
            peers.push(peer);
        }
        daemon.close(1);
        for frontend in daemon.frontends.values() {
            assert!(!frontend.is_visible(1));
            assert!(frontend.view(1).is_none());
            assert!(!frontend.has_git_subscriptions());
            assert_eq!(frontend.retained_screens(), 0);
            assert!(frontend.paste.is_none());
            assert!(frontend.panes.is_empty());
            assert_eq!(frontend.panes.capacity(), 0);
        }
        assert!(daemon.next_update().is_none());
        // A stale queued subscription cannot recreate a closed pane owner.
        let mut frontend = daemon.frontends.remove(&1).unwrap();
        daemon
            .input(
                1,
                &mut frontend,
                StreamRequest::pane(
                    1,
                    StreamCommand::Subscribe {
                        panes: HashSet::from([1]),
                    },
                ),
            )
            .unwrap();
        assert!(frontend.panes.is_empty());
    }

    #[test]
    fn admission_counts_unattached_streams_and_releases_slots_on_disconnect() {
        let mut daemon = Daemon::empty_for_test();
        let mut peers = Vec::new();
        for _ in 0..resources::MAX_FRONTENDS {
            let (socket, peer) = UnixStream::pair().unwrap();
            assert!(daemon.admit_frontend(socket).unwrap());
            peers.push(peer);
        }
        let (socket, mut rejected) = UnixStream::pair().unwrap();
        assert!(!daemon.admit_frontend(socket).unwrap());
        assert_eq!(rejected.read(&mut [0]).unwrap(), 0);
        drop(peers.pop());
        // Input EOF, rather than a write, detects an idle peer's teardown.
        daemon
            .frontends
            .retain(|_, frontend| frontend.requests().is_ok());
        assert_eq!(daemon.frontends.len(), resources::MAX_FRONTENDS - 1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert!(daemon.admit_frontend(socket).unwrap());
    }

    #[test]
    fn cache_pressure_releases_capacity_and_keeps_delta_invalidation_identity() {
        let (mut focused, _peer) = connection();
        let (mut hidden, _other_peer) = connection();
        focused.focused = true;
        for frontend in [&mut focused, &mut hidden] {
            let mut screen = SentScreen {
                cells: Vec::with_capacity(1024),
                source_revision: Some(9),
                generation: 7,
                cache_active: true,
                cached: 10..20,
                ..SentScreen::default()
            };
            screen.cache_bytes = screen.memory_size();
            let engine = crate::engine::Engine::new().unwrap();
            let mut presentation = PanePresentation::new(engine.new_view().unwrap());
            presentation.screen = Some(Box::new(screen));
            frontend.panes.entry(1).or_default().presentation = Some(Box::new(presentation));
        }
        let limit = focused.retained_screens();
        let mut others = HashMap::from([(2, hidden)]);
        trim_screen_caches(&mut focused, &mut others, limit);
        assert_eq!(focused.retained_screens(), limit);
        let screen = others[&2].panes[&1].screen().unwrap();
        assert_eq!(screen.memory_size(), 0);
        assert_eq!(screen.source_revision, None);
        assert_eq!(screen.generation, 7);
        assert!(screen.cache_active);
        assert_eq!(screen.cached, 10..20);
        trim_screen_caches(&mut focused, &mut others, 0);
        assert_eq!(focused.retained_screens(), 0);
        assert_eq!(focused.panes[&1].screen().unwrap().memory_size(), 0);
    }

    #[test]
    fn clipboard_binary_chunks_preserve_newlines_and_following_commands() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut frontend = Frontend::new(socket, &resources::OutputBudget::default());
        let mut header =
            serde_json::to_vec(&json!({"method":"clipboard-read-data","pane":1,"id":7,"size":6}))
                .unwrap();
        header.push(b'\n');
        peer.write_all(&header).unwrap();
        peer.write_all(b"a\n").unwrap();
        assert!(frontend.requests().unwrap().is_empty());
        assert!(frontend.binary_deadline.is_some());
        peer.write_all(b"\0{}x").unwrap();
        let mut ping = serde_json::to_vec(&json!({"method":"ping"})).unwrap();
        ping.push(b'\n');
        peer.write_all(&ping).unwrap();
        let requests = frontend.requests().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(&requests[0].command, StreamCommand::ClipboardReadData { id: 7, size: 6, bytes } if bytes == b"a\n\0{}x")
        );
        assert!(matches!(requests[1].command, StreamCommand::Ping));
        assert!(frontend.binary_deadline.is_none());
    }

    #[test]
    fn same_count_query_changes_send_new_highlights_without_terminal_output() {
        let mut daemon = Daemon::empty_for_test();
        let mut session = session(1);
        let mut seed = session.new_view().unwrap();
        session.with_view(&mut seed, |e| e.feed(b"one two one two"));
        daemon.sessions.insert(1, session);
        let (mut frontend, peer) = client((80, 24, 8, 16, 640, 384));
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut peer = std::io::BufReader::new(peer);
        for (query, start) in [("one", 0), ("two", 4)] {
            daemon
                .input(
                    1,
                    &mut frontend,
                    StreamRequest::pane(
                        1,
                        StreamCommand::Find {
                            query: query.into(),
                        },
                    ),
                )
                .unwrap();
            daemon
                .update_frontend(&mut frontend, false, &mut HashMap::new())
                .unwrap();
            let state = Packet::read(&mut peer).unwrap().header.find.unwrap();
            assert_eq!(state.total, 2);
            assert_eq!(
                state.highlights,
                [(start, start + 3, 2), (start + 8, start + 11, 1)]
            );
        }
    }

    #[test]
    fn same_count_rescans_send_highlights_without_a_viewport_change() {
        let mut daemon = Daemon::empty_for_test();
        let mut session = session(1);
        session.resize(10, 4, 8, 16, 80, 64).unwrap();
        let mut seed = session.new_view().unwrap();
        session.with_view(&mut seed, |e| {
            e.feed(b"zero\r\nfirst\r\nsecond\r\nthird\r\n         abc");
        });
        daemon.sessions.insert(1, session);
        let (mut frontend, peer) = client((10, 4, 8, 16, 80, 64));
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut peer = std::io::BufReader::new(peer);
        for command in [
            StreamCommand::ScrollTo { row: 1, serial: 1 },
            StreamCommand::Find {
                query: "abc".into(),
            },
        ] {
            daemon
                .input(1, &mut frontend, StreamRequest::pane(1, command))
                .unwrap();
        }
        daemon
            .update_frontend(&mut frontend, false, &mut HashMap::new())
            .unwrap();
        let first = Packet::read(&mut peer).unwrap().header.find.unwrap();
        assert_eq!(first.highlights, [(39, 40, 2)]);
        assert_eq!(first.total, 1);
        assert!(!first.pending);

        // Move the wrapped match wholly below the viewport, preserving the
        // visible cells, cursor, count, and selected ordinal. This scan fits
        // in one batch, so the client never sees an intermediate pending state.
        daemon
            .sessions
            .get_mut(&1)
            .unwrap()
            .with_view(&mut seed, |e| {
                e.feed(b"\x1b[4;1Hzzabc\x1b[4;3H");
            });
        frontend.panes.get_mut(&1).unwrap().terminal_changed = true;
        daemon
            .update_frontend(&mut frontend, false, &mut HashMap::new())
            .unwrap();
        let header = Packet::read(&mut peer).unwrap().header;
        assert!(header.frame.is_none());
        assert!(header.rows.is_empty());
        let second = header.find.unwrap();
        assert!(second.status() == first.status());
        assert!(second.highlights.is_empty());
    }

    #[test]
    fn search_limit_is_sent_as_an_outcome_and_hidden_views_keep_admission() {
        let mut daemon = Daemon::empty_for_test();
        std::rc::Rc::get_mut(&mut daemon.view_resources)
            .unwrap()
            .search = gmux_protocol::search::SearchBudget::new(32 * 1024);
        let mut session = session(1);
        let mut seed = session.new_view().unwrap();
        session.with_view(&mut seed, |e| e.feed(&vec![b'a'; 16000]));
        daemon.sessions.insert(1, session);
        let (mut frontend, _peer) = client((80, 24, 8, 16, 640, 384));
        daemon
            .input(
                1,
                &mut frontend,
                StreamRequest::pane(1, StreamCommand::Find { query: "a".into() }),
            )
            .unwrap();
        for _ in 0..10 {
            daemon
                .update_frontend(&mut frontend, false, &mut HashMap::new())
                .unwrap();
            if !frontend.panes[&1].find_pending() {
                break;
            }
        }
        assert!(
            frontend.panes[&1]
                .presentation
                .as_ref()
                .unwrap()
                .find
                .as_ref()
                .unwrap()
                .resource_limited
        );
        assert_eq!(daemon.view_resources.search.used(), 0);
        frontend.panes.get_mut(&1).unwrap().set_visible(false);
        assert_eq!(daemon.view_resources.count(), 1);
        frontend.close_pane(1);
        assert_eq!(daemon.view_resources.count(), 0);
        assert_eq!(daemon.view_resources.native_reserved(), 0);
    }

    #[test]
    fn metadata_dirty_ids_survive_a_skipped_frontend_deadline() {
        // Exercise the fan-out and due filter without PTYs, snapshots or sleeps.
        // Actual progress values/expiry delivery are covered by the stream smoke.
        let mut daemon = Daemon::empty_for_test();
        daemon.metadata_changed.insert(1);
        let (mut fast, _fast_peer) = connection();
        let (mut slow, _slow_peer) = connection();
        fast.attached = true;
        slow.attached = true;
        fast.state_revision = Some(0);
        slow.state_revision = Some(0);
        slow.next_frame = Instant::now() + Duration::from_secs(60);
        let slow_deadline = slow.next_frame;
        daemon.frontends = HashMap::from([(1, fast), (2, slow)]);
        daemon.updates(false);
        assert!(daemon.metadata_changed.is_empty());
        assert!(
            !daemon.frontends[&1]
                .panes
                .values()
                .any(|p| p.metadata_changed)
        );
        assert!(daemon.frontends[&2].panes[&1].metadata_changed);
        assert_eq!(daemon.next_update(), Some(slow_deadline));
        // Another connection's pass must not drain the slower pending value.
        daemon.updates(false);
        assert!(daemon.frontends[&2].panes[&1].metadata_changed);
        daemon.frontends.get_mut(&2).unwrap().next_frame = Instant::now();
        daemon.updates(false);
        assert!(
            !daemon.frontends[&2]
                .panes
                .values()
                .any(|p| p.metadata_changed)
        );
        assert!(daemon.next_update().is_none());
        daemon
            .frontends
            .get_mut(&2)
            .unwrap()
            .panes
            .entry(1)
            .or_default()
            .metadata_changed = true;
        daemon.close(1);
        assert!(
            !daemon.frontends[&2]
                .panes
                .values()
                .any(|p| p.metadata_changed)
        );
        assert_eq!((daemon.snapshots, daemon.sent_rows), (0, 0));
    }

    fn client(size: TerminalSize) -> (Frontend, UnixStream) {
        let (socket, peer) = UnixStream::pair().unwrap();
        let mut frontend = Frontend::new(socket, &resources::OutputBudget::default());
        frontend.attached = true;
        let pane = frontend.panes.entry(1).or_default();
        pane.visible = true;
        pane.size = Some(size);
        (frontend, peer)
    }

    #[test]
    fn rich_clipboard_queue_preserves_offers_and_clear_then_closes_on_overflow() {
        use gmux_protocol::clipboard::{Content, Destination, Write};
        let (mut frontend, _peer) = client((80, 24, 8, 16, 640, 384));
        let offer = |character: char| {
            let text = character.to_string().repeat(transport::MAX_PASTE);
            let html = Some(format!("<div>{text}</div>"));
            Write {
                destination: Destination::Clipboard,
                content: Some(Content { text, html }.into()),
            }
        };
        for character in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            frontend.queue_clipboard(1, offer(character)).unwrap();
        }
        frontend
            .queue_clipboard(
                1,
                Write {
                    destination: Destination::Clipboard,
                    content: None,
                },
            )
            .unwrap();
        let storage = frontend.output.inspect();
        let mut reader = storage.bytes.as_slice();
        for character in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            let packet = Packet::read(&mut reader).unwrap();
            let content = packet.header.copy.unwrap().content.unwrap();
            let text = content.plain_text().unwrap();
            assert!(text.chars().all(|c| c == character));
            assert_eq!(
                content.get("text/html").unwrap(),
                format!("<div>{text}</div>").as_bytes()
            );
        }
        assert!(
            Packet::read(&mut reader)
                .unwrap()
                .header
                .copy
                .unwrap()
                .content
                .is_none()
        );
        assert!(reader.is_empty());
        drop(storage);
        assert!(frontend.queue_clipboard(1, offer('h')).is_err());
        assert!(frontend.output.closed());
        assert_eq!(frontend.retained_output(), 0);
    }

    #[test]
    fn size_selection_prefers_focus_then_connection_order_including_the_current_client() {
        let first_size = (50, 12, 8, 16, 400, 192);
        let second_size = (70, 20, 10, 20, 700, 400);
        let (first, _first_peer) = client(first_size);
        let (mut second, _second_peer) = client(second_size);
        assert_eq!(
            requested_size(1, [(2, &second), (1, &first)].into_iter()),
            Some(first_size)
        );
        second.focused = true;
        let stored = HashMap::from([(1, first)]);
        assert_eq!(
            requested_size(
                1,
                stored
                    .iter()
                    .map(|(&id, client)| (id, client))
                    .chain(Some((2, &second)))
            ),
            Some(second_size)
        );
        second.panes.get_mut(&1).unwrap().set_visible(false);
        assert_eq!(
            requested_size(
                1,
                stored
                    .iter()
                    .map(|(&id, client)| (id, client))
                    .chain(Some((2, &second)))
            ),
            Some(first_size)
        );
    }

    #[test]
    fn size_selection_skips_unsized_hidden_and_unattached_clients() {
        let size = (70, 20, 10, 20, 700, 400);
        let (mut focused, _focused_peer) = client(size);
        focused.focused = true;
        focused.panes.get_mut(&1).unwrap().size = None;
        let (mut fallback, _fallback_peer) = client(size);
        assert_eq!(
            requested_size(1, [(1, &focused), (2, &fallback)].into_iter()),
            Some(size)
        );
        fallback.panes.get_mut(&1).unwrap().set_visible(false);
        assert_eq!(
            requested_size(1, [(1, &focused), (2, &fallback)].into_iter()),
            None
        );
        fallback.panes.get_mut(&1).unwrap().set_visible(true);
        fallback.attached = false;
        assert_eq!(
            requested_size(1, [(1, &focused), (2, &fallback)].into_iter()),
            None
        );
        assert_eq!(
            requested_size(2, [(1, &focused), (2, &fallback)].into_iter()),
            None
        );
    }

    fn connection() -> (Frontend, UnixStream) {
        let (socket, writer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        (
            Frontend::new(socket, &resources::OutputBudget::default()),
            writer,
        )
    }

    #[test]
    fn request_limit_preserves_order_and_buffered_work_without_socket_readiness() {
        let (mut frontend, mut writer) = connection();
        let count = MAX_REQUESTS_PER_VISIT * 3 + 1;
        let mut wire = Vec::new();
        for serial in 0..count {
            writeln!(
                wire,
                r#"{{"method":"scroll-to","row":0,"serial":{serial}}}"#
            )
            .unwrap();
        }
        writer.write_all(&wire).unwrap();
        let first = frontend.requests().unwrap();
        assert_eq!(first.len(), MAX_REQUESTS_PER_VISIT);
        assert!(frontend.input_pending());
        let mut fd = libc::pollfd {
            fd: frontend.socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 0) }, 0);
        let mut received = first;
        while frontend.input_pending() {
            let batch = frontend.requests().unwrap();
            assert!(batch.len() <= MAX_REQUESTS_PER_VISIT);
            received.extend(batch);
        }
        assert_eq!(received.len(), count);
        for (serial, request) in received.into_iter().enumerate() {
            assert!(
                matches!(request.command, StreamCommand::ScrollTo { serial: n, .. } if n == serial as u64)
            );
        }
        assert!(frontend.requests().unwrap().is_empty());
        assert!(!frontend.input_pending());
    }

    #[test]
    fn byte_limit_preserves_partial_lines_without_scheduling_an_idle_spin() {
        let (mut frontend, mut writer) = connection();
        writer.write_all(&vec![b' '; READ_CHUNK + 1]).unwrap();
        assert!(frontend.requests().unwrap().is_empty());
        assert_eq!(frontend.input.len(), READ_CHUNK + 1);
        assert!(!frontend.input_pending());
        let sender = std::thread::spawn(move || {
            writer
                .write_all(&vec![b' '; MAX_INPUT_PER_VISIT + 1])
                .unwrap();
            writer
                .write_all(b"{\"method\":\"attach\",\"version\":9,\"daemon\":\"00000000000000000000000000000001\"}\n")
                .unwrap();
            writer
        });
        let mut requests = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while requests.is_empty() {
            assert!(Instant::now() < deadline, "Partial line did not complete");
            let before = frontend.input.len();
            requests = frontend.requests().unwrap();
            if requests.is_empty() {
                assert!(frontend.input.len() - before <= MAX_INPUT_PER_VISIT);
                assert!(!frontend.input_pending());
            }
            std::thread::yield_now();
        }
        let _writer = sender.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].command,
            StreamCommand::Attach { version: 9, .. }
        ));
        assert!(frontend.input.is_empty());
    }

    #[test]
    fn invalid_lines_also_count_toward_the_request_limit() {
        let (mut frontend, mut writer) = connection();
        writer
            .write_all(&[b'\n'; MAX_REQUESTS_PER_VISIT + 1])
            .unwrap();
        let batch = frontend.requests().unwrap();
        assert_eq!(batch.len(), MAX_REQUESTS_PER_VISIT);
        assert!(
            batch
                .iter()
                .all(|r| matches!(r.command, StreamCommand::Invalid { .. }))
        );
        assert!(frontend.input_pending());
        assert_eq!(frontend.requests().unwrap().len(), 1);
        assert!(!frontend.input_pending());
    }

    #[test]
    fn fragmented_utf8_and_paste_commands_survive_request_and_byte_limits() {
        let (mut frontend, mut writer) = connection();
        writer.write_all(&[b'\n'; MAX_REQUESTS_PER_VISIT]).unwrap();
        writer.write_all(b"{\"method\":\"attach\",\"version\":9,\"daemon\":\"00000000000000000000000000000001\"}\n{\"method\":\"paste-begin\",\"size\":6}\n{\"method\":\"paste-data\",\"text\":\"").unwrap();
        writer.write_all(&[0xc3]).unwrap();
        assert_eq!(frontend.requests().unwrap().len(), MAX_REQUESTS_PER_VISIT);
        let batch = frontend.requests().unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(
            batch[0].command,
            StreamCommand::Attach { version: 9, .. }
        ));
        assert!(matches!(
            batch[1].command,
            StreamCommand::PasteBegin { size: 6, .. }
        ));
        writer.write_all(&[0xa9]).unwrap();
        writer
            .write_all("🙂\"}\n{\"method\":\"paste-end\"}\n".as_bytes())
            .unwrap();
        let batch = frontend.requests().unwrap();
        assert_eq!(batch.len(), 2);
        assert!(matches!(&batch[0].command, StreamCommand::PasteData { text } if text == "é🙂"));
        assert!(matches!(batch[1].command, StreamCommand::PasteEnd));
        assert!(frontend.input.is_empty());
        assert!(!frontend.input_pending());
    }

    #[test]
    fn oversized_lines_report_once_and_discard_across_turns_before_recovering() {
        let (mut frontend, mut writer) = connection();
        let sender = std::thread::spawn(move || {
            writer
                .write_all(&vec![b'x'; MAX_COMMAND + MAX_INPUT_PER_VISIT * 2])
                .unwrap();
            writer
                .write_all(b"\n{\"method\":\"attach\",\"version\":9,\"daemon\":\"00000000000000000000000000000001\"}\n")
                .unwrap();
            writer
        });
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while received.len() < 2 {
            assert!(
                Instant::now() < deadline,
                "Oversized command did not recover"
            );
            received.extend(frontend.requests().unwrap());
            assert!(frontend.input.len() <= MAX_COMMAND);
            std::thread::yield_now();
        }
        let _writer = sender.join().unwrap();
        assert_eq!(received.len(), 2);
        assert!(
            matches!(&received[0].command, StreamCommand::Invalid { message, .. } if message == "Frontend command is too large")
        );
        assert!(matches!(
            received[1].command,
            StreamCommand::Attach { version: 9, .. }
        ));
        assert!(!frontend.discarding);
        assert!(frontend.input.is_empty());
        assert!(!frontend.input_pending());
    }
}

#[cfg(test)]
#[path = "frontend_benchmark.rs"]
mod benchmark;
