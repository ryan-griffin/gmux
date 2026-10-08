//! Optional, bounded Ghostty checkpoints owned by the daemon's saved layout.
use crate::{model::Session, storage};
use anyhow::Context;
use std::{
    collections::HashSet, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
};

pub(crate) const MAX_PANE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_PANES: usize = 128;
const MAGIC: &[u8; 8] = b"GMUXSNP\0";
const FORMAT: u32 = 1;
const SOURCE: &str = env!("GMUX_GHOSTTY_REVISION");
const HEADER: usize = 8 + 4 + 40 + 8 + 8;

pub(crate) struct Checkpoint {
    pub(crate) session: Session,
    pub(crate) panes: Vec<(u64, Vec<u8>)>,
    pub(crate) warnings: Vec<String>,
}
impl Checkpoint {
    pub(crate) fn new(
        mut session: Session,
        panes: Vec<(u64, Vec<u8>)>,
        warnings: Vec<String>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            panes.len() <= MAX_PANES
                && panes.iter().all(|(_, bytes)| bytes.len() <= MAX_PANE_BYTES)
                && panes.iter().map(|(_, bytes)| bytes.len()).sum::<usize>() <= MAX_TOTAL_BYTES,
            "Checkpoint exceeds aggregate budget"
        );
        session.scrollback.clear();
        Ok(Self {
            session,
            panes,
            warnings,
        })
    }

    // Called only by the ordered disk worker. Layout publication is the commit
    // point. A crash before it leaves the previous layout and snapshots usable.
    pub(crate) fn write(self) -> anyhow::Result<()> {
        storage::with_state_lock("session.json", |path| self.write_locked(path))
    }

    fn write_locked(mut self, path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let directory = path.parent().unwrap().join("scrollback");
        // Reclaim interrupted publications before adding another generation.
        // A damaged layout must still remain untouched, as for normal saves.
        let committed: Session = storage::load_at(path)?;
        let ready = prune_at(&committed, &directory).and_then(|()| {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)?;
            // Generation names and all filesystem work belong to this worker.
            if !self.panes.is_empty() {
                let mut random = [0u8; 16];
                std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
                let token = random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                self.session.scrollback = self
                    .panes
                    .iter()
                    .map(|(id, _)| (*id, format!("{id}-{token}.snapshot")))
                    .collect();
            }
            Ok(())
        });
        if let Err(error) = ready {
            self.session.scrollback.clear();
            self.warnings.push(format!("Scrollback directory: {error}"));
            self.panes.clear();
        }
        for (id, bytes) in self.panes {
            let name = &self.session.scrollback[&id];
            let result = storage::atomic_write_at(
                &directory.join(name),
                |writer| {
                    writer.write_all(&header(id, bytes.len())?)?;
                    writer.write_all(&bytes)?;
                    Ok(())
                },
                || Ok(()),
            );
            if let Err(error) = result {
                self.session.scrollback.remove(&id);
                self.warnings.push(format!("Pane {id}: {error}"));
            }
        }
        storage::save_locked_at(path, &self.session)?;
        prune_at(&self.session, &directory)?;
        anyhow::ensure!(self.warnings.is_empty(), "{}", self.warnings.join("; "));
        Ok(())
    }
}

fn header(id: u64, len: usize) -> anyhow::Result<[u8; HEADER]> {
    anyhow::ensure!(len <= MAX_PANE_BYTES, "Snapshot exceeds 8 MiB");
    let mut header = [0u8; HEADER];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    header[12..52].copy_from_slice(SOURCE.as_bytes());
    header[52..60].copy_from_slice(&id.to_le_bytes());
    header[60..68].copy_from_slice(&(len as u64).to_le_bytes());
    Ok(header)
}
fn payload_len(id: u64, header: &[u8], max_bytes: usize) -> anyhow::Result<usize> {
    anyhow::ensure!(header.len() == HEADER, "Truncated snapshot header");
    anyhow::ensure!(&header[..8] == MAGIC, "Invalid snapshot magic");
    anyhow::ensure!(
        u32::from_le_bytes(header[8..12].try_into()?) == FORMAT,
        "Incompatible Gmux snapshot format"
    );
    // Ghostty's format version 1 can change without a version bump. Only the
    // exact build pin is accepted, even when its own envelope version matches.
    anyhow::ensure!(
        &header[12..52] == SOURCE.as_bytes(),
        "Incompatible Ghostty source revision"
    );
    anyhow::ensure!(
        u64::from_le_bytes(header[52..60].try_into()?) == id,
        "Snapshot belongs to another pane"
    );
    let len = u64::from_le_bytes(header[60..68].try_into()?);
    anyhow::ensure!(
        len <= MAX_PANE_BYTES.min(max_bytes) as u64,
        "Snapshot exceeds restore byte budget"
    );
    Ok(len as usize)
}
#[cfg(test)]
fn encode(id: u64, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut file = header(id, bytes.len())?.to_vec();
    file.extend_from_slice(bytes);
    Ok(file)
}
#[cfg(test)]
fn decode(id: u64, file: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(file.len() >= HEADER, "Truncated snapshot header");
    let len = payload_len(id, &file[..HEADER], MAX_PANE_BYTES)?;
    anyhow::ensure!(
        len == file.len() - HEADER,
        "Truncated snapshot or trailing bytes"
    );
    Ok(file[HEADER..].to_vec())
}
fn owned_name(name: &str) -> Option<u64> {
    let (id, token) = name.strip_suffix(".snapshot")?.split_once('-')?;
    let id: u64 = id.parse().ok()?;
    (id > 0 && token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

pub(crate) fn load(id: u64, name: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let path = storage::config_dir().join("scrollback").join(name);
    load_at(id, name, &path, max_bytes).with_context(|| {
        format!(
            "Could not restore scrollback for pane {id} from {}",
            path.display()
        )
    })
}
fn load_at(id: u64, name: &str, path: &Path, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(owned_name(name) == Some(id), "Invalid snapshot reference");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= (HEADER + MAX_PANE_BYTES) as u64,
        "Invalid snapshot file size or type"
    );
    let mut header = [0u8; HEADER];
    file.read_exact(&mut header)?;
    let len = payload_len(id, &header, max_bytes)?;
    anyhow::ensure!(
        metadata.len() == (HEADER + len) as u64,
        "Truncated snapshot or trailing bytes"
    );
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len)?;
    bytes.resize(len, 0);
    file.read_exact(&mut bytes)?;
    anyhow::ensure!(file.read(&mut [0])? == 0, "Trailing snapshot bytes");
    Ok(bytes)
}

pub(crate) fn save_session(session: &Session) -> anyhow::Result<()> {
    storage::with_state_lock("session.json", |path| {
        storage::save_locked_at(path, session)?;
        prune_at(session, &path.parent().unwrap().join("scrollback"))
    })
}

pub(crate) fn prune_committed() -> anyhow::Result<()> {
    storage::with_state_lock("session.json", |path| {
        let session = storage::load_at(path)?;
        prune_at(&session, &path.parent().unwrap().join("scrollback"))
    })
}
fn prune_at(session: &Session, directory: &Path) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let panes: HashSet<_> = session
        .tabs
        .iter()
        .flat_map(|tab| tab.layout.ids())
        .collect();
    let keep: HashSet<_> = session
        .scrollback
        .iter()
        .filter(|(id, _)| panes.contains(id))
        .map(|(_, name)| name.as_str())
        .collect();
    let mut changed = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if (owned_name(name).is_some() && !keep.contains(name)) || temporary_name(name) {
            std::fs::remove_file(entry.path())?;
            changed = true;
        }
    }
    if changed {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn temporary_name(name: &str) -> bool {
    let Some((snapshot, suffix)) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .and_then(|name| name.split_once(".snapshot."))
    else {
        return false;
    };
    let Some((pid, next)) = suffix.split_once('-') else {
        return false;
    };
    owned_name(&format!("{snapshot}.snapshot")).is_some()
        && pid.parse::<u32>().is_ok()
        && next.parse::<u64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Layout, SavedTab};
    use std::collections::HashMap;

    fn test_directory() -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "gmux-scrollback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    fn saved_pane() -> Session {
        Session {
            tabs: vec![SavedTab {
                id: 1,
                layout: Layout::Pane(2),
                active: 2,
                name: String::new(),
                custom_name: false,
                workspace: None,
                directories: HashMap::new(),
            }],
            ..Session::default()
        }
    }

    #[test]
    fn concurrent_checkpoints_keep_committed_history_and_preserve_damaged_layouts() {
        let directory = test_directory();
        let path = directory.join("session.json");
        let cache = directory.join("scrollback");
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let path = &path;
                let cache = &cache;
                scope.spawn(move || {
                    for _ in 0..8 {
                        let checkpoint =
                            Checkpoint::new(saved_pane(), vec![(2, vec![writer])], vec![]).unwrap();
                        storage::with_lock_at(path, || checkpoint.write_locked(path)).unwrap();
                        storage::with_lock_at(path, || {
                            let committed: Session = storage::load_at(path)?;
                            prune_at(&committed, cache)?;
                            let name = &committed.scrollback[&2];
                            let bytes = load_at(2, name, &cache.join(name), MAX_PANE_BYTES)?;
                            assert!(bytes.len() == 1 && bytes[0] < 4);
                            Ok(())
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        std::fs::write(&path, b"{damaged layout").unwrap();
        let checkpoint = Checkpoint::new(saved_pane(), vec![(2, vec![])], vec![]).unwrap();
        assert!(storage::with_lock_at(&path, || checkpoint.write_locked(&path)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{damaged layout");
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn snapshot_envelopes_reject_incompatible_sources_ownership_and_lengths() {
        let file = encode(2, b"payload").unwrap();
        assert_eq!(decode(2, file.clone()).unwrap(), b"payload");
        assert!(decode(3, file.clone()).is_err());
        for offset in [0, 8, 12, 52, 60] {
            let mut changed = file.clone();
            changed[offset] ^= 1;
            assert!(decode(2, changed).is_err());
        }
        for len in [0, 1, HEADER - 1, file.len() - 1] {
            assert!(decode(2, file[..len].to_vec()).is_err());
        }
        let mut trailing = file;
        trailing.push(0);
        assert!(decode(2, trailing).is_err());
        assert!(encode(2, &vec![0; MAX_PANE_BYTES + 1]).is_err());
        assert!(
            Checkpoint::new(
                Session::default(),
                (1..=MAX_PANES as u64 + 1).map(|id| (id, vec![])).collect(),
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn cleanup_removes_deleted_panes_and_orphans_without_following_references() {
        let directory = test_directory();
        let live = format!("2-{}.snapshot", "a".repeat(32));
        let deleted = format!("3-{}.snapshot", "b".repeat(32));
        let orphan = format!("4-{}.snapshot", "c".repeat(32));
        let temporary = format!(".{orphan}.123-0.tmp");
        for name in [&live, &deleted, &orphan] {
            std::fs::write(directory.join(name), encode(2, b"payload").unwrap()).unwrap();
        }
        std::fs::write(directory.join("unrelated"), b"keep").unwrap();
        std::fs::write(directory.join(&temporary), b"incomplete write").unwrap();
        let session = Session {
            scrollback: HashMap::from([(2, live.clone()), (3, deleted.clone())]),
            ..saved_pane()
        };
        prune_at(&session, &directory).unwrap();
        assert!(directory.join(&live).exists());
        assert!(!directory.join(deleted).exists() && !directory.join(orphan).exists());
        assert!(directory.join("unrelated").exists());
        assert!(!directory.join(temporary).exists());
        assert_eq!(
            load_at(2, &live, &directory.join(&live), MAX_PANE_BYTES).unwrap(),
            b"payload"
        );
        assert!(load_at(2, &live, &directory.join(&live), 3).is_err());
        assert!(
            load_at(
                2,
                "../../session.json",
                &directory.join(&live),
                MAX_PANE_BYTES
            )
            .is_err()
        );
        std::os::unix::fs::symlink("unrelated", directory.join("link")).unwrap();
        assert!(load_at(2, &live, &directory.join("link"), MAX_PANE_BYTES).is_err());
        OpenOptions::new()
            .write(true)
            .open(directory.join(&live))
            .unwrap()
            .set_len((HEADER + MAX_PANE_BYTES + 1) as u64)
            .unwrap();
        assert!(load_at(2, &live, &directory.join(&live), MAX_PANE_BYTES).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
