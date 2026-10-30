//! Outgoing authenticated TCP connections on the shared client executor.
use crate::{io, runtime};
use gmux_platform::unix::ipc::Cancellation;
use gmux_protocol::network::*;
use gmux_protocol::{control::ExecutionRequest, model::DaemonId, transport};
use serde_json::Value;
use std::{net::TcpStream, path::PathBuf, time::Instant};
use tokio::io::{AsyncReadExt, BufReader};

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

pub(crate) async fn connect_async(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
    deadline: Instant,
) -> anyhow::Result<(tokio::net::TcpStream, ConnectionInfo)> {
    runtime::deadline(deadline.min(Instant::now() + TIMEOUT), async {
        let address = with_default_port(address);
        let mut failure = None;
        let mut stream = None;
        for address in resolve(address.as_ref()).await? {
            match tokio::net::TcpStream::connect(address).await {
                Ok(socket) => {
                    stream = Some(socket);
                    break;
                }
                Err(error) => failure = Some(error),
            }
        }
        let stream = stream.ok_or_else(|| {
            anyhow::anyhow!(
                "Could not connect to {address}: {}",
                failure.map_or_else(|| "No reachable address".into(), |error| error.to_string())
            )
        })?;
        stream.set_nodelay(true)?;
        if matches!(channel, Channel::Stream) {
            gmux_platform::unix::network::configure_liveness(&stream)?;
        }
        let mut stream = stream;
        authenticate(&mut stream, credentials, channel, expected)
            .await
            .map(|info| (stream, info))
    })
    .await
}
// System DNS is blocking and cannot be interrupted. Keep both its workers and
// waiting requests bounded; active lookups retain their permits after cancellation.
async fn resolve(address: &str) -> anyhow::Result<Vec<std::net::SocketAddr>> {
    use std::{
        net::ToSocketAddrs,
        sync::{Arc, OnceLock},
    };
    use tokio::sync::Semaphore;
    if let Ok(address) = address.parse() {
        return Ok(vec![address]);
    }
    static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static REQUESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let request = REQUESTS
        .get_or_init(|| Arc::new(Semaphore::new(34)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("Name resolution queue is full"))?;
    let worker = WORKERS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await?;
    let address = address.to_owned();
    Ok(tokio::task::spawn_blocking(move || {
        let (_request, _worker) = (request, worker);
        address
            .to_socket_addrs()
            .map(|addresses| addresses.take(8).collect())
    })
    .await??)
}

async fn authenticate(
    stream: &mut tokio::net::TcpStream,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
) -> anyhow::Result<ConnectionInfo> {
    io::write_json(
        stream,
        &Handshake {
            version: transport::VERSION,
            channel,
            token: credentials.token.clone(),
            daemon: expected,
        },
    )
    .await?;
    let value = read_handshake_async(stream).await?;
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
    Ok(hello)
}
/// Peek bounded chunks to leave the first viewport packet on the socket.
async fn read_handshake_async(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Value> {
    let mut bytes = [0; MAX_HANDSHAKE + 1];
    let mut length = 0;
    loop {
        let available = stream.peek(&mut bytes[length..]).await?;
        anyhow::ensure!(available != 0, "Incomplete network handshake");
        let size = bytes[length..length + available]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(available, |end| end + 1);
        stream.read_exact(&mut bytes[length..length + size]).await?;
        length += size;
        anyhow::ensure!(length <= MAX_HANDSHAKE, "Network handshake is too large");
        if bytes[length - 1] == b'\n' {
            return Ok(serde_json::from_slice(&bytes[..length])?);
        }
    }
}

pub fn connect(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    connect_cancellable(address, credentials, channel, expected, None)
}
pub fn connect_cancellable(
    address: &str,
    credentials: &Credentials,
    channel: Channel,
    expected: Option<DaemonId>,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    runtime::block_on(runtime::cancellable(cancellation, async {
        let (stream, info) = connect_async(
            address,
            credentials,
            channel,
            expected,
            Instant::now() + TIMEOUT,
        )
        .await?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        Ok((stream, info))
    }))
}
pub fn connect_stream(
    address: &str,
    credentials: &Credentials,
    expected: Option<DaemonId>,
) -> anyhow::Result<(TcpStream, ConnectionInfo)> {
    connect(address, credentials, Channel::Stream, expected)
}
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
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Value> {
    runtime::block_on(runtime::cancellable(
        cancellation,
        call_async(address, credentials, expected, request),
    ))
}
pub(crate) async fn call_async(
    address: &str,
    credentials: &Credentials,
    expected: Option<DaemonId>,
    request: &ExecutionRequest,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    let mut client = Client::connect_async(address, credentials, expected, deadline).await?;
    client.call_async(request, deadline).await
}

pub struct Client {
    reader: BufReader<tokio::net::TcpStream>,
    pub daemon: DaemonId,
    pub home_directory: PathBuf,
    pub machine_kind: gmux_protocol::model::MachineKind,
    pub hostname: String,
}
impl Client {
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
        runtime::block_on(Self::connect_async(
            address,
            credentials,
            expected,
            deadline,
        ))
    }
    pub fn connect_cancellable(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<Self> {
        runtime::block_on(runtime::cancellable(
            cancellation,
            Self::connect_async(address, credentials, expected, Instant::now() + TIMEOUT),
        ))
    }
    pub(crate) async fn connect_async(
        address: &str,
        credentials: &Credentials,
        expected: Option<DaemonId>,
        deadline: Instant,
    ) -> anyhow::Result<Self> {
        let (stream, hello) =
            connect_async(address, credentials, Channel::Control, expected, deadline).await?;
        Ok(Self {
            reader: BufReader::new(stream),
            daemon: hello.daemon,
            home_directory: hello.home_directory,
            machine_kind: hello.machine_kind,
            hostname: hello.hostname,
        })
    }
    pub fn call(&mut self, request: &ExecutionRequest, deadline: Instant) -> anyhow::Result<Value> {
        runtime::block_on(self.call_async(request, deadline))
    }
    async fn call_async(
        &mut self,
        request: &ExecutionRequest,
        deadline: Instant,
    ) -> anyhow::Result<Value> {
        runtime::deadline(deadline.min(Instant::now() + TIMEOUT), async {
            io::write_json(self.reader.get_mut(), request).await?;
            let value = io::read_json(&mut self.reader, MAX_REPLY).await?;
            check_error(&value)?;
            if let Some(id) = value.get("daemon") {
                anyhow::ensure!(
                    serde_json::from_value::<DaemonId>(id.clone())? == self.daemon,
                    "Remote connection identity changed"
                );
            }
            if matches!(request, ExecutionRequest::Stop) {
                let mut trailing = [0; 1];
                anyhow::ensure!(
                    self.reader.read(&mut trailing).await? == 0,
                    "Unexpected reply during daemon shutdown"
                );
            }
            Ok(value)
        })
        .await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};
    fn read_response(
        stream: &mut TcpStream,
        deadline: Instant,
        handshake: bool,
    ) -> anyhow::Result<Value> {
        runtime::block_on(async {
            let socket = stream.try_clone()?;
            socket.set_nonblocking(true)?;
            let mut socket = tokio::net::TcpStream::from_std(socket)?;
            let result = runtime::deadline(deadline, async {
                if handshake {
                    read_handshake_async(&mut socket).await
                } else {
                    io::read_json(&mut tokio::io::BufReader::new(&mut socket), MAX_HANDSHAKE).await
                }
            })
            .await;
            socket.into_std()?.set_nonblocking(false)?;
            result
        })
    }
    use crate::ipc;
    use serde_json::json;
    use std::{
        io::Write,
        net::{Shutdown, TcpListener},
        os::fd::AsRawFd,
        time::Duration,
    };
    #[test]
    fn canceling_a_pending_handshake_releases_its_socket_and_executor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let cancellation = Cancellation::default();
        let calls = cancellation.clone();
        let pending = runtime::spawn(async move {
            runtime::cancellable(
                Some(&calls),
                connect_async(
                    &address,
                    &Credentials::default(),
                    Channel::Control,
                    None,
                    Instant::now() + TIMEOUT,
                ),
            )
            .await
        });
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        read_json(
            &mut BufReader::new(peer.try_clone().unwrap()),
            MAX_HANDSHAKE,
        )
        .unwrap();
        let (progress, progressed) = std::sync::mpsc::channel();
        runtime::spawn(async move {
            progress.send(()).unwrap();
        });
        progressed.recv_timeout(Duration::from_secs(2)).unwrap();
        cancellation.cancel();
        let error = runtime::block_on(pending).unwrap().err().unwrap();
        assert_eq!(error.to_string(), "Request canceled");
        assert_eq!(std::io::Read::read(&mut peer, &mut [0]).unwrap(), 0);
    }

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
            let mut stream = socket;
            let result = read_response(&mut stream, Instant::now() + Duration::from_secs(2), true);
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
            let mut stream = socket;
            let result = read_response(
                &mut stream,
                Instant::now() + Duration::from_millis(100),
                handshake,
            );
            let error = result.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ));
            stream.shutdown(Shutdown::Both).unwrap();
            writer.join().unwrap();
        }
    }
}
