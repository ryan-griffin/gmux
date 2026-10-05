//! Frontend socket buffering, terminal input, and viewport updates.
use super::*;
use crate::transport::{PasteOptions, StreamCommand, StreamRequest};

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
}
pub(super) struct Paste {
    pub(super) pane: u64,
    pub(super) size: usize,
    pub(super) text: String,
    pub(super) options: PasteOptions,
}
impl Frontend {
    pub(super) fn queue(&mut self, packet: Packet) -> anyhow::Result<()> {
        // Compact after consuming at least half the buffer, rather than moving
        // pending packets for every partial socket write.
        if self.offset > 0
            && (self.offset >= self.output.len() / 2 || self.output.len() >= MAX_PENDING)
        {
            self.output.drain(..self.offset);
            self.offset = 0;
        }
        let start = self.output.len();
        packet.encode_into(&mut self.output)?;
        if self.output.len() - self.offset > MAX_PENDING {
            self.output.truncate(start);
            anyhow::bail!("Frontend is too slow");
        }
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
impl Daemon {
    pub(super) fn input(&mut self, request: StreamRequest) -> anyhow::Result<()> {
        let StreamRequest { pane, command } = request;
        match command {
            StreamCommand::Attach { version } => {
                anyhow::ensure!(
                    version == transport::VERSION,
                    "Incompatible frontend protocol"
                );
                self.frontend.as_mut().unwrap().attached = true;
                return Ok(());
            }
            StreamCommand::Invalid { message, .. } => {
                self.frontend.as_mut().unwrap().paste = None;
                anyhow::bail!("{message}");
            }
            _ => {}
        }
        anyhow::ensure!(
            self.frontend.as_ref().is_some_and(|f| f.attached),
            "Frontend must attach first"
        );
        if let StreamCommand::Subscribe { panes } = command {
            let frontend = self.frontend.as_mut().unwrap();
            for id in &panes {
                if !frontend.visible.contains(id) {
                    frontend.screens.remove(id);
                    self.changed.insert(*id);
                }
            }
            frontend.screens.retain(|id, _| panes.contains(id));
            frontend.visible = panes;
            return Ok(());
        }
        let id = match self.resolve_pane(pane) {
            Ok(id) => id,
            Err(error) => {
                self.frontend.as_mut().unwrap().paste = None;
                return Err(error);
            }
        };
        match command {
            StreamCommand::PasteBegin { size, options } => {
                self.frontend.as_mut().unwrap().paste = None;
                anyhow::ensure!(
                    size <= transport::MAX_PASTE,
                    "Paste is too large. The limit is 4 MiB."
                );
                self.frontend.as_mut().unwrap().paste = Some(Paste {
                    pane: id,
                    size,
                    text: String::with_capacity(size),
                    options,
                });
                return Ok(());
            }
            StreamCommand::PasteData { text } => {
                let mut paste = self
                    .frontend
                    .as_mut()
                    .unwrap()
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
                self.frontend.as_mut().unwrap().paste = Some(paste);
                return Ok(());
            }
            StreamCommand::PasteEnd => {
                let paste = self
                    .frontend
                    .as_mut()
                    .unwrap()
                    .paste
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("No paste is in progress"))?;
                anyhow::ensure!(
                    paste.pane == id && paste.text.len() == paste.size,
                    "Incomplete paste"
                );
                return self.input(StreamRequest::pane(
                    id,
                    StreamCommand::Paste {
                        text: paste.text,
                        options: paste.options,
                    },
                ));
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
        match command {
            StreamCommand::Copy => {
                self.clipboard.insert(id, session.copy()?);
            }
            StreamCommand::Find { query } => session.find(&query),
            StreamCommand::FindAdvance { previous } => session.find_advance(previous),
            StreamCommand::Select { start, end } => session.select(start, end)?,
            StreamCommand::ClearSelection => session.clear_selection(),
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
                    session.scroll(0, true);
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
                        session.scroll(0, true);
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
            StreamCommand::Scroll { delta, bottom } => session.scroll(delta, bottom),
            StreamCommand::ScrollTo { row, serial } => {
                session.scroll_to(row);
                self.frontend
                    .as_mut()
                    .unwrap()
                    .scroll_serials
                    .insert(id, serial);
            }
            StreamCommand::Theme { theme } => session.set_theme(&theme)?,
            StreamCommand::Style { style } => session.set_cursor_style(style)?,
            StreamCommand::Resize {
                cols,
                rows,
                cw,
                ch,
                width,
                height,
            } => {
                anyhow::ensure!(
                    crate::viewport::valid(cols.into(), rows.into()),
                    "Invalid terminal size"
                );
                session.resize(cols, rows, cw, ch, width, height)?;
            }
            // Connection, paste framing, and acknowledgement commands return above.
            StreamCommand::Attach { .. }
            | StreamCommand::Subscribe { .. }
            | StreamCommand::PasteBegin { .. }
            | StreamCommand::PasteData { .. }
            | StreamCommand::PasteEnd
            | StreamCommand::MarkRead { .. }
            | StreamCommand::Invalid { .. } => unreachable!(),
        }
        self.changed.insert(id);
        Ok(())
    }
    pub(super) fn updates(&mut self, metadata_due: bool) -> anyhow::Result<()> {
        let Some(frontend) = self.frontend.as_mut().filter(|f| f.attached) else {
            self.changed.clear();
            return Ok(());
        };
        for (&id, session) in &mut self.sessions {
            let notify: Vec<_> = self
                .notifications
                .remove(&id)
                .unwrap_or_default()
                .messages
                .into_iter()
                .collect();
            let error = self.errors.remove(&id);
            let mut copy = self.clipboard.remove(&id);
            let changed = self.changed.contains(&id);
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
                && notify.is_empty()
                && error.is_none()
                && copy.is_none()
                && frontend.metadata.get(&id) == Some(&meta)
            {
                continue;
            }
            let mut rows = Vec::new();
            let mut cells = Vec::new();
            let frame = if screen {
                let snapshot = session.snapshot()?;
                self.snapshots += 1;
                let cols = snapshot.frame.cols as usize;
                let old = frontend.screens.entry(id).or_default();
                let full =
                    old.frame.cols != snapshot.frame.cols || old.frame.rows != snapshot.frame.rows;
                if full {
                    old.cells.resize(snapshot.cells.len(), Cell::default());
                }
                for (row, data) in snapshot.cells.chunks_exact(cols).enumerate() {
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
                    || old.frame != snapshot.frame
                    || frontend.sent_scroll_serials.get(&id) != Some(&serial);
                old.frame = snapshot.frame;
                send.then_some(snapshot.frame)
            } else {
                None
            };
            let find = session.find_state();
            let selection = session.selection();
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
        self.changed.clear();
        frontend.flush()?;
        Ok(())
    }
}
