//! Daemon-owned retained storage. No waits or PTY backpressure on exhaustion.
use super::*;
use gmux_protocol::graphics::Image;
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{self, IoSlice},
    rc::{Rc, Weak},
    sync::Arc,
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
    images: Rc<RefCell<ImagePool>>,
}

#[derive(Default)]
struct ImagePool {
    bytes: usize,
    retained: HashMap<*const Image, Weak<RetainedImage>>,
}

struct RetainedImage {
    image: Arc<Image>,
    pool: Rc<RefCell<ImagePool>>,
}

impl RetainedImage {
    fn capacity(&self) -> usize {
        self.image.pixels.capacity()
            + size_of::<Image>()
            + size_of::<Self>()
            + 4 * size_of::<usize>()
    }
}

impl Drop for RetainedImage {
    fn drop(&mut self) {
        let mut pool = self.pool.borrow_mut();
        pool.retained.remove(&Arc::as_ptr(&self.image));
        pool.bytes -= self.capacity();
        if pool.retained.is_empty() && pool.retained.capacity() > 1024 {
            pool.retained = HashMap::new();
        }
    }
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
            images: Default::default(),
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
            let mut total = pool.images.borrow().bytes;
            let mut victim = None;
            let mut largest = (0, 0);
            for stream in pool.streams.iter().filter_map(Weak::upgrade) {
                let storage = stream.borrow();
                total += storage.capacity();
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
    pub(super) fn capacity(&self) -> usize {
        let pool = self.0.borrow();
        let images = pool.images.borrow().bytes;
        images
            + pool
                .streams
                .iter()
                .filter_map(Weak::upgrade)
                .map(|stream| stream.borrow().capacity())
                .sum::<usize>()
    }
    fn retain_images(&self, images: Vec<Arc<Image>>) -> VecDeque<Rc<RetainedImage>> {
        let pool = self.0.borrow().images.clone();
        let mut budget = pool.borrow_mut();
        images
            .into_iter()
            .map(|image| {
                let key = Arc::as_ptr(&image);
                if let Some(retained) = budget.retained.get(&key).and_then(Weak::upgrade) {
                    return retained;
                }
                let retained = Rc::new(RetainedImage {
                    image,
                    pool: pool.clone(),
                });
                budget.bytes += retained.capacity();
                budget.retained.insert(key, Rc::downgrade(&retained));
                retained
            })
            .collect()
    }
}

struct ImageOutput {
    /// The byte buffer position where this payload belongs on the wire.
    at: usize,
    images: VecDeque<Rc<RetainedImage>>,
    offset: usize,
    pending: usize,
}

#[derive(Default)]
pub(super) struct OutputStorage {
    pub(super) bytes: Vec<u8>,
    pub(super) offset: usize,
    graphics: Option<ImageOutput>,
    closed: bool,
    blocked: bool,
}
impl OutputStorage {
    fn pending(&self) -> usize {
        self.bytes.len() - self.offset + self.graphics.as_ref().map_or(0, |g| g.pending)
    }
    fn capacity(&self) -> usize {
        self.bytes.capacity()
            + self.graphics.as_ref().map_or(0, |graphics| {
                graphics.images.capacity() * size_of::<Rc<RetainedImage>>()
            })
    }
    fn close(&mut self) {
        self.closed = true;
        self.bytes = Vec::new();
        self.offset = 0;
        self.graphics = None;
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
            if let Some(graphics) = &mut self.graphics {
                graphics.at -= self.offset;
            }
            self.offset = 0;
        }
    }
    fn write(&self, socket: &mut impl Write, mut limit: usize) -> io::Result<usize> {
        let end = if let Some(graphics) = &self.graphics {
            if self.offset == graphics.at {
                let mut slices = [IoSlice::new(&[]); 64];
                let mut used = 0;
                let mut offset = graphics.offset;
                for (slot, image) in slices.iter_mut().zip(&graphics.images) {
                    let pixels = &image.image.pixels[offset..];
                    let len = limit.min(pixels.len());
                    *slot = IoSlice::new(&pixels[..len]);
                    used += 1;
                    limit -= len;
                    if limit == 0 {
                        break;
                    }
                    offset = 0;
                }
                return socket.write_vectored(&slices[..used]);
            }
            graphics.at
        } else {
            self.bytes.len()
        };
        socket.write(&self.bytes[self.offset..end.min(self.offset + limit)])
    }
    fn advance(&mut self, len: usize) {
        if let Some(graphics) = &mut self.graphics
            && self.offset == graphics.at
        {
            graphics.offset += len;
            graphics.pending -= len;
            while let Some(image) = graphics.images.front()
                && graphics.offset >= image.image.pixels.len()
            {
                graphics.offset -= image.image.pixels.len();
                graphics.images.pop_front();
            }
            if graphics.images.is_empty() {
                self.graphics = None;
            }
        } else {
            self.offset += len;
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
    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.storage.borrow().capacity()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.storage.borrow().pending() == 0
    }
    pub(super) fn closed(&self) -> bool {
        self.storage.borrow().closed
    }
    pub(super) fn graphics_pending(&self) -> bool {
        self.storage.borrow().graphics.is_some()
    }
    pub(super) fn backlogged(&self) -> bool {
        let storage = self.storage.borrow();
        storage.blocked && storage.pending() != 0
    }
    fn enqueue(
        &self,
        additional: usize,
        encode: impl FnOnce(&mut OutputStorage, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let result = {
            let mut storage = self.storage.borrow_mut();
            anyhow::ensure!(!storage.closed, "Frontend output budget exhausted");
            storage.compact(additional);
            let limit = MAX_STREAM_OUTPUT - storage.pending();
            let result = encode(&mut storage, limit);
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
        self.enqueue(0, |storage, limit| {
            let _cost = super::profile::scope(super::profile::Stage::Encode, 1);
            let images = packet.encode_into_parts(&mut storage.bytes, limit)?;
            if !images.is_empty() {
                anyhow::ensure!(storage.graphics.is_none(), "Image payload already queued");
                storage.graphics = Some(ImageOutput {
                    at: storage.bytes.len(),
                    pending: images.iter().map(|image| image.pixels.len()).sum(),
                    images: self.budget.retain_images(images),
                    offset: 0,
                });
            }
            Ok(())
        })
    }
    pub(super) fn queue_encoded(&mut self, encoded: &[u8]) -> anyhow::Result<()> {
        self.enqueue(encoded.len(), |storage, limit| {
            anyhow::ensure!(encoded.len() <= limit, "Frontend is too slow");
            storage.bytes.extend_from_slice(encoded);
            Ok(())
        })
    }
    pub(super) fn flush(
        &mut self,
        socket: &mut impl Write,
        mut limit: usize,
    ) -> anyhow::Result<()> {
        let _cost = super::profile::scope(super::profile::Stage::Flush, 1);
        let mut storage = self.storage.borrow_mut();
        anyhow::ensure!(!storage.closed, "Frontend output budget exhausted");
        for _ in 0..16 {
            if limit == 0 || storage.pending() == 0 {
                break;
            }
            match storage.write(socket, limit) {
                Ok(0) => anyhow::bail!("Frontend disconnected"),
                Ok(n) => {
                    storage.advance(n);
                    limit -= n;
                }
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
        budget.capacity()
    }

    #[test]
    fn streams_share_queued_pixels_and_flush_them_in_wire_order() {
        use gmux_protocol::graphics::{GraphicsUpdate, Layer, PixelFormat, Placement};

        let images: Vec<_> = (1..=2)
            .map(|key| {
                Arc::new(Image {
                    key,
                    width: 32,
                    height: 32,
                    format: PixelFormat::Rgba,
                    pixels: vec![key as u8; 4096],
                })
            })
            .collect();
        let packet = || {
            Packet::Pane(transport::PaneUpdate {
                header: PaneHeader {
                    frame: Some(Frame {
                        cols: 80,
                        rows: 24,
                        ..Frame::default()
                    }),
                    graphics: Some(GraphicsUpdate {
                        replace: true,
                        cell_pixels: [8, 16],
                        placements: images
                            .iter()
                            .map(|image| Placement {
                                image: image.key,
                                layer: Layer::AboveText,
                                cell: [0, 0],
                                offset: [0, 0],
                                size: [8, 16],
                                source: [0, 0, image.width, image.height],
                                source_subpixels: None,
                                fit_rows: None,
                                sizing: None,
                            })
                            .collect(),
                        images: images.clone(),
                    }),
                    ..PaneHeader::default()
                },
                cells: Vec::new(),
            })
        };
        let mut wire = transport::HEARTBEAT_REPLY.to_vec();
        packet().encode_into(&mut wire).unwrap();
        wire.extend_from_slice(transport::HEARTBEAT_REPLY);

        let budget = OutputBudget::new(16 * 1024);
        let mut outputs = [(); 3].map(|()| Output::new(&budget));
        for output in &mut outputs {
            output.queue_encoded(transport::HEARTBEAT_REPLY).unwrap();
            output.queue(packet()).unwrap();
            output.queue_encoded(transport::HEARTBEAT_REPLY).unwrap();
            assert!(output.graphics_pending());
        }
        for (index, output) in outputs.iter_mut().enumerate() {
            let mut bytes = Vec::new();
            while !output.is_empty() {
                output.flush(&mut bytes, 769).unwrap();
            }
            assert_eq!(bytes, wire);
            assert!(!output.graphics_pending());
            if index < 2 {
                assert!(used(&budget) >= 8192);
            }
        }
        assert_eq!(
            used(&budget),
            outputs.iter().map(Output::capacity).sum::<usize>()
        );
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
        idle.flush(&mut socket, 256 * 1024).unwrap();
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
                .enqueue(0, |storage, _| packet
                    .encode_into_limit(&mut storage.bytes, 1024))
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
        output.flush(&mut socket, 256 * 1024).unwrap();
        assert!(output.backlogged());
        assert!(!output.is_empty());
        assert!(used(&budget) >= IDLE_OUTPUT * 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.is_empty() {
            assert!(Instant::now() < deadline);
            let mut bytes = [0; 65536];
            let _ = peer.read(&mut bytes);
            output.flush(&mut socket, 256 * 1024).unwrap();
        }
        assert_eq!(used(&budget), 0);
        assert!(!output.backlogged());
    }
}
