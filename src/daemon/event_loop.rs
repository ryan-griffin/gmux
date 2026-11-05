//! Poll PTYs, frontend traffic, control requests, and save completions.
use super::*;
use crate::persistence::SaveMode;

/// Run without initializing GTK or creating a graphics context.
pub fn run() -> anyhow::Result<()> {
    let path = gmux_platform::unix::ipc::socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(gmux_platform::unix::ipc::companion_path(".lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(gmux_protocol::control::DaemonAlreadyRunning.into());
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
    let wake = gmux_platform::unix::ipc::Wake::new()?;
    let server = gmux_platform::unix::ipc::Server::start_at(path, wake.clone())?;
    let stream_path = gmux_platform::unix::ipc::companion_path(".stream");
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
    daemon.remote_access = remote_access::RemoteAccess::start(daemon.id)?;
    let mut persistence = Worker::new(wake.clone());
    // Optional cache cleanup must not rewrite execution.json during startup.
    persistence.submit(Save::Prune, SaveMode::Explicit)?;
    let mut saving: Option<PendingControl> = None;
    let mut controls = VecDeque::new();
    let mut control_pending = false;
    let mut next_metadata = Instant::now();
    let mut next_held = Instant::now();
    let mut quit_at = None;
    let mut fds = Vec::new();
    let mut pane_indices = Vec::new();
    let mut frontend_indices = Vec::new();
    let mut ready = gmux_platform::work::Queue::default();
    let mut frontend_ready = gmux_platform::work::Queue::default();
    let mut exited = Vec::new();
    let mut metadata_pending = HashSet::new();
    let mut compression_budget = crate::compression::CompressionBudget::new(Instant::now());
    loop {
        daemon.maintain_clipboard();
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
        let git_index = daemon
            .git
            .as_ref()
            .and_then(crate::branch::Service::fd)
            .map(|fd| {
                let index = fds.len();
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                index
            });
        frontend_indices.clear();
        for (&id, frontend) in &daemon.frontends {
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
            frontend_indices.push((id, index));
        }
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
        let attached = daemon.frontends.values().any(|f| f.attached);
        let held = daemon.sessions.values().any(TerminalSession::render_held);
        let compression_deadline = daemon
            .sessions
            .values()
            .filter_map(TerminalSession::compression_deadline)
            .min();
        let mut deadline = quit_at;
        for next in [
            daemon.clipboard_deadline(),
            (!ready.is_empty() || !frontend_ready.is_empty() || control_pending).then_some(now),
            daemon
                .sessions
                .values()
                .any(TerminalSession::needs_output_poll)
                .then_some(now),
            (saving.is_none() && !controls.is_empty()).then_some(now),
            // Socket readiness cannot report commands already read into userspace.
            daemon
                .frontends
                .values()
                .any(Frontend::input_pending)
                .then_some(now),
            attached.then_some(next_metadata),
            daemon.next_update(),
            compression_budget
                .deadline(compression_deadline)
                .filter(|_| !daemon.quitting),
            daemon
                .sessions
                .values()
                .filter_map(TerminalSession::progress_deadline)
                .min(),
            held.then_some(next_held),
            daemon
                .git
                .as_ref()
                .and_then(crate::branch::Service::next_retry),
        ]
        .into_iter()
        .flatten()
        {
            deadline = Some(deadline.map_or(next, |current| current.min(next)));
        }
        let timeout = poll_timeout(deadline, Instant::now());
        let result = unsafe {
            libc::ppoll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                timeout
                    .as_ref()
                    .map_or(std::ptr::null(), std::ptr::from_ref),
                std::ptr::null(),
            )
        };
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
                        if daemon.admit_frontend(socket)? {
                            next_metadata = Instant::now();
                        }
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
            if saving.as_ref().is_some_and(|pending| pending.id() == id) {
                saving.take().unwrap().complete(&mut daemon, result);
            } else if let Err(error) = result {
                eprintln!("Could not save layout: {error}");
            }
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
        for &(id, index) in &frontend_indices {
            if fds[index].revents != 0 || daemon.frontends[&id].input_pending() {
                frontend_ready.push(id);
            }
        }
        let mut commands_budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 8);
        while !frontend_ready.is_empty() && commands_budget.take() {
            let id = frontend_ready.pop().unwrap();
            let Some(mut frontend) = daemon.frontends.remove(&id) else {
                continue;
            };
            let mut connected = true;
            match frontend.requests() {
                Ok(requests) => {
                    for request in requests {
                        let pane = daemon.resolve_pane(request.pane).ok();
                        let attach = request.is_attach();
                        let clipboard_result = matches!(
                            &request.command,
                            transport::StreamCommand::ClipboardReadResult { .. }
                                | transport::StreamCommand::ClipboardReadData { .. }
                                | transport::StreamCommand::ClipboardReadEnd { .. }
                        );
                        if let Err(error) = daemon.input(id, &mut frontend, request) {
                            if attach || clipboard_result || frontend.output.closed() {
                                connected = false;
                                break;
                            }
                            if let Some(pane) = pane {
                                frontend.pane_error(pane, error.to_string());
                                daemon.changed.insert(pane);
                            } else {
                                eprintln!("Frontend command failed: {error}");
                            }
                        }
                    }
                }
                Err(_) => connected = false,
            }
            if connected {
                daemon.frontends.insert(id, frontend);
            } else {
                daemon.cancel_frontend_reads(id);
                daemon.git_dirty = true;
                daemon.resize_sessions(None);
            }
        }
        let mut control_budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 32);
        while saving.is_none() && control_budget.take() {
            let Some(request) = controls.pop_front() else {
                break;
            };
            saving = PendingControl::start(request, &mut daemon, &persistence);
        }
        loop {
            if !control_budget.take() {
                // The wake may already be drained while requests remain in userspace.
                control_pending = true;
                break;
            }
            let Ok(request) = server.receiver.try_recv() else {
                control_pending = false;
                break;
            };
            let method = request.body["method"].as_str().unwrap_or("");
            if remote_access::RemoteAccess::handles(method) {
                daemon.remote_access.handle(daemon.id, request);
                continue;
            }
            let Some(request) = ControlRequest::decode(request) else {
                continue;
            };
            if saving.is_some() && request.body.mutates_state() && !daemon.quitting {
                controls.push_back(request);
            } else {
                let pending = PendingControl::start(request, &mut daemon, &persistence);
                if pending.is_some() {
                    saving = pending;
                }
            }
        }
        let metadata_due =
            daemon.frontends.values().any(|f| f.attached) && Instant::now() >= next_metadata;
        for &(id, index) in &pane_indices {
            if fds[index].revents != 0 {
                ready.push(id);
            }
        }
        if metadata_due {
            metadata_pending.extend(daemon.sessions.keys().copied());
        }
        if child_event || metadata_due {
            for &id in daemon.sessions.keys() {
                ready.push(id);
            }
        }
        let held_due = Instant::now() >= next_held;
        for (&id, session) in &daemon.sessions {
            if (held_due && session.render_held())
                || session
                    .progress_deadline()
                    .is_some_and(|at| Instant::now() >= at)
                || session.needs_output_poll()
            {
                ready.push(id);
            }
        }
        if held_due {
            next_held = Instant::now() + Duration::from_millis(100);
        }
        exited.clear();
        let mut pty_budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 32);
        while !ready.is_empty() && pty_budget.take() {
            let id = ready.pop().unwrap();
            let refresh_metadata = metadata_pending.remove(&id);
            let recipient = daemon.clipboard_recipient(id);
            let read_recipient = daemon.read_recipient(id);
            let Some(session) = daemon.sessions.get_mut(&id) else {
                continue;
            };
            session.set_read_enabled(read_recipient.is_some());
            let (update, clipboard) = session.poll_with_clipboard(recipient.is_some());
            session.set_read_enabled(false);
            let read = session.take_read_request();
            if update.progress_changed || update.program_status_changed {
                daemon.metadata_changed.insert(id);
            }
            daemon.git_dirty |= update.directory_changed && daemon.git.is_some();
            if update.changed {
                daemon.changed.insert(id);
            }
            if let Some(error) = update.error {
                daemon.errors.insert(id, error);
            }
            if refresh_metadata {
                session.refresh_directory();
                // This pane's periodic refresh may have yielded past the
                // broadcast that first marked its frontend metadata dirty.
                daemon.metadata_changed.insert(id);
                daemon.git_dirty |= daemon.git.is_some();
            }
            if session.exited() && !session.retain_exited() {
                exited.push(id);
                daemon.changed.insert(id);
            }
            if !update.notifications.is_empty() {
                daemon.queue_notifications(id, update.notifications);
            }
            if !clipboard.is_empty()
                && let Some(recipient) = recipient
            {
                daemon.deliver_clipboard(recipient, id, clipboard);
            }
            if let Some(request) = read {
                daemon.admit_read(id, read_recipient, request);
            }
        }
        if metadata_due {
            daemon.git_dirty |= daemon.git.is_some();
            next_metadata = Instant::now() + Duration::from_secs(1);
        }
        daemon.sync_git(&wake);
        if let Some(git) = &mut daemon.git
            && (fds[1].revents != 0
                || git_index.is_some_and(|index| fds[index].revents != 0)
                || git.next_retry().is_some_and(|at| Instant::now() >= at))
        {
            git.poll();
        }
        if daemon.next_update().is_some_and(|at| Instant::now() >= at)
            || metadata_due
            || !exited.is_empty()
        {
            let mut budget = gmux_platform::work::Budget::new(Duration::from_millis(2), 64);
            daemon.updates_budgeted(metadata_due, &mut budget);
        } else {
            let mut budget = gmux_platform::work::Budget::new(Duration::from_millis(1), 16);
            daemon.flush_frontends_budgeted(&mut budget);
        }
        let mutation = daemon.process_exits(exited.drain(..));
        if let Progress::Complete(Completion::SaveFailed(error)) =
            mutation
                .effects
                .start(&mut daemon, &persistence, SaveMode::Automatic)
        {
            eprintln!("Could not save layout after process exit: {error}");
        }
        daemon.sync_git(&wake);
        if daemon.frontends.is_empty() {
            daemon.changed.clear();
            daemon.metadata_changed.clear();
            daemon.errors.clear();
        }
        // Input, PTY reads, searches, frames, and teardown all take priority.
        // Observe activity once after all terminal access, including reads
        // that restore compressed history. Compression itself leaves the token
        // unchanged, so the next poll can use these cached deadlines.
        let now = Instant::now();
        if !daemon.quitting {
            let next = daemon
                .sessions
                .iter_mut()
                .filter_map(|(&id, session)| session.refresh_compression(now).map(|at| (id, at)))
                .min_by_key(|&(id, at)| (at, id));
            if let Some((id, at)) = next
                && compression_budget.ready(Some(at), now)
            {
                daemon.sessions.get_mut(&id).unwrap().compress();
                compression_budget.used(Instant::now());
            }
        }
    }
    let mut shutdown = None;
    if !daemon.checkpointed {
        let mutation = daemon.shutdown();
        if let Some(warning) = mutation.value.warning {
            eprintln!("{warning}");
        }
        match mutation
            .effects
            .start(&mut daemon, &persistence, SaveMode::Explicit)
        {
            Progress::Saving(pending) => shutdown = Some(pending),
            Progress::Complete(Completion::SaveFailed(error)) => {
                eprintln!("Could not save during shutdown: {error}");
            }
            _ => unreachable!(),
        }
    }
    persistence.finish();
    for (id, result) in persistence.results() {
        if shutdown.as_ref().is_some_and(|pending| pending.id == id) {
            if let Completion::SaveFailed(error) =
                shutdown.take().unwrap().complete(&mut daemon, result)
            {
                eprintln!("Could not save during shutdown: {error}");
            }
        } else if let Err(error) = result {
            eprintln!("Could not save layout during shutdown: {error}");
        }
    }
    Ok(())
}

use std::os::unix::fs::OpenOptionsExt;
