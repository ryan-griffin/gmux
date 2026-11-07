//! Ordered pane reconciliation and local viewport state. No stream I/O or wakeups.
use gmux_protocol::{
    Cell, Frame,
    selection::Selection,
    session::{ScreenSnapshot, SessionUpdate},
    theme::Theme,
    transport::{KeyAction, Metadata, PaneUpdate, StreamCommand},
};
use std::{
    cell::Cell as ValueCell,
    time::{Duration, Instant},
};

const CACHED_SCROLL_SETTLE: Duration = Duration::from_millis(100);
static NO_HYPERLINKS: gmux_protocol::hyperlinks::Hyperlinks =
    gmux_protocol::hyperlinks::Hyperlinks {
        targets: Vec::new(),
        spans: Vec::new(),
    };

/// An enqueue attempt is committed separately from producing an effect. The
/// caller must report its result before applying another action or pane update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScrollCommand {
    pub row: u64,
    pub serial: u64,
}
impl ScrollCommand {
    pub fn command(self) -> StreamCommand {
        StreamCommand::ScrollTo {
            row: self.row,
            serial: self.serial,
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum EnqueueOutcome {
    Accepted,
    Rejected,
}
/// Presentation flags can coalesce; the scroll effect must be attempted and
/// acknowledged before processing the next event. Neither owns queue credit.
pub(super) struct ViewChanges {
    pub presentation: SessionUpdate,
    pub scroll: Option<ScrollCommand>,
}
pub(super) struct Viewport {
    meta: Metadata,
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
    cache: crate::scroll_cache::RowCache,
    prediction: Option<Prediction>,
    scroll_target: Option<u64>,
    scroll_defer_until: ValueCell<Option<Instant>>,
    scroll_retry_pending: ValueCell<bool>,
}
struct Prediction {
    frame: Frame,
    cells: Vec<Cell>,
    row_flags: Vec<u8>,
    // Drop storage before returning its reservation to the shared pool.
    reservation: Option<crate::scroll_cache::PredictionReservation>,
}
impl Viewport {
    pub fn new(meta: Metadata, theme: &Theme, cadence: Duration, focused: bool) -> Self {
        Self {
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
            scroll_cadence: cadence,
            pending_scroll: ValueCell::new(None),
            find: gmux_protocol::search::FindState::default(),
            selection: None,
            selection_serial: ValueCell::new(0),
            selection_acknowledged: 0,
            selection_origin: None,
            row_flags: vec![0; 24],
            hyperlinks: Default::default(),
            hyperlink_revision: 0,
            cache: {
                let cache = crate::scroll_cache::RowCache::default();
                cache.set_focused(focused);
                cache
            },
            prediction: None,
            scroll_target: None,
            scroll_defer_until: ValueCell::new(None),
            scroll_retry_pending: ValueCell::new(false),
        }
    }
    /// Packets have already passed protocol validation and must arrive in wire
    /// order. Always update canonical cells, even while showing a prediction.
    pub fn apply(&mut self, packet: PaneUpdate, visible: bool, now: Instant) -> ViewChanges {
        let mut update = SessionUpdate {
            exited: packet.header.meta.exited,
            notifications: packet.header.notifications,
            directory_changed: self.meta.directory != packet.header.meta.directory,
            progress_changed: self.meta.progress != packet.header.meta.progress,
            program_status_changed: self.meta.program_status != packet.header.meta.program_status,
            error: packet.header.error,
            ..SessionUpdate::default()
        };
        self.meta = packet.header.meta;
        update.unread = Some(self.unread());
        let invalidated =
            packet.header.generation != 0 && self.cache.invalidate(packet.header.generation);
        if invalidated {
            self.scroll_defer_until.set(None);
            if self.prediction.take().is_some() {
                self.revision = self.revision.wrapping_add(1);
                self.text_revision = self.text_revision.wrapping_add(1);
                update.changed = true;
            }
        }
        let removed = if packet.header.generation != 0 {
            self.cache.rebase(packet.header.cache_origin)
        } else {
            0
        };
        if !invalidated && removed != 0 {
            if let Some(prediction) = &mut self.prediction {
                if prediction.frame.scrollback.offset < removed {
                    self.prediction = None;
                } else {
                    prediction.frame.scrollback.offset -= removed;
                    prediction.frame.scrollback.total =
                        prediction.frame.scrollback.total.saturating_sub(removed);
                }
                self.revision = self.revision.wrapping_add(1);
                self.text_revision = self.text_revision.wrapping_add(1);
                update.changed = true;
            }
            if let Some(target) = self.scroll_target {
                let target = target.saturating_sub(removed);
                self.scroll_target = Some(target);
                self.pending_scroll.set(Some(target));
                self.scroll_serial.set(self.scroll_serial.get() + 1);
                self.scroll_defer_until.set(None);
            }
        }
        if let Some(end) = packet.header.cache_valid_before {
            self.cache.truncate(end);
            if self
                .scroll_target
                .is_some_and(|row| !self.cache.has_margin(self.frame, row))
            {
                self.scroll_defer_until.set(None);
            }
            if let Some(prediction) = &self.prediction {
                let start = prediction.frame.scrollback.offset;
                if start + prediction.frame.rows as u64 > end {
                    self.prediction = None;
                    self.revision = self.revision.wrapping_add(1);
                    self.text_revision = self.text_revision.wrapping_add(1);
                    update.changed = true;
                }
            }
        }
        if let Some(find) = packet.header.find {
            update.changed |= self.find != find;
            if (find.total != 0 || find.pending) && self.prediction.take().is_some() {
                self.scroll_defer_until.set(None);
                self.revision = self.revision.wrapping_add(1);
                self.text_revision = self.text_revision.wrapping_add(1);
                update.changed = true;
            }
            self.find = find;
        }
        // Preserve the local prediction until the daemon has
        // processed the latest selection command, including clears.
        if packet.header.selection_serial >= self.selection_serial.get() {
            update.changed |= self.selection != packet.header.selection
                || self.selection_acknowledged != packet.header.selection_serial;
            self.selection = packet.header.selection;
            self.selection_origin = packet.header.selection_origin;
            self.selection_acknowledged = packet.header.selection_serial;
        }
        if let Some(frame) = packet.header.frame {
            let dimensions_changed = (frame.cols, frame.rows) != (self.frame.cols, self.frame.rows);
            let mut text_changed = dimensions_changed || packet.header.row_shift != 0;
            self.cells
                .resize(frame.cols as usize * frame.rows as usize, Cell::default());
            self.row_flags.resize(frame.rows as usize, 0);
            let cols = frame.cols as usize;
            gmux_protocol::viewport::shift_rows(&mut self.cells, cols, packet.header.row_shift);
            gmux_protocol::viewport::shift_rows(&mut self.row_flags, 1, packet.header.row_shift);
            for (index, row) in packet.header.rows.iter().enumerate() {
                let flags = packet.header.row_flags.get(index).copied().unwrap_or(0);
                text_changed |= self.row_flags[*row as usize] != flags;
                self.row_flags[*row as usize] = flags;
                let previous = &mut self.cells[*row as usize * cols..(*row as usize + 1) * cols];
                let next = &packet.cells[index * cols..(index + 1) * cols];
                // Text consumers use glyph bytes and wide-cell
                // layout, not rendition.
                text_changed =
                    text_changed || previous.iter().zip(next).any(|(a, b)| !a.text_eq(b));
                previous.clone_from_slice(next);
            }
            if packet.header.hyperlinks.is_some() || dimensions_changed {
                self.hyperlink_revision = self.hyperlink_revision.wrapping_add(1);
            }
            if let Some(links) = packet.header.hyperlinks {
                self.hyperlinks = links;
            } else if dimensions_changed {
                self.hyperlinks = Default::default();
            }
            self.frame = frame;
            if visible && (packet.header.cache.is_some() || !self.cache.is_empty()) {
                if self.cache.configure(packet.header.generation, frame) {
                    self.scroll_defer_until.set(None);
                    if self.prediction.take().is_some() {
                        text_changed = true;
                    }
                }
                if let Some(cache) = packet.header.cache {
                    self.cache.insert(
                        cache.start,
                        &packet.cells[packet.header.rows.len() * cols..],
                        &cache.row_flags,
                    );
                }
                // Include unchanged visible rows too. Supplemental
                // history never substitutes for authoritative deltas.
                let Self {
                    cache,
                    cells,
                    row_flags,
                    ..
                } = self;
                cache.insert(frame.scrollback.offset, cells, row_flags);
            }
            self.scroll_acknowledged = self.scroll_acknowledged.max(packet.header.scroll_serial);
            let mut prediction_changed = false;
            if !self.scroll_pending() {
                self.scroll_target = None;
                if self.prediction.take().is_some() {
                    text_changed = true;
                    prediction_changed = true;
                }
            } else if let Some(target) = self.scroll_target {
                prediction_changed = self.predict(target);
                text_changed |= prediction_changed;
            }
            if !packet.header.rows.is_empty() || packet.header.row_shift != 0 || prediction_changed
            {
                self.revision = self.revision.wrapping_add(1);
            }
            if text_changed {
                self.text_revision = self.text_revision.wrapping_add(1);
            }
            update.changed = true;
        }

        ViewChanges {
            presentation: update,
            scroll: self.due_scroll(now),
        }
    }
    pub fn scroll_to(&mut self, row: u64, now: Instant) -> ViewChanges {
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
            .then(|| now + CACHED_SCROLL_SETTLE),
        );
        ViewChanges {
            presentation: SessionUpdate {
                changed: predicted,
                ..SessionUpdate::default()
            },
            scroll: self.due_scroll(now),
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
            reservation: None,
            frame: self.frame,
            cells: Vec::new(),
            row_flags: Vec::new(),
        });
        if !self.cache.copy_prediction(
            row,
            self.frame.rows as usize,
            &mut prediction.cells,
            &mut prediction.row_flags,
            &mut prediction.reservation,
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
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.prediction = None;
        self.scroll_defer_until.set(None);
    }
    pub fn scroll_release(&self) -> Option<Instant> {
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
    fn due_scroll(&self, now: Instant) -> Option<ScrollCommand> {
        if self.scroll_defer_until.get().is_some_and(|at| now < at) {
            return None;
        }
        // Release at the display cadence even if replies take longer than one
        // frame. Waiting for every acknowledgement would pace input at the RTT.
        (self.scroll_acknowledged >= self.scroll_sent.get()
            || now.saturating_duration_since(self.scroll_sent_at.get()) >= self.scroll_cadence)
            .then(|| self.forced_scroll())
            .flatten()
    }
    pub fn forced_scroll(&self) -> Option<ScrollCommand> {
        self.pending_scroll.get().map(|row| ScrollCommand {
            row,
            serial: self.scroll_serial.get(),
        })
    }
    pub fn scroll_enqueued(&self, command: ScrollCommand, now: Instant, outcome: EnqueueOutcome) {
        match outcome {
            EnqueueOutcome::Rejected => self.scroll_retry_pending.set(true),
            EnqueueOutcome::Accepted => {
                self.scroll_retry_pending.set(false);
                self.scroll_sent.set(command.serial);
                self.scroll_sent_at.set(now);
                self.pending_scroll.set(None);
                self.scroll_defer_until.set(None);
            }
        }
    }
    pub fn retry_scroll(&self, now: Instant) -> Option<ScrollCommand> {
        // Manager maintenance runs on incoming data, writer progress or a paced
        // deadline. A rejected enqueue suppresses deadlines until the next turn.
        self.scroll_retry_pending.set(false);
        self.due_scroll(now)
    }
    // Key releases and appearance updates keep scroll coalescing. All other
    // commands must follow the latest target, even before its paced deadline.
    pub fn command_needs_scroll(command: &StreamCommand) -> bool {
        !matches!(
            command,
            StreamCommand::Key {
                action: KeyAction::Release,
                ..
            } | StreamCommand::Theme { .. }
                | StreamCommand::Style { .. }
        )
    }
    pub fn next_selection_serial(&self) -> u64 {
        self.selection_serial.get() + 1
    }
    pub fn selection_enqueued(&self, serial: u64) {
        self.selection_serial.set(serial);
    }
    pub fn next_prompt_serial(&self) -> u64 {
        self.scroll_serial.get() + 1
    }
    pub fn prompt_enqueued(&mut self, serial: u64) {
        self.scroll_serial.set(serial);
        self.scroll_sent.set(serial);
        self.scroll_target = None;
        self.prediction = None;
    }
    pub fn resized(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != (self.frame.cols, self.frame.rows) {
            self.clear_cache();
        }
    }
    pub fn meta(&self) -> &Metadata {
        &self.meta
    }
    pub fn install_metadata(&mut self, mut meta: Metadata) {
        // A control reply can predate queued stream progress reports.
        meta.progress = self.meta.progress;
        meta.program_status = self.meta.program_status.take();
        self.meta = meta;
    }
    pub fn mark_read(&mut self) -> u64 {
        let sequence = self.meta.unread_sequence;
        self.read_through = self.read_through.max(sequence);
        sequence
    }
    pub fn unread(&self) -> bool {
        self.meta.unread_sequence > self.read_through
    }
    pub fn set_cadence(&mut self, interval: Duration) {
        self.scroll_cadence = interval;
    }
    pub fn set_focused(&self, active: bool) {
        self.cache.set_focused(active);
    }
    pub fn find_state(&self) -> &gmux_protocol::search::FindState {
        &self.find
    }
    pub fn selection_pending(&self) -> bool {
        self.selection_acknowledged < self.selection_serial.get()
    }
    pub fn selection_origin(&self) -> Option<u64> {
        (!self.selection_pending())
            .then_some(self.selection_origin)
            .flatten()
    }
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }
}

#[cfg(test)]
mod tests;
