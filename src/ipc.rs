use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
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
    let mut stream = UnixStream::connect(path).map_err(|e| {
        anyhow::anyhow!(
            "Cannot connect to gmux at {}: {e}. Start the app first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    stream.set_write_timeout(Some(Duration::from_secs(6)))?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let value: Value = serde_json::from_str(&response)?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("Request failed"));
    }
    Ok(value)
}

enum Job {
    Request(u64, Value),
    Focus(u64, Option<u64>),
    Save(crate::model::Session),
    Persist(crate::model::Settings),
}
#[derive(Default)]
struct Jobs {
    queue: std::collections::VecDeque<Job>,
    closed: bool,
}
pub struct Client {
    jobs: Arc<(Mutex<Jobs>, Condvar)>,
    replies: mpsc::Receiver<(u64, anyhow::Result<Value>)>,
    next: std::cell::Cell<u64>,
}
impl Client {
    pub fn new(wake: Wake) -> Self {
        Self::at(socket_path(), wake)
    }
    fn at(path: PathBuf, wake: Wake) -> Self {
        let jobs = Arc::new((Mutex::new(Jobs::default()), Condvar::new()));
        let worker = jobs.clone();
        let (sender, replies) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let job = {
                    let (lock, ready) = &*worker;
                    let mut jobs = lock.lock().unwrap();
                    while jobs.queue.is_empty() && !jobs.closed {
                        jobs = ready.wait(jobs).unwrap();
                    }
                    let Some(job) = jobs.queue.pop_front() else {
                        break;
                    };
                    job
                };
                let (id, result) = match job {
                    Job::Request(id, request) => (id, call_at(&path, request)),
                    Job::Focus(tab, pane) => (
                        0,
                        call_at(&path, crate::control::InternalRequest::Focus { tab, pane }),
                    ),
                    Job::Save(session) => (
                        0,
                        call_at(
                            &path,
                            crate::control::InternalRequest::Presentation { session },
                        ),
                    ),
                    Job::Persist(settings) => (
                        0,
                        crate::storage::save("settings.json", &settings).map(|()| Value::Null),
                    ),
                };
                if sender.send((id, result)).is_err() {
                    break;
                }
                wake.notify();
            }
        });
        Self {
            jobs,
            replies,
            next: std::cell::Cell::new(1),
        }
    }
    pub fn request(&self, request: impl serde::Serialize) -> anyhow::Result<u64> {
        let id = self.next.get();
        self.next.set(
            id.checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("No request IDs remain"))?,
        );
        self.enqueue(Job::Request(id, serde_json::to_value(request)?))?;
        Ok(id)
    }
    pub fn save(&self, session: crate::model::Session) -> anyhow::Result<()> {
        self.enqueue(Job::Save(session))
    }
    pub fn focus(&self, tab: u64, pane: Option<u64>) -> anyhow::Result<()> {
        self.enqueue(Job::Focus(tab, pane))
    }
    pub fn persist(&self, settings: crate::model::Settings) -> anyhow::Result<()> {
        self.enqueue(Job::Persist(settings))
    }
    fn enqueue(&self, job: Job) -> anyhow::Result<()> {
        let (lock, ready) = &*self.jobs;
        let mut jobs = lock.lock().unwrap();
        // Only adjacent saves coalesce. A layout command is an ordering barrier.
        match (job, jobs.queue.back_mut()) {
            (Job::Focus(tab, pane), Some(Job::Focus(last_tab, last_pane))) => {
                *last_tab = tab;
                *last_pane = pane;
            }
            (Job::Save(value), Some(Job::Save(last))) => *last = value,
            (Job::Persist(settings), Some(Job::Persist(last))) => *last = settings,
            (job, _) => {
                anyhow::ensure!(jobs.queue.len() < 128, "Daemon request queue is full");
                jobs.queue.push_back(job);
            }
        }
        ready.notify_one();
        Ok(())
    }
    pub fn replies(&self) -> impl Iterator<Item = (u64, anyhow::Result<Value>)> + '_ {
        self.replies.try_iter()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let (lock, ready) = &*self.jobs;
        lock.lock().unwrap().closed = true;
        ready.notify_one();
    }
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
    fn slow_requests_leave_the_caller_free_and_focus_and_saves_preserve_command_order() {
        let path =
            std::env::temp_dir().join(format!("gmux-worker-test-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let (started, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for i in 0..6 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&socket).read_line(&mut line).unwrap();
                let body: Value = serde_json::from_str(&line).unwrap();
                if i == 0 {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                seen.push(body);
                writeln!(socket, "{{\"ok\":true}}").unwrap();
            }
            seen
        });
        let client = Client::at(path.clone(), Wake::new().unwrap());
        client
            .request(serde_json::json!({"method":"slow"}))
            .unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        client.focus(1, Some(10)).unwrap();
        client.focus(2, Some(20)).unwrap();
        client
            .save(crate::model::Session {
                width: 1,
                ..Default::default()
            })
            .unwrap();
        client
            .save(crate::model::Session {
                width: 2,
                ..Default::default()
            })
            .unwrap();
        client
            .request(serde_json::json!({"method":"command"}))
            .unwrap();
        client.focus(3, Some(30)).unwrap();
        client
            .save(crate::model::Session {
                width: 3,
                ..Default::default()
            })
            .unwrap();
        release.send(()).unwrap();
        let seen = worker.join().unwrap();
        assert_eq!(seen.len(), 6);
        assert_eq!(seen[1]["method"], "_focus");
        assert_eq!(seen[1]["tab"], 2);
        assert_eq!(seen[1]["pane"], 20);
        assert_eq!(seen[2]["session"]["width"], 2);
        assert_eq!(seen[3]["method"], "command");
        assert_eq!(seen[4]["tab"], 3);
        assert_eq!(seen[5]["session"]["width"], 3);
        std::fs::remove_file(path).unwrap();
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
