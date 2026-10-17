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

// Leave time for durable execution saves and GTK organization updates.
const RESPONSE_TIMEOUT: Duration = crate::network::CONTROL_TIMEOUT
    .saturating_add(crate::network::TIMEOUT)
    .saturating_add(crate::network::TIMEOUT);

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

/// An execution destination. Connecting never starts a daemon.
#[derive(Clone)]
pub enum Endpoint {
    Unix {
        control: PathBuf,
        stream: PathBuf,
    },
    Tcp {
        address: String,
        credentials: crate::network::Credentials,
        daemon: Option<crate::model::DaemonId>,
    },
    Saved {
        name: String,
        daemon: Option<crate::model::DaemonId>,
    },
}
impl Endpoint {
    pub fn local() -> Self {
        Self::unix(socket_path(), companion_path(".stream"))
    }
    pub fn unix(control: PathBuf, stream: PathBuf) -> Self {
        Self::Unix { control, stream }
    }
    pub fn remote(name: String, daemon: Option<crate::model::DaemonId>) -> Self {
        Self::Saved { name, daemon }
    }
    pub fn tcp(
        address: String,
        credentials: crate::network::Credentials,
        daemon: Option<crate::model::DaemonId>,
    ) -> Self {
        Self::Tcp {
            address,
            credentials,
            daemon,
        }
    }
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Unix { .. })
    }
    pub fn pin(&mut self, id: crate::model::DaemonId) {
        match self {
            Self::Tcp { daemon, .. } | Self::Saved { daemon, .. } => *daemon = Some(id),
            Self::Unix { .. } => (),
        }
    }
    fn resolve(name: &str, daemon: Option<crate::model::DaemonId>) -> anyhow::Result<Self> {
        let remote = crate::connections::Catalog::load()?
            .remote(name, daemon)?
            .clone();
        Ok(Self::tcp(
            remote.address,
            remote.authentication.credentials()?,
            Some(remote.daemon),
        ))
    }
    pub fn call(&self, request: impl serde::Serialize) -> anyhow::Result<Value> {
        self.call_inner(serde_json::to_value(request)?, None)
    }
    pub fn call_cancellable(
        &self,
        request: impl serde::Serialize,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Value> {
        self.call_inner(serde_json::to_value(request)?, Some(cancellation))
    }
    fn call_inner(
        &self,
        body: Value,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<Value> {
        match self {
            Self::Unix { control, .. } => call_at_inner(control, body, cancellation),
            Self::Tcp {
                address,
                credentials,
                daemon,
            } => {
                use crate::control::{InternalRequest, Request};
                let request = Request::decode(body)?;
                if matches!(request, Request::Internal(InternalRequest::Hello)) {
                    let client = crate::network::Client::connect_cancellable(
                        address,
                        credentials,
                        *daemon,
                        cancellation,
                    )?;
                    return Ok(
                        serde_json::json!({"version":crate::transport::VERSION,"daemon":client.daemon,
                        "hostname":client.hostname,"home_directory":client.home_directory,
                        "machine_kind":client.machine_kind,"quitting":false}),
                    );
                }
                let (request, output_only) = request.into_execution(|pane| {
                    pane.ok_or_else(|| {
                        anyhow::anyhow!("Remote pane requests require an explicit pane field")
                    })
                })?;
                let reply = self.execute(&request, cancellation)?;
                if output_only {
                    return Ok(serde_json::to_value(
                        crate::control::CommandOutcome::from_execution(reply)?,
                    )?);
                }
                Ok(reply)
            }
            Self::Saved { name, daemon } => {
                Self::resolve(name, *daemon)?.call_inner(body, cancellation)
            }
        }
    }
    /// Execute the same typed request over Unix or TCP. Mutation replies include
    /// execution state; callers decide how to display their command results.
    pub fn execute(
        &self,
        request: &crate::control::ExecutionRequest,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<Value> {
        match self {
            Self::Unix { control, .. } => call_at_inner(
                control,
                serde_json::json!({"method":"_execution","request":request}),
                cancellation,
            ),
            Self::Tcp {
                address,
                credentials,
                daemon,
            } => crate::network::call_cancellable(
                address,
                credentials,
                *daemon,
                request,
                cancellation,
            ),
            Self::Saved { name, daemon } => {
                Self::resolve(name, *daemon)?.execute(request, cancellation)
            }
        }
    }
    pub fn state(
        &self,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<crate::control::CommandReply> {
        Ok(serde_json::from_value(self.execute(
            &crate::control::ExecutionRequest::State,
            cancellation,
        )?)?)
    }
    /// Apply a layout command and receive its typed result and execution state.
    pub fn command(
        &self,
        command: crate::control::Command,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<crate::control::CommandReply> {
        let request = crate::control::InternalRequest::Command { command };
        let value = match cancellation {
            Some(c) => self.call_cancellable(request, c)?,
            None => self.call(request)?,
        };
        Ok(serde_json::from_value(value)?)
    }
    /// Return only the typed command result. Local commands avoid transferring
    /// an execution snapshot; TCP replies are reduced at the control API boundary.
    pub fn command_result(
        &self,
        command: crate::control::Command,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<crate::control::CommandOutcome> {
        let value = match cancellation {
            Some(c) => self.call_cancellable(command, c)?,
            None => self.call(command)?,
        };
        Ok(serde_json::from_value(value)?)
    }
    /// Stop execution and return after this daemon has shut down.
    pub fn stop(&self, cancellation: Option<&Cancellation>) -> anyhow::Result<Value> {
        match self {
            Self::Unix { .. } => crate::local::stop(self, cancellation),
            Self::Tcp { .. } => self.execute(&crate::control::ExecutionRequest::Stop, cancellation),
            Self::Saved { name, daemon } => Self::resolve(name, *daemon)?.stop(cancellation),
        }
    }
    pub fn client(&self, wake: Wake) -> Client {
        Client::for_endpoint(self.clone(), wake)
    }
    pub fn connect_stream(&self) -> anyhow::Result<Stream> {
        self.connect_stream_inner(None)
    }
    pub fn connect_stream_cancellable(
        &self,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Stream> {
        self.connect_stream_inner(Some(cancellation))
    }
    fn connect_stream_inner(&self, cancellation: Option<&Cancellation>) -> anyhow::Result<Stream> {
        if let Some(c) = cancellation {
            c.check()?;
        }
        match self {
            Self::Unix { stream, .. } => Ok(Stream::Unix(UnixStream::connect(stream)?)),
            Self::Tcp {
                address,
                credentials,
                daemon,
            } => {
                let (stream, _) = crate::network::connect_cancellable(
                    address,
                    credentials,
                    crate::network::Channel::Stream,
                    *daemon,
                    cancellation,
                )?;
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                Ok(Stream::Tcp(stream))
            }
            Self::Saved { name, daemon } => {
                Self::resolve(name, *daemon)?.connect_stream_inner(cancellation)
            }
        }
    }
}
/// A direct daemon stream over a Unix socket or TCP.
pub enum Stream {
    Unix(UnixStream),
    Tcp(std::net::TcpStream),
}
impl From<UnixStream> for Stream {
    fn from(s: UnixStream) -> Self {
        Self::Unix(s)
    }
}
impl From<std::net::TcpStream> for Stream {
    fn from(s: std::net::TcpStream) -> Self {
        Self::Tcp(s)
    }
}
impl Stream {
    pub fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Unix(s) => s.try_clone().map(Self::Unix),
            Self::Tcp(s) => s.try_clone().map(Self::Tcp),
        }
    }
    pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.shutdown(how),
            Self::Tcp(s) => s.shutdown(how),
        }
    }
    pub fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.set_read_timeout(t),
            Self::Tcp(s) => s.set_read_timeout(t),
        }
    }
    pub fn set_write_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.set_write_timeout(t),
            Self::Tcp(s) => s.set_write_timeout(t),
        }
    }
}
impl std::io::Read for Stream {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(b),
            Self::Tcp(s) => s.read(b),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(b),
            Self::Tcp(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}
impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Unix(s) => s.as_raw_fd(),
            Self::Tcp(s) => s.as_raw_fd(),
        }
    }
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
    call_at_inner(path, request, None)
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
fn call_at_inner(
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

enum Operation<R> {
    Request(Value),
    Update(Box<dyn FnOnce() -> anyhow::Result<R> + Send>),
}
struct Job<R> {
    id: u64,
    operation: Operation<R>,
    coalesce: Option<&'static str>,
}
struct Jobs<R> {
    queue: std::collections::VecDeque<Job<R>>,
    closed: bool,
    cancel_requests: bool,
}
impl<R> Default for Jobs<R> {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            closed: false,
            cancel_requests: false,
        }
    }
}
pub struct Client<R: Send + 'static = Value> {
    jobs: Arc<(Mutex<Jobs<R>>, Condvar)>,
    replies: mpsc::Receiver<(u64, anyhow::Result<R>)>,
    next: std::cell::Cell<u64>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Client<Value> {
    fn for_endpoint(endpoint: Endpoint, wake: Wake) -> Self {
        Self::with_handler(wake, move |request| endpoint.call(request))
    }
}
impl<R: Send + 'static> Client<R> {
    pub fn with_handler(
        wake: Wake,
        handler: impl Fn(Value) -> anyhow::Result<R> + Send + 'static,
    ) -> Self {
        let jobs = Arc::new((Mutex::new(Jobs::default()), Condvar::new()));
        let worker = jobs.clone();
        let (sender, replies) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            loop {
                let (job, canceled) = {
                    let (lock, ready) = &*worker;
                    let mut jobs = lock.lock().unwrap();
                    while jobs.queue.is_empty() && !jobs.closed {
                        jobs = ready.wait(jobs).unwrap();
                    }
                    let Some(job) = jobs.queue.pop_front() else {
                        break;
                    };
                    let canceled = jobs.cancel_requests
                        && job.coalesce.is_none()
                        && matches!(job.operation, Operation::Request(_));
                    (job, canceled)
                };
                let id = job.id;
                let result = if canceled {
                    Err(anyhow::anyhow!("Request canceled"))
                } else {
                    match job.operation {
                        Operation::Request(body) => handler(body),
                        Operation::Update(update) => update(),
                    }
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
            thread: Some(thread),
        }
    }
    pub fn request(&self, request: impl serde::Serialize) -> anyhow::Result<u64> {
        let id = self.next.get();
        self.next.set(
            id.checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("No request IDs remain"))?,
        );
        self.request_with_id(id, request)?;
        Ok(id)
    }
    pub fn request_with_id(&self, id: u64, request: impl serde::Serialize) -> anyhow::Result<()> {
        self.enqueue(Job {
            id,
            operation: Operation::Request(serde_json::to_value(request)?),
            coalesce: None,
        })
    }
    /// Queue an automatic update, coalescing adjacent updates of the same kind.
    pub fn update(&self, key: &'static str, request: impl serde::Serialize) -> anyhow::Result<()> {
        self.enqueue(Job {
            id: 0,
            operation: Operation::Request(serde_json::to_value(request)?),
            coalesce: Some(key),
        })
    }
    /// Run a typed automatic update on the worker without serializing its data on the caller.
    pub fn update_with(
        &self,
        key: &'static str,
        update: impl FnOnce() -> anyhow::Result<R> + Send + 'static,
    ) -> anyhow::Result<()> {
        self.enqueue(Job {
            id: 0,
            operation: Operation::Update(Box::new(update)),
            coalesce: Some(key),
        })
    }
    fn enqueue(&self, job: Job<R>) -> anyhow::Result<()> {
        let (lock, ready) = &*self.jobs;
        let mut jobs = lock.lock().unwrap();
        anyhow::ensure!(!jobs.closed, "Request worker is closed");
        if job.coalesce.is_some()
            && jobs
                .queue
                .back()
                .is_some_and(|last| last.coalesce == job.coalesce)
        {
            *jobs.queue.back_mut().unwrap() = job;
        } else {
            anyhow::ensure!(jobs.queue.len() < 128, "Request queue is full");
            jobs.queue.push_back(job);
        }
        ready.notify_one();
        Ok(())
    }
    pub fn replies(&self) -> impl Iterator<Item = (u64, anyhow::Result<R>)> + '_ {
        self.replies.try_iter()
    }
    /// Cancel queued explicit requests, while flushing automatic updates.
    /// The handler's active I/O can be interrupted with its Cancellation token.
    pub fn cancel_requests(&self) {
        let (lock, ready) = &*self.jobs;
        let mut jobs = lock.lock().unwrap();
        jobs.cancel_requests = true;
        jobs.closed = true;
        ready.notify_one();
    }
    /// Finish queued work and stop accepting requests without waiting for the worker.
    pub fn close(&self) {
        let (lock, ready) = &*self.jobs;
        lock.lock().unwrap().closed = true;
        ready.notify_one();
    }
    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
    pub fn join(&mut self) {
        self.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl<R: Send + 'static> Drop for Client<R> {
    fn drop(&mut self) {
        self.join();
    }
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
            crate::model::DaemonId::generate().unwrap()
        ));
        let request = serde_json::json!({"method":"new"});
        assert!(call_if_running(&path, &request, None).unwrap().is_none());
        let listener = UnixListener::bind(&path).unwrap();
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
            crate::model::DaemonId::generate().unwrap()
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
    fn slow_requests_leave_the_caller_free_and_automatic_updates_preserve_command_order() {
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
        let client = Endpoint::unix(path.clone(), companion_at(&path, ".stream"))
            .client(Wake::new().unwrap());
        client
            .request(serde_json::json!({"method":"slow"}))
            .unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        client
            .update(
                "focus",
                serde_json::json!({"method":"focus","tab":1,"pane":10}),
            )
            .unwrap();
        client
            .update(
                "focus",
                serde_json::json!({"method":"focus","tab":2,"pane":20}),
            )
            .unwrap();
        let saved_path = path.clone();
        client
            .update_with("save", move || {
                call_at(&saved_path, serde_json::json!({"session":{"width":1}}))
            })
            .unwrap();
        let saved_path = path.clone();
        client
            .update_with("save", move || {
                call_at(&saved_path, serde_json::json!({"session":{"width":2}}))
            })
            .unwrap();
        client
            .request(serde_json::json!({"method":"command"}))
            .unwrap();
        client
            .update(
                "focus",
                serde_json::json!({"method":"focus","tab":3,"pane":30}),
            )
            .unwrap();
        let saved_path = path.clone();
        client
            .update_with("save", move || {
                call_at(&saved_path, serde_json::json!({"session":{"width":3}}))
            })
            .unwrap();
        release.send(()).unwrap();
        let seen = worker.join().unwrap();
        assert_eq!(seen.len(), 6);
        assert_eq!(seen[1]["method"], "focus");
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
