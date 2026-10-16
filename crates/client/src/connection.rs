//! One daemon's connection lifecycle, independent of client organization.
use crate::{
    control::CommandReply, ipc, model::DaemonId, remote::SessionManager, session::SessionUpdate,
};
use std::{collections::HashMap, sync::mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status<'a> {
    Disconnected { error: Option<&'a str> },
    Connecting,
    Connected,
}

enum State {
    Disconnected(Option<String>),
    Connecting(ipc::Cancellation),
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
}

impl Connection {
    pub fn new(daemon: DaemonId, mut endpoint: ipc::Endpoint, wake: ipc::Wake) -> Self {
        endpoint.pin(daemon);
        let (sender, receiver) = mpsc::channel();
        Self {
            daemon,
            endpoint,
            state: State::Disconnected(None),
            generation: 0,
            sender,
            receiver,
            wake,
            changed: false,
        }
    }
    pub fn from_session(manager: SessionManager, wake: ipc::Wake) -> Self {
        let mut connection = Self::new(manager.daemon_id(), manager.endpoint().clone(), wake);
        connection.state = State::Connected(Box::new(manager));
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
    pub fn status(&self) -> Status<'_> {
        match &self.state {
            State::Disconnected(error) => Status::Disconnected {
                error: error.as_deref(),
            },
            State::Connecting(_) => Status::Connecting,
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
                let socket = endpoint.connect_stream_cancellable(&cancellation)?;
                let state = endpoint.state(Some(&cancellation))?;
                anyhow::ensure!(state.daemon == daemon, "Remote connection identity changed");
                Ok((socket, state))
            })();
            let _ = sender.send((generation, result));
            wake.notify();
        });
        true
    }
    fn start_attempt(&mut self) -> Option<(u64, ipc::Cancellation)> {
        if !matches!(self.state, State::Disconnected(_)) {
            return None;
        }
        self.generation += 1;
        let cancellation = ipc::Cancellation::default();
        self.state = State::Connecting(cancellation.clone());
        self.changed = true;
        self.wake.notify();
        Some((self.generation, cancellation))
    }
    /// Cancel setup or disconnect, releasing streams and subscriptions.
    pub fn cancel(&mut self) -> bool {
        if matches!(self.state, State::Disconnected(_)) {
            return false;
        }
        if let State::Connecting(cancellation) = &self.state {
            cancellation.cancel();
        }
        self.state = State::Disconnected(None);
        self.generation += 1;
        self.changed = true;
        self.wake.notify();
        true
    }
    pub fn poll(&mut self) -> Update {
        let mut update = Update::default();
        for (generation, result) in self.receiver.try_iter() {
            if generation != self.generation || !matches!(self.state, State::Connecting(_)) {
                continue;
            }
            self.changed = true;
            match result.and_then(|(stream, state)| {
                let mut manager = SessionManager::from_stream(
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
                    update.state = Some(state);
                }
                Err(error) => self.state = State::Disconnected(Some(error.to_string())),
            }
        }
        if let State::Connected(manager) = &mut self.state {
            update.panes = manager.poll();
            if let Some(state) = manager.take_state() {
                manager.install_metadata(&state.metadata, state.revision);
                update.state = Some(state);
            }
            if manager.disconnected() {
                self.state = State::Disconnected(Some(
                    manager
                        .disconnect_reason()
                        .unwrap_or("Connection closed")
                        .to_owned(),
                ));
                self.changed = true;
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
