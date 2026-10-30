//! Clipboard authority is tied to one attached desktop connection and one pane.
use super::*;
use gmux_protocol::{
    clipboard::{self, Destination, ReadEvent, ReadRequest, ReadResult, ReadStatus},
    transport::StreamCommand,
};

const MAX_PENDING_READS: usize = 16;
const MAX_READ_MEMORY: usize = 16 * 1024 * 1024;

// The pinned native Kitty encoder adds less than 1024 header bytes to each
// 5464-byte base64 DATA packet. OSC 52 is smaller. Count duplicate requests
// before reserving, and leave room for status and the bounded targets listing.
fn response_budget(request: &ReadRequest, result: &ReadResult) -> anyhow::Result<(usize, usize)> {
    let mut raw = 0;
    let mut packets = 0;
    for mime in &request.mimes {
        if let Some(c) = result.contents.iter().find(|c| &c.mime == mime) {
            raw += c.len;
            anyhow::ensure!(
                raw <= clipboard::MAX_READ_BYTES,
                "Expanded clipboard reply is too large"
            );
            packets += c.len.div_ceil(4096);
        }
    }
    Ok((
        4096 + usize::from(request.list) * 8192 + packets * 6500,
        raw,
    ))
}

struct Pending {
    request: ReadRequest,
    frontend: u64,
    deadline: Instant,
    result: Option<ReadResult>,
    data: Vec<u8>,
    reserved: usize,
    responding: bool,
}
struct PasteLease {
    frontend: u64,
    destination: Destination,
    epoch: u64,
    serial: u64,
    expires: Instant,
    rich: bool,
}
#[derive(Default)]
pub(super) struct Broker {
    pending: HashMap<u64, Pending>,
    leases: HashMap<u64, PasteLease>,
    pump: VecDeque<u64>,
    cooldown: HashMap<u64, Instant>,
    rates: HashMap<u64, (f64, Instant)>,
}

impl Frontend {
    fn queue_read(&mut self, pane: u64, event: ReadEvent) -> anyhow::Result<()> {
        self.queue(Packet {
            header: Header {
                meta: Metadata {
                    id: pane,
                    ..Default::default()
                },
                clipboard_read: Some(event),
                ..Default::default()
            },
            cells: vec![],
        })
    }
}
impl Daemon {
    pub(super) fn read_recipient(&self, pane: u64) -> Option<u64> {
        self.frontends
            .iter()
            .filter(|(_, f)| f.attached && f.clipboard_reads && f.visible.contains(&pane))
            .min_by_key(|(id, f)| (!f.focused, **id))
            .map(|(&id, _)| id)
    }
    pub(super) fn clipboard_deadline(&self) -> Option<Instant> {
        if !self.clipboard.pump.is_empty() {
            return Some(Instant::now());
        }
        self.clipboard
            .pending
            .values()
            .map(|p| p.deadline)
            .chain(self.clipboard.leases.values().map(|p| p.expires))
            .chain(self.frontends.values().filter_map(|f| f.binary_deadline))
            .min()
    }
    fn failure_result(id: u64, status: ReadStatus) -> ReadResult {
        ReadResult {
            id,
            status,
            epoch: 0,
            available: vec![],
            contents: vec![],
        }
    }
    fn reject_read(&mut self, pane: u64, id: u64, status: ReadStatus) {
        if let Some(session) = self.sessions.get_mut(&pane)
            && session.read_pending() == id
            && let Err(error) = session.complete_read(&Self::failure_result(id, status), &[], 0)
        {
            self.errors.insert(pane, error.to_string());
        }
    }
    fn cancel_read(&mut self, pane: u64, status: ReadStatus) {
        let Some(pending) = self.clipboard.pending.remove(&pane) else {
            return;
        };
        if pending.responding {
            // Already-authorized bytes must finish their frame. Keep pumping;
            // lifecycle validation is no longer a reason to truncate the OSC.
            self.clipboard.pending.insert(pane, pending);
            return;
        }
        self.reject_read(pane, pending.request.id, status);
        if let Some(frontend) = self.frontends.get_mut(&pending.frontend) {
            let _ = frontend.queue_read(
                pane,
                ReadEvent::Cancel {
                    id: pending.request.id,
                },
            );
        }
    }
    pub(super) fn cancel_frontend_reads(&mut self, frontend: u64) {
        let panes: Vec<_> = self
            .clipboard
            .pending
            .iter()
            .filter(|(_, p)| p.frontend == frontend)
            .map(|(&id, _)| id)
            .collect();
        for pane in panes {
            self.cancel_read(pane, ReadStatus::Denied);
        }
        let panes: Vec<_> = self
            .clipboard
            .leases
            .iter()
            .filter(|(_, p)| p.frontend == frontend)
            .map(|(&id, _)| id)
            .collect();
        for pane in panes {
            self.revoke_paste(pane);
        }
    }
    fn revoke_paste(&mut self, pane: u64) {
        self.clipboard.leases.remove(&pane);
        if let Some(session) = self.sessions.get_mut(&pane) {
            session.clear_grants();
        }
    }
    pub(super) fn clipboard_changed(&mut self, frontend: u64, destination: Destination) {
        let panes: Vec<_> = self
            .clipboard
            .leases
            .iter()
            .filter(|(_, p)| p.frontend == frontend && p.destination == destination)
            .map(|(&id, _)| id)
            .collect();
        for pane in panes {
            self.revoke_paste(pane);
        }
        let panes: Vec<_> = self
            .clipboard
            .pending
            .iter()
            .filter(|(_, p)| {
                p.frontend == frontend && p.request.granted && p.request.destination == destination
            })
            .map(|(&id, _)| id)
            .collect();
        for pane in panes {
            self.cancel_read(pane, ReadStatus::Denied);
        }
    }
    pub(super) fn admit_read(
        &mut self,
        pane: u64,
        recipient: Option<u64>,
        mut request: ReadRequest,
    ) {
        if self
            .sessions
            .get(&pane)
            .is_none_or(|s| s.exited() || s.read_pending() != request.id)
        {
            return;
        }
        let Some(frontend_id) = recipient else {
            self.reject_read(pane, request.id, ReadStatus::Unsupported);
            return;
        };
        let now = Instant::now();
        let frontend = &self.frontends[&frontend_id];
        if request.granted {
            let valid = self.clipboard.leases.get(&pane).is_some_and(|p| {
                p.rich
                    && p.frontend == frontend_id
                    && p.destination == request.destination
                    && p.expires > now
                    && p.epoch == frontend.clipboard_epochs[p.destination as usize]
            });
            // A native grant alone cannot authorize a different window or clipboard.
            if !valid {
                self.reject_read(pane, request.id, ReadStatus::Denied);
                return;
            }
            request.epoch = self.clipboard.leases.get(&pane).map(|p| p.epoch);
            self.revoke_paste(pane);
            request.granted = true;
        }
        if request.validate().is_err()
            || self.clipboard.pending.len() >= MAX_PENDING_READS
            || self
                .clipboard
                .pending
                .values()
                .any(|p| p.frontend == frontend_id)
            || self
                .clipboard
                .cooldown
                .get(&pane)
                .is_some_and(|until| *until > now && !request.granted && !request.mimes.is_empty())
        {
            self.reject_read(pane, request.id, ReadStatus::Busy);
            return;
        }
        let bucket = self
            .clipboard
            .rates
            .entry(frontend_id)
            .or_insert((8.0, now));
        bucket.0 = (bucket.0 + 2.0 * now.duration_since(bucket.1).as_secs_f64()).min(8.0);
        bucket.1 = now;
        if bucket.0 < 1.0 {
            self.reject_read(pane, request.id, ReadStatus::Busy);
            return;
        }
        bucket.0 -= 1.0;
        let event = ReadEvent::Request {
            request: request.clone(),
        };
        if self
            .frontends
            .get_mut(&frontend_id)
            .unwrap()
            .queue_read(pane, event)
            .is_err()
        {
            self.reject_read(pane, request.id, ReadStatus::Busy);
            return;
        }
        self.clipboard.pending.insert(
            pane,
            Pending {
                request,
                frontend: frontend_id,
                deadline: now + clipboard::READ_TIMEOUT,
                result: None,
                data: vec![],
                reserved: 0,
                responding: false,
            },
        );
    }
    /// Called with the current frontend temporarily removed from self.frontends.
    pub(super) fn clipboard_input(
        &mut self,
        frontend_id: u64,
        frontend: &mut Frontend,
        pane: u64,
        command: &StreamCommand,
    ) -> anyhow::Result<bool> {
        match command {
            StreamCommand::UserPaste {
                serial,
                destination,
                epoch,
                mimes,
            } => {
                anyhow::ensure!(
                    frontend.clipboard_reads
                        && frontend.visible.contains(&pane)
                        && *serial > frontend.paste_serial,
                    "Invalid desktop paste intent"
                );
                clipboard::validate_targets(mimes)?;
                anyhow::ensure!(
                    *epoch == frontend.clipboard_epochs[*destination as usize],
                    "Clipboard changed before paste"
                );
                frontend.paste_serial = *serial;
                self.revoke_paste(pane);
                // Each frontend has at most one unanswered read. A new paste
                // supersedes it even in another pane. This frontend is removed
                // from self.frontends during input, so queue its cancel here.
                if let Some((previous, id)) =
                    self.clipboard.pending.iter().find_map(|(&pane, p)| {
                        (p.frontend == frontend_id && !p.responding).then_some((pane, p.request.id))
                    })
                {
                    self.cancel_read(previous, ReadStatus::Busy);
                    frontend.queue_read(previous, ReadEvent::Cancel { id })?;
                }
                // A response already encoding finishes its OSC; send() queues
                // a paste for that pane after the complete response.
                self.cancel_read(pane, ReadStatus::Busy);
                let session = self.sessions.get_mut(&pane).unwrap();
                anyhow::ensure!(!session.exited(), "Session is closed");
                let rich = session.mode(5522);
                let lease = PasteLease {
                    frontend: frontend_id,
                    destination: *destination,
                    epoch: *epoch,
                    serial: *serial,
                    expires: Instant::now() + clipboard::TRANSFER_TIMEOUT,
                    rich,
                };
                if rich {
                    session.user_paste(*destination, mimes)?;
                } else {
                    frontend.queue_read(
                        pane,
                        ReadEvent::PasteText {
                            serial: *serial,
                            destination: *destination,
                            epoch: *epoch,
                        },
                    )?;
                }
                self.clipboard.leases.insert(pane, lease);
            }
            StreamCommand::UserPasteBegin { serial, epoch, .. } => {
                anyhow::ensure!(
                    self.clipboard.leases.get(&pane).is_some_and(|p| !p.rich
                        && p.frontend == frontend_id
                        && p.serial == *serial
                        && p.epoch == *epoch
                        && p.expires > Instant::now()
                        && *epoch == frontend.clipboard_epochs[p.destination as usize]),
                    "Expired desktop paste"
                );
                return Ok(false);
            }
            StreamCommand::UserPasteText {
                serial,
                epoch,
                text,
            } => {
                let valid = self.clipboard.leases.get(&pane).is_some_and(|p| {
                    !p.rich
                        && p.frontend == frontend_id
                        && p.serial == *serial
                        && p.epoch == *epoch
                        && p.expires > Instant::now()
                        && *epoch == frontend.clipboard_epochs[p.destination as usize]
                });
                anyhow::ensure!(
                    valid && text.len() <= clipboard::MAX_READ_BYTES,
                    "Expired desktop paste"
                );
                self.revoke_paste(pane);
                let session = self.sessions.get_mut(&pane).unwrap();
                let bytes = session.paste(text)?;
                session.send(&bytes)?;
            }
            StreamCommand::ClipboardReadResult { result } => {
                let Some(p) = self.clipboard.pending.get(&pane) else {
                    return Ok(true);
                };
                if p.frontend != frontend_id
                    || p.request.id != result.id
                    || p.responding
                    || p.deadline <= Instant::now()
                {
                    return Ok(true);
                }
                let total = result.byte_len()?;
                let used: usize = self.clipboard.pending.values().map(|p| p.reserved).sum();
                let p = self
                    .clipboard
                    .pending
                    .get_mut(&pane)
                    .ok_or_else(|| anyhow::anyhow!("Expired clipboard read"))?;
                anyhow::ensure!(
                    p.frontend == frontend_id
                        && p.request.id == result.id
                        && p.result.is_none()
                        && !p.responding
                        && p.deadline > Instant::now(),
                    "Stale clipboard result"
                );
                anyhow::ensure!(
                    result
                        .contents
                        .iter()
                        .all(|c| p.request.mimes.contains(&c.mime))
                        && (p.request.list || result.available.is_empty()),
                    "Unrequested clipboard data"
                );
                let Ok((_, expanded)) = response_budget(&p.request, result) else {
                    self.cancel_read(pane, ReadStatus::Busy);
                    return Ok(true);
                };
                // Reserve both exact-sized raw copies during completion. The
                // native copy expands repeated MIME entries and has less than
                // 8 KiB of descriptors. Include the owned request metadata too.
                let memory = total + expanded + 16 * 1024;
                if used + memory > MAX_READ_MEMORY || self.clipboard.pump.len() >= 4 {
                    self.cancel_read(pane, ReadStatus::Busy);
                    return Ok(true);
                }
                let p = self.clipboard.pending.get_mut(&pane).unwrap();
                if p.request.granted && result.status == ReadStatus::Success {
                    anyhow::ensure!(
                        Some(result.epoch) == p.request.epoch
                            && result.epoch
                                == frontend.clipboard_epochs[p.request.destination as usize],
                        "Clipboard changed after paste"
                    );
                }
                if result.status == ReadStatus::Denied {
                    self.clipboard
                        .cooldown
                        .insert(pane, Instant::now() + Duration::from_secs(30));
                }
                if p.data.try_reserve_exact(total).is_err() {
                    self.cancel_read(pane, ReadStatus::Busy);
                    return Ok(true);
                }
                p.reserved = memory;
                p.deadline = Instant::now() + clipboard::TRANSFER_TIMEOUT;
                p.result = Some(result.clone());
            }
            StreamCommand::ClipboardReadData { id, size, bytes } => {
                anyhow::ensure!(
                    *size == bytes.len() && *size <= clipboard::READ_CHUNK,
                    "Invalid clipboard chunk"
                );
                let Some(p) = self.clipboard.pending.get_mut(&pane) else {
                    return Ok(true);
                };
                if p.frontend != frontend_id
                    || p.request.id != *id
                    || p.responding
                    || p.deadline <= Instant::now()
                {
                    return Ok(true);
                }
                let result = p
                    .result
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing clipboard descriptors"))?;
                anyhow::ensure!(
                    p.data.len() + size <= result.byte_len()?,
                    "Excess clipboard data"
                );
                p.data.extend_from_slice(bytes);
            }
            StreamCommand::ClipboardReadEnd { id } => {
                let Some(p) = self.clipboard.pending.get_mut(&pane) else {
                    return Ok(true);
                };
                if p.frontend != frontend_id
                    || p.request.id != *id
                    || p.responding
                    || p.deadline <= Instant::now()
                {
                    return Ok(true);
                }
                let result = p
                    .result
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing clipboard descriptors"))?;
                anyhow::ensure!(
                    p.data.len() == result.byte_len()?,
                    "Incomplete clipboard data"
                );
                anyhow::ensure!(
                    result.status != ReadStatus::Success
                        || result.epoch
                            == frontend.clipboard_epochs[p.request.destination as usize],
                    "Clipboard changed during read"
                );
                let mut offset = 0;
                for c in &result.contents {
                    let data = &p.data[offset..offset + c.len];
                    offset += c.len;
                    if c.mime == "text/plain" || c.mime == "text/plain;charset=utf-8" {
                        anyhow::ensure!(
                            !data.contains(&0) && std::str::from_utf8(data).is_ok(),
                            "Invalid clipboard text"
                        );
                    }
                }
                let reservation = if result.status == ReadStatus::Success {
                    let Ok((bytes, _)) = response_budget(&p.request, result) else {
                        self.cancel_read(pane, ReadStatus::Busy);
                        return Ok(true);
                    };
                    bytes
                } else {
                    0
                };
                let queued: usize = self
                    .sessions
                    .values()
                    .map(TerminalSession::clipboard_queue_budget)
                    .sum();
                if result.status == ReadStatus::Success && queued + reservation > 32 * 1024 * 1024 {
                    self.cancel_read(pane, ReadStatus::Busy);
                    return Ok(true);
                }
                let status = result.status;
                if self
                    .sessions
                    .get_mut(&pane)
                    .unwrap()
                    .complete_read(result, &p.data, reservation)
                    .is_err()
                {
                    self.cancel_read(pane, ReadStatus::Busy);
                    return Ok(true);
                }
                p.data = vec![];
                if status == ReadStatus::Success {
                    p.responding = true;
                    self.clipboard.pump.push_back(pane);
                } else {
                    self.clipboard.pending.remove(&pane);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    pub(super) fn maintain_clipboard(&mut self) {
        let now = Instant::now();
        let stalled: Vec<_> = self
            .frontends
            .iter()
            .filter(|(_, f)| f.binary_deadline.is_some_and(|at| at <= now))
            .map(|(&id, _)| id)
            .collect();
        if !stalled.is_empty() {
            self.git_dirty = true;
        }
        for id in stalled {
            let panes: Vec<_> = self
                .clipboard
                .pending
                .iter()
                .filter(|(_, p)| p.frontend == id)
                .map(|(&pane, _)| pane)
                .collect();
            for pane in panes {
                self.cancel_read(pane, ReadStatus::Busy);
            }
            self.frontends.remove(&id);
            self.cancel_frontend_reads(id);
        }
        self.clipboard
            .rates
            .retain(|id, _| self.frontends.contains_key(id));
        let revoked: Vec<_> = self
            .clipboard
            .leases
            .iter()
            .filter(|(pane, p)| {
                p.expires <= now
                    || self.sessions.get(pane).is_none_or(TerminalSession::exited)
                    || self
                        .frontends
                        .get(&p.frontend)
                        .is_none_or(|f| !f.clipboard_reads || !f.visible.contains(pane))
                    || self.read_recipient(**pane) != Some(p.frontend)
            })
            .map(|(&id, _)| id)
            .collect();
        for pane in revoked {
            self.revoke_paste(pane);
        }
        let invalid: Vec<_> = self
            .clipboard
            .pending
            .iter()
            .filter(|(pane, p)| {
                !p.responding
                    && (p.deadline <= now
                        || self.sessions.get(pane).is_none_or(TerminalSession::exited)
                        || self
                            .frontends
                            .get(&p.frontend)
                            .is_none_or(|f| !f.clipboard_reads || !f.visible.contains(pane)))
            })
            .map(|(&id, _)| id)
            .collect();
        for pane in invalid {
            self.cancel_read(pane, ReadStatus::Busy);
        }
        self.clipboard
            .cooldown
            .retain(|pane, deadline| *deadline > now && self.sessions.contains_key(pane));
        // Exactly one bounded native encoding step per event-loop turn.
        if let Some(pane) = self.clipboard.pump.pop_front() {
            let result = self.sessions.get_mut(&pane).map(TerminalSession::step_read);
            match result {
                Some(Ok(false)) => self.clipboard.pump.push_back(pane),
                Some(Err(error)) => {
                    if let Some(session) = self.sessions.get_mut(&pane) {
                        session.abort_read();
                    }
                    self.errors.insert(pane, error.to_string());
                    self.changed.insert(pane);
                    self.clipboard.pending.remove(&pane);
                }
                _ => {
                    self.clipboard.pending.remove(&pane);
                }
            }
        }
    }
}
