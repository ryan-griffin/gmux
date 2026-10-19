//! Frontend socket buffering, terminal input, and viewport updates.
use super::*;
use gmux_client::transport::{PasteOptions, StreamCommand, StreamRequest};

type TerminalSize = (u16, u16, u32, u32, u16, u16);
const READ_CHUNK: usize = 16 * 1024;
const MAX_INPUT_PER_TURN: usize = 512 * 1024;
const MAX_REQUESTS_PER_TURN: usize = 64;
const MAX_COMMAND: usize = 1_048_577;

#[derive(Default)]
pub(super) struct SentScreen {
    pub(super) frame: Frame,
    pub(super) cells: Vec<Cell>,
    pub(super) row_flags: Vec<u8>,
}
pub(super) struct SentSelection {
    range: Option<(u64, u64)>,
    serial: u64,
}
pub(super) struct Frontend {
    pub(super) socket: UnixStream,
    pub(super) input: Vec<u8>,
    read_buffer: Vec<u8>,
    read_offset: usize,
    pub(super) output: Vec<u8>,
    pub(super) offset: usize,
    pub(super) attached: bool,
    pub(super) remote: bool,
    pub(super) visible: HashSet<u64>,
    pub(super) git_panes: HashSet<u64>,
    pub(super) git_sent: HashMap<u64, transport::GitMetadata>,
    git_revision: Option<u64>,
    pub(super) screens: HashMap<u64, SentScreen>,
    pub(super) metadata: HashMap<u64, Metadata>,
    pub(super) scroll_serials: HashMap<u64, u64>,
    pub(super) sent_scroll_serials: HashMap<u64, u64>,
    pub(super) find: HashMap<u64, gmux_client::search::FindState>,
    pub(super) selections: HashMap<u64, SentSelection>,
    pub(super) selection_serials: HashMap<u64, u64>,
    pub(super) discarding: bool,
    pub(super) paste: Option<Paste>,
    pub(super) views: HashMap<u64, crate::engine::EngineView>,
    pub(super) clipboard: HashMap<u64, String>,
    pub(super) errors: HashMap<u64, String>,
    pub(super) state_revision: Option<u64>,
    pub(super) sizes: HashMap<u64, TerminalSize>,
    pub(super) focused: bool,
    pub(super) changed: HashSet<u64>,
}
pub(super) struct Paste {
    pub(super) pane: u64,
    pub(super) size: usize,
    pub(super) text: String,
    pub(super) options: PasteOptions,
}
impl Frontend {
    pub(super) fn new(socket: UnixStream) -> Self {
        Self {
            socket,
            input: Vec::new(),
            read_buffer: Vec::new(),
            read_offset: 0,
            output: Vec::new(),
            offset: 0,
            attached: false,
            remote: false,
            visible: HashSet::new(),
            git_panes: HashSet::new(),
            git_sent: HashMap::new(),
            git_revision: None,
            screens: HashMap::new(),
            metadata: HashMap::new(),
            scroll_serials: HashMap::new(),
            sent_scroll_serials: HashMap::new(),
            find: HashMap::new(),
            selections: HashMap::new(),
            selection_serials: HashMap::new(),
            discarding: false,
            paste: None,
            views: HashMap::new(),
            clipboard: HashMap::new(),
            errors: HashMap::new(),
            state_revision: None,
            sizes: HashMap::new(),
            focused: false,
            changed: HashSet::new(),
        }
    }
    fn compact_output(&mut self) {
        // Compact after consuming at least half the buffer, rather than moving
        // pending packets for every partial socket write.
        if self.offset > 0
            && (self.offset >= self.output.len() / 2 || self.output.len() >= MAX_PENDING)
        {
            self.output.drain(..self.offset);
            self.offset = 0;
        }
    }
    pub(super) fn queue(&mut self, packet: Packet) -> anyhow::Result<()> {
        self.compact_output();
        let start = self.output.len();
        packet.encode_into(&mut self.output)?;
        if self.output.len() - self.offset > MAX_PENDING {
            self.output.truncate(start);
            anyhow::bail!("Frontend is too slow");
        }
        Ok(())
    }
    pub(super) fn queue_encoded(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.compact_output();
        anyhow::ensure!(
            self.output.len() - self.offset + bytes.len() <= MAX_PENDING,
            "Frontend is too slow"
        );
        self.output.extend_from_slice(bytes);
        Ok(())
    }
    pub(super) fn flush(&mut self) -> anyhow::Result<()> {
        while self.offset < self.output.len() {
            match self.socket.write(&self.output[self.offset..]) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => self.offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if self.offset == self.output.len() {
            self.output.clear();
            self.offset = 0;
        }
        Ok(())
    }
    pub(super) fn input_pending(&self) -> bool {
        self.read_offset < self.read_buffer.len()
    }
    pub(super) fn requests(&mut self) -> anyhow::Result<Vec<StreamRequest>> {
        // Keep the decoded batch small even for empty or invalid JSON lines.
        // The unread suffix survives the request limit and schedules another turn.
        let mut requests = Vec::new();
        let mut budget = MAX_INPUT_PER_TURN;
        let mut reads = 0;
        while budget > 0 && requests.len() < MAX_REQUESTS_PER_TURN {
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
            let end = self.read_buffer.len().min(self.read_offset + budget);
            for part in self.read_buffer[self.read_offset..end].split_inclusive(|c| *c == b'\n') {
                if requests.len() == MAX_REQUESTS_PER_TURN {
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
                    requests.push(StreamRequest::decode(&self.input));
                    self.input.clear();
                }
            }
        }
        Ok(requests)
    }
}

fn size_owner<'a>(
    pane: u64,
    frontends: impl Iterator<Item = (u64, &'a Frontend)>,
) -> Option<&'a Frontend> {
    frontends
        .filter(|(_, frontend)| {
            frontend.attached
                && frontend.visible.contains(&pane)
                && frontend.sizes.contains_key(&pane)
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
    size_owner(pane, frontends).map(|frontend| frontend.sizes[&pane])
}

impl Daemon {
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
        if let Some(view) = owner.views.get(&pane)
            && let Err(error) = session.use_view_appearance(view)
        {
            self.errors.insert(pane, error.to_string());
            self.changed.insert(pane);
        }
        let (cols, rows, cw, ch, width, height) = owner.sizes[&pane];
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

    pub(super) fn flush_frontends(&mut self) {
        let before = self.frontends.len();
        self.frontends
            .retain(|_, frontend| frontend.flush().is_ok());
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
            for id in &panes {
                if !frontend.visible.contains(id) {
                    frontend.screens.remove(id);
                    frontend.changed.insert(*id);
                }
            }
            frontend.screens.retain(|id, _| panes.contains(id));
            for (id, view) in &mut frontend.views {
                view.set_active(panes.contains(id));
            }
            frontend.visible = panes;
            self.resize_sessions(Some((frontend_id, frontend)));
            return Ok(());
        }
        if let StreamCommand::GitSubscribe { mut panes } = command {
            panes.retain(|id| self.sessions.contains_key(id));
            frontend.git_sent.retain(|id, _| panes.contains(id));
            frontend.git_panes = panes;
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
                gmux_client::viewport::valid(cols.into(), rows.into()),
                "Invalid terminal size"
            );
            frontend
                .sizes
                .insert(id, (cols, rows, cw, ch, width, height));
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
                        StreamCommand::Paste {
                            text: paste.text,
                            options: paste.options,
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
                    if let Some(view) = client.views.get_mut(&id) {
                        view.restart_find();
                    }
                }
                self.changed.insert(id);
                return Ok(());
            }
            _ => {}
        }
        let session = self.sessions.get_mut(&id).unwrap();
        if let std::collections::hash_map::Entry::Vacant(entry) = frontend.views.entry(id) {
            let mut view = session.new_view()?;
            view.set_active(frontend.visible.contains(&id));
            entry.insert(view);
        }
        let view = frontend.views.get_mut(&id).unwrap();
        let appearance_changed = matches!(
            command,
            StreamCommand::Theme { .. } | StreamCommand::Style { .. }
        );
        match command {
            StreamCommand::Input { bytes } => session.send(&bytes)?,
            StreamCommand::Paste { text, options } => {
                anyhow::ensure!(
                    text.len() <= transport::MAX_PASTE,
                    "Paste is too large. The limit is 4 MiB."
                );
                let mut bytes = session.paste(&text, options.clipboard)?;
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
                cp,
                mods,
                release,
                text,
            } => {
                let bytes = session.key(&name, cp, mods, release, &text);
                if !bytes.is_empty() {
                    if !release {
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
            }
            StreamCommand::Style { style } => {
                view.set_cursor_style(style)?;
            }
            command => session.with_view(view, |engine| -> anyhow::Result<()> {
                // Search navigation uses this client's current viewport metadata.
                engine.snapshot()?;
                match command {
                    StreamCommand::Copy => {
                        anyhow::ensure!(
                            engine.selection().is_some(),
                            "Select terminal text to copy it"
                        );
                        frontend.clipboard.insert(id, engine.copy()?);
                    }
                    StreamCommand::Find { query } => engine.find(&query),
                    StreamCommand::FindAdvance { previous } => engine.find_advance(previous),
                    StreamCommand::Select {
                        start,
                        end,
                        mode,
                        serial,
                    } => {
                        frontend.selection_serials.insert(id, serial);
                        engine.select(start, end, mode)?
                    }
                    StreamCommand::ClearSelection { serial } => {
                        frontend.selection_serials.insert(id, serial);
                        engine.clear_selection();
                    }
                    StreamCommand::Scroll { delta, bottom } => engine.scroll(delta, bottom),
                    StreamCommand::ScrollTo { row, serial } => {
                        engine.scroll_to(row);
                        frontend.scroll_serials.insert(id, serial);
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })?,
        }
        frontend.changed.insert(id);
        if appearance_changed {
            self.resize_session(id, Some((frontend_id, frontend)));
        }
        Ok(())
    }
    pub(super) fn updates_pending(&self) -> bool {
        self.frontends.values().any(|frontend| {
            frontend.attached
                && (!self.changed.is_empty()
                    || !self.notifications.is_empty()
                    || !self.errors.is_empty()
                    || !frontend.changed.is_empty()
                    || frontend.state_revision != Some(self.revision)
                    || (!frontend.git_panes.is_empty()
                        && frontend.git_revision
                            != self.git.as_ref().map(crate::branch::Service::revision))
                    || frontend
                        .views
                        .values()
                        .any(crate::engine::EngineView::find_pending))
        })
    }
    /// Return whether viewport or search work used the frame interval.
    pub(super) fn updates(&mut self, metadata_due: bool) -> bool {
        let before = self.frontends.len();
        let mut frame_activity = false;
        let mut ids: Vec<_> = self.frontends.keys().copied().collect();
        ids.sort_by_key(|id| (!self.frontends[id].focused, *id));
        // Every client receives the same execution revision. Serialize each transport snapshot once per broadcast.
        let mut state = None;
        // Deliver each alert once, preferring a focused client. All clients receive unread state.
        let mut notifications = std::mem::take(&mut self.notifications);
        for id in ids {
            let mut frontend = self.frontends.remove(&id).unwrap();
            if !frontend.attached {
                self.frontends.insert(id, frontend);
                continue;
            }
            let result = (|| {
                if frontend.state_revision != Some(self.revision) {
                    let bytes = state
                        .get_or_insert_with(|| self.state_packet())
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    frontend.queue_encoded(bytes)?;
                    frontend.state_revision = Some(self.revision);
                }
                self.update_frontend(&mut frontend, metadata_due, &mut notifications)
            })();
            match result {
                Ok(active) => {
                    frame_activity |= active;
                    self.frontends.insert(id, frontend);
                }
                Err(error) => eprintln!("Detaching frontend: {error}"),
            }
        }
        if !self.frontends.values().any(|f| f.attached) {
            self.notifications = notifications;
        }
        self.changed.clear();
        self.errors.clear();
        if self.frontends.len() != before {
            self.git_dirty = true;
            // Keep handoff changes pending even if survivors were already sent a frame.
            self.resize_sessions(None);
        }
        frame_activity
    }
    fn state_packet(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Packet {
            header: Header {
                state: Some(self.execution_state()),
                meta: Metadata {
                    id: 0,
                    directory: PathBuf::new(),
                    title: String::new(),
                    size: (80, 24),
                    exited: false,
                    unread_sequence: 0,
                    busy: false,
                    modes: [false; 2],
                },
                ..Header::default()
            },
            cells: Vec::new(),
        }
        .encode_into(&mut bytes)?;
        Ok(bytes)
    }
    fn update_frontend(
        &mut self,
        frontend: &mut Frontend,
        metadata_due: bool,
        notifications: &mut HashMap<u64, NotificationQueue>,
    ) -> anyhow::Result<bool> {
        let mut frame_activity = false;
        if let Some(git) = &self.git
            && frontend.git_revision != Some(git.revision())
        {
            let updates: Vec<_> = frontend
                .git_panes
                .iter()
                .filter_map(|&pane| git.metadata(pane))
                .filter(|meta| frontend.git_sent.get(&meta.id) != Some(meta))
                .collect();
            for meta in updates {
                frontend.git_sent.insert(meta.id, meta.clone());
                frontend.queue(Packet {
                    header: Header {
                        git: Some(meta),
                        ..Header::default()
                    },
                    cells: Vec::new(),
                })?;
            }
            frontend.git_revision = Some(git.revision());
        }
        for (&id, session) in &mut self.sessions {
            let notify: Vec<_> = notifications
                .remove(&id)
                .unwrap_or_default()
                .messages
                .into_iter()
                .collect();
            let error = frontend
                .errors
                .remove(&id)
                .or_else(|| self.errors.get(&id).cloned());
            let mut copy = frontend.clipboard.remove(&id);
            let terminal_changed = self.changed.contains(&id);
            let changed = terminal_changed
                || frontend.changed.contains(&id)
                || frontend
                    .views
                    .get(&id)
                    .is_some_and(crate::engine::EngineView::find_pending);
            let screen =
                frontend.visible.contains(&id) && (changed || !frontend.screens.contains_key(&id));
            if !metadata_due
                && !changed
                && !screen
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.contains_key(&id)
            {
                continue;
            }
            let meta = Self::metadata(session, self.unread.get(&id).copied().unwrap_or(0));
            if !screen
                && !changed
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            let mut rows = Vec::new();
            let mut cells = Vec::new();
            let mut row_flags = Vec::new();
            let (frame, find, selection, selection_origin) = if screen
                || frontend.views.contains_key(&id)
            {
                if let std::collections::hash_map::Entry::Vacant(entry) = frontend.views.entry(id) {
                    let mut view = session.new_view()?;
                    view.set_active(frontend.visible.contains(&id));
                    entry.insert(view);
                }
                let view = frontend.views.get_mut(&id).unwrap();
                if terminal_changed {
                    view.invalidate();
                }
                session.with_view(view, |engine| -> anyhow::Result<_> {
                    // Search scans bounded batches even when its visible state
                    // is unchanged. Keep those batches paced instead of spinning.
                    frame_activity |= engine.find_pending();
                    engine.poll_find();
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
                        let old = frontend.screens.entry(id).or_default();
                        let full = old.frame.cols != frame.cols || old.frame.rows != frame.rows;
                        if full {
                            old.cells.resize(cells_snapshot.len(), Cell::default());
                            old.row_flags.resize(frame.rows as usize, 0);
                        }
                        for (row, data) in cells_snapshot.chunks_exact(cols).enumerate() {
                            let previous = &mut old.cells[row * cols..(row + 1) * cols];
                            let flags = engine.row_flags[row];
                            if full || *previous != *data || old.row_flags[row] != flags {
                                rows.push(row as u16);
                                row_flags.push(flags);
                                old.row_flags[row] = flags;
                                cells.extend_from_slice(data);
                                previous.copy_from_slice(data);
                            }
                        }
                        self.sent_rows += rows.len() as u64;
                        // A client can discard every row while detached; attachment always resends a full viewport.
                        let serial = frontend.scroll_serials.get(&id).copied().unwrap_or(0);
                        let send = full
                            || !rows.is_empty()
                            || old.frame != frame
                            || frontend.sent_scroll_serials.get(&id) != Some(&serial);
                        old.frame = frame;
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
                (None, gmux_client::search::FindState::default(), None, None)
            };
            let find_changed = frontend.find.get(&id) != Some(&find);
            let selection_serial = frontend.selection_serials.get(&id).copied().unwrap_or(0);
            let previous = frontend.selections.get(&id);
            let selection_changed = previous.is_none_or(|sent| sent.range != selection);
            let selection_ack_changed = previous.is_none_or(|sent| sent.serial != selection_serial);
            if frame.is_none()
                && !find_changed
                && !selection_changed
                && !selection_ack_changed
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            frontend.find.insert(id, find.clone());
            frontend.selections.insert(
                id,
                SentSelection {
                    range: selection,
                    serial: selection_serial,
                },
            );
            frontend.metadata.insert(id, meta.clone());
            frontend
                .sent_scroll_serials
                .insert(id, frontend.scroll_serials.get(&id).copied().unwrap_or(0));
            let viewport_changed = screen && (frame.is_some() || find_changed || selection_changed);
            // Large selections should not share the size limit of a full viewport.
            if let Some(text) = copy.take() {
                frontend.queue(Packet {
                    header: Header {
                        git: None,
                        state: None,
                        meta: meta.clone(),
                        frame: None,
                        scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
                        rows: Vec::new(),
                        notifications: Vec::new(),
                        error: None,
                        find: None,
                        selection,
                        selection_serial,
                        selection_origin,
                        row_flags: Vec::new(),
                        copy: Some(text),
                    },
                    cells: Vec::new(),
                })?;
            }
            frontend.queue(Packet {
                header: Header {
                    git: None,
                    state: None,
                    meta,
                    frame,
                    scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
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
                    copy,
                },
                cells,
            })?;
            frame_activity |= viewport_changed;
        }
        frontend.changed.clear();
        frontend.flush()?;
        Ok(frame_activity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(size: TerminalSize) -> (Frontend, UnixStream) {
        let (socket, peer) = UnixStream::pair().unwrap();
        let mut frontend = Frontend::new(socket);
        frontend.attached = true;
        frontend.visible.insert(1);
        frontend.sizes.insert(1, size);
        (frontend, peer)
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
        second.visible.clear();
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
        focused.sizes.clear();
        let (mut fallback, _fallback_peer) = client(size);
        assert_eq!(
            requested_size(1, [(1, &focused), (2, &fallback)].into_iter()),
            Some(size)
        );
        fallback.visible.clear();
        assert_eq!(
            requested_size(1, [(1, &focused), (2, &fallback)].into_iter()),
            None
        );
        fallback.visible.insert(1);
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
        (Frontend::new(socket), writer)
    }

    #[test]
    fn request_limit_preserves_order_and_buffered_work_without_socket_readiness() {
        let (mut frontend, mut writer) = connection();
        let count = MAX_REQUESTS_PER_TURN * 3 + 1;
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
        assert_eq!(first.len(), MAX_REQUESTS_PER_TURN);
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
            assert!(batch.len() <= MAX_REQUESTS_PER_TURN);
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
                .write_all(&vec![b' '; MAX_INPUT_PER_TURN + 1])
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
                assert!(frontend.input.len() - before <= MAX_INPUT_PER_TURN);
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
            .write_all(&[b'\n'; MAX_REQUESTS_PER_TURN + 1])
            .unwrap();
        let batch = frontend.requests().unwrap();
        assert_eq!(batch.len(), MAX_REQUESTS_PER_TURN);
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
        writer.write_all(&[b'\n'; MAX_REQUESTS_PER_TURN]).unwrap();
        writer.write_all(b"{\"method\":\"attach\",\"version\":9,\"daemon\":\"00000000000000000000000000000001\"}\n{\"method\":\"paste-begin\",\"size\":6}\n{\"method\":\"paste-data\",\"text\":\"").unwrap();
        writer.write_all(&[0xc3]).unwrap();
        assert_eq!(frontend.requests().unwrap().len(), MAX_REQUESTS_PER_TURN);
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
                .write_all(&vec![b'x'; MAX_COMMAND + MAX_INPUT_PER_TURN * 2])
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
