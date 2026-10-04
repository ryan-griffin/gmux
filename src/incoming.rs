//! Ordered stream backpressure bounded by retained bytes as well as packet count.
use crate::transport::Packet;
use std::sync::{Arc, Condvar, Mutex, mpsc};

pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct State {
    bytes: usize,
    closed: bool,
}
struct Budget {
    limit: usize,
    state: Mutex<State>,
    ready: Condvar,
}
struct Permit {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.state.lock().unwrap().bytes -= self.bytes;
        self.budget.ready.notify_all();
    }
}
struct Message {
    item: Result<Packet, String>,
    _permit: Permit,
}
pub(crate) fn packet_size(packet: &Packet) -> usize {
    packet.memory_size() + std::mem::size_of::<Message>() - std::mem::size_of::<Packet>()
}
#[derive(Clone)]
pub(crate) struct Sender {
    sender: mpsc::SyncSender<Message>,
    budget: Arc<Budget>,
}
pub(crate) struct Receiver {
    receiver: mpsc::Receiver<Message>,
    budget: Arc<Budget>,
}

pub(crate) fn channel(bytes: usize, packets: usize) -> (Sender, Receiver) {
    let (sender, receiver) = mpsc::sync_channel(packets);
    let budget = Arc::new(Budget {
        limit: bytes,
        state: Mutex::new(State::default()),
        ready: Condvar::new(),
    });
    (
        Sender {
            sender,
            budget: budget.clone(),
        },
        Receiver { receiver, budget },
    )
}
impl Sender {
    pub(crate) fn send(&self, item: Result<Packet, String>) -> anyhow::Result<()> {
        let bytes = match &item {
            Ok(packet) => packet_size(packet),
            Err(error) => std::mem::size_of::<Message>() + error.capacity(),
        };
        anyhow::ensure!(
            bytes <= self.budget.limit,
            "Incoming packet exceeds memory budget"
        );
        let mut state = self.budget.state.lock().unwrap();
        while !state.closed && state.bytes + bytes > self.budget.limit {
            state = self.budget.ready.wait(state).unwrap();
        }
        anyhow::ensure!(!state.closed, "Incoming stream is closed");
        state.bytes += bytes;
        drop(state);
        self.sender
            .send(Message {
                item,
                _permit: Permit {
                    budget: self.budget.clone(),
                    bytes,
                },
            })
            .map_err(|_| anyhow::anyhow!("Incoming stream is closed"))
    }
}
impl Receiver {
    pub(crate) fn try_recv(&self) -> Result<Result<Packet, String>, mpsc::TryRecvError> {
        // Release the queue's reservation as ownership moves to the main thread.
        self.receiver.try_recv().map(|message| message.item)
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.budget.state.lock().unwrap().closed = true;
        self.budget.ready.notify_all();
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
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            sender.send(Err("c".repeat(1024))).unwrap();
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(receiver.budget.state.lock().unwrap().bytes, size * 2);
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('a')));
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('b')));
        assert!(matches!(receiver.try_recv(), Ok(Err(text)) if text.starts_with('c')));
        assert_eq!(receiver.budget.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn dropping_the_receiver_unblocks_a_waiting_reader() {
        let size = std::mem::size_of::<Message>() + 1024;
        let (sender, receiver) = channel(size, 8);
        sender.send(Err("a".repeat(1024))).unwrap();
        let worker = std::thread::spawn(move || sender.send(Err("b".repeat(1024))));
        drop(receiver);
        assert!(worker.join().unwrap().is_err());
    }
}
