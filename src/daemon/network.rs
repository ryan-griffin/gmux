//! Bounded TCP setup and control I/O. Authenticated streams move to the PTY owner.
use gmux_platform::tcp::TimedStream;
use gmux_protocol::{
    control::ExecutionRequest,
    model::DaemonId,
    network::{
        CONTROL_TIMEOUT, Channel, ConnectionInfo, Handshake, MAX_HANDSHAKE, MAX_REPLY, MAX_REQUEST,
        TIMEOUT, read_json, write_json,
    },
    transport,
};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Instant,
};
use subtle::ConstantTimeEq;

mod control;
use control::{Control, Controls};

const MAX_CONNECTIONS: usize = 32;

struct Clients {
    sockets: Mutex<HashMap<u64, Weak<Connection>>>,
    wake: gmux_platform::unix::ipc::Wake,
    token: Mutex<Option<String>>,
    next: AtomicU64,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// Listener shutdown also interrupts idle authenticated connections.
pub struct Server {
    stop: gmux_platform::unix::ipc::Wake,
    clients: Arc<Clients>,
    thread: Option<std::thread::JoinHandle<()>>,
    controls: Controls,
    dispatch: Dispatch,
    pub address: SocketAddr,
}
pub struct PreparedServer {
    listener: Listener,
    control_wake: gmux_platform::unix::ipc::Wake,
    stop: gmux_platform::unix::ipc::Wake,
    token: Option<String>,
    address: SocketAddr,
}

struct Listener(TcpListener);
impl Drop for Listener {
    fn drop(&mut self) {
        // Forked shell children can retain a close-on-exec descriptor until exec.
        // Shut down the shared socket so replacement can bind immediately.
        unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

impl Server {
    pub fn prepare(address: SocketAddr, token: Option<String>) -> anyhow::Result<PreparedServer> {
        let listener = bind(address)?;
        let address = listener.0.local_addr()?;
        let stop = gmux_platform::unix::ipc::Wake::new()?;
        Ok(PreparedServer {
            listener,
            control_wake: gmux_platform::unix::ipc::Wake::new()?,
            stop,
            token,
            address,
        })
    }
    /// Release the listening address while retaining authenticated clients for rollback.
    pub fn pause(&mut self) {
        self.stop.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
    pub fn resume(&mut self, daemon: DaemonId) -> anyhow::Result<()> {
        let listener = bind(self.address)?;
        self.stop.drain();
        self.thread = Some(accept(
            listener,
            self.stop.clone(),
            self.clients.clone(),
            self.dispatch.clone(),
            self.controls.sender.clone(),
            self.controls.wake.clone(),
            daemon,
        ));
        Ok(())
    }
    pub fn listening(&self) -> bool {
        self.thread.is_some()
    }
    pub fn set_token(&self, token: Option<String>) {
        let sockets = self.clients.sockets.lock().unwrap();
        *self.clients.token.lock().unwrap() = token;
        let active: Vec<_> = sockets.values().filter_map(Weak::upgrade).collect();
        // No new socket can register with the old credential after this point.
        for connection in &active {
            connection.cancelled.store(true, Ordering::Release);
        }
        drop(sockets);
        // Do not hold the admission lock across socket syscalls. The owner can
        // release a frontend while listener management interrupts these peers.
        for connection in &active {
            connection.cancel();
        }
        drop(active);
        self.clients.wake.notify();
        self.controls.wake.notify();
    }
    pub fn token_matches(&self, token: &Option<String>) -> bool {
        *self.clients.token.lock().unwrap() == *token
    }
}
impl PreparedServer {
    pub fn start(self, dispatch: Dispatch, daemon: DaemonId) -> Server {
        let Self {
            listener,
            control_wake,
            stop,
            token,
            address,
        } = self;
        let clients = Arc::new(Clients {
            sockets: Mutex::new(HashMap::new()),
            wake: dispatch.wake.clone(),
            token: Mutex::new(token),
            next: AtomicU64::new(0),
            workers: Mutex::new(Vec::new()),
        });
        let controls = Controls::start(dispatch.clone(), control_wake);
        let thread = accept(
            listener,
            stop.clone(),
            clients.clone(),
            dispatch.clone(),
            controls.sender.clone(),
            controls.wake.clone(),
            daemon,
        );
        Server {
            stop,
            clients,
            thread: Some(thread),
            address,
            controls,
            dispatch,
        }
    }
}
fn bind(address: SocketAddr) -> std::io::Result<Listener> {
    let listener = Listener(TcpListener::bind(address)?);
    listener.0.set_nonblocking(true)?;
    Ok(listener)
}
fn accept(
    listener: Listener,
    stopping: gmux_platform::unix::ipc::Wake,
    clients: Arc<Clients>,
    dispatch: Dispatch,
    controls: mpsc::SyncSender<Control>,
    control_wake: gmux_platform::unix::ipc::Wake,
    daemon: DaemonId,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut fds = [
            libc::pollfd {
                fd: listener.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stopping.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if fds[1].revents != 0 {
                break;
            }
            if result < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let Ok((stream, _)) = listener.0.accept() else {
                continue;
            };
            let deadline = Instant::now() + TIMEOUT;
            let mut sockets = clients.sockets.lock().unwrap();
            if sockets.len() >= MAX_CONNECTIONS {
                continue;
            }
            let id = clients.next.fetch_add(1, Ordering::Relaxed) + 1;
            let connection = Arc::new(Connection {
                socket: stream,
                cancelled: Arc::new(AtomicBool::new(false)),
                clients: Arc::downgrade(&clients),
                id,
            });
            sockets.insert(id, Arc::downgrade(&connection));
            let token = clients.token.lock().unwrap().clone();
            drop(sockets);
            let dispatch = dispatch.clone();
            let controls = controls.clone();
            let control_wake = control_wake.clone();
            let worker = std::thread::spawn(move || {
                if let Ok(setup) = serve(connection, daemon, token.as_deref(), deadline) {
                    match setup {
                        Setup::Stream(stream) => {
                            let _ = dispatch.send(Incoming::Stream(stream));
                        }
                        Setup::Control(control) => {
                            let _ = controls.try_send(control);
                            control_wake.notify();
                        }
                    }
                }
            });
            let mut workers = clients.workers.lock().unwrap();
            workers.retain(|worker| !worker.is_finished());
            workers.push(worker);
        }
    })
}
impl Drop for Server {
    fn drop(&mut self) {
        self.pause();
        self.set_token(None);
        // Setup owns duplicated blocking descriptors only until handoff. Cancel
        // first, then join every remaining setup worker before releasing state.
        for worker in self.clients.workers.lock().unwrap().drain(..) {
            let _ = worker.join();
        }
    }
}
fn serve(
    connection: Arc<Connection>,
    daemon: DaemonId,
    token: Option<&str>,
    deadline: Instant,
) -> anyhow::Result<Setup> {
    let stream = &connection.socket;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(TimedStream {
        socket: stream.try_clone()?,
        deadline: Some(deadline),
    });
    let handshake = (|| -> anyhow::Result<Handshake> {
        let hello: Handshake = serde_json::from_value(read_json(&mut reader, MAX_HANDSHAKE)?)?;
        anyhow::ensure!(
            hello.version == transport::VERSION,
            "Gmux versions are incompatible"
        );
        let authenticated = match (token, hello.token.as_deref()) {
            (None, None) => true,
            (Some(expected), Some(actual)) => {
                bool::from(expected.as_bytes().ct_eq(actual.as_bytes()))
            }
            _ => false,
        };
        anyhow::ensure!(authenticated, "Authentication failed");
        anyhow::ensure!(
            hello.daemon.is_none_or(|id| id == daemon),
            "Remote connection identity changed"
        );
        Ok(hello)
    })();
    let hello = match handshake {
        Ok(hello) => hello,
        Err(error) => {
            write_json(reader.get_mut(), &json!({"error":error.to_string()}))?;
            return Err(error);
        }
    };
    write_json(
        reader.get_mut(),
        &ConnectionInfo {
            version: transport::VERSION,
            daemon,
            home_directory: gmux_platform::unix::paths::home(),
            machine_kind: gmux_platform::unix::machine::machine_kind(),
            hostname: gmux_platform::unix::machine::hostname().into(),
        },
    )?;
    gmux_platform::unix::network::configure_liveness(stream)?;
    let setup = match hello.channel {
        Channel::Control => {
            let prefix = reader.buffer().to_vec();
            Setup::Control(Control::new(connection.clone(), prefix))
        }
        Channel::Stream => {
            reader.get_mut().deadline = Some(Instant::now() + TIMEOUT);
            let value = read_json(&mut reader, MAX_REQUEST)?;
            let mut request: transport::StreamRequest = serde_json::from_value(value)?;
            match &mut request.command {
                transport::StreamCommand::Attach {
                    version,
                    daemon: id,
                    remote,
                } => {
                    anyhow::ensure!(
                        *version == transport::VERSION && *id == daemon,
                        "Invalid remote attachment"
                    );
                    *remote = true;
                }
                _ => anyhow::bail!("Remote stream must attach first"),
            }
            Setup::Stream(Stream {
                connection: connection.clone(),
                attach: request,
                prefix: reader.buffer().to_vec(),
            })
        }
    };
    // Timeout settings and nonblocking mode are shared by duplicated descriptors.
    // Drop the setup reader before transferring sole I/O ownership.
    drop(reader);
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    anyhow::ensure!(!connection.cancelled(), "Connection cancelled during setup");
    Ok(setup)
}

/// One RAII admission slot and one socket descriptor throughout setup, queueing,
/// reactor service, and frontend ownership. The registry holds only weak refs.
pub(super) struct Connection {
    pub(super) socket: TcpStream,
    cancelled: Arc<AtomicBool>,
    clients: Weak<Clients>,
    id: u64,
}
impl Connection {
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel();
        if let Some(clients) = self.clients.upgrade() {
            clients.sockets.lock().unwrap().remove(&self.id);
            // Expiry/disconnect can cancel a mutation waiting behind storage.
            clients.wake.notify();
        }
    }
}
pub(super) struct Stream {
    pub(super) connection: Arc<Connection>,
    pub(super) attach: transport::StreamRequest,
    pub(super) prefix: Vec<u8>,
}
enum Setup {
    Control(Control),
    Stream(Stream),
}
pub(super) enum Incoming {
    Control(super::commands::ControlRequest),
    Stream(Stream),
}
#[derive(Clone)]
pub(super) struct Dispatch {
    sender: mpsc::SyncSender<Incoming>,
    admission: gmux_platform::unix::ipc::ControlAdmission,
    wake: gmux_platform::unix::ipc::Wake,
}
impl Dispatch {
    pub(super) fn new(
        wake: gmux_platform::unix::ipc::Wake,
        admission: gmux_platform::unix::ipc::ControlAdmission,
    ) -> (Self, mpsc::Receiver<Incoming>) {
        let (sender, receiver) = mpsc::sync_channel(32);
        (
            Self {
                sender,
                wake,
                admission,
            },
            receiver,
        )
    }
    fn send(&self, incoming: Incoming) -> anyhow::Result<()> {
        self.sender
            .try_send(incoming)
            .map_err(|_| anyhow::anyhow!("Remote dispatch queue is full or closed"))?;
        self.wake.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
