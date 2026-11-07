//! One bounded, nonblocking reactor for authenticated control sockets.
use super::*;

const READ_CHUNK: usize = 16 * 1024;

pub(super) struct Controls {
    pub(super) sender: mpsc::SyncSender<Control>,
    pub(super) wake: gmux_platform::unix::ipc::Wake,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Controls {
    pub(super) fn start(dispatch: Dispatch, wake: gmux_platform::unix::ipc::Wake) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Control>(32);
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = stopped.clone();
        let waker = wake.clone();
        let thread = std::thread::spawn(move || {
            let mut controls = Vec::<Control>::new();
            let mut fds = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                waker.drain();
                controls.extend(receiver.try_iter());
                // Every connection performs at most one request and 256 KiB of
                // socket writes per turn. No completion wait can stall its peers.
                controls.retain_mut(|control| control.service(&dispatch, &waker).is_ok());
                // Stop may arrive between the loop condition and wake drain.
                // Recheck before sleeping so draining cannot lose shutdown.
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                fds.clear();
                fds.push(libc::pollfd {
                    fd: waker.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.extend(controls.iter().map(Control::pollfd));
                let deadline = controls.iter().filter_map(|c| c.deadline).min();
                let immediate = controls.iter().any(Control::buffered_request);
                let timeout = if immediate {
                    0
                } else {
                    deadline.map_or(-1, |at| {
                        at.saturating_duration_since(Instant::now())
                            .as_millis()
                            .max(1) as i32
                    })
                };
                if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        });
        Self {
            sender,
            wake,
            stopped,
            thread: Some(thread),
        }
    }
}
impl Drop for Controls {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) struct Control {
    connection: Arc<Connection>,
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    pending: Option<mpsc::Receiver<serde_json::Value>>,
    slot: Option<gmux_platform::unix::ipc::ControlSlot>,
    pub(super) deadline: Option<Instant>,
    eof: bool,
}
impl Control {
    pub(super) fn new(connection: Arc<Connection>, input: Vec<u8>) -> Self {
        let deadline = (!input.is_empty()).then(|| Instant::now() + TIMEOUT);
        Self {
            connection,
            input,
            output: Vec::new(),
            offset: 0,
            pending: None,
            slot: None,
            deadline,
            eof: false,
        }
    }
    fn buffered_request(&self) -> bool {
        self.pending.is_none() && self.output.is_empty() && self.input.contains(&b'\n')
    }
    fn pollfd(&self) -> libc::pollfd {
        let events = if !self.output.is_empty() {
            libc::POLLOUT
        } else if self.pending.is_none() && !self.eof {
            libc::POLLIN
        } else {
            0
        };
        // A half-closed socket reports HUP unconditionally. While awaiting a
        // save only the completion wake and deadline can make progress.
        libc::pollfd {
            fd: if events == 0 {
                -1
            } else {
                self.connection.socket.as_raw_fd()
            },
            events,
            revents: 0,
        }
    }
    pub(super) fn reply(&mut self, value: serde_json::Value) -> anyhow::Result<()> {
        self.output = serde_json::to_vec(&value)?;
        anyhow::ensure!(self.output.len() <= MAX_REPLY, "Remote reply is too large");
        self.output.push(b'\n');
        self.offset = 0;
        self.deadline = Some(Instant::now() + TIMEOUT);
        Ok(())
    }
    pub(super) fn service(
        &mut self,
        dispatch: &Dispatch,
        wake: &gmux_platform::unix::ipc::Wake,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.connection.cancelled(), "Remote connection cancelled");
        anyhow::ensure!(
            !self.deadline.is_some_and(|at| Instant::now() >= at),
            "Remote request timed out"
        );
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(value) => {
                    self.pending = None;
                    self.reply(value)?;
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        if !self.output.is_empty() {
            let end = self.output.len().min(self.offset + 256 * 1024);
            match (&self.connection.socket).write(&self.output[self.offset..end]) {
                Ok(0) => anyhow::bail!("Remote control disconnected"),
                Ok(size) => self.offset += size,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            if self.offset < self.output.len() {
                return Ok(());
            }
            self.output = Vec::new();
            self.slot = None;
            self.deadline = (!self.input.is_empty()).then(|| Instant::now() + TIMEOUT);
        }
        if !self.input.contains(&b'\n') && !self.eof {
            let mut bytes = [0; READ_CHUNK];
            match (&self.connection.socket).read(&mut bytes) {
                Ok(0) => self.eof = true,
                Ok(size) => {
                    if self.input.is_empty() {
                        self.deadline = Some(Instant::now() + TIMEOUT);
                    }
                    self.input.extend_from_slice(&bytes[..size]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(end) = self.input.iter().position(|b| *b == b'\n') {
            anyhow::ensure!(end < MAX_REQUEST, "Remote request is too large");
            // Decode to the public execution type here. Remote clients cannot
            // submit internal, desktop, or listener-management requests.
            let request = serde_json::from_slice::<ExecutionRequest>(&self.input[..end]);
            self.input.drain(..=end);
            // Keep read-ahead in order without retaining a large request's
            // allocation for the rest of an otherwise idle connection.
            if self.input.capacity() > READ_CHUNK && self.input.len() <= READ_CHUNK {
                self.input = self.input.to_vec();
            }
            match request {
                Ok(request) => {
                    let Some(slot) = dispatch.admission.acquire() else {
                        self.reply(json!({"error":"Control admission limit reached"}))?;
                        return Ok(());
                    };
                    self.slot = Some(slot);
                    let (reply, result) = mpsc::sync_channel(1);
                    let request = super::super::commands::ControlRequest::remote(
                        request,
                        super::super::commands::Reply::remote(reply, wake.clone()),
                        self.connection.cancelled.clone(),
                    );
                    if let Err(error) = dispatch.send(Incoming::Control(request)) {
                        self.slot = None;
                        self.reply(json!({"error":error.to_string()}))?;
                    } else {
                        self.pending = Some(result);
                        self.deadline = Some(Instant::now() + CONTROL_TIMEOUT + TIMEOUT + TIMEOUT);
                    }
                }
                Err(error) => self.reply(json!({"error":error.to_string()}))?,
            }
        } else {
            anyhow::ensure!(
                self.input.len() <= MAX_REQUEST,
                "Remote request is too large"
            );
            anyhow::ensure!(!self.eof, "Remote control disconnected");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_reactor_shutdown_cannot_lose_the_stop_notification() {
        let (finished, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (dispatch, _incoming) = Dispatch::new(
                gmux_platform::unix::ipc::Wake::new().unwrap(),
                Default::default(),
            );
            for _ in 0..256 {
                drop(Controls::start(
                    dispatch.clone(),
                    gmux_platform::unix::ipc::Wake::new().unwrap(),
                ));
            }
            finished.send(()).unwrap();
        });
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("Reactor lost shutdown wake");
        worker.join().unwrap();
    }

    #[test]
    fn large_control_requests_release_input_capacity_and_preserve_read_ahead() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        socket.set_nonblocking(true).unwrap();
        let connection = Arc::new(Connection {
            socket,
            cancelled: Arc::new(AtomicBool::new(false)),
            clients: Weak::new(),
            id: 0,
        });
        let mut input =
            serde_json::to_vec(&json!({"method":"send","pane":1,"text":"x".repeat(512 * 1024)}))
                .unwrap();
        input.extend_from_slice(b"\n{\"method\":\"state\"}\n");
        let (dispatch, incoming) = Dispatch::new(
            gmux_platform::unix::ipc::Wake::new().unwrap(),
            Default::default(),
        );
        let wake = gmux_platform::unix::ipc::Wake::new().unwrap();
        let mut control = Control::new(connection, input);
        control.service(&dispatch, &wake).unwrap();
        let Incoming::Control(request) = incoming.try_recv().unwrap() else {
            panic!("Expected control request")
        };
        assert!(
            matches!(request.body, gmux_protocol::control::Request::Internal(gmux_protocol::control::InternalRequest::Execution {
            request: ExecutionRequest::Send { ref text, .. }
        }) if text.len() == 512 * 1024)
        );
        assert_eq!(control.input, b"{\"method\":\"state\"}\n");
        assert!(control.input.capacity() <= READ_CHUNK);
        assert!(
            incoming.try_recv().is_err(),
            "Read-ahead passed its predecessor"
        );
        // Simulate completion without executing input against a terminal. The
        // queued state request must still follow the preceding reply write.
        control.pending = None;
        control.reply(json!({"ok":true})).unwrap();
        control.service(&dispatch, &wake).unwrap();
        let Incoming::Control(request) = incoming.try_recv().unwrap() else {
            panic!("Expected pipelined control")
        };
        assert!(matches!(
            request.body,
            gmux_protocol::control::Request::Internal(
                gmux_protocol::control::InternalRequest::Execution {
                    request: ExecutionRequest::State
                }
            )
        ));
        assert!(control.input.is_empty());
        assert!(control.input.capacity() <= READ_CHUNK);
    }
}
