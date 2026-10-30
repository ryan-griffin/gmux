//! Incoming TCP authentication and terminal stream forwarding for this daemon.
use gmux_platform::tcp::TimedStream;
use gmux_protocol::{
    control::{ExecutionRequest, InternalRequest},
    model::DaemonId,
    network::{
        Channel, ConnectionInfo, Handshake, MAX_HANDSHAKE, MAX_REPLY, MAX_REQUEST, TIMEOUT,
        read_json, write_json,
    },
    transport,
};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

struct Clients {
    sockets: Mutex<HashMap<u64, TcpStream>>,
    token: Mutex<Option<String>>,
    next: AtomicU64,
}

/// Listener shutdown also interrupts idle authenticated connections.
pub struct Server {
    stop: gmux_platform::unix::ipc::Wake,
    clients: Arc<Clients>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub address: SocketAddr,
}
pub struct PreparedServer {
    listener: Listener,
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
    pub fn resume(&mut self, control: PathBuf, daemon: DaemonId) -> anyhow::Result<()> {
        let listener = bind(self.address)?;
        self.stop.drain();
        self.thread = Some(accept(
            listener,
            self.stop.clone(),
            self.clients.clone(),
            control,
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
        for socket in sockets.values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
    pub fn token_matches(&self, token: &Option<String>) -> bool {
        *self.clients.token.lock().unwrap() == *token
    }
}
impl PreparedServer {
    pub fn start(self, control: PathBuf, daemon: DaemonId) -> Server {
        let Self {
            listener,
            stop,
            token,
            address,
        } = self;
        let clients = Arc::new(Clients {
            sockets: Mutex::new(HashMap::new()),
            token: Mutex::new(token),
            next: AtomicU64::new(0),
        });
        let thread = accept(listener, stop.clone(), clients.clone(), control, daemon);
        Server {
            stop,
            clients,
            thread: Some(thread),
            address,
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
    control: PathBuf,
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
            if sockets.len() >= 32 {
                continue;
            }
            let Ok(copy) = stream.try_clone() else {
                continue;
            };
            let id = clients.next.fetch_add(1, Ordering::Relaxed) + 1;
            sockets.insert(id, copy);
            let token = clients.token.lock().unwrap().clone();
            drop(sockets);
            let active = clients.clone();
            let control = control.clone();
            std::thread::spawn(move || {
                let _ = serve(stream, &control, daemon, token.as_deref(), deadline);
                active.sockets.lock().unwrap().remove(&id);
            });
        }
    })
}
impl Drop for Server {
    fn drop(&mut self) {
        self.pause();
        for socket in self.clients.sockets.lock().unwrap().values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}
fn serve(
    mut stream: TcpStream,
    control: &Path,
    daemon: DaemonId,
    token: Option<&str>,
    deadline: Instant,
) -> anyhow::Result<()> {
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
            return Ok(());
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
    stream.set_write_timeout(Some(TIMEOUT))?;
    match hello.channel {
        Channel::Control => {
            reader.get_mut().deadline = None;
            reader.get_mut().socket.set_read_timeout(None)?;
            loop {
                let value = read_json(&mut reader, MAX_REQUEST)?;
                let result = serde_json::from_value::<ExecutionRequest>(value)
                    .map_err(anyhow::Error::from)
                    .and_then(|request| {
                        gmux_platform::unix::ipc::call_at(
                            control,
                            InternalRequest::Execution { request },
                        )
                    });
                let value = result.unwrap_or_else(|error| json!({"error":error.to_string()}));
                let encoded = serde_json::to_vec(&value)?;
                anyhow::ensure!(encoded.len() <= MAX_REPLY, "Remote reply is too large");
                stream.write_all(&encoded)?;
                stream.write_all(b"\n")?;
            }
        }
        Channel::Stream => {
            gmux_platform::unix::network::configure_liveness(&stream)?;
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
            let mut local =
                UnixStream::connect(gmux_platform::unix::ipc::companion_at(control, ".stream"))?;
            transport::command(&mut local, &request)?;
            bridge(local, stream, reader.buffer())
        }
    }
}

const BRIDGE_BUFFER: usize = 16 * 1024;

#[derive(Default)]
struct Forwarding {
    bytes: Vec<u8>,
    offset: usize,
    eof: bool,
    finished: bool,
    hung_up: bool,
}
impl Forwarding {
    fn read(&mut self, source: &mut impl Read, events: i16, closing: bool) -> anyhow::Result<()> {
        if self.eof
            || !self.bytes.is_empty()
            || events & (libc::POLLIN | libc::POLLHUP | libc::POLLRDHUP) == 0
        {
            return Ok(());
        }
        self.bytes.resize(BRIDGE_BUFFER, 0);
        let result = loop {
            match source.read(&mut self.bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => break result,
            }
        };
        match result {
            Ok(0) => {
                self.bytes.clear();
                self.eof = true;
            }
            Ok(size) => self.bytes.truncate(size),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.bytes.clear();
                self.eof = closing;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn write(&mut self, destination: &mut impl Write, events: i16) -> anyhow::Result<()> {
        if self.bytes.is_empty() || events & (libc::POLLOUT | libc::POLLHUP) == 0 {
            return Ok(());
        }
        match destination.write(&self.bytes[self.offset..]) {
            Ok(0) => anyhow::bail!("Stream bridge could not forward buffered data"),
            Ok(size) => {
                self.offset += size;
                if self.offset == self.bytes.len() {
                    self.bytes.clear();
                    self.offset = 0;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn ready_to_finish(&self) -> bool {
        self.eof && self.bytes.is_empty() && !self.finished
    }
}
fn bridge_pollfd(fd: libc::c_int, source: &Forwarding, destination: &Forwarding) -> libc::pollfd {
    let events = if !source.eof && source.bytes.is_empty() {
        libc::POLLIN
    } else {
        0
    } | if !source.eof && !source.hung_up {
        libc::POLLRDHUP
    } else {
        0
    } | if !destination.bytes.is_empty() {
        libc::POLLOUT
    } else {
        0
    };
    // poll reports HUP even without requested events. Disable a socket while
    // its unread bytes are behind backpressure, rather than spinning on HUP.
    libc::pollfd {
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    }
}

/// Drain buffered and readable bytes before closing a terminal stream, with bounded buffers.
pub fn bridge(local: UnixStream, remote: TcpStream, prefix: &[u8]) -> anyhow::Result<()> {
    bridge_with_drain_timeout(local, remote, prefix, TIMEOUT)
}
fn bridge_with_drain_timeout(
    mut local: UnixStream,
    mut remote: TcpStream,
    prefix: &[u8],
    drain_timeout: Duration,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        prefix.len() <= BRIDGE_BUFFER,
        "Stream bridge prefix is too large"
    );
    local.set_nonblocking(true)?;
    remote.set_nonblocking(true)?;
    remote.set_read_timeout(None)?;
    remote.set_write_timeout(None)?;
    let mut to_local = Forwarding {
        bytes: Vec::with_capacity(BRIDGE_BUFFER),
        ..Default::default()
    };
    to_local.bytes.extend_from_slice(prefix);
    let mut to_remote = Forwarding {
        bytes: Vec::with_capacity(BRIDGE_BUFFER),
        ..Default::default()
    };
    let mut drain_deadline = None;
    loop {
        // EOF ends this terminal channel. Drain data already available in the
        // other direction, but do not wait indefinitely for an idle peer's FIN.
        if to_local.eof || to_local.hung_up {
            to_remote.read(&mut local, libc::POLLIN, true)?;
        }
        if to_remote.eof || to_remote.hung_up {
            to_local.read(&mut remote, libc::POLLIN, true)?;
        }
        if to_local.ready_to_finish() {
            local.shutdown(Shutdown::Write)?;
            to_local.finished = true;
        }
        if to_remote.ready_to_finish() {
            remote.shutdown(Shutdown::Write)?;
            to_remote.finished = true;
        }
        if to_local.finished && to_remote.finished {
            return Ok(());
        }
        // A receiver which stops reading cannot retain a closed daemon's
        // connection indefinitely. Ordinary streams have no idle timeout.
        let timeout = if to_local.eof || to_remote.eof || to_local.hung_up || to_remote.hung_up {
            let deadline = *drain_deadline.get_or_insert_with(|| Instant::now() + drain_timeout);
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "Timed out draining terminal stream");
            remaining.as_millis().max(1) as libc::c_int
        } else {
            -1
        };
        let mut fds = [
            bridge_pollfd(local.as_raw_fd(), &to_remote, &to_local),
            bridge_pollfd(remote.as_raw_fd(), &to_local, &to_remote),
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        for fd in &fds {
            anyhow::ensure!(
                fd.revents & libc::POLLNVAL == 0,
                "Invalid stream bridge socket"
            );
        }
        to_remote.hung_up |= fds[0].revents & (libc::POLLHUP | libc::POLLRDHUP) != 0;
        to_local.hung_up |= fds[1].revents & (libc::POLLHUP | libc::POLLRDHUP) != 0;
        if fds[0].revents & libc::POLLERR != 0
            && let Some(error) = local.take_error()?
        {
            return Err(error.into());
        }
        if fds[1].revents & libc::POLLERR != 0
            && let Some(error) = remote.take_error()?
        {
            return Err(error.into());
        }
        to_remote.read(&mut local, fds[0].revents, to_local.eof)?;
        to_local.read(&mut remote, fds[1].revents, to_remote.eof)?;
        to_local.write(&mut local, fds[0].revents)?;
        to_remote.write(&mut remote, fds[1].revents)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discarded_and_paused_listeners_release_addresses_with_inherited_descriptors() {
        let daemon = gmux_platform::unix::identity::daemon_id().unwrap();
        for running in [false, true] {
            let prepared = Server::prepare("127.0.0.1:0".parse().unwrap(), None).unwrap();
            let address = prepared.address;
            // A duplicate keeps the same socket alive, as a child does after fork.
            let inherited = prepared.listener.0.try_clone().unwrap();
            if running {
                let mut server = prepared.start(PathBuf::from("unused-control.sock"), daemon);
                server.pause();
            } else {
                drop(prepared);
            }
            let replacement =
                TcpListener::bind(address).expect("Inherited listener retained the port");
            drop(inherited);
            drop(replacement);
        }
    }

    #[test]
    fn trickling_unauthenticated_handshakes_release_the_connection_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let (completed, result) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            completed
                .send(serve(
                    socket,
                    Path::new("unused-control.sock"),
                    gmux_platform::unix::identity::daemon_id().unwrap(),
                    Some("0123456789abcdef0123456789abcdef"),
                    Instant::now() + Duration::from_millis(100),
                ))
                .unwrap();
        });
        let writer = std::thread::spawn(move || {
            for _ in 0..100 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let error = result
            .recv_timeout(Duration::from_secs(1))
            .expect("An unauthenticated trickle retained the connection")
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
        server.join().unwrap();
        writer.join().unwrap();
    }

    fn bridge_sockets() -> (UnixStream, UnixStream, TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (tcp_bridge, _) = listener.accept().unwrap();
        let (unix_bridge, unix_peer) = UnixStream::pair().unwrap();
        unix_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        tcp_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        (unix_bridge, unix_peer, tcp_bridge, tcp_peer)
    }

    #[test]
    fn bridge_drains_final_bytes_in_either_direction_and_releases_an_idle_peer() {
        for from_tcp in [true, false] {
            let (unix_bridge, mut unix_peer, tcp_bridge, mut tcp_peer) = bridge_sockets();
            if from_tcp {
                tcp_peer.write_all(b"final terminal bytes").unwrap();
                tcp_peer.shutdown(Shutdown::Write).unwrap();
            } else {
                unix_peer.write_all(b"final terminal bytes").unwrap();
                unix_peer.shutdown(Shutdown::Write).unwrap();
            }
            let worker = std::thread::spawn(move || bridge(unix_bridge, tcp_bridge, &[]));
            let mut final_bytes = Vec::new();
            let mut reverse = Vec::new();
            // The other peer deliberately keeps its write side open, as a
            // frontend may do after a daemon rejects its terminal stream.
            if from_tcp {
                unix_peer.read_to_end(&mut final_bytes).unwrap();
                tcp_peer.read_to_end(&mut reverse).unwrap();
            } else {
                tcp_peer.read_to_end(&mut final_bytes).unwrap();
                unix_peer.read_to_end(&mut reverse).unwrap();
            }
            assert_eq!(final_bytes, b"final terminal bytes");
            assert!(reverse.is_empty());
            worker.join().unwrap().unwrap();
        }
    }

    #[test]
    fn bridge_preserves_prefix_and_large_payloads_under_write_backpressure() {
        for from_tcp in [true, false] {
            let (unix_bridge, mut unix_peer, tcp_bridge, mut tcp_peer) = bridge_sockets();
            let send_buffer: libc::c_int = 1024;
            let destination = if from_tcp {
                unix_bridge.as_raw_fd()
            } else {
                tcp_bridge.as_raw_fd()
            };
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        destination,
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&send_buffer as *const libc::c_int).cast(),
                        std::mem::size_of_val(&send_buffer) as libc::socklen_t,
                    )
                },
                0
            );
            let prefix = b"already buffered bytes";
            let payload: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            let mut unix_writer = unix_peer.try_clone().unwrap();
            let mut tcp_writer = tcp_peer.try_clone().unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(move || bridge(unix_bridge, tcp_bridge, prefix));
                scope.spawn(|| {
                    if from_tcp {
                        tcp_writer.write_all(&payload).unwrap();
                        tcp_writer.shutdown(Shutdown::Write).unwrap();
                    } else {
                        unix_writer.write_all(&payload).unwrap();
                        unix_writer.shutdown(Shutdown::Write).unwrap();
                    }
                });
                let mut received = Vec::new();
                if from_tcp {
                    unix_peer.read_to_end(&mut received).unwrap();
                    assert_eq!(&received[..prefix.len()], prefix);
                    assert_eq!(&received[prefix.len()..], payload);
                } else {
                    tcp_peer.read_to_end(&mut received).unwrap();
                    assert_eq!(received, payload);
                    received.clear();
                    unix_peer.read_to_end(&mut received).unwrap();
                    assert_eq!(received, prefix);
                }
                worker.join().unwrap().unwrap();
            });
        }
    }

    #[test]
    fn a_nonreading_peer_cannot_retain_a_closed_stream_or_spin_on_hangup() {
        let (unix_bridge, mut unix_peer, tcp_bridge, tcp_peer) = bridge_sockets();
        for (fd, option) in [
            (tcp_bridge.as_raw_fd(), libc::SO_SNDBUF),
            (tcp_peer.as_raw_fd(), libc::SO_RCVBUF),
        ] {
            let size: libc::c_int = 1024;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        option,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
        }
        unix_peer.set_nonblocking(true).unwrap();
        assert!(unix_peer.write(&vec![b'x'; 512 * 1024]).unwrap() > 32 * 1024);
        unix_peer.shutdown(Shutdown::Write).unwrap();
        let thread_cpu = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
                0
            );
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        };
        let cpu = thread_cpu();
        let started = Instant::now();
        let error =
            bridge_with_drain_timeout(unix_bridge, tcp_bridge, &[], Duration::from_millis(200))
                .unwrap_err();
        assert!(error.to_string().contains("Timed out draining"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            thread_cpu() - cpu < Duration::from_millis(100),
            "Hangup caused a busy poll loop"
        );
    }
}
