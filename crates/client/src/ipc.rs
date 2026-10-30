//! Execution endpoints and ordered request workers.
use gmux_platform::unix::ipc::{
    Cancellation, Wake, call_at_cancellable, companion_path, socket_path,
};
use serde_json::Value;
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::Duration,
};
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
        daemon: Option<gmux_protocol::model::DaemonId>,
    },
    Saved {
        name: String,
        daemon: Option<gmux_protocol::model::DaemonId>,
    },
}
impl Endpoint {
    pub fn local() -> Self {
        Self::unix(socket_path(), companion_path(".stream"))
    }
    pub fn unix(control: PathBuf, stream: PathBuf) -> Self {
        Self::Unix { control, stream }
    }
    pub fn remote(name: String, daemon: Option<gmux_protocol::model::DaemonId>) -> Self {
        Self::Saved { name, daemon }
    }
    pub fn tcp(
        address: String,
        credentials: crate::network::Credentials,
        daemon: Option<gmux_protocol::model::DaemonId>,
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
    pub fn pin(&mut self, id: gmux_protocol::model::DaemonId) {
        match self {
            Self::Tcp { daemon, .. } | Self::Saved { daemon, .. } => *daemon = Some(id),
            Self::Unix { .. } => (),
        }
    }
    fn resolve(name: &str, daemon: Option<gmux_protocol::model::DaemonId>) -> anyhow::Result<Self> {
        Self::resolve_inner(name, daemon).map_err(crate::network::ConnectionRejected::from_error)
    }
    fn resolve_inner(
        name: &str,
        daemon: Option<gmux_protocol::model::DaemonId>,
    ) -> anyhow::Result<Self> {
        let remote = crate::connections::Catalog::load()?
            .remote(name, daemon)?
            .clone();
        Ok(Self::tcp(
            remote.address,
            crate::network::credentials(&remote.authentication)?,
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
            Self::Unix { control, .. } => call_at_cancellable(control, body, cancellation),
            Self::Tcp {
                address,
                credentials,
                daemon,
            } => {
                use gmux_protocol::control::{InternalRequest, Request};
                let request = Request::decode(body)?;
                if matches!(request, Request::Internal(InternalRequest::Hello)) {
                    let client = crate::network::Client::connect_cancellable(
                        address,
                        credentials,
                        *daemon,
                        cancellation,
                    )?;
                    return Ok(
                        serde_json::json!({"version":gmux_protocol::transport::VERSION,"daemon":client.daemon,
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
                    return Ok(serde_json::to_value(command_outcome(reply)?)?);
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
        request: &gmux_protocol::control::ExecutionRequest,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<Value> {
        match self {
            Self::Unix { control, .. } => call_at_cancellable(
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
    ) -> anyhow::Result<gmux_protocol::control::CommandReply> {
        Ok(serde_json::from_value(self.execute(
            &gmux_protocol::control::ExecutionRequest::State,
            cancellation,
        )?)?)
    }
    /// Apply a layout command and receive its typed result and execution state.
    pub fn command(
        &self,
        command: gmux_protocol::control::Command,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<gmux_protocol::control::CommandReply> {
        let request = gmux_protocol::control::InternalRequest::Command { command };
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
        command: gmux_protocol::control::Command,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<gmux_protocol::control::CommandOutcome> {
        let value = match cancellation {
            Some(c) => self.call_cancellable(command, c)?,
            None => self.call(command)?,
        };
        Ok(serde_json::from_value(value)?)
    }
    /// Stop execution and return after this daemon has shut down.
    pub fn stop(&self, cancellation: Option<&Cancellation>) -> anyhow::Result<Value> {
        match self {
            Self::Unix { .. } => {
                let hello = match cancellation {
                    Some(c) => {
                        self.call_cancellable(gmux_protocol::control::InternalRequest::Hello, c)?
                    }
                    None => self.call(gmux_protocol::control::InternalRequest::Hello)?,
                };
                let pid = hello["pid"]
                    .as_u64()
                    .and_then(|pid| i32::try_from(pid).ok())
                    .ok_or_else(|| anyhow::anyhow!("Daemon handshake requires a process ID"))?;
                gmux_platform::unix::local::stop(
                    pid,
                    || {
                        self.execute(
                            &gmux_protocol::control::ExecutionRequest::Stop,
                            cancellation,
                        )
                    },
                    cancellation,
                )
            }
            Self::Tcp { .. } => self.execute(
                &gmux_protocol::control::ExecutionRequest::Stop,
                cancellation,
            ),
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
                    gmux_protocol::network::Channel::Stream,
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
/// Reduce an execution snapshot to the public command result at the client boundary.
fn command_outcome(mut reply: Value) -> anyhow::Result<gmux_protocol::control::CommandOutcome> {
    Ok(gmux_protocol::control::CommandOutcome {
        daemon: serde_json::from_value(reply["daemon"].take())?,
        result: serde_json::from_value(reply["result"].take())?,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_platform::unix::ipc::{call_at, companion_at};
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
    };
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
}
