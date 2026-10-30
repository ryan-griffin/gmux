//! Linux notifications for the saved destination catalog.
pub struct Watch {
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: gmux_platform::unix::ipc::Wake,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watch {
    pub fn new(wake: gmux_platform::unix::ipc::Wake) -> anyhow::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let directory = gmux_platform::unix::storage::config_dir();
        std::fs::create_dir_all(&directory)?;
        let directory = std::ffi::CString::new(directory.as_os_str().as_encoded_bytes())?;
        let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        anyhow::ensure!(
            raw >= 0,
            "Could not watch connection settings: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        anyhow::ensure!(
            unsafe {
                libc::inotify_add_watch(
                    raw,
                    directory.as_ptr(),
                    libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_DELETE,
                )
            } >= 0,
            "Could not watch connection settings"
        );
        let stop = gmux_platform::unix::ipc::Wake::new()?;
        let stopping = stop.clone();
        let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pending = changed.clone();
        let thread = std::thread::spawn(move || {
            let mut fds = [
                libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stopping.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut buffer = [0u8; 8192];
            loop {
                let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if fds[1].revents != 0 {
                    break;
                }
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                let n =
                    unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if n <= 0 {
                    continue;
                }
                let mut offset = 0;
                let mut notify = false;
                while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                    let event = unsafe {
                        std::ptr::read_unaligned(
                            buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                        )
                    };
                    let start = offset + std::mem::size_of::<libc::inotify_event>();
                    let end = start + event.len as usize;
                    if end > n as usize {
                        break;
                    }
                    let name = &buffer[start..end];
                    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                    notify |= name == b"connections.json" || event.mask & libc::IN_Q_OVERFLOW != 0;
                    offset = end;
                }
                if notify {
                    pending.store(true, std::sync::atomic::Ordering::Release);
                    wake.notify();
                }
            }
        });
        Ok(Self {
            changed,
            stop,
            thread: Some(thread),
        })
    }
    pub fn changed(&self) -> bool {
        self.changed
            .swap(false, std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
