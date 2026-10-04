//! Poll PTYs, frontend traffic, control requests, and save completions.
use super::*;

/// Run without initializing GTK or creating a graphics context.
pub fn run() -> anyhow::Result<()> {
    let path = ipc::socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(ipc::companion_path(".lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error.into());
    }
    // Blocking these signals before spawning shells lets signalfd wake the poll loop.
    // Child shells restore their signal mask in portable-pty's exec setup.
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::sigemptyset(&mut mask);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut mask, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let signal_fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
    anyhow::ensure!(signal_fd >= 0, "Could not create signal descriptor");
    let signal_fd = unsafe { OwnedFd::from_raw_fd(signal_fd) };
    let wake = ipc::Wake::new()?;
    let server = ipc::Server::start_at(path, wake.clone())?;
    let stream_path = ipc::companion_path(".stream");
    if stream_path.exists() {
        use std::os::unix::fs::FileTypeExt;
        anyhow::ensure!(
            std::fs::symlink_metadata(&stream_path)?
                .file_type()
                .is_socket(),
            "Refusing to replace a non-socket stream path"
        );
        std::fs::remove_file(&stream_path)?;
    }
    let listener = UnixListener::bind(&stream_path)?;
    std::fs::set_permissions(&stream_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    struct SocketCleanup(PathBuf);
    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = SocketCleanup(stream_path);
    let mut daemon = Daemon::new()?;
    let mut persistence = Worker::new(wake.clone());
    let mut saving: Option<PendingControl> = None;
    let mut controls = VecDeque::new();
    let mut next_metadata = Instant::now();
    let mut next_frame = Instant::now();
    let mut quit_at = None;
    let mut fds = Vec::new();
    let mut pane_indices = Vec::new();
    let mut ready = HashSet::new();
    let mut exited = Vec::new();
    loop {
        let now = Instant::now();
        if quit_at.is_some_and(|deadline| now >= deadline) {
            break;
        }
        if daemon.quitting && saving.is_none() && quit_at.is_none() {
            quit_at = Some(now + Duration::from_millis(100));
        }
        fds.clear();
        fds.extend([
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signal_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ]);
        let frontend_index = daemon.frontend.as_ref().map(|frontend| {
            let index = fds.len();
            fds.push(libc::pollfd {
                fd: frontend.socket.as_raw_fd(),
                events: libc::POLLIN
                    | if frontend.output.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            });
            index
        });
        pane_indices.clear();
        for (&id, session) in &daemon.sessions {
            if let Some((fd, events)) = session.poll_fd() {
                pane_indices.push((id, fds.len()));
                fds.push(libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                });
            }
        }
        let attached = daemon.frontend.as_ref().is_some_and(|f| f.attached);
        let pending = attached
            && (!daemon.changed.is_empty()
                || !daemon.notifications.is_empty()
                || !daemon.errors.is_empty());
        let searching = daemon.sessions.values().any(TerminalSession::find_pending);
        let held = daemon.sessions.values().any(TerminalSession::render_held);
        let mut deadline = quit_at;
        for next in [
            (saving.is_none() && !controls.is_empty()).then_some(now),
            attached.then_some(next_metadata),
            pending.then_some(next_frame),
            held.then_some(now + Duration::from_millis(100)),
            searching.then_some(now + Duration::from_millis(5)),
        ]
        .into_iter()
        .flatten()
        {
            deadline = Some(deadline.map_or(next, |current| current.min(next)));
        }
        let timeout = poll_timeout(deadline, now);
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            for _ in 0..8 {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket.set_nonblocking(true)?;
                        daemon.frontend = Some(Frontend {
                            socket,
                            input: Vec::new(),
                            output: Vec::new(),
                            offset: 0,
                            attached: false,
                            visible: HashSet::new(),
                            screens: HashMap::new(),
                            metadata: HashMap::new(),
                            scroll_serials: HashMap::new(),
                            sent_scroll_serials: HashMap::new(),
                            find: HashMap::new(),
                            selections: HashMap::new(),
                            discarding: false,
                            paste: None,
                        });
                        next_metadata = Instant::now();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            wake.drain();
        }
        for (id, result) in persistence.results() {
            if saving.as_ref().is_some_and(|pending| pending.id == id) {
                saving = saving
                    .take()
                    .unwrap()
                    .complete(&mut daemon, &persistence, result);
            } else if let Err(error) = result {
                eprintln!("Could not save layout: {error}");
            }
        }
        while saving.is_none() {
            let Some(request) = controls.pop_front() else {
                break;
            };
            saving = PendingControl::start(request, &mut daemon, &persistence);
        }
        let mut child_event = false;
        if fds[2].revents & libc::POLLIN != 0 {
            loop {
                let mut info = unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() };
                let n = unsafe {
                    libc::read(
                        signal_fd.as_raw_fd(),
                        (&mut info as *mut libc::signalfd_siginfo).cast(),
                        std::mem::size_of_val(&info),
                    )
                };
                if n <= 0 {
                    break;
                }
                if info.ssi_signo == libc::SIGCHLD as u32 {
                    child_event = true;
                } else {
                    daemon.quitting = true;
                }
            }
        }
        if let Some(index) = frontend_index
            && fds[index].revents != 0
            && let Some(frontend) = daemon.frontend.as_mut()
        {
            match frontend.requests() {
                Ok(requests) => {
                    for request in requests {
                        let id = daemon.resolve_pane(request.pane).ok();
                        let attach = request.is_attach();
                        if let Err(e) = daemon.input(request) {
                            if attach {
                                daemon.frontend = None;
                                break;
                            } else if let Some(id) = id {
                                daemon.errors.insert(id, e.to_string());
                            } else {
                                eprintln!("Frontend command failed: {e}");
                            }
                        }
                    }
                }
                Err(_) => {
                    daemon.frontend = None;
                }
            }
        }
        for _ in 0..32 {
            let Ok(request) = server.receiver.try_recv() else {
                break;
            };
            let method = request.body["method"].as_str().unwrap_or("");
            if !method.starts_with('_')
                && method != "quit"
                && method != "daemon-info"
                && method != "notify"
                && daemon.frontend.as_ref().is_some_and(|f| f.attached)
                && ipc::companion_path(".ui").exists()
            {
                // Forward UI actions on a worker so the owner can service backend requests
                // made by that action and keep draining terminal output.
                std::thread::spawn(move || {
                    let response = ipc::call_at(&ipc::companion_path(".ui"), request.body)
                        .unwrap_or_else(|e| json!({"error":e.to_string()}));
                    let _ = request.reply.try_send(response);
                });
            } else {
                let Some(request) = ControlRequest::decode(request) else {
                    continue;
                };
                if saving.is_some() && request.body.mutates_state() {
                    controls.push_back(request);
                } else {
                    let pending = PendingControl::start(request, &mut daemon, &persistence);
                    if pending.is_some() {
                        saving = pending;
                    }
                }
            }
        }
        let metadata_due =
            daemon.frontend.as_ref().is_some_and(|f| f.attached) && Instant::now() >= next_metadata;
        ready.clear();
        ready.extend(
            pane_indices
                .iter()
                .filter(|(_, index)| fds[*index].revents != 0)
                .map(|(id, _)| *id),
        );
        if child_event || metadata_due {
            ready.extend(daemon.sessions.keys().copied());
        }
        ready.extend(daemon.sessions.iter().filter_map(|(&id, session)| {
            (session.render_held() || session.find_pending()).then_some(id)
        }));
        // Flush queued input immediately, including terminal protocol replies.
        ready.extend(daemon.sessions.iter().filter_map(|(&id, s)| {
            s.poll_fd()
                .filter(|(_, events)| events & libc::POLLOUT != 0)
                .map(|_| id)
        }));
        exited.clear();
        for id in ready.drain() {
            let Some(session) = daemon.sessions.get_mut(&id) else {
                continue;
            };
            let update = session.poll();
            if update.changed {
                daemon.changed.insert(id);
            }
            if let Some(error) = update.error {
                daemon.errors.insert(id, error);
            }
            if metadata_due {
                session.refresh_directory();
            }
            if session.exited() {
                exited.push(id);
                daemon.changed.insert(id);
            }
            if !update.notifications.is_empty() {
                daemon.queue_notifications(id, update.notifications);
            }
        }
        if metadata_due {
            next_metadata = Instant::now() + Duration::from_secs(1);
        }
        if (Instant::now() >= next_frame
            && (!daemon.changed.is_empty()
                || !daemon.notifications.is_empty()
                || !daemon.errors.is_empty()))
            || metadata_due
            || !exited.is_empty()
        {
            if let Err(error) = daemon.updates(metadata_due) {
                eprintln!("Detaching frontend: {error}");
                daemon.frontend = None;
            }
            next_frame = Instant::now() + Duration::from_millis(16);
        } else if let Some(frontend) = daemon.frontend.as_mut()
            && frontend.flush().is_err()
        {
            daemon.frontend = None;
        }
        let layout_changed = !exited.is_empty();
        for id in exited.drain(..) {
            daemon.close(id);
        }
        if layout_changed
            && let Err(error) = persistence.submit(Save::Session(daemon.session_snapshot()), true)
        {
            eprintln!("Could not save layout after process exit: {error}");
        }
        if daemon.frontend.is_none() {
            daemon.changed.clear();
            daemon.errors.clear();
        }
    }
    if let Err(error) = persistence.submit(Save::Session(daemon.session_snapshot()), false) {
        eprintln!("Could not save layout during shutdown: {error}");
    }
    persistence.finish();
    for (_, result) in persistence.results() {
        if let Err(error) = result {
            eprintln!("Could not save layout during shutdown: {error}");
        }
    }
    Ok(())
}

use std::os::unix::fs::OpenOptionsExt;
