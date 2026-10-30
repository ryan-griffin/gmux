//! This daemon's TCP listener configuration. Outgoing destinations belong to clients.
use super::network as server;
use gmux_platform::unix::storage;
use gmux_protocol::model::DaemonId;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, mpsc};

/// The PTY thread reads only this cache. File I/O and listener replacement run
/// on a serialized worker, created on the first edit to avoid idle overhead.
pub(super) struct RemoteAccess {
    status: Arc<Mutex<Status>>,
    state: Option<State>,
    worker: Option<Worker>,
}
struct Status {
    config: gmux_protocol::network::RemoteAccess,
    bound_address: Option<std::net::SocketAddr>,
    error: Option<String>,
}
#[derive(Default)]
struct State {
    config: gmux_protocol::network::RemoteAccess,
    server: Option<server::Server>,
    error: Option<String>,
}
struct Update {
    config: gmux_protocol::network::RemoteAccess,
    reply: mpsc::SyncSender<Value>,
}
struct Worker {
    requests: Option<mpsc::SyncSender<Update>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    RemoteAccessGet,
    RemoteAccessSet {
        remote_access: gmux_protocol::network::RemoteAccess,
    },
}
impl Default for RemoteAccess {
    fn default() -> Self {
        Self::from_state(State::default())
    }
}
impl RemoteAccess {
    fn from_state(state: State) -> Self {
        Self {
            status: Arc::new(Mutex::new(state.status())),
            state: Some(state),
            worker: None,
        }
    }
    pub fn start(daemon: DaemonId) -> anyhow::Result<Self> {
        let mut state = State {
            config: storage::load("remote-access.json")?,
            ..Default::default()
        };
        if state.config.enabled {
            let result = gmux_platform::unix::network::token(&state.config.authentication)
                .and_then(|token| server::Server::prepare(state.config.listen_address, token));
            match result {
                Ok(prepared) => {
                    state.server =
                        Some(prepared.start(gmux_platform::unix::ipc::socket_path(), daemon))
                }
                Err(error) => state.error = Some(error.to_string()),
            }
        }
        Ok(Self::from_state(state))
    }
    pub fn bound_address(&self) -> Option<std::net::SocketAddr> {
        self.status.lock().unwrap().bound_address
    }
    pub fn handles(method: &str) -> bool {
        matches!(method, "remote-access-get" | "remote-access-set")
    }
    pub fn handle(&mut self, daemon: DaemonId, request: gmux_platform::unix::ipc::Request) {
        let result = (|| -> anyhow::Result<Option<Value>> {
            match serde_json::from_value(request.body)? {
                Request::RemoteAccessGet => {
                    let status = self.status.lock().unwrap();
                    Ok(Some(
                        json!({"local":{"name":gmux_platform::unix::machine::hostname(),"daemon":daemon,"machine_kind":gmux_platform::unix::machine::machine_kind()},"hostname":gmux_platform::unix::machine::hostname(),"remote_access":status.config,"remote_access_error":status.error,"bound_address":status.bound_address}),
                    ))
                }
                Request::RemoteAccessSet { remote_access } => {
                    let worker = self.worker.get_or_insert_with(|| {
                        Worker::start(self.state.take().unwrap(), self.status.clone(), daemon)
                    });
                    worker
                        .requests
                        .as_ref()
                        .unwrap()
                        .try_send(Update {
                            config: remote_access,
                            reply: request.reply.clone(),
                        })
                        .map_err(|_| {
                            anyhow::anyhow!("Remote access update queue is full or closed")
                        })?;
                    Ok(None)
                }
            }
        })();
        let response = match result {
            Ok(None) => return,
            Ok(Some(value)) => value,
            Err(error) => json!({"error":error.to_string()}),
        };
        let _ = request.reply.try_send(response);
    }
}
impl State {
    fn status(&self) -> Status {
        Status {
            config: self.config.clone(),
            bound_address: self
                .server
                .as_ref()
                .filter(|server| server.listening())
                .map(|server| server.address),
            error: self.error.clone(),
        }
    }
    fn set(
        &mut self,
        config: gmux_protocol::network::RemoteAccess,
        daemon: DaemonId,
        save: impl FnOnce(&gmux_protocol::network::RemoteAccess) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let token = if config.enabled {
            gmux_platform::unix::network::token(&config.authentication)?
        } else {
            None
        };
        let mut paused = false;
        let prepared = (|| {
            let prepared = if config.enabled
                && self.server.as_ref().is_none_or(|server| {
                    !server.listening()
                        || (self.config.listen_address != config.listen_address
                            && server.address != config.listen_address)
                }) {
                match server::Server::prepare(config.listen_address, token.clone()) {
                    Ok(prepared) => Some(prepared),
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
                            && self.server.as_ref().is_some_and(|server| {
                                server.listening()
                                    && server.address.port() == config.listen_address.port()
                            }) =>
                    {
                        // Wildcard and specific addresses can cover the same port.
                        // Keep existing clients alive until the replacement commits.
                        self.server.as_mut().unwrap().pause();
                        paused = true;
                        Some(server::Server::prepare(
                            config.listen_address,
                            token.clone(),
                        )?)
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            save(&config)?;
            Ok(prepared)
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                // The failed replacement has dropped its socket before rebinding.
                if paused
                    && let Err(restore) = self
                        .server
                        .as_mut()
                        .unwrap()
                        .resume(gmux_platform::unix::ipc::socket_path(), daemon)
                {
                    self.error = Some(format!("Could not restore TCP listener: {restore}"));
                    anyhow::bail!("{error}; {}", self.error.as_ref().unwrap());
                }
                return Err(error);
            }
        };
        if let Some(prepared) = prepared {
            self.server = Some(prepared.start(gmux_platform::unix::ipc::socket_path(), daemon));
        } else if !config.enabled {
            self.server = None;
        } else if let Some(server) = &self.server
            && !server.token_matches(&token)
        {
            server.set_token(token);
        }
        self.config = config;
        self.error = None;
        Ok(())
    }
}
impl Worker {
    fn start(mut state: State, status: Arc<Mutex<Status>>, daemon: DaemonId) -> Self {
        let (requests, receiver) = mpsc::sync_channel::<Update>(32);
        let thread = std::thread::spawn(move || {
            for update in receiver {
                let result = state.set(update.config, daemon, |config| {
                    storage::save("remote-access.json", config)
                });
                *status.lock().unwrap() = state.status();
                let response = match result {
                    Ok(()) => json!({"ok":true,"daemon":daemon}),
                    Err(error) => json!({"error":error.to_string()}),
                };
                let _ = update.reply.try_send(response);
            }
        });
        Self {
            requests: Some(requests),
            thread: Some(thread),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, net::TcpListener, time::Duration};

    #[test]
    fn overlapping_listener_edits_commit_or_restore_the_listener_and_its_clients() {
        let daemon = gmux_platform::unix::identity::daemon_id().unwrap();
        let mut state = State::default();
        state
            .set(
                gmux_protocol::network::RemoteAccess {
                    enabled: true,
                    // Allocate across all interfaces so another address cannot
                    // already own the port when we later widen the listener.
                    listen_address: "0.0.0.0:0".parse().unwrap(),
                    authentication: gmux_protocol::network::Authentication::None,
                },
                daemon,
                |_| Ok(()),
            )
            .unwrap();
        let address = std::net::SocketAddr::from((
            [127, 0, 0, 1],
            state.status().bound_address.unwrap().port(),
        ));
        state
            .set(
                gmux_protocol::network::RemoteAccess {
                    listen_address: address,
                    ..state.config.clone()
                },
                daemon,
                |_| Ok(()),
            )
            .unwrap();
        let original = state.config.clone();
        let connect = || {
            gmux_client::network::connect(
                &address.to_string(),
                &gmux_client::network::Credentials::default(),
                gmux_protocol::network::Channel::Control,
                Some(daemon),
            )
            .unwrap()
            .0
        };
        let mut client = connect();
        client
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let replacement = gmux_protocol::network::RemoteAccess {
            listen_address: std::net::SocketAddr::from(([0, 0, 0, 0], address.port())),
            ..original.clone()
        };
        for occupied in [false, true] {
            let _other = occupied.then(|| {
                TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 2], address.port())))
                    .unwrap()
            });
            let mut saved = false;
            let error = state
                .set(replacement.clone(), daemon, |_| {
                    saved = true;
                    anyhow::bail!("disk write failed")
                })
                .unwrap_err();
            assert_eq!(saved, !occupied, "Listener replacement failed: {error:#}");
            assert!(
                if occupied {
                    error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
                } else {
                    error.to_string().contains("disk write failed")
                },
                "occupied={occupied}: {error:#}"
            );
            assert!(state.config == original);
            assert_eq!(state.status().bound_address, Some(address));
            assert!(state.error.is_none());
            // Rollback preserves existing authenticated clients and accepts new ones.
            let error = client.peek(&mut [0]).unwrap_err();
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ));
            drop(connect());
        }
        state.set(replacement.clone(), daemon, |_| Ok(())).unwrap();
        assert!(state.config == replacement);
        assert_eq!(
            state.status().bound_address,
            Some(replacement.listen_address)
        );
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        drop(connect());
        // Narrowing the wildcard listener back to loopback uses the same replacement path.
        let loopback = gmux_protocol::network::RemoteAccess {
            listen_address: address,
            ..original
        };
        state.set(loopback, daemon, |_| Ok(())).unwrap();
        assert_eq!(state.status().bound_address, Some(address));
        drop(connect());
    }
}
