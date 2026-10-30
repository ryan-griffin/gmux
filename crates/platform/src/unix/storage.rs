//! State-file reads and durable, serialized atomic writes.
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "gmux")
        .unwrap()
        .config_dir()
        .to_owned()
}
pub fn load<T: serde::de::DeserializeOwned + Default>(name: &str) -> anyhow::Result<T> {
    load_at(&config_dir().join(name))
}
pub fn load_at<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    use anyhow::Context;
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!("Could not load {}. The file is unchanged. Repair it or move it aside to start with defaults", path.display())
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}
pub fn save<T: Serialize + serde::de::DeserializeOwned + Default>(
    name: &str,
    value: &T,
) -> anyhow::Result<()> {
    with_state_lock(name, |path| save_locked_at(path, value))
}
pub fn with_state_lock<T>(
    name: &str,
    operation: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let directory = config_dir();
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    std::fs::create_dir_all(path.parent().unwrap())?;
    with_lock_at(&path, || operation(&path))
}
#[cfg(test)]
fn save_at<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    value: &T,
) -> anyhow::Result<()> {
    with_lock_at(path, || save_locked_at(path, value))
}

/// Hold the state writer lock through publication and any dependent cleanup.
pub fn with_lock_at<T>(
    path: &Path,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    use std::{
        fs::OpenOptions,
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid state path"))?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(name);
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(parent.join(lock_name))?;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    operation()
}

/// The caller must hold this path's state writer lock.
pub fn save_locked_at<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    value: &T,
) -> anyhow::Result<()> {
    save_prepared_locked_at(path, value, || Ok(()))
}

/// Stage and sync the file before applying dependent configuration. A rejected
/// preparation leaves the old file intact. The caller must hold its writer lock.
pub fn save_prepared_locked_at<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    value: &T,
    before_publish: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // An autosave must not replace a file that became unreadable or invalid
    // after startup. Missing files are the only case that starts from defaults.
    let _: T = load_at(path)?;
    atomic_write_at(
        path,
        |writer| {
            serde_json::to_writer_pretty(writer, value)?;
            Ok(())
        },
        || {
            let _: T = load_at(path)?;
            before_publish()
        },
    )
}

/// Publish bytes durably. Callers serialize ownership and validate as needed.
pub fn atomic_write_at(
    path: &Path,
    write: impl FnOnce(&mut dyn std::io::Write) -> anyhow::Result<()>,
    validate: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::{
        fs::{File, OpenOptions},
        io::Write,
        os::unix::fs::OpenOptionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid state path"))?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let temp = Temporary(temp);
    let mut writer = std::io::BufWriter::new(&mut file);
    write(&mut writer)?;
    writer.flush()?;
    drop(writer);
    file.sync_all()?;
    // Recheck after writing, too, in case an external editor damaged the file.
    validate()?;
    std::fs::rename(&temp.0, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_protocol::model::ExecutionState;
    use serde::Deserialize;
    fn test_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gmux-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn missing_state_uses_defaults_but_invalid_and_unreadable_state_is_preserved() {
        let path = test_directory();
        let file = path.join("execution.json");
        assert!(load_at::<ExecutionState>(&file).unwrap().tabs.is_empty());
        assert!(load_at::<ExecutionState>(&path).is_err());
        for bytes in [b"{broken".as_slice(), b"{\"tabs\":\"invalid\"}".as_slice()] {
            std::fs::write(&file, bytes).unwrap();
            assert!(load_at::<ExecutionState>(&file).is_err());
            assert!(save_at(&file, &ExecutionState::default()).is_err());
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            assert!(!file.with_extension("tmp").exists());
        }
        let mut outdated = serde_json::to_value(ExecutionState::default()).unwrap();
        outdated.as_object_mut().unwrap().remove("tabs");
        let bytes = serde_json::to_vec(&outdated).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        assert!(load_at::<ExecutionState>(&file).is_err());
        assert!(save_at(&file, &ExecutionState::default()).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn concurrent_saves_publish_complete_files_and_leave_no_temporary_files() {
        use std::{
            os::unix::fs::PermissionsExt,
            sync::atomic::{AtomicBool, Ordering},
        };
        let path = test_directory();
        let file = path.join("execution.json");
        let stale = path.join("execution.tmp");
        std::fs::write(&stale, b"previous temporary file").unwrap();
        save_at(&file, &ExecutionState::default()).unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let value = load_at::<ExecutionState>(&file).unwrap();
                    assert_eq!(
                        value.last_id.to_string(),
                        value
                            .scrollback
                            .get(&0)
                            .cloned()
                            .unwrap_or_else(|| "0".into())
                    );
                }
            });
            let writers = (1..=8)
                .map(|last_id| {
                    let file = &file;
                    scope.spawn(move || {
                        for _ in 0..16 {
                            save_at(
                                file,
                                &ExecutionState {
                                    last_id,
                                    scrollback: std::collections::HashMap::from([(
                                        0,
                                        last_id.to_string(),
                                    )]),
                                    ..ExecutionState::default()
                                },
                            )
                            .unwrap();
                        }
                    })
                })
                .collect::<Vec<_>>();
            let results = writers
                .into_iter()
                .map(|writer| writer.join())
                .collect::<Vec<_>>();
            done.store(true, Ordering::Relaxed);
            for result in results {
                result.unwrap();
            }
        });
        assert!((1..=8).contains(&load_at::<ExecutionState>(&file).unwrap().last_id));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&stale).unwrap(), b"previous temporary file");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 3);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn serialization_failure_preserves_state_and_removes_temporary_file() {
        #[derive(Default, Deserialize)]
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "deliberate serialization failure",
                ))
            }
        }
        let path = test_directory();
        let file = path.join("state.json");
        std::fs::write(&file, b"null").unwrap();
        assert!(
            save_at(&file, &Invalid)
                .unwrap_err()
                .to_string()
                .contains("deliberate")
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"null");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 2);
        std::fs::remove_dir_all(path).unwrap();
    }
}
