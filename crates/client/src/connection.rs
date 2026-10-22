//! One daemon's connection lifecycle, independent of client organization.
use crate::{
    control::CommandReply, ipc, model::DaemonId, remote::SessionManager, session::SessionUpdate,
};
use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::{BuildHasher, Hasher},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Recovery backs off across failed attachments and short-lived connections.
/// A connection must stay healthy for ten seconds before the delay resets.
pub struct Backoff {
    seconds: u8,
    connected_at: Option<Instant>,
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            seconds: 1,
            connected_at: None,
        }
    }
}
impl Backoff {
    pub fn connected(&mut self, now: Instant) {
        self.connected_at.get_or_insert(now);
    }
    pub fn next_delay(&mut self, now: Instant) -> Duration {
        if self
            .connected_at
            .take()
            .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(10))
        {
            self.seconds = 1;
        }
        let seconds = self.seconds;
        self.seconds = (seconds * 2).min(30);
        Duration::from_secs(u64::from(seconds))
    }
    fn jitter(delay: Duration) -> Duration {
        // Independent seeds spread windows and clients across 80% to 100% of the delay.
        let random = RandomState::new().build_hasher().finish();
        delay.mul_f64(0.8 + (random % 201) as f64 / 1000.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status<'a> {
    Disconnected {
        error: Option<&'a str>,
    },
    Connecting,
    Reconnecting {
        error: Option<&'a str>,
        retry_at: Option<Instant>,
    },
    Connected,
}

enum State {
    Disconnected {
        error: Option<String>,
        retry_at: Option<Instant>,
    },
    Connecting {
        cancellation: ipc::Cancellation,
        error: Option<String>,
    },
    Connected(Box<SessionManager>),
}

type Completion = (u64, anyhow::Result<(ipc::Stream, CommandReply)>);

#[derive(Default)]
pub struct Update {
    pub status_changed: bool,
    pub state: Option<CommandReply>,
    pub panes: HashMap<u64, SessionUpdate>,
}

pub struct Connection {
    daemon: DaemonId,
    endpoint: ipc::Endpoint,
    state: State,
    generation: u64,
    sender: mpsc::Sender<Completion>,
    receiver: mpsc::Receiver<Completion>,
    wake: ipc::Wake,
    changed: bool,
    auto_reconnect: bool,
    backoff: Backoff,
}

impl Connection {
    pub fn new(daemon: DaemonId, mut endpoint: ipc::Endpoint, wake: ipc::Wake) -> Self {
        endpoint.pin(daemon);
        let (sender, receiver) = mpsc::channel();
        Self {
            daemon,
            endpoint,
            state: State::Disconnected {
                error: None,
                retry_at: None,
            },
            generation: 0,
            sender,
            receiver,
            wake,
            changed: false,
            auto_reconnect: false,
            backoff: Backoff::default(),
        }
    }
    pub fn from_session(manager: SessionManager, wake: ipc::Wake) -> Self {
        let mut connection = Self::new(manager.daemon_id(), manager.endpoint().clone(), wake);
        connection.state = State::Connected(Box::new(manager));
        connection.backoff.connected(Instant::now());
        connection
    }
    pub fn endpoint(&self) -> &ipc::Endpoint {
        &self.endpoint
    }
    pub fn set_endpoint(&mut self, mut endpoint: ipc::Endpoint) {
        endpoint.pin(self.daemon);
        if let Some(manager) = self.manager_mut() {
            manager.set_endpoint(endpoint.clone());
        }
        self.endpoint = endpoint;
    }
    /// Enable retries on future transient failures. Disabling cancels a waiting retry.
    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
        if !enabled
            && let State::Disconnected { retry_at, .. } = &mut self.state
            && retry_at.take().is_some()
        {
            self.changed = true;
            self.wake.notify();
        }
    }
    /// Schedule a poll at this deadline. Waiting creates no worker.
    pub fn next_retry(&self) -> Option<Instant> {
        match &self.state {
            State::Disconnected { retry_at, .. } => *retry_at,
            _ => None,
        }
    }
    /// Earliest deadline among this connection's coalesced scroll targets.
    pub fn next_scroll(&self) -> Option<Instant> {
        self.manager().and_then(|manager| manager.next_scroll())
    }
    pub fn status(&self) -> Status<'_> {
        match &self.state {
            State::Disconnected {
                error,
                retry_at: Some(at),
            } => Status::Reconnecting {
                error: error.as_deref(),
                retry_at: Some(*at),
            },
            State::Disconnected { error, .. } => Status::Disconnected {
                error: error.as_deref(),
            },
            State::Connecting {
                error: Some(error), ..
            } => Status::Reconnecting {
                error: Some(error),
                retry_at: None,
            },
            State::Connecting { .. } => Status::Connecting,
            State::Connected(_) => Status::Connected,
        }
    }
    pub fn manager(&self) -> Option<&SessionManager> {
        match &self.state {
            State::Connected(manager) => Some(manager),
            _ => None,
        }
    }
    pub fn manager_mut(&mut self) -> Option<&mut SessionManager> {
        match &mut self.state {
            State::Connected(manager) => Some(manager),
            _ => None,
        }
    }
    /// Start asynchronously. Repeated requests during setup or while connected
    /// reuse the current connection. Connecting never starts a daemon.
    pub fn connect(&mut self) -> bool {
        let Some((generation, cancellation)) = self.start_attempt() else {
            return false;
        };
        let sender = self.sender.clone();
        let endpoint = self.endpoint.clone();
        let daemon = self.daemon;
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let mut socket = endpoint.connect_stream_cancellable(&cancellation)?;
                // Attach within the stream's handshake deadline, independently
                // of how long the separate control request takes to reply.
                SessionManager::attach_stream(&mut socket, daemon)?;
                let state = endpoint.state(Some(&cancellation))?;
                anyhow::ensure!(
                    state.daemon == daemon,
                    crate::network::ConnectionRejected("Remote connection identity changed".into())
                );
                Ok((socket, state))
            })();
            let _ = sender.send((generation, result));
            wake.notify();
        });
        true
    }
    fn start_attempt(&mut self) -> Option<(u64, ipc::Cancellation)> {
        let State::Disconnected { error, .. } = &mut self.state else {
            return None;
        };
        let error = error.take();
        self.generation += 1;
        let cancellation = ipc::Cancellation::default();
        self.state = State::Connecting {
            cancellation: cancellation.clone(),
            error,
        };
        self.changed = true;
        self.wake.notify();
        Some((self.generation, cancellation))
    }
    /// Cancel setup or disconnect, releasing streams and subscriptions.
    pub fn cancel(&mut self) -> bool {
        let inactive = matches!(
            self.state,
            State::Disconnected {
                error: None,
                retry_at: None
            }
        );
        self.backoff = Backoff::default();
        if inactive {
            return false;
        }
        if let State::Connecting { cancellation, .. } = &self.state {
            cancellation.cancel();
        }
        self.state = State::Disconnected {
            error: None,
            retry_at: None,
        };
        self.generation += 1;
        self.changed = true;
        self.wake.notify();
        true
    }
    pub fn poll(&mut self) -> Update {
        self.poll_at(Instant::now())
    }
    fn failed(&mut self, error: String, retryable: bool, now: Instant) {
        let delay = self.backoff.next_delay(now);
        self.state = State::Disconnected {
            error: Some(error),
            retry_at: (self.auto_reconnect && retryable).then(|| now + Backoff::jitter(delay)),
        };
        self.changed = true;
    }
    /// Poll connections in one event-loop iteration against a shared timestamp.
    pub fn poll_at(&mut self, now: Instant) -> Update {
        self.poll_at_with_clipboard(now, |_| {})
    }
    pub fn poll_at_with_clipboard(
        &mut self,
        now: Instant,
        clipboard: impl FnMut(crate::clipboard::Write),
    ) -> Update {
        let mut update = Update::default();
        if self.next_retry().is_some_and(|at| at <= now) {
            self.connect();
        }
        while let Ok((generation, result)) = self.receiver.try_recv() {
            if generation != self.generation || !matches!(self.state, State::Connecting { .. }) {
                continue;
            }
            self.changed = true;
            match result.and_then(|(stream, state)| {
                let mut manager = SessionManager::from_attached_stream(
                    self.endpoint.clone(),
                    self.wake.clone(),
                    self.daemon,
                    stream,
                )?;
                manager.install_metadata(&state.metadata, state.revision);
                Ok((manager, state))
            }) {
                Ok((manager, state)) => {
                    self.state = State::Connected(Box::new(manager));
                    self.backoff.connected(now);
                    update.state = Some(state);
                }
                Err(error) => {
                    let retryable = !error.is::<crate::network::ConnectionRejected>();
                    self.failed(error.to_string(), retryable, now);
                }
            }
        }
        if let State::Connected(manager) = &mut self.state {
            update.panes = manager.poll_with_clipboard(clipboard);
            if let Some(state) = manager.take_state() {
                manager.install_metadata(&state.metadata, state.revision);
                update.state = Some(state);
            }
            if manager.disconnected() {
                let error = manager.disconnect_error();
                let retryable =
                    error.is_none_or(|error| !error.is::<crate::network::ConnectionRejected>());
                let message = error.map_or_else(|| "Connection closed".into(), ToString::to_string);
                self.failed(message, retryable, now);
            }
        }
        update.status_changed = std::mem::take(&mut self.changed);
        update
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream};

    #[test]
    fn backoff_survives_failed_attachments_and_flapping_connections() {
        let mut backoff = Backoff::default();
        let mut now = Instant::now();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            assert_eq!(backoff.next_delay(now), Duration::from_secs(seconds));
            backoff.connected(now);
            now += Duration::from_secs(1);
        }
        backoff.connected(now);
        assert_eq!(
            backoff.next_delay(now + Duration::from_secs(10)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn retries_wait_for_their_deadline_allow_manual_retry_and_stop_on_rejection_or_cancel() {
        let daemon = DaemonId::generate().unwrap();
        let directory = std::env::temp_dir().join(format!("gmux-retry-{daemon}"));
        let mut connection = Connection::new(
            daemon,
            ipc::Endpoint::unix(directory.join("control"), directory.join("stream")),
            ipc::Wake::new().unwrap(),
        );
        connection.set_auto_reconnect(true);
        let (generation, _) = connection.start_attempt().unwrap();
        let now = Instant::now();
        connection
            .sender
            .send((generation, Err(anyhow::anyhow!("offline"))))
            .unwrap();
        connection.poll_at(now);
        let retry_at = connection.next_retry().unwrap();
        assert!(
            (Duration::from_millis(800)..=Duration::from_secs(1))
                .contains(&retry_at.duration_since(now))
        );
        connection.poll_at(retry_at - Duration::from_millis(1));
        assert_eq!(connection.generation, generation);
        assert!(matches!(
            connection.status(),
            Status::Reconnecting {
                retry_at: Some(_),
                ..
            }
        ));
        // An explicit request bypasses the delay without resetting backoff.
        let (generation, _) = connection.start_attempt().unwrap();
        assert!(connection.next_retry().is_none());
        connection
            .sender
            .send((generation, Err(anyhow::anyhow!("still offline"))))
            .unwrap();
        connection.poll_at(now);
        let retry_at = connection.next_retry().unwrap();
        assert!(
            (Duration::from_millis(1600)..=Duration::from_secs(2))
                .contains(&retry_at.duration_since(now))
        );
        assert!(connection.cancel());
        connection.poll_at(retry_at + Duration::from_secs(60));
        assert_eq!(connection.status(), Status::Disconnected { error: None });
        assert!(connection.next_retry().is_none());

        let (generation, _) = connection.start_attempt().unwrap();
        connection
            .sender
            .send((
                generation,
                Err(crate::network::ConnectionRejected("Authentication failed".into()).into()),
            ))
            .unwrap();
        connection.poll_at(now);
        assert_eq!(
            connection.status(),
            Status::Disconnected {
                error: Some("Authentication failed")
            }
        );
        assert!(connection.next_retry().is_none());
        // Ordinary transport failures still start a new attempt when due.
        connection.failed("offline".into(), true, now);
        let retry_at = connection.next_retry().unwrap();
        connection.poll_at(retry_at);
        assert!(connection.generation > generation);
        connection.cancel();
    }

    #[test]
    fn disabling_retries_wakes_observers_and_cancels_the_deadline() {
        use std::os::fd::AsRawFd;
        let daemon = DaemonId::generate().unwrap();
        let mut connection =
            Connection::new(daemon, ipc::Endpoint::local(), ipc::Wake::new().unwrap());
        let now = Instant::now();
        connection.set_auto_reconnect(true);
        connection.failed("offline".into(), true, now);
        assert!(connection.poll_at(now).status_changed);
        assert!(connection.next_retry().is_some());
        connection.wake.drain();
        connection.set_auto_reconnect(false);
        let mut event = libc::pollfd {
            fd: connection.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut event, 1, 0) }, 1);
        assert!(
            connection
                .poll_at(now + Duration::from_secs(60))
                .status_changed
        );
        assert_eq!(
            connection.status(),
            Status::Disconnected {
                error: Some("offline")
            }
        );
        assert!(connection.next_retry().is_none());
        assert_eq!(connection.generation, 0);
    }

    #[test]
    fn stream_attachment_does_not_wait_for_the_control_state_reply() {
        use std::{
            io::{BufRead, BufReader},
            os::unix::net::UnixListener,
            time::Duration,
        };
        let daemon = DaemonId::generate().unwrap();
        let directory = std::env::temp_dir().join(format!("gmux-connect-order-{daemon}"));
        let control = directory.join("control");
        let stream = directory.join("stream");
        let server = ipc::Server::start_at(control.clone(), ipc::Wake::new().unwrap()).unwrap();
        let listener = UnixListener::bind(&stream).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut line = String::new();
            let attached = BufReader::new(&socket).read_line(&mut line).is_ok()
                && serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"] == "attach";
            // A control response can wait on the remote machine. The stream
            // must already be attached while the request is outstanding.
            let request = server
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            request.reply.send(serde_json::json!({
                "daemon":daemon,"home_directory":"/tmp","machine_kind":"unknown","hostname":"host", "revision":0,
                "result":{"ok":true},"execution":{"last_id":0,"tabs":[],"scrollback":{}},"metadata":[]
            })).unwrap();
            sender.send(attached).unwrap();
        });
        let mut connection = Connection::new(
            daemon,
            ipc::Endpoint::unix(control, stream),
            ipc::Wake::new().unwrap(),
        );
        connection.connect();
        let attached = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        connection.cancel();
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            attached,
            "The control request delayed the stream attachment"
        );
    }

    #[test]
    fn retries_ignore_late_completions_and_release_their_streams() {
        let daemon = DaemonId::generate().unwrap();
        let mut connection =
            Connection::new(daemon, ipc::Endpoint::local(), ipc::Wake::new().unwrap());
        let (first, _) = connection.start_attempt().unwrap();
        assert!(connection.start_attempt().is_none());
        connection.cancel();
        let (second, _) = connection.start_attempt().unwrap();
        connection
            .sender
            .send((first, Err(anyhow::anyhow!("stale failure"))))
            .unwrap();
        connection.poll();
        assert_eq!(connection.status(), Status::Connecting);
        connection
            .sender
            .send((second, Err(anyhow::anyhow!("current failure"))))
            .unwrap();
        connection.poll();
        assert_eq!(
            connection.status(),
            Status::Disconnected {
                error: Some("current failure")
            }
        );
        let (generation, _) = connection.start_attempt().unwrap();
        connection.cancel();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let state: CommandReply = serde_json::from_value(serde_json::json!({
            "daemon":daemon,"home_directory":"/tmp","machine_kind":"unknown","hostname":"host", "revision":0,
            "result":{"ok":true},"execution":{"last_id":0,"tabs":[],"scrollback":{}},"metadata":[]
        })).unwrap();
        connection
            .sender
            .send((generation, Ok((stream.into(), state))))
            .unwrap();
        connection.poll();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert_eq!(connection.status(), Status::Disconnected { error: None });
    }
}
