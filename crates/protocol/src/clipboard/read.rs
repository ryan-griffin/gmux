//! Transient reads from one desktop frontend. Data travels in bounded binary chunks.
use super::Destination;
use serde::{Deserialize, Serialize};

pub const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
pub const READ_CHUNK: usize = 16 * 1024;
pub const MAX_MIMES: usize = 4;
pub const MAX_TARGETS: usize = 16;
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
pub const TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() <= 256
        && mime
            .bytes()
            .all(|b| !b.is_ascii_whitespace() && !b.is_ascii_control())
}
pub fn validate_targets(mimes: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        mimes.len() <= MAX_TARGETS && mimes.iter().all(|m| valid_mime(m)),
        "Invalid clipboard targets"
    );
    anyhow::ensure!(
        mimes.iter().map(|m| m.len() + 1).sum::<usize>() <= 4096,
        "Clipboard targets are too large"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ReadStatus {
    Success = 0,
    Denied = 1,
    Unsupported = 2,
    Busy = 3,
    IoError = 4,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadRequest {
    pub id: u64,
    pub destination: Destination,
    pub mimes: Vec<String>,
    pub list: bool,
    pub name: String,
    /// Only true after the daemon validates the native grant against a live paste lease.
    pub granted: bool,
    pub epoch: Option<u64>,
}
impl ReadRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.id != 0
                && self.mimes.len() <= MAX_MIMES
                && self.mimes.iter().all(|m| valid_mime(m))
                && self.name.len() <= 256,
            "Invalid clipboard read request"
        );
        Ok(())
    }
    pub fn memory_size(&self) -> usize {
        self.name.capacity()
            + self.mimes.capacity() * std::mem::size_of::<String>()
            + self.mimes.iter().map(String::capacity).sum::<usize>()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadEvent {
    Request {
        request: ReadRequest,
    },
    Cancel {
        id: u64,
    },
    PasteText {
        serial: u64,
        destination: Destination,
        epoch: u64,
    },
}
impl ReadEvent {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Self::Request { request } = self {
            request.validate()?;
        }
        Ok(())
    }
    pub fn memory_size(&self) -> usize {
        if let Self::Request { request } = self {
            request.memory_size()
        } else {
            0
        }
    }
}

pub enum Event {
    Write(super::Write),
    Read { pane: u64, event: ReadEvent },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadDescriptor {
    pub mime: String,
    pub len: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadResult {
    pub id: u64,
    pub status: ReadStatus,
    pub epoch: u64,
    pub available: Vec<String>,
    pub contents: Vec<ReadDescriptor>,
}
impl ReadResult {
    pub fn byte_len(&self) -> anyhow::Result<usize> {
        validate_targets(&self.available)?;
        anyhow::ensure!(
            self.contents.len() <= MAX_MIMES && self.contents.iter().all(|c| valid_mime(&c.mime)),
            "Invalid clipboard representations"
        );
        let mut len = 0usize;
        for (i, c) in self.contents.iter().enumerate() {
            anyhow::ensure!(
                !self.contents[..i].iter().any(|v| v.mime == c.mime),
                "Duplicate clipboard representation"
            );
            len = len
                .checked_add(c.len)
                .ok_or_else(|| anyhow::anyhow!("Clipboard data length overflow"))?;
            anyhow::ensure!(len <= MAX_READ_BYTES, "Clipboard read is too large");
        }
        anyhow::ensure!(
            self.status == ReadStatus::Success
                || (len == 0 && self.contents.is_empty() && self.available.is_empty()),
            "Failure result carries clipboard data"
        );
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn result_bounds_apply_before_payload_allocation() {
        let mut r = ReadResult {
            id: 1,
            status: ReadStatus::Success,
            epoch: 0,
            available: vec![],
            contents: vec![ReadDescriptor {
                mime: "image/png".into(),
                len: MAX_READ_BYTES,
            }],
        };
        assert_eq!(r.byte_len().unwrap(), MAX_READ_BYTES);
        r.contents[0].len += 1;
        assert!(r.byte_len().is_err());
        r.contents[0].len = 0;
        r.status = ReadStatus::Denied;
        assert!(r.byte_len().is_err());
        r.status = ReadStatus::Success;
        r.contents.push(r.contents[0].clone());
        assert!(r.byte_len().is_err());
    }
}
