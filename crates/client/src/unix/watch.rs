//! Linux notifications for the saved destination catalog.
pub struct Watch {
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
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
        // OwnedFd keeps the descriptor stable until its registration is dropped.
        let fd =
            crate::runtime::block_on(async { unsafe { tokio::io::unix::AsyncFd::register(fd) } })?;
        let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pending = changed.clone();
        let task = crate::runtime::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                let Ok(mut ready) = fd.readable().await else {
                    break;
                };
                let result = ready.try_io(|fd| {
                    let n = unsafe {
                        libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                });
                let n = match result {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => n,
                    Err(_) => continue,
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(Err(_)) => break,
                };
                let mut offset = 0;
                let mut notify = false;
                while offset + std::mem::size_of::<libc::inotify_event>() <= n {
                    let event = unsafe {
                        std::ptr::read_unaligned(
                            buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                        )
                    };
                    let start = offset + std::mem::size_of::<libc::inotify_event>();
                    let end = start + event.len as usize;
                    if end > n {
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
        Ok(Self { changed, task })
    }
    pub fn changed(&self) -> bool {
        self.changed
            .swap(false, std::sync::atomic::Ordering::Acquire)
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}
