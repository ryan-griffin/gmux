//! State-file reads and durable, serialized atomic writes.
use crate::{guard::DeleteOnDrop, model::WorkspaceCatalog};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "gmux") {
        let dir = dirs.config_dir().to_owned();
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir.join("gmux");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            return home.join(".config").join("gmux");
        }
    }
    PathBuf::from("/.config/gmux")
}
pub fn load<T: serde::de::DeserializeOwned + Default>(name: &str) -> anyhow::Result<T> {
    load_at(&config_dir().join(name))
}
fn load_at<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
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
    std::fs::create_dir_all(config_dir())?;
    let path = config_dir().join(name);
    save_at(&path, value)
}
fn save_at<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    value: &T,
) -> anyhow::Result<()> {
    use std::{
        fs::{File, OpenOptions},
        io::Write,
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
        sync::atomic::{AtomicU64, Ordering},
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
    // An autosave must not replace a file that became unreadable or invalid
    // after startup. Missing files are the only case that starts from defaults.
    let _: T = load_at(path)?;
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
    let temp = DeleteOnDrop(temp);
    let mut writer = std::io::BufWriter::new(&mut file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    drop(writer);
    file.sync_all()?;
    // Recheck after writing, too, in case an external editor damaged the file.
    let _: T = load_at(path)?;
    std::fs::rename(&temp.0, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn load_catalog() -> anyhow::Result<WorkspaceCatalog> {
    let mut catalog: WorkspaceCatalog = load("workspaces.json")?;
    catalog.retain_valid();
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;
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
        let file = path.join("session.json");
        assert!(load_at::<Session>(&file).unwrap().tabs.is_empty());
        assert!(load_at::<Session>(&path).is_err());
        for bytes in [b"{broken".as_slice(), b"{\"tabs\":\"invalid\"}".as_slice()] {
            std::fs::write(&file, bytes).unwrap();
            assert!(load_at::<Session>(&file).is_err());
            assert!(save_at(&file, &Session::default()).is_err());
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            assert!(!file.with_extension("tmp").exists());
        }
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn concurrent_saves_publish_complete_files_and_leave_no_temporary_files() {
        use std::{
            os::unix::fs::PermissionsExt,
            sync::atomic::{AtomicBool, Ordering},
        };
        let path = test_directory();
        let file = path.join("session.json");
        let stale = path.join("session.tmp");
        std::fs::write(&stale, b"previous temporary file").unwrap();
        save_at(&file, &Session::default()).unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let value = load_at::<Session>(&file).unwrap();
                    assert_eq!(value.width, value.height);
                }
            });
            let writers = (1..=8)
                .map(|width| {
                    let file = &file;
                    scope.spawn(move || {
                        for _ in 0..16 {
                            save_at(
                                file,
                                &Session {
                                    width,
                                    height: width,
                                    ..Session::default()
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
        assert!((1..=8).contains(&load_at::<Session>(&file).unwrap().width));
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
