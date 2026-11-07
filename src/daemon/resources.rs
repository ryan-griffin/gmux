//! Daemon-owned retained storage. No waits or PTY backpressure on exhaustion.
use super::*;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

pub(super) const MAX_OUTPUT: usize = 128 * 1024 * 1024;
pub(super) const MAX_SCREEN_CACHES: usize = 32 * 1024 * 1024;
pub(super) const MAX_FRONTENDS: usize = 64;
const MAX_STREAM_OUTPUT: usize = 64 * 1024 * 1024;
const IDLE_OUTPUT: usize = 256 * 1024;

#[derive(Clone)]
pub(super) struct OutputBudget(Rc<RefCell<OutputPool>>);
struct OutputPool {
    limit: usize,
    streams: Vec<Weak<RefCell<OutputStorage>>>,
}
impl Default for OutputBudget {
    fn default() -> Self {
        Self::new(MAX_OUTPUT)
    }
}
impl OutputBudget {
    fn new(limit: usize) -> Self {
        Self(Rc::new(RefCell::new(OutputPool {
            limit,
            streams: Vec::new(),
        })))
    }
    fn register(&self) -> Output {
        let storage = Rc::new(RefCell::new(OutputStorage::default()));
        let mut pool = self.0.borrow_mut();
        pool.streams.retain(|stream| stream.strong_count() != 0);
        pool.streams.push(Rc::downgrade(&storage));
        Output {
            storage,
            budget: self.clone(),
        }
    }
    fn enforce(&self) {
        let pool = self.0.borrow();
        loop {
            let mut total = 0;
            let mut victim = None;
            let mut largest = (0, 0);
            for stream in pool.streams.iter().filter_map(Weak::upgrade) {
                let storage = stream.borrow();
                total += storage.bytes.capacity();
                // Prefer the largest unsent backlog. Capacity breaks ties,
                // including idle reusable buffers. Local/remote and focus do
                // not affect eligibility for slow-consumer disconnection.
                let rank = (storage.pending(), storage.bytes.capacity());
                if rank > largest {
                    largest = rank;
                    victim = Some(stream.clone());
                }
            }
            if total <= pool.limit {
                return;
            }
            // Reclaim idle capacity before disconnecting a consumer.
            for stream in pool.streams.iter().filter_map(Weak::upgrade) {
                let mut storage = stream.borrow_mut();
                if storage.pending() == 0 {
                    total -= storage.bytes.capacity();
                    storage.bytes = Vec::new();
                }
            }
            if total <= pool.limit {
                return;
            }
            let victim = victim.expect("Over-budget output has an allocation");
            victim.borrow_mut().close();
        }
    }
}

#[derive(Default)]
pub(super) struct OutputStorage {
    pub(super) bytes: Vec<u8>,
    pub(super) offset: usize,
    closed: bool,
    blocked: bool,
}
impl OutputStorage {
    fn pending(&self) -> usize {
        self.bytes.len() - self.offset
    }
    fn close(&mut self) {
        self.closed = true;
        self.bytes = Vec::new();
        self.offset = 0;
        self.blocked = false;
    }
    fn compact(&mut self, additional: usize) {
        // Amortize partial-write compaction, but reclaim consumed space before
        // a known append would grow the buffer past the stream ceiling.
        if self.offset > 0
            && (self.offset >= self.bytes.len() / 2
                || self.bytes.len().saturating_add(additional) >= MAX_STREAM_OUTPUT)
        {
            self.bytes.drain(..self.offset);
            self.offset = 0;
        }
    }
}

pub(super) struct Output {
    storage: Rc<RefCell<OutputStorage>>,
    budget: OutputBudget,
}
impl Output {
    pub(super) fn new(budget: &OutputBudget) -> Self {
        budget.register()
    }
    pub(super) fn capacity(&self) -> usize {
        self.storage.borrow().bytes.capacity()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.storage.borrow().pending() == 0
    }
    pub(super) fn closed(&self) -> bool {
        self.storage.borrow().closed
    }
    pub(super) fn backlogged(&self) -> bool {
        let storage = self.storage.borrow();
        storage.blocked && storage.pending() != 0
    }
    fn enqueue(
        &mut self,
        additional: usize,
        encode: impl FnOnce(&mut Vec<u8>, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let result = {
            let mut storage = self.storage.borrow_mut();
            anyhow::ensure!(!storage.closed, "Frontend output budget exhausted");
            storage.compact(additional);
            let limit = MAX_STREAM_OUTPUT - storage.pending();
            let result = encode(&mut storage.bytes, limit);
            if result.is_err() {
                // Encoding may grow capacity even when it rolls back length.
                // A failed enqueue closes the stream, never skips a record.
                storage.close();
            } else {
                // Packet sizes become known during encoding. Keep the full
                // pending-byte allowance even when a consumed prefix remained.
                storage.compact(0);
                if storage.bytes.capacity() > MAX_STREAM_OUTPUT {
                    storage.bytes.shrink_to_fit();
                }
            }
            result
        };
        self.budget.enforce();
        result?;
        anyhow::ensure!(!self.closed(), "Frontend output budget exhausted");
        Ok(())
    }
    pub(super) fn queue(&mut self, packet: Packet) -> anyhow::Result<()> {
        self.enqueue(0, |bytes, limit| packet.encode_into_limit(bytes, limit))
    }
    pub(super) fn queue_encoded(&mut self, encoded: &[u8]) -> anyhow::Result<()> {
        self.enqueue(encoded.len(), |bytes, limit| {
            anyhow::ensure!(encoded.len() <= limit, "Frontend is too slow");
            bytes.extend_from_slice(encoded);
            Ok(())
        })
    }
    pub(super) fn flush(&mut self, socket: &mut UnixStream) -> anyhow::Result<()> {
        let mut storage = self.storage.borrow_mut();
        anyhow::ensure!(!storage.closed, "Frontend output budget exhausted");
        let end = storage.bytes.len().min(storage.offset + 256 * 1024);
        while storage.offset < end {
            match socket.write(&storage.bytes[storage.offset..end]) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => storage.offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    storage.blocked = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if storage.pending() == 0 {
            if storage.bytes.capacity() > IDLE_OUTPUT {
                storage.bytes = Vec::new();
            } else {
                storage.bytes.clear();
            }
            storage.offset = 0;
            storage.blocked = false;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn inspect(&self) -> std::cell::Ref<'_, OutputStorage> {
        self.storage.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(budget: &OutputBudget) -> usize {
        budget
            .0
            .borrow()
            .streams
            .iter()
            .filter_map(Weak::upgrade)
            .map(|stream| stream.borrow().bytes.capacity())
            .sum()
    }

    #[test]
    fn aggregate_capacity_evicts_the_largest_backlog_and_tracks_detached_handles() {
        let budget = OutputBudget::new(32 * 1024);
        let mut slow = Output::new(&budget);
        let mut other = Output::new(&budget);
        let mut interactive = Output::new(&budget);
        slow.queue_encoded(&vec![0; 16 * 1024]).unwrap();
        other.queue_encoded(&vec![1; 8 * 1024]).unwrap();
        assert_eq!(used(&budget), slow.capacity() + other.capacity());
        interactive.queue_encoded(&vec![2; 12 * 1024]).unwrap();
        assert!(slow.closed());
        assert!(!other.closed());
        assert_eq!(used(&budget), other.capacity() + interactive.capacity());
        assert!(used(&budget) <= 32 * 1024);
        drop(other);
        assert_eq!(used(&budget), interactive.capacity());
        drop(interactive);
        assert_eq!(used(&budget), 0);
        assert!(slow.queue_encoded(b"no reopening").is_err());
    }

    #[test]
    fn idle_capacity_is_charged_and_reclaimed_before_disconnect() {
        let budget = OutputBudget::new(24 * 1024);
        let mut idle = Output::new(&budget);
        let mut active = Output::new(&budget);
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        idle.queue_encoded(&vec![1; 16 * 1024]).unwrap();
        idle.flush(&mut socket).unwrap();
        peer.read_exact(&mut vec![0; 16 * 1024]).unwrap();
        assert_eq!(used(&budget), idle.capacity());
        assert!(used(&budget) >= 16 * 1024);
        active.queue_encoded(&vec![2; 20 * 1024]).unwrap();
        assert!(!idle.closed());
        assert!(!active.closed());
        assert_eq!(idle.capacity(), 0);
        assert_eq!(used(&budget), active.capacity());
    }

    #[test]
    fn failed_encoding_releases_growth_and_prior_storage_without_skipping_a_record() {
        let budget = OutputBudget::default();
        let mut output = Output::new(&budget);
        output.queue_encoded(transport::HEARTBEAT_REPLY).unwrap();
        let packet = Packet::Pane(gmux_protocol::transport::PaneUpdate {
            header: PaneHeader {
                error: Some("x".repeat(8192)),
                ..PaneHeader::default()
            },
            cells: Vec::new(),
        });
        assert!(
            output
                .enqueue(0, |bytes, _| packet.encode_into_limit(bytes, 1024))
                .is_err()
        );
        assert!(output.closed());
        assert_eq!(used(&budget), 0);
        assert!(output.queue_encoded(b"later reply").is_err());
    }

    #[test]
    fn consumed_prefix_does_not_reject_output_that_fits_the_stream_ceiling() {
        const MIB: usize = 1024 * 1024;
        for preencoded in [true, false] {
            let budget = OutputBudget::default();
            let mut output = Output::new(&budget);
            let packet = Packet::Pane(gmux_protocol::transport::PaneUpdate {
                header: PaneHeader {
                    error: Some("x".repeat(8 * MIB)),
                    ..PaneHeader::default()
                },
                cells: Vec::new(),
            });
            let mut encoded = Vec::new();
            packet.encode_into(&mut encoded).unwrap();
            output.queue_encoded(&vec![1; 60 * MIB]).unwrap();
            // Model a partial write smaller than the normal compaction threshold.
            output.storage.borrow_mut().offset = 5 * MIB;
            if preencoded {
                output.queue_encoded(&encoded).unwrap();
            } else {
                output.queue(packet).unwrap();
            }
            let storage = output.inspect();
            assert_eq!(storage.pending(), 55 * MIB + encoded.len());
            assert_eq!(storage.offset, 0);
            assert!(storage.bytes[..55 * MIB].iter().all(|byte| *byte == 1));
            assert_eq!(storage.bytes[55 * MIB..], encoded);
            assert!(storage.bytes.capacity() <= MAX_STREAM_OUTPUT);
            assert!(!storage.closed);
        }
    }

    #[test]
    fn a_drained_large_buffer_releases_its_allocation() {
        let budget = OutputBudget::default();
        let mut output = Output::new(&budget);
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        let send_buffer: libc::c_int = 8192;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&send_buffer as *const libc::c_int).cast(),
                    size_of_val(&send_buffer) as libc::socklen_t,
                )
            },
            0
        );
        socket.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        output.queue_encoded(&vec![3; IDLE_OUTPUT * 2]).unwrap();
        assert!(!output.backlogged());
        output.flush(&mut socket).unwrap();
        assert!(output.backlogged());
        assert!(!output.is_empty());
        assert!(used(&budget) >= IDLE_OUTPUT * 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.is_empty() {
            assert!(Instant::now() < deadline);
            let mut bytes = [0; 65536];
            let _ = peer.read(&mut bytes);
            output.flush(&mut socket).unwrap();
        }
        assert_eq!(used(&budget), 0);
        assert!(!output.backlogged());
    }
}
