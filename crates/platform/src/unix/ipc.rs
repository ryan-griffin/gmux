use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

// Leave time for durable execution saves and GTK organization updates.
const RESPONSE_TIMEOUT: Duration = gmux_protocol::network::CONTROL_TIMEOUT
    .saturating_add(gmux_protocol::network::TIMEOUT)
    .saturating_add(gmux_protocol::network::TIMEOUT);

/// Coalesced, nonblocking notifications for a poll loop or GLib fd source.
#[derive(Clone)]
pub struct Wake(Arc<OwnedFd>);

impl Wake {
    pub fn new() -> std::io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    pub fn notify(&self) {
        let value = 1u64;
        loop {
            let result = unsafe { libc::write(self.as_raw_fd(), (&value as *const u64).cast(), 8) };
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }

    pub fn drain(&self) {
        let mut value = 0u64;
        loop {
            let result =
                unsafe { libc::read(self.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}

impl AsRawFd for Wake {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub struct Request {
    pub body: Value,
    pub reply: mpsc::SyncSender<Value>,
}
pub struct Server {
    pub receiver: mpsc::Receiver<Request>,
    path: PathBuf,
    stop: Wake,
    thread: Option<std::thread::JoinHandle<()>>,
}
pub fn socket_path() -> PathBuf {
    std::env::var_os("GMUX_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::temp_dir().join(format!("gmux-{}", unsafe { libc::getuid() }))
                })
                .join("gmux.sock")
        })
}

impl Server {
    pub fn start_at(path: PathBuf, wake: Wake) -> anyhow::Result<Self> {
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("gmux is already listening on {}", path.display());
        }
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            use std::os::unix::fs::FileTypeExt;
            anyhow::ensure!(
                std::fs::symlink_metadata(&path)?.file_type().is_socket(),
                "Refusing to replace a non-socket file at {}",
                path.display()
            );
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Wake::new()?;
        let stopping = stop.clone();
        let (sender, receiver) = mpsc::sync_channel(32);
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread = std::thread::spawn(move || {
            let mut fds = [
                libc::pollfd {
                    fd: listener.as_raw_fd(),
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
                if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if fds[1].revents != 0 {
                    break;
                }
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue;
                    }
                    Err(_) => break,
                };
                use std::sync::atomic::Ordering;
                if active.fetch_add(1, Ordering::Relaxed) >= 64 {
                    active.fetch_sub(1, Ordering::Relaxed);
                    continue;
                }
                let active = active.clone();
                let sender = sender.clone();
                let wake = wake.clone();
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    use std::io::Read;
                    let response = if BufReader::new(&stream)
                        .take(1_048_577)
                        .read_line(&mut line)
                        .is_err()
                    {
                        serde_json::json!({"error": "Could not read request"})
                    } else if line.len() > 1_048_576 {
                        serde_json::json!({"error": "Request is too large"})
                    } else if let Ok(body) = serde_json::from_str(&line) {
                        let (reply, response) = mpsc::sync_channel(1);
                        if sender.try_send(Request { body, reply }).is_err() {
                            serde_json::json!({"error": "Request queue is full"})
                        } else {
                            wake.notify();
                            response.recv_timeout(RESPONSE_TIMEOUT).unwrap_or_else(
                                |_| serde_json::json!({"error": "Application did not respond"}),
                            )
                        }
                    } else {
                        serde_json::json!({"error": "Invalid JSON request"})
                    };
                    let _ = writeln!(stream, "{response}");
                    active.fetch_sub(1, Ordering::Relaxed);
                });
            }
        });
        Ok(Self {
            receiver,
            path,
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn call(request: impl serde::Serialize) -> anyhow::Result<Value> {
    call_at(&socket_path(), request)
}
pub fn call_at(path: &Path, request: impl serde::Serialize) -> anyhow::Result<Value> {
    call_at_cancellable(path, request, None)
}
/// Probe a client socket, returning None only when no request could have been sent.
pub fn call_if_running(
    path: &Path,
    request: impl serde::Serialize,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Option<Value>> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    call_on(stream, request, cancellation).map(Some)
}

/// Interrupt one active control call or stream handshake without waiting for its socket timeout.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<Mutex<CancelableCall>>);
#[derive(Default)]
struct CancelableCall {
    stopped: bool,
    socket: Option<OwnedFd>,
}
impl Cancellation {
    pub fn cancel(&self) {
        let mut call = self.0.lock().unwrap();
        call.stopped = true;
        if let Some(socket) = &call.socket {
            unsafe {
                libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR);
            }
        }
    }
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.0.lock().unwrap().stopped, "Request canceled");
        Ok(())
    }
    pub fn register(&self, socket: &impl AsRawFd) -> anyhow::Result<ActiveCall> {
        let mut call = self.0.lock().unwrap();
        anyhow::ensure!(!call.stopped, "Request canceled");
        anyhow::ensure!(
            call.socket.is_none(),
            "Request worker already has an active call"
        );
        let fd = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        call.socket = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(ActiveCall(self.clone()))
    }
}
pub struct ActiveCall(Cancellation);
impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().socket.take();
    }
}
pub fn call_at_cancellable(
    path: &Path,
    request: impl serde::Serialize,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Value> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let stream = UnixStream::connect(path).map_err(|e| {
        anyhow::anyhow!(
            "Cannot connect to gmux at {}: {e}. Run gmux serve first.",
            path.display()
        )
    })?;
    call_on(stream, request, cancellation)
}
fn call_on(
    mut stream: UnixStream,
    request: impl serde::Serialize,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Value> {
    let _active = cancellation
        .map(|cancellation| cancellation.register(&stream))
        .transpose()?;
    stream.set_read_timeout(Some(RESPONSE_TIMEOUT + Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(6)))?;
    let result = (|| {
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response)?;
        let value: Value = serde_json::from_str(&response)?;
        if let Some(error) = value.get("error") {
            anyhow::bail!("{}", error.as_str().unwrap_or("Request failed"));
        }
        Ok(value)
    })();
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    result
}

/// Separate sockets share the explicitly selected runtime directory.
pub fn companion_path(suffix: &str) -> PathBuf {
    companion_at(&socket_path(), suffix)
}
pub fn companion_at(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_probe_falls_back_only_before_sending_a_request() {
        let path = std::env::temp_dir().join(format!(
            "gmux-client-probe-{}.sock",
            crate::unix::identity::daemon_id().unwrap()
        ));
        let request = serde_json::json!({"method":"new"});
        assert!(call_if_running(&path, &request, None).unwrap().is_none());
        let listener = UnixListener::bind(&path).unwrap();
        // Another test can fork while this listener is open. Shutdown also
        // disables the child's inherited descriptor before it reaches exec.
        assert_eq!(
            unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) },
            0
        );
        drop(listener);
        assert!(path.exists());
        assert!(call_if_running(&path, &request, None).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(socket).read_line(&mut line).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), request);
            // A crash after accepting a mutation must never resubmit it elsewhere.
        });
        assert!(call_if_running(&path, serde_json::json!({"method":"new"}), None).is_err());
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn canceling_a_client_probe_interrupts_the_reply_without_triggering_offline_fallback() {
        let path = std::env::temp_dir().join(format!(
            "gmux-cancel-probe-{}.sock",
            crate::unix::identity::daemon_id().unwrap()
        ));
        let server = Server::start_at(path.clone(), Wake::new().unwrap()).unwrap();
        let cancellation = Cancellation::default();
        let calls = cancellation.clone();
        let control = path.clone();
        let worker = std::thread::spawn(move || {
            call_if_running(
                &control,
                serde_json::json!({"method":"remote-access-get"}),
                Some(&calls),
            )
        });
        let held = server
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        cancellation.cancel();
        let started = std::time::Instant::now();
        assert_eq!(
            worker.join().unwrap().unwrap_err().to_string(),
            "Request canceled"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        drop(server);
        assert!(
            call_if_running(
                &path,
                serde_json::json!({"method":"remote-access-get"}),
                Some(&cancellation)
            )
            .is_err()
        );
    }

    #[test]
    fn dropping_a_server_releases_its_listener_and_allows_restarting() {
        let path = std::env::temp_dir().join(format!(
            "gmux-server-lifecycle-test-{}.sock",
            std::process::id()
        ));
        let wake = Wake::new().unwrap();
        let retained_wake = Arc::downgrade(&wake.0);
        let server = Server::start_at(path.clone(), wake).unwrap();
        assert!(retained_wake.upgrade().is_some());
        drop(server);
        assert!(retained_wake.upgrade().is_none());
        assert!(!path.exists());

        let server = Server::start_at(path.clone(), Wake::new().unwrap()).unwrap();
        let client = std::thread::spawn(move || {
            call_at(&path, serde_json::json!({"method": "hello"})).unwrap()
        });
        let request = server
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(request.body["method"], "hello");
        request.reply.send(serde_json::json!({"ok": true})).unwrap();
        assert_eq!(client.join().unwrap()["ok"], true);
    }

    #[test]
    fn wake_coalesces_notifications_and_rearms_after_drain() {
        let wake = Wake::new().unwrap();
        let mut fd = libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = |fd: &mut libc::pollfd| unsafe { libc::poll(fd, 1, 0) };
        wake.drain();
        assert_eq!(ready(&mut fd), 0);
        let writer = wake.clone();
        std::thread::spawn(move || {
            for _ in 0..1000 {
                writer.notify();
            }
        })
        .join()
        .unwrap();
        assert_eq!(ready(&mut fd), 1);
        wake.drain();
        assert_eq!(ready(&mut fd), 0);
        wake.notify();
        assert_eq!(ready(&mut fd), 1);
        wake.drain();
        assert_eq!(ready(&mut fd), 0);
    }
}
