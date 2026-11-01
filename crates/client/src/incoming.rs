//! Ordered stream backpressure bounded by retained bytes as well as packet count.
use gmux_protocol::transport::Packet;
use std::sync::{Arc, OnceLock, mpsc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MAX_BYTES: usize = 64 * 1024 * 1024;
pub const PROCESS_BYTES: usize = 128 * 1024 * 1024;

fn process_budget() -> Arc<Semaphore> {
    static BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BUDGET
        .get_or_init(|| Arc::new(Semaphore::new(PROCESS_BYTES)))
        .clone()
}

struct Budget {
    limit: usize,
    ready: Arc<tokio::sync::Semaphore>,
}
struct Message {
    item: Result<Packet, String>,
    reservation: Reservation,
}
pub struct Reservation {
    _permit: OwnedSemaphorePermit,
    _shared: OwnedSemaphorePermit,
}
fn packet_size(packet: &Packet) -> usize {
    packet.memory_size() + std::mem::size_of::<Message>() - std::mem::size_of::<Packet>()
}
#[derive(Clone)]
pub struct Sender {
    sender: tokio::sync::mpsc::Sender<Message>,
    budget: Arc<Budget>,
    shared: Arc<Semaphore>,
}
pub struct Receiver {
    receiver: std::cell::RefCell<tokio::sync::mpsc::Receiver<Message>>,
    budget: Arc<Budget>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    TooLarge,
    Closed,
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "Incoming packet exceeds memory budget",
            Self::Closed => "Incoming stream is closed",
        })
    }
}
impl std::error::Error for SendError {}

pub fn channel(bytes: usize, packets: usize) -> (Sender, Receiver) {
    channel_with_budget(bytes, packets, process_budget())
}

pub(super) fn channel_with_budget(
    bytes: usize,
    packets: usize,
    shared: Arc<Semaphore>,
) -> (Sender, Receiver) {
    let (sender, receiver) = tokio::sync::mpsc::channel(packets);
    let budget = Arc::new(Budget {
        limit: bytes,
        ready: Arc::new(tokio::sync::Semaphore::new(bytes)),
    });
    (
        Sender {
            sender,
            budget: budget.clone(),
            shared,
        },
        Receiver {
            receiver: std::cell::RefCell::new(receiver),
            budget,
        },
    )
}
impl Sender {
    #[cfg(test)]
    pub fn send(&self, item: Result<Packet, String>) -> Result<(), SendError> {
        crate::runtime::block_on(self.send_async(item))
    }
    // Wait only after a non-heartbeat prefix. Idle connections hold no decode
    // reservation. The full local ceiling covers decoding and admission; shrink
    // it to retained capacity when the packet moves into the queue.
    pub async fn reserve_decode(&self) -> Result<OwnedSemaphorePermit, SendError> {
        self.acquire_shared(self.budget.limit).await
    }
    async fn acquire_shared(&self, bytes: usize) -> Result<OwnedSemaphorePermit, SendError> {
        tokio::select! {
            biased;
            _ = self.sender.closed() => Err(SendError::Closed),
            permit = self.shared.clone().acquire_many_owned(bytes as u32) => {
                permit.map_err(|_| SendError::Closed)
            }
        }
    }
    pub async fn send_async(&self, item: Result<Packet, String>) -> Result<(), SendError> {
        self.send_reserved(item, None).await
    }
    pub async fn send_reserved(
        &self,
        item: Result<Packet, String>,
        shared: Option<OwnedSemaphorePermit>,
    ) -> Result<(), SendError> {
        let bytes = match &item {
            Ok(packet) => packet_size(packet),
            Err(error) => std::mem::size_of::<Message>() + error.capacity(),
        };
        if bytes > self.budget.limit {
            return Err(SendError::TooLarge);
        }
        let mut shared = match shared {
            Some(permit) => permit,
            None => self.acquire_shared(bytes).await?,
        };
        assert!(shared.num_permits() >= bytes);
        let excess = shared.num_permits() - bytes;
        drop(shared.split(excess));
        let permit = self
            .budget
            .ready
            .clone()
            .acquire_many_owned(bytes as u32)
            .await
            .map_err(|_| SendError::Closed)?;
        self.sender
            .send(Message {
                item,
                reservation: Reservation {
                    _permit: permit,
                    _shared: shared,
                },
            })
            .await
            .map_err(|_| SendError::Closed)
    }
}
impl Receiver {
    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.budget.limit - self.budget.ready.available_permits()
    }
    #[cfg(test)]
    pub fn try_recv(&self) -> Result<Result<Packet, String>, mpsc::TryRecvError> {
        self.try_recv_reserved().map(|(item, _reservation)| item)
    }
    pub fn try_recv_reserved(
        &self,
    ) -> Result<(Result<Packet, String>, Reservation), mpsc::TryRecvError> {
        // Keep the reservation until the UI finishes applying the packet.
        self.receiver
            .borrow_mut()
            .try_recv()
            .map(|message| (message.item, message.reservation))
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TryRecvError::Empty => mpsc::TryRecvError::Empty,
                tokio::sync::mpsc::error::TryRecvError::Disconnected => {
                    mpsc::TryRecvError::Disconnected
                }
            })
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.budget.ready.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn byte_backpressure_preserves_order_and_releases_reservations() {
        let size = std::mem::size_of::<Message>() + 1024;
        let (sender, receiver) = channel(size * 2, 8);
        sender.send(Err("a".repeat(1024))).unwrap();
        sender.send(Err("b".repeat(1024))).unwrap();
        let (started, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = crate::runtime::spawn(async move {
            started.send(()).unwrap();
            sender.send_async(Err("c".repeat(1024))).await.unwrap();
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done.recv_timeout(Duration::from_millis(30)).is_err());
        // A full byte budget must yield the shared executor to unrelated work.
        let (progress, progressed) = mpsc::channel();
        crate::runtime::spawn(async move {
            progress.send(()).unwrap();
        });
        progressed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(receiver.budget.ready.available_permits(), 0);
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('a')));
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        crate::runtime::block_on(worker).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('b')));
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('c')));
        assert_eq!(receiver.budget.ready.available_permits(), size * 2);
    }

    #[test]
    fn dropping_the_receiver_unblocks_a_waiting_reader() {
        let size = std::mem::size_of::<Message>() + 1024;
        let (sender, receiver) = channel(size, 8);
        sender.send(Err("a".repeat(1024))).unwrap();
        let worker =
            crate::runtime::spawn(async move { sender.send_async(Err("b".repeat(1024))).await });
        drop(receiver);
        assert!(crate::runtime::block_on(worker).unwrap().is_err());
    }
}

#[cfg(test)]
mod memory_tests;
#[cfg(test)]
mod packet_tests;

#[cfg(test)]
mod shared_tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    #[test]
    fn aggregate_admission_is_fifo_and_transfer_remains_charged() {
        let size = size_of::<Message>() + 100;
        let shared = Arc::new(Semaphore::new(size * 2));
        let (a, ar) = channel_with_budget(size * 2, 8, shared.clone());
        let (b, br) = channel_with_budget(size * 2, 8, shared.clone());
        let (c, cr) = channel_with_budget(size * 2, 8, shared.clone());
        a.send(Err("a".repeat(100))).unwrap();
        b.send(Err("b".repeat(100))).unwrap();
        let mut first = pin!(c.send_async(Err("c".repeat(100))));
        let mut second = pin!(b.send_async(Err("d".repeat(100))));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        let (_, transfer) = ar.try_recv_reserved().unwrap();
        assert_eq!(shared.available_permits(), 0);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        drop(transfer);
        // Polling the later waiter first cannot steal admission.
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        assert!(matches!(br.try_recv(), Ok(Err(s)) if s.starts_with('b')));
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        assert!(matches!(br.try_recv(), Ok(Err(s)) if s.starts_with('d')));
        assert!(matches!(cr.try_recv(), Ok(Err(s)) if s.starts_with('c')));
        assert_eq!(shared.available_permits(), size * 2);
    }

    #[test]
    fn decode_credit_shrinks_without_double_charging_and_packet_slot_cancellation_releases_it() {
        let size = size_of::<Message>() + 100;
        let shared = Arc::new(Semaphore::new(size * 8));
        let (sender, receiver) = channel_with_budget(size * 4, 1, shared.clone());
        let decode = crate::runtime::block_on(sender.reserve_decode()).unwrap();
        assert_eq!(shared.available_permits(), size * 4);
        crate::runtime::block_on(sender.send_reserved(Err("x".repeat(100)), Some(decode))).unwrap();
        assert_eq!(shared.available_permits(), size * 7);
        {
            let mut pending = pin!(sender.send_async(Err("y".repeat(100))));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            assert_eq!(shared.available_permits(), size * 6);
            assert_eq!(receiver.retained_bytes(), size * 2);
        }
        assert_eq!(shared.available_permits(), size * 7);
        assert_eq!(receiver.retained_bytes(), size);
        drop(receiver);
        assert_eq!(shared.available_permits(), size * 8);
    }

    #[test]
    fn cancellation_and_closure_release_shared_and_local_waits() {
        let size = size_of::<Message>() + 100;
        let shared = Arc::new(Semaphore::new(size * 2));
        let (a, ar) = channel_with_budget(size, 1, shared.clone());
        let (b, br) = channel_with_budget(size, 1, shared.clone());
        a.send(Err("a".repeat(100))).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        {
            // Holds shared bytes while waiting for the stream-local limit.
            let mut pending = pin!(a.send_async(Err("b".repeat(100))));
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            assert_eq!(shared.available_permits(), 0);
        }
        assert_eq!(shared.available_permits(), size);
        let decode = crate::runtime::block_on(b.reserve_decode()).unwrap();
        assert_eq!(shared.available_permits(), 0);
        let mut waiting = pin!(b.reserve_decode());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        drop(br);
        assert!(matches!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Err(SendError::Closed))
        ));
        drop(decode);
        drop(ar);
        assert_eq!(shared.available_permits(), size * 2);
    }
}
