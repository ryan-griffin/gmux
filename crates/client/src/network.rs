//! Outgoing authenticated TCP connections.
use gmux_platform::tcp::{TimedStream, remaining};
use gmux_platform::unix::network::configure_liveness;
use gmux_protocol::network::*;
use gmux_protocol::{control::ExecutionRequest, model::DaemonId, transport};
use serde_json::Value;
use std::{
    io::{BufReader, Read},
    net::TcpStream,
    path::PathBuf,
    time::Instant,
};
#[path = "unix/connect.rs"]
mod connect;
/// Setup was rejected and needs a settings change or an explicit retry.
#[derive(Debug)]
pub(crate) struct ConnectionRejected(pub String);
impl std::fmt::Display for ConnectionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ConnectionRejected {}
impl ConnectionRejected {
    pub(crate) fn from_error(error: anyhow::Error) -> anyhow::Error {
        Self(error.to_string()).into()
    }
}

/// Connection credentials supplied by a client, independently of how it stores
/// secrets. Saved connections can load these from their configured token file.
#[derive(Clone, Default)]
pub struct Credentials {
    token: Option<String>,
}
impl Credentials {
    pub fn token(token: impl Into<String>) -> anyhow::Result<Self> {
        let token = token.into();
        anyhow::ensure!(
            (32..=1024).contains(&token.len()) && !token.chars().any(char::is_whitespace),
            "Token must contain 32 to 1024 bytes without whitespace"
        );
        Ok(Self { token: Some(token) })
    }
}
pub fn credentials(authentication: &Authentication) -> anyhow::Result<Credentials> {
    Ok(Credentials {
        token: gmux_platform::unix::network::token(authentication)?,
    })
}
pub fn connect(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    connect_before(
        address,
        credentials,
        channel,
        expected,
        Instant::now() + TIMEOUT,
        None,
    )
}
fn connect_before(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
    deadline: Instant,
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    if let Some(c) = cancellation {
        c.check()?;
    }
    let token = credentials.token.clone();
    let deadline = deadline.min(Instant::now() + TIMEOUT);
    let mut failure = None;
    let mut connected = None;
    for address in connect::resolve(address, deadline, cancellation)? {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match connect::connect(address, deadline, cancellation) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => failure = Some(error),
        }
    }
    let stream = connected.ok_or_else(|| {
        anyhow::anyhow!(
            "Could not connect to {address}: {}",
            failure
                .map(|e| e.to_string())
                .unwrap_or_else(|| "No reachable address".into())
        )
    })?;
    let _active = cancellation.map(|c| c.register(&stream)).transpose()?;
    stream.set_nodelay(true)?;
    if matches!(channel, Channel::Stream) {
        configure_liveness(&stream)?;
    }
    let mut stream = TimedStream {
        socket: stream,
        deadline: Some(deadline),
    };
    write_json(
        &mut stream,
        &Handshake {
            version: transport::VERSION,
            channel,
            token,
            daemon: expected,
        },
    )?;
    let value = read_handshake(&mut stream)?;
    check_error(&value).map_err(ConnectionRejected::from_error)?;
    let hello: ConnectionInfo =
        serde_json::from_value(value).map_err(|error| ConnectionRejected(error.to_string()))?;
    anyhow::ensure!(
        hello.version == transport::VERSION,
        ConnectionRejected("Gmux versions are incompatible".into())
    );
    anyhow::ensure!(
        expected.is_none_or(|id| hello.daemon == id),
        ConnectionRejected("Remote connection identity changed".into())
    );
    Ok((stream.socket, hello))
}

/// Read in chunks without consuming viewport packets after the handshake.
fn read_handshake(stream: &mut TimedStream) -> anyhow::Result<Value> {
    let mut bytes = [0; MAX_HANDSHAKE + 1];
    let mut length = 0;
    loop {
        if let Some(deadline) = stream.deadline {
            stream.socket.set_read_timeout(Some(remaining(deadline)?))?;
        }
        let available = match stream.socket.peek(&mut bytes[length..]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        anyhow::ensure!(available != 0, "Incomplete network handshake");
        let size = bytes[length..length + available]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(available, |end| end + 1);
        stream.read_exact(&mut bytes[length..length + size])?;
        length += size;
        anyhow::ensure!(length <= MAX_HANDSHAKE, "Network handshake is too large");
        if bytes[length - 1] == b'\n' {
            return Ok(serde_json::from_slice(&bytes[..length])?);
        }
    }
}
/// Send a public execution request directly to an authenticated remote daemon.
/// Connection setup and the reply share a bounded deadline. No local daemon is needed.
pub fn call(
    address: &str,
    credentials: &Credentials,
    expected: Option<DaemonId>,
    request: &ExecutionRequest,
) -> anyhow::Result<Value> {
    call_cancellable(address, credentials, expected, request, None)
}
pub fn call_cancellable(
    address: &str,
    credentials: &Credentials,
    expected: Option<DaemonId>,
    request: &ExecutionRequest,
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    let mut client =
        Client::connect_before_cancellable(address, credentials, expected, deadline, cancellation)?;
    let _active = cancellation
        .map(|c| c.register(client.socket()))
        .transpose()?;
    let result = client.call(request, deadline);
    if let Some(c) = cancellation {
        c.check()?;
    }
    result
}
pub fn connect_cancellable(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    let result = connect_before(
        address,
        credentials,
        channel,
        expected,
        Instant::now() + TIMEOUT,
        cancellation,
    );
    if let Some(c) = cancellation {
        c.check()?;
    }
    result
}

/// Open a terminal stream directly to a remote daemon.
/// Send an `attach` command with the returned version and daemon identity, then subscribe.
pub fn connect_stream(
    address: &str,
    credentials: &Credentials,
    expected: Option<DaemonId>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    let (stream, info) = connect(address, credentials, Channel::Stream, expected)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok((stream, info))
}

pub struct Client {
    reader: BufReader<TimedStream>,
    pub daemon: DaemonId,
    pub home_directory: PathBuf,
    pub machine_kind: gmux_protocol::model::MachineKind,
    pub hostname: String,
}
impl Client {
    pub fn socket(&self) -> &TcpStream {
        &self.reader.get_ref().socket
    }
    pub fn connect(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
    ) -> anyhow::Result<Self> {
        Self::connect_before(address, credentials, expected, Instant::now() + TIMEOUT)
    }
    pub fn connect_before(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
        deadline: Instant,
    ) -> anyhow::Result<Self> {
        Self::connect_before_cancellable(address, credentials, expected, deadline, None)
    }
    pub fn connect_cancellable(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
        cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
    ) -> anyhow::Result<Self> {
        Self::connect_before_cancellable(
            address,
            credentials,
            expected,
            Instant::now() + TIMEOUT,
            cancellation,
        )
    }
    fn connect_before_cancellable(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
        deadline: Instant,
        cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
    ) -> anyhow::Result<Self> {
        let (stream, hello) = connect_before(
            address,
            credentials,
            Channel::Control,
            expected,
            deadline,
            cancellation,
        )?;
        Ok(Self {
            reader: BufReader::new(TimedStream {
                socket: stream,
                deadline: Some(deadline),
            }),
            daemon: hello.daemon,
            home_directory: hello.home_directory,
            machine_kind: hello.machine_kind,
            hostname: hello.hostname,
        })
    }
    pub fn call(&mut self, request: &ExecutionRequest, deadline: Instant) -> anyhow::Result<Value> {
        let deadline = deadline.min(Instant::now() + TIMEOUT);
        self.reader.get_mut().deadline = Some(deadline);
        remaining(deadline)?;
        write_json(self.reader.get_mut(), request)?;
        let value = read_json(&mut self.reader, MAX_REPLY)?;
        check_error(&value)?;
        if let Some(id) = value.get("daemon") {
            anyhow::ensure!(
                serde_json::from_value::<DaemonId>(id.clone())? == self.daemon,
                "Remote connection identity changed"
            );
        }
        if matches!(request, ExecutionRequest::Stop) {
            let mut trailing = [0u8; 1];
            anyhow::ensure!(
                self.reader.read(&mut trailing)? == 0,
                "Unexpected reply during daemon shutdown"
            );
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;
    use serde_json::json;
    use std::{
        io::Write,
        net::{Shutdown, TcpListener},
        os::fd::AsRawFd,
        time::Duration,
    };
    #[test]
    fn stream_setup_rejects_permanent_errors_and_configures_idle_liveness() {
        let daemon = gmux_platform::unix::identity::daemon_id().unwrap();
        for failure in [
            Some("authentication"),
            Some("older-version"),
            Some("newer-version"),
            Some("identity"),
            None,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_json(
                    &mut BufReader::new(stream.try_clone().unwrap()),
                    MAX_HANDSHAKE,
                )
                .unwrap();
                let mut reply = serde_json::to_value(ConnectionInfo {
                    version: transport::VERSION,
                    daemon,
                    home_directory: "/tmp".into(),
                    machine_kind: gmux_protocol::model::MachineKind::Unknown,
                    hostname: "server".into(),
                })
                .unwrap();
                match failure {
                    Some("authentication") => {
                        reply = serde_json::json!({"error":"Authentication failed"})
                    }
                    Some("older-version") => reply["version"] = (transport::VERSION - 1).into(),
                    Some("newer-version") => reply["version"] = (transport::VERSION + 1).into(),
                    Some("identity") => {
                        reply["daemon"] = serde_json::to_value(
                            gmux_platform::unix::identity::daemon_id().unwrap(),
                        )
                        .unwrap()
                    }
                    _ => (),
                }
                write_json(&mut stream, &reply).unwrap();
            });
            let result =
                ipc::Endpoint::tcp(address, Credentials::default(), Some(daemon)).connect_stream();
            if failure.is_some() {
                assert!(result.err().unwrap().is::<ConnectionRejected>());
            } else {
                let ipc::Stream::Tcp(stream) = result.unwrap() else {
                    panic!("Expected TCP stream")
                };
                assert!(stream.read_timeout().unwrap().is_none());
                for (level, option, expected) in [
                    (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1i32),
                    (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 15),
                    (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5),
                    (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3),
                    (libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 30_000),
                ] {
                    let mut actual = 0i32;
                    let mut length = std::mem::size_of_val(&actual) as libc::socklen_t;
                    assert_eq!(
                        unsafe {
                            libc::getsockopt(
                                stream.as_raw_fd(),
                                level,
                                option,
                                (&mut actual as *mut i32).cast(),
                                &mut length,
                            )
                        },
                        0
                    );
                    assert_eq!(actual, expected);
                }
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn credentials_can_be_supplied_from_memory_without_a_token_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let daemon = gmux_platform::unix::identity::daemon_id().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Handshake = serde_json::from_value(
                read_json(
                    &mut BufReader::new(stream.try_clone().unwrap()),
                    MAX_HANDSHAKE,
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                request.token.as_deref(),
                Some("0123456789abcdef0123456789abcdef")
            );
            write_json(
                &mut stream,
                &ConnectionInfo {
                    version: transport::VERSION,
                    daemon,
                    home_directory: "/tmp".into(),
                    machine_kind: gmux_protocol::model::MachineKind::Unknown,
                    hostname: "server".into(),
                },
            )
            .unwrap();
        });
        let credentials = Credentials::token("0123456789abcdef0123456789abcdef").unwrap();
        let (_, info) = connect(&address, &credentials, Channel::Control, Some(daemon)).unwrap();
        assert_eq!(info.daemon, daemon);
        server.join().unwrap();
    }

    #[test]
    fn chunked_handshakes_preserve_following_packets_and_enforce_the_size_limit() {
        for (first, last, valid) in [
            (b"{\"ok\":true}\nviewport".to_vec(), Vec::new(), true),
            (b"{\"ok\":".to_vec(), b"true}\nviewport".to_vec(), true),
            (vec![b' '; MAX_HANDSHAKE + 1], Vec::new(), false),
            (b"{\"ok\":true}".to_vec(), Vec::new(), false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            let writer = std::thread::spawn(move || {
                peer.write_all(&first).unwrap();
                if !last.is_empty() {
                    std::thread::sleep(Duration::from_millis(20));
                    peer.write_all(&last).unwrap();
                }
            });
            let mut stream = TimedStream {
                socket,
                deadline: Some(Instant::now() + Duration::from_secs(2)),
            };
            let result = read_handshake(&mut stream);
            if valid {
                assert_eq!(result.unwrap(), json!({"ok":true}));
                let mut trailing = [0; 8];
                stream.read_exact(&mut trailing).unwrap();
                assert_eq!(&trailing, b"viewport");
            } else {
                assert!(result.is_err());
            }
            writer.join().unwrap();
        }
    }
    #[test]
    fn partial_messages_do_not_restart_the_io_deadline() {
        for handshake in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            let writer = std::thread::spawn(move || {
                for byte in b"{\"slow\":true}\n" {
                    if peer.write_all(&[*byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
            });
            let mut stream = TimedStream {
                socket,
                deadline: Some(Instant::now() + Duration::from_millis(100)),
            };
            let result = if handshake {
                read_handshake(&mut stream)
            } else {
                read_json(&mut BufReader::new(&mut stream), MAX_HANDSHAKE)
            };
            let error = result.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ));
            stream.socket.shutdown(Shutdown::Both).unwrap();
            writer.join().unwrap();
        }
    }
}
