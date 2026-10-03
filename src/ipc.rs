use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

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
        let (sender, receiver) = mpsc::sync_channel(32);
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
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
                            response
                                .recv_timeout(Duration::from_secs(5))
                                .unwrap_or_else(
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
        Ok(Self { receiver, path })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn call(request: Value) -> anyhow::Result<Value> {
    call_at(&socket_path(), request)
}
pub fn call_at(path: &Path, request: Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(path).map_err(|e| {
        anyhow::anyhow!(
            "Cannot connect to gmux at {}: {e}. Start the app first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    writeln!(stream, "{request}")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let value: Value = serde_json::from_str(&response)?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("Request failed"));
    }
    Ok(value)
}

/// Separate sockets share the explicitly selected runtime directory.
pub fn companion_path(suffix: &str) -> PathBuf {
    let mut path = socket_path().into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
