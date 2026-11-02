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

#[derive(Clone, Debug, Serialize)]
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
// Deriving Deserialize for an internally tagged enum buffers arbitrary JSON in
// serde's Content tree before decoding fields. Decode the flat envelope directly
// so packet admission only needs to cover this schema's typed allocations.
impl<'de> Deserialize<'de> for ReadEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            Request,
            Cancel,
            PasteText,
        }
        #[derive(Deserialize)]
        struct Event {
            kind: Kind,
            request: Option<ReadRequest>,
            id: Option<u64>,
            serial: Option<u64>,
            destination: Option<Destination>,
            epoch: Option<u64>,
        }
        let event = Event::deserialize(deserializer)?;
        use serde::de::Error;
        Ok(match event.kind {
            Kind::Request => Self::Request {
                request: event
                    .request
                    .ok_or_else(|| D::Error::missing_field("request"))?,
            },
            Kind::Cancel => Self::Cancel {
                id: event.id.ok_or_else(|| D::Error::missing_field("id"))?,
            },
            Kind::PasteText => Self::PasteText {
                serial: event
                    .serial
                    .ok_or_else(|| D::Error::missing_field("serial"))?,
                destination: event
                    .destination
                    .ok_or_else(|| D::Error::missing_field("destination"))?,
                epoch: event
                    .epoch
                    .ok_or_else(|| D::Error::missing_field("epoch"))?,
            },
        })
    }
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

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn read_event_envelopes_preserve_variants_and_require_their_fields() {
        for value in [
            serde_json::json!({"request":{"id":1,"destination":"clipboard","mimes":["text/plain"],"list":false,"name":"","granted":false,"epoch":null},"kind":"request"}),
            serde_json::json!({"id":42,"kind":"cancel"}),
            serde_json::json!({"epoch":3,"serial":4,"destination":"primary","kind":"paste_text"}),
        ] {
            let event: ReadEvent = serde_json::from_value(value.clone()).unwrap();
            event.validate().unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), value);
        }
        for text in [
            r#"{"kind":"request"}"#,
            r#"{"kind":"cancel"}"#,
            r#"{"kind":"paste_text","serial":4,"destination":"primary"}"#,
            r#"{"kind":"cancel","id":1,"id":2}"#,
        ] {
            assert!(serde_json::from_str::<ReadEvent>(text).is_err());
        }
    }
}
