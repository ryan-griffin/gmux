//! Execution-only TCP transport. Local Unix sockets remain the default.
mod connect;
use crate::{control::ExecutionRequest, ipc, model::DaemonId, transport};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    io::{BufRead, BufReader, Read, Write},
    net::{AddrParseError, Ipv6Addr, SocketAddr, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

pub const DEFAULT_PORT: u16 = 7331;
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const CONTROL_TIMEOUT: Duration = TIMEOUT.saturating_add(TIMEOUT);
pub const MAX_HANDSHAKE: usize = 4096;
pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_REPLY: usize = 16 * 1024 * 1024;

/// Supply the default port without resolving hostnames on the caller's thread.
pub fn with_default_port(address: &str) -> Cow<'_, str> {
    if !address.contains(':') || address.ends_with(']') {
        format!("{address}:{DEFAULT_PORT}").into()
    } else if address
        .split_once('%')
        .map_or(address, |(ip, _)| ip)
        .parse::<Ipv6Addr>()
        .is_ok()
    {
        format!("[{address}]:{DEFAULT_PORT}").into()
    } else {
        address.into()
    }
}

/// Parse a listening IP address, using the default port when omitted.
pub fn parse_listen_address(address: &str) -> Result<SocketAddr, AddrParseError> {
    with_default_port(address).parse()
}

fn deserialize_listen_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SocketAddr, D::Error> {
    let address = String::deserialize(deserializer)?;
    parse_listen_address(&address).map_err(serde::de::Error::custom)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Authentication {
    None,
    Token { file: PathBuf },
}
impl Authentication {
    pub fn credentials(&self) -> anyhow::Result<Credentials> {
        Ok(Credentials {
            token: self.token()?,
        })
    }
    pub fn token(&self) -> anyhow::Result<Option<String>> {
        match self {
            Self::None => Ok(None),
            Self::Token { file } => {
                use std::os::unix::fs::OpenOptionsExt;
                // Open before checking the type so replacing the path cannot
                // make a FIFO block setup or cancellation. Symlinks to regular
                // token files remain valid.
                let input = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(file)
                    .with_context(|| format!("Could not read token file {}", file.display()))?;
                anyhow::ensure!(
                    input.metadata()?.is_file(),
                    "Token file must be a regular file"
                );
                let mut token = String::new();
                input.take(1027).read_to_string(&mut token)?;
                anyhow::ensure!(token.len() <= 1026, "Token file is too large");
                let token = token.trim_end_matches(['\r', '\n']);
                anyhow::ensure!(
                    (32..=1024).contains(&token.len()) && !token.chars().any(char::is_whitespace),
                    "Token file must contain 32 to 1024 bytes without whitespace"
                );
                Ok(Some(token.to_owned()))
            }
        }
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAccess {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_listen_address")]
    pub listen_address: SocketAddr,
    pub authentication: Authentication,
}
impl Default for RemoteAccess {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_address: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            authentication: Authentication::None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    Control,
    Stream,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    pub version: u32,
    pub channel: Channel,
    pub token: Option<String>,
    pub daemon: Option<DaemonId>,
}
#[derive(Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub version: u32,
    pub daemon: DaemonId,
    pub home_directory: PathBuf,
    pub machine_kind: crate::model::MachineKind,
    pub hostname: String,
}

pub fn read_json(reader: &mut impl BufRead, limit: usize) -> anyhow::Result<Value> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= limit && bytes.last() == Some(&b'\n'),
        "Invalid or oversized network message"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn write_json(writer: &mut impl Write, value: &impl Serialize) -> anyhow::Result<()> {
    // Serialize before writing so JSON fragments do not each need a socket
    // write and deadline update with TCP_NODELAY enabled.
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    Ok(())
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
    cancellation: Option<&ipc::Cancellation>,
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
    check_error(&value)?;
    let hello: ConnectionInfo = serde_json::from_value(value)?;
    anyhow::ensure!(
        hello.version == transport::VERSION,
        "Gmux versions are incompatible"
    );
    anyhow::ensure!(
        expected.is_none_or(|id| hello.daemon == id),
        "Remote connection identity changed"
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
pub fn check_error(value: &Value) -> anyhow::Result<()> {
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("Connection request failed"));
    }
    Ok(())
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
    cancellation: Option<&ipc::Cancellation>,
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
    cancellation: Option<&ipc::Cancellation>,
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
    pub machine_kind: crate::model::MachineKind,
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
        cancellation: Option<&ipc::Cancellation>,
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
        cancellation: Option<&ipc::Cancellation>,
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

pub fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Remote request timed out",
        ));
    }
    Ok(remaining)
}

// Socket timeouts apply to individual system calls. Recompute the remaining
// time so a partial write or a trickling reply cannot extend the operation.
pub struct TimedStream {
    pub socket: TcpStream,
    pub deadline: Option<Instant>,
}
impl Read for TimedStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            self.socket.set_read_timeout(Some(remaining(deadline)?))?;
        }
        self.socket.read(bytes)
    }
}
impl Write for TimedStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            self.socket.set_write_timeout(Some(remaining(deadline)?))?;
        }
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::Cursor,
        net::{Shutdown, TcpListener},
    };

    #[test]
    fn listener_settings_accept_optional_ports_and_save_the_resolved_port() {
        for (input, expected) in [
            ("127.0.0.1", "127.0.0.1:7331"),
            ("::1", "[::1]:7331"),
            ("[::1]", "[::1]:7331"),
            ("fe80::1%3", "[fe80::1%3]:7331"),
            ("127.0.0.1:0", "127.0.0.1:0"),
            ("127.0.0.1:8400", "127.0.0.1:8400"),
            ("[::1]:8400", "[::1]:8400"),
        ] {
            assert_eq!(parse_listen_address(input).unwrap().to_string(), expected);
            let settings: RemoteAccess = serde_json::from_value(json!({
                "enabled":true,"listen_address":input,"authentication":{"mode":"none"}
            }))
            .unwrap();
            assert_eq!(settings.listen_address.to_string(), expected);
            assert_eq!(
                serde_json::to_value(settings).unwrap()["listen_address"],
                expected
            );
        }
        for input in [
            "",
            "localhost",
            "[::1",
            "127.0.0.1:",
            "[::1]:",
            "127.0.0.1:65536",
        ] {
            assert!(parse_listen_address(input).is_err(), "Accepted {input}");
        }
    }

    #[test]
    fn token_files_follow_regular_symlinks_and_reject_fifos_without_waiting() {
        use std::{os::unix::fs::OpenOptionsExt, sync::mpsc};
        let directory =
            std::env::temp_dir().join(format!("gmux-token-file-{}", DaemonId::generate().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("token");
        let token = "0123456789abcdef0123456789abcdef";
        std::fs::write(&path, format!("{token}\n")).unwrap();
        let link = directory.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        for file in [path, link] {
            assert_eq!(
                Authentication::Token { file }.token().unwrap().as_deref(),
                Some(token)
            );
        }
        let fifo = directory.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = mpsc::channel();
        let file = fifo.clone();
        let worker = std::thread::spawn(move || {
            sender.send(Authentication::Token { file }.token()).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            // Release a regressed blocking read before failing the test.
            let mut writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fifo)
                .unwrap();
            writer.write_all(token.as_bytes()).unwrap();
        }
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let error = result
            .expect("Token loading waited for a FIFO writer")
            .unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn credentials_can_be_supplied_from_memory_without_a_token_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let daemon = DaemonId::generate().unwrap();
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
                    machine_kind: crate::model::MachineKind::Unknown,
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

    #[test]
    fn messages_require_a_newline_and_obey_the_byte_limit() {
        let mut pipelined = Cursor::new(b"{}\n{\"next\":true}\n");
        assert_eq!(read_json(&mut pipelined, 3).unwrap(), json!({}));
        assert_eq!(read_json(&mut pipelined, 14).unwrap(), json!({"next":true}));
        assert!(read_json(&mut Cursor::new(b"{}"), 3).is_err());
        let mut oversized = Cursor::new(vec![b' '; MAX_HANDSHAKE * 2]);
        assert!(read_json(&mut oversized, MAX_HANDSHAKE).is_err());
        assert_eq!(oversized.position(), MAX_HANDSHAKE as u64 + 1);
    }
}
