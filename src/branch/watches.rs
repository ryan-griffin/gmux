//! Directory watches stay on the parent through Git's atomic HEAD replacement.
use std::{
    collections::{HashMap, HashSet},
    ffi::{CString, OsStr},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

pub(super) enum Change {
    File {
        directory: PathBuf,
        head: bool,
        lost: bool,
    },
    Overflow,
}

pub(super) struct Watches {
    fd: OwnedFd,
    paths: HashMap<PathBuf, i32>,
    descriptors: HashMap<i32, HashSet<PathBuf>>,
}

impl Watches {
    pub(super) fn new() -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            paths: HashMap::new(),
            descriptors: HashMap::new(),
        })
    }

    pub(super) fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
    pub(super) fn len(&self) -> usize {
        self.descriptors.len()
    }
    pub(super) fn contains(&self, path: &Path) -> bool {
        self.paths.contains_key(path)
    }

    pub(super) fn retain(&mut self, wanted: &HashSet<&Path>) {
        let removed: Vec<_> = self
            .paths
            .keys()
            .filter(|path| !wanted.contains(path.as_path()))
            .cloned()
            .collect();
        for path in removed {
            self.remove(&path);
        }
    }

    fn remove(&mut self, path: &Path) {
        let Some(wd) = self.paths.remove(path) else {
            return;
        };
        let aliases = self.descriptors.get_mut(&wd).unwrap();
        aliases.remove(path);
        if aliases.is_empty() {
            self.descriptors.remove(&wd);
            unsafe {
                libc::inotify_rm_watch(self.fd(), wd);
            }
        }
    }

    pub(super) fn add(&mut self, path: &Path) -> bool {
        if self.contains(path) {
            return false;
        }
        let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_UNMOUNT
            | libc::IN_ONLYDIR;
        let wd = unsafe { libc::inotify_add_watch(self.fd(), name.as_ptr(), mask) };
        if wd < 0 {
            return false;
        }
        self.paths.insert(path.to_path_buf(), wd);
        self.descriptors
            .entry(wd)
            .or_default()
            .insert(path.to_path_buf());
        true
    }

    pub(super) fn changes(&mut self) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut buffer = [0u8; 8192];
        // Leave busy directories readable for the next daemon turn.
        for _ in 0..8 {
            let size = unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if size < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if size == 0 {
                break;
            }
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= size as usize {
                // Kernel records are aligned, but this byte buffer need not be.
                let event = unsafe {
                    buffer
                        .as_ptr()
                        .add(offset)
                        .cast::<libc::inotify_event>()
                        .read_unaligned()
                };
                let start = offset + std::mem::size_of::<libc::inotify_event>();
                let end = start + event.len as usize;
                if end > size as usize {
                    break;
                }
                let name = &buffer[start..end];
                let name =
                    OsStr::from_bytes(name.split(|byte| *byte == 0).next().unwrap_or_default());
                offset = end;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    changes.push(Change::Overflow);
                    continue;
                }
                let lost = event.mask
                    & (libc::IN_IGNORED
                        | libc::IN_DELETE_SELF
                        | libc::IN_MOVE_SELF
                        | libc::IN_UNMOUNT)
                    != 0;
                if !lost && name != "HEAD" && name != ".git" {
                    continue;
                }
                if let Some(paths) = self.descriptors.get(&event.wd) {
                    for directory in paths {
                        changes.push(Change::File {
                            directory: directory.clone(),
                            head: name == "HEAD",
                            lost,
                        });
                    }
                }
                if lost && let Some(paths) = self.descriptors.remove(&event.wd) {
                    for path in paths {
                        self.paths.remove(&path);
                    }
                    if event.mask & libc::IN_IGNORED == 0 {
                        unsafe {
                            libc::inotify_rm_watch(self.fd(), event.wd);
                        }
                    }
                }
            }
        }
        changes
    }
}
