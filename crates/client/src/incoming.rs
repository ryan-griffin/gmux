//! Ordered stream backpressure bounded by retained bytes as well as packet count.
use gmux_protocol::transport::Packet;
use std::sync::{Arc, mpsc};

pub const MAX_BYTES: usize = 64 * 1024 * 1024;

struct Budget {
    limit: usize,
    ready: Arc<tokio::sync::Semaphore>,
}
struct Message {
    item: Result<Packet, String>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
fn packet_size(packet: &Packet) -> usize {
    packet.memory_size() + std::mem::size_of::<Message>() - std::mem::size_of::<Packet>()
}
#[derive(Clone)]
pub struct Sender {
    sender: tokio::sync::mpsc::Sender<Message>,
    budget: Arc<Budget>,
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
    let (sender, receiver) = tokio::sync::mpsc::channel(packets);
    let budget = Arc::new(Budget {
        limit: bytes,
        ready: Arc::new(tokio::sync::Semaphore::new(bytes)),
    });
    (
        Sender {
            sender,
            budget: budget.clone(),
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
    pub async fn send_async(&self, item: Result<Packet, String>) -> Result<(), SendError> {
        let bytes = match &item {
            Ok(packet) => packet_size(packet),
            Err(error) => std::mem::size_of::<Message>() + error.capacity(),
        };
        if bytes > self.budget.limit {
            return Err(SendError::TooLarge);
        }
        let permit = self
            .budget
            .ready
            .clone()
            .acquire_many_owned(bytes.try_into().unwrap())
            .await
            .map_err(|_| SendError::Closed)?;
        self.sender
            .send(Message {
                item,
                _permit: permit,
            })
            .await
            .map_err(|_| SendError::Closed)
    }
}
impl Receiver {
    pub fn try_recv(&self) -> Result<Result<Packet, String>, mpsc::TryRecvError> {
        // Release the queue's reservation as ownership moves to the main thread.
        self.receiver
            .borrow_mut()
            .try_recv()
            .map(|message| message.item)
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
