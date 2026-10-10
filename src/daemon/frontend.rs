//! Frontend socket buffering, terminal input, and viewport updates.
use super::*;
use crate::transport::{PasteOptions, StreamCommand, StreamRequest};

type TerminalSize = (u16, u16, u32, u32, u16, u16);

#[derive(Default)]
pub(super) struct SentScreen {
    pub(super) frame: Frame,
    pub(super) cells: Vec<Cell>,
}
pub(super) struct Frontend {
    pub(super) socket: UnixStream,
    pub(super) input: Vec<u8>,
    pub(super) output: Vec<u8>,
    pub(super) offset: usize,
    pub(super) attached: bool,
    pub(super) visible: HashSet<u64>,
    pub(super) screens: HashMap<u64, SentScreen>,
    pub(super) metadata: HashMap<u64, Metadata>,
    pub(super) scroll_serials: HashMap<u64, u64>,
    pub(super) sent_scroll_serials: HashMap<u64, u64>,
    pub(super) find: HashMap<u64, crate::search::FindState>,
    pub(super) selections: HashMap<u64, Option<(u64, u64)>>,
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
            output: Vec::new(),
            offset: 0,
            attached: false,
            visible: HashSet::new(),
            screens: HashMap::new(),
            metadata: HashMap::new(),
            scroll_serials: HashMap::new(),
            sent_scroll_serials: HashMap::new(),
            find: HashMap::new(),
            selections: HashMap::new(),
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
    pub(super) fn requests(&mut self) -> anyhow::Result<Vec<StreamRequest>> {
        let mut buffer = [0; 16384];
        let mut requests = Vec::new();
        for _ in 0..32 {
            let n = match self.socket.read(&mut buffer) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            for part in buffer[..n].split_inclusive(|c| *c == b'\n') {
                let complete = part.ends_with(b"\n");
                if self.discarding {
                    self.discarding = !complete;
                    continue;
                }
                if self.input.len() + part.len() > 1_048_577 {
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

fn requested_size<'a>(
    pane: u64,
    frontends: impl Iterator<Item = (u64, &'a Frontend)>,
) -> Option<TerminalSize> {
    frontends
        .filter(|(_, frontend)| {
            frontend.attached
                && frontend.visible.contains(&pane)
                && frontend.sizes.contains_key(&pane)
        })
        // Without a focused subscriber, keep a stable owner to avoid contention.
        .min_by_key(|(id, frontend)| (!frontend.focused, *id))
        .map(|(_, frontend)| frontend.sizes[&pane])
}

impl Daemon {
    pub(super) fn resize_sessions(&mut self, current: Option<(u64, &Frontend)>) {
        // Input processing temporarily removes its frontend from the map.
        for (&pane, session) in &mut self.sessions {
            let Some((cols, rows, cw, ch, width, height)) = requested_size(
                pane,
                self.frontends
                    .iter()
                    .map(|(&id, frontend)| (id, frontend))
                    .chain(current),
            ) else {
                continue;
            };
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
    }

    pub(super) fn flush_frontends(&mut self) {
        let before = self.frontends.len();
        self.frontends
            .retain(|_, frontend| frontend.flush().is_ok());
        if self.frontends.len() != before {
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
            StreamCommand::Attach { version } => {
                anyhow::ensure!(
                    version == transport::VERSION,
                    "Incompatible frontend protocol"
                );
                frontend.attached = true;
                return Ok(());
            }
            StreamCommand::Invalid { message, .. } => {
                frontend.paste = None;
                anyhow::bail!("{message}");
            }
            _ => {}
        }
        anyhow::ensure!(frontend.attached, "Frontend must attach first");
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
            frontend.visible = panes;
            self.resize_sessions(Some((frontend_id, frontend)));
            return Ok(());
        }
        let id = match self.resolve_pane(pane) {
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
                crate::viewport::valid(cols.into(), rows.into()),
                "Invalid terminal size"
            );
            frontend
                .sizes
                .insert(id, (cols, rows, cw, ch, width, height));
            self.resize_sessions(Some((frontend_id, frontend)));
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
            _ => {}
        }
        let session = self.sessions.get_mut(&id).unwrap();
        if let std::collections::hash_map::Entry::Vacant(entry) = frontend.views.entry(id) {
            entry.insert(session.new_view()?);
        }
        let view = frontend.views.get_mut(&id).unwrap();
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
                session.set_theme(&theme)?;
                self.changed.insert(id);
            }
            StreamCommand::Style { style } => {
                session.set_cursor_style(style)?;
                self.changed.insert(id);
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
                    StreamCommand::Select { start, end } => engine.select(start, end)?,
                    StreamCommand::ClearSelection => engine.clear_selection(),
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
                    || frontend
                        .views
                        .values()
                        .any(crate::engine::EngineView::find_pending))
        })
    }
    pub(super) fn updates(&mut self, metadata_due: bool) {
        let before = self.frontends.len();
        let mut ids: Vec<_> = self.frontends.keys().copied().collect();
        ids.sort_by_key(|id| (!self.frontends[id].focused, *id));
        // Every window receives the same revision. Read the catalog, capture
        // metadata, and serialize it once for this broadcast.
        let mut state = None;
        // Deliver each alert once, preferring a focused GUI. All clients receive unread state.
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
            if let Err(error) = result {
                eprintln!("Detaching frontend: {error}");
            } else {
                self.frontends.insert(id, frontend);
            }
        }
        if !self.frontends.values().any(|f| f.attached) {
            self.notifications = notifications;
        }
        self.changed.clear();
        self.errors.clear();
        if self.frontends.len() != before {
            // Keep handoff changes pending even if survivors were already sent a frame.
            self.resize_sessions(None);
        }
    }
    fn state_packet(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Packet {
            header: Header {
                state: Some(self.shared_state()?),
                meta: Metadata {
                    id: 0,
                    directory: PathBuf::new(),
                    title: String::new(),
                    size: (80, 24),
                    exited: false,
                    unread: 0,
                    busy: false,
                    modes: [false; 2],
                },
                frame: None,
                scroll_serial: 0,
                rows: Vec::new(),
                notifications: Vec::new(),
                error: None,
                find: None,
                selection: None,
                copy: None,
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
    ) -> anyhow::Result<()> {
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
            let (frame, find, selection) = if screen || frontend.views.contains_key(&id) {
                if let std::collections::hash_map::Entry::Vacant(entry) = frontend.views.entry(id) {
                    entry.insert(session.new_view()?);
                }
                let view = frontend.views.get_mut(&id).unwrap();
                if terminal_changed {
                    view.invalidate();
                }
                session.with_view(view, |engine| -> anyhow::Result<_> {
                    engine.poll_find();
                    if screen {
                        engine.snapshot()?;
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
                        }
                        for (row, data) in cells_snapshot.chunks_exact(cols).enumerate() {
                            let previous = &mut old.cells[row * cols..(row + 1) * cols];
                            if full || *previous != *data {
                                rows.push(row as u16);
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
                    Ok((frame, engine.find_state(), engine.selection()))
                })?
            } else {
                (None, crate::search::FindState::default(), None)
            };
            let find_changed = frontend.find.get(&id) != Some(&find);
            let selection_changed = frontend.selections.get(&id) != Some(&selection);
            if frame.is_none()
                && !find_changed
                && !selection_changed
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            frontend.find.insert(id, find.clone());
            frontend.selections.insert(id, selection);
            frontend.metadata.insert(id, meta.clone());
            frontend
                .sent_scroll_serials
                .insert(id, frontend.scroll_serials.get(&id).copied().unwrap_or(0));
            // Large selections should not share the size limit of a full viewport.
            if let Some(text) = copy.take() {
                frontend.queue(Packet {
                    header: Header {
                        state: None,
                        meta: meta.clone(),
                        frame: None,
                        scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
                        rows: Vec::new(),
                        notifications: Vec::new(),
                        error: None,
                        find: None,
                        selection,
                        copy: Some(text),
                    },
                    cells: Vec::new(),
                })?;
            }
            frontend.queue(Packet {
                header: Header {
                    state: None,
                    meta,
                    frame,
                    scroll_serial: frontend.scroll_serials.get(&id).copied().unwrap_or(0),
                    rows,
                    notifications: notify,
                    error,
                    find: find_changed.then_some(find),
                    selection,
                    copy,
                },
                cells,
            })?;
        }
        frontend.changed.clear();
        frontend.flush()?;
        Ok(())
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
}
