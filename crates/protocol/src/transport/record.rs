//! Admission metadata is checked before allocating the JSON header or payload.
use super::{CELL_PREFIX_BYTES, MAX_PACKET, Packet, PaneHeader, PaneUpdate};

pub const RECORD_PREFIX_BYTES: usize = 20;
pub(super) const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RecordKind {
    Pane = 1,
    Execution = 2,
    Clipboard = 3,
    Git = 4,
    ClipboardRead = 5,
}
impl RecordKind {
    pub(super) fn header_limit(self) -> usize {
        match self {
            Self::Clipboard | Self::ClipboardRead => crate::clipboard::MAX_HEADER_BYTES,
            Self::Pane | Self::Execution | Self::Git => MAX_HEADER_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Record {
    size: usize,
    header: usize,
    kind: RecordKind,
    json: usize,
    cells: usize,
}
impl Record {
    pub(super) fn new(
        size: usize,
        header: usize,
        kind: RecordKind,
        json: usize,
        cells: usize,
    ) -> anyhow::Result<Self> {
        let record = Self {
            size,
            header,
            kind,
            json,
            cells,
        };
        record.validate()?;
        Ok(record)
    }
    pub fn is_heartbeat(prefix: [u8; 4]) -> anyhow::Result<bool> {
        let size = u32::from_le_bytes(prefix) as usize;
        anyhow::ensure!(
            size == 0 || (RECORD_PREFIX_BYTES - 4..=MAX_PACKET).contains(&size),
            "Invalid record size"
        );
        Ok(size == 0)
    }
    pub fn decode(bytes: [u8; RECORD_PREFIX_BYTES]) -> anyhow::Result<Self> {
        let values = std::array::from_fn::<_, 5, _>(|i| {
            u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()) as usize
        });
        let kind = match values[2] {
            1 => RecordKind::Pane,
            2 => RecordKind::Execution,
            3 => RecordKind::Clipboard,
            4 => RecordKind::Git,
            5 => RecordKind::ClipboardRead,
            _ => anyhow::bail!("Unknown record kind"),
        };
        Self::new(values[0], values[1], kind, values[3], values[4])
    }
    pub(super) fn encode(self) -> [u8; RECORD_PREFIX_BYTES] {
        let mut bytes = [0; RECORD_PREFIX_BYTES];
        for (out, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip([
            self.size,
            self.header,
            self.kind as usize,
            self.json,
            self.cells,
        ]) {
            out.copy_from_slice(&(value as u32).to_le_bytes());
        }
        bytes
    }
    fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (RECORD_PREFIX_BYTES - 4..=MAX_PACKET).contains(&self.size),
            "Invalid record size"
        );
        anyhow::ensure!(
            self.header <= self.kind.header_limit()
                && self.header <= self.size - (RECORD_PREFIX_BYTES - 4),
            "Invalid record header size"
        );
        anyhow::ensure!(
            self.json >= 4096 + 4 * self.header && self.json <= MAX_DECODE_BYTES,
            "Invalid JSON decode allowance"
        );
        anyhow::ensure!(
            self.cells <= self.payload_size() / CELL_PREFIX_BYTES,
            "Invalid record cell count"
        );
        match self.kind {
            RecordKind::Clipboard => anyhow::ensure!(
                self.cells == 0 && self.payload_size() <= crate::clipboard::MAX_OFFER_BYTES,
                "Invalid clipboard record"
            ),
            RecordKind::ClipboardRead | RecordKind::Git | RecordKind::Execution => anyhow::ensure!(
                self.cells == 0 && self.payload_size() == 0,
                "Unexpected binary payload"
            ),
            RecordKind::Pane => {}
        }
        anyhow::ensure!(
            self.decode_bytes() <= MAX_DECODE_BYTES,
            "Record exceeds decoded-size limit"
        );
        Ok(())
    }
    pub fn kind(self) -> RecordKind {
        self.kind
    }
    pub fn header_size(self) -> usize {
        self.header
    }
    pub(super) fn cell_count(self) -> usize {
        self.cells
    }
    pub(super) fn payload_size(self) -> usize {
        self.size - (RECORD_PREFIX_BYTES - 4) - self.header
    }
    /// Includes JSON input and scratch, all typed header allocations, exact cell
    /// vector storage, overflow text and Arc headers, and queue transfer overhead.
    pub fn decode_bytes(self) -> usize {
        self.json
            + self.payload_size()
            + self.cells * (size_of::<crate::Cell>() + 2 * size_of::<usize>())
    }
    /// Decode event metadata using the framed kind. Pane cells remain empty
    /// until read_body; clipboard buffers are allocated only after descriptors
    /// match framing. No untyped JSON tree or trial enum decoding is involved.
    pub fn parse_header(self, bytes: &[u8]) -> anyhow::Result<Packet> {
        anyhow::ensure!(
            bytes.len() == self.header && json_budget(bytes)? <= self.json,
            "JSON exceeds declared decode allowance"
        );
        let mut de = serde_json::Deserializer::from_slice(bytes);
        use serde::{Deserialize, de::DeserializeSeed};
        let packet = match self.kind {
            RecordKind::Pane => Packet::Pane(PaneUpdate {
                header: PaneHeader::deserialize(&mut de)?,
                cells: Vec::new(),
            }),
            RecordKind::Execution => {
                Packet::Execution(crate::control::CommandReply::deserialize(&mut de)?)
            }
            RecordKind::Git => Packet::Git(super::GitMetadata::deserialize(&mut de)?),
            RecordKind::Clipboard => Packet::Clipboard(
                crate::clipboard::wire::WriteSeed {
                    payload: self.payload_size(),
                }
                .deserialize(&mut de)?,
            ),
            RecordKind::ClipboardRead => {
                let read = super::ClipboardRead::deserialize(&mut de)?;
                read.event.validate()?;
                Packet::ClipboardRead(read)
            }
        };
        de.end()?;
        Ok(packet)
    }
    pub fn validate_packet(self, packet: &Packet) -> anyhow::Result<usize> {
        let bytes = packet.memory_size();
        anyhow::ensure!(
            packet.kind() == self.kind
                && packet.as_pane().map_or(0, |p| p.cells.len()) == self.cells
                && bytes + 256 <= self.decode_bytes(),
            "Decoded record exceeds allowance"
        );
        if let Packet::Clipboard(copy) = packet {
            copy.validate()?;
        }
        Ok(bytes)
    }
}

/// Allocation bound for the stream payload schemas, computed without allocating
/// or decoding strings. Numeric values never control allocation, except clipboard
/// lengths checked separately above. Strings use at most their encoded length.
/// Four times the input covers the input itself, serde's geometrically grown
/// escape scratch and owned strings. Each object gets 2048 bytes for typed
/// structs, Vec minimum capacity and tree boxes. Arrays
/// get 32 bytes. Array commas get 64 bytes for geometric growth of scalar, tuple
/// and String vectors. Object commas get 256 bytes for both hash tables in
/// pane_directories, including rounding, keys and temporary storage.
/// The largest vector struct, Metadata, is charged at its object opener instead.
/// These constants must be audited when stream payloads or their reachable types change.
/// Duplicate/unknown fields still count. Limit nesting before serde recursion.
pub(super) fn json_budget(bytes: &[u8]) -> anyhow::Result<usize> {
    let mut budget = 4096 + 4 * bytes.len();
    let mut depth = 0usize;
    let mut objects = [false; 64];
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                // Search quotes in SIMD-sized blocks. Only quotes preceded
                // by an odd run of backslashes are escaped; other escapes have
                // no effect on the structural allocation bound.
                let mut end = bytes.len();
                for offset in memchr::memchr_iter(b'"', &bytes[index..]) {
                    let quote = index + offset;
                    let mut slash = quote;
                    while slash > index && bytes[slash - 1] == b'\\' {
                        slash -= 1;
                    }
                    if (quote - slash) % 2 == 0 {
                        end = quote;
                        break;
                    }
                }
                index = end;
            }
            b'{' | b'[' => {
                anyhow::ensure!(depth < 64, "JSON nesting is too deep");
                objects[depth] = bytes[index] == b'{';
                depth += 1;
                budget += if bytes[index] == b'{' { 2048 } else { 32 };
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' => {
                budget += if depth != 0 && objects[depth - 1] {
                    256
                } else {
                    64
                }
            }
            _ => {}
        }
        index += 1;
    }
    anyhow::ensure!(
        budget <= MAX_DECODE_BYTES,
        "JSON exceeds decoded-size limit"
    );
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn wire(header: &[u8], kind: RecordKind, payload: usize, cells: usize, json: usize) -> Vec<u8> {
        // Deliberately bypass Record::new to test hostile declarations.
        let record = Record {
            size: RECORD_PREFIX_BYTES - 4 + header.len() + payload,
            header: header.len(),
            kind,
            json,
            cells,
        };
        let mut bytes = record.encode().to_vec();
        bytes.extend_from_slice(header);
        bytes
    }
    #[test]
    fn untrusted_framing_and_json_expansion_fail_before_payload_allocation() {
        let header = serde_json::to_vec(&PaneHeader::default()).unwrap();
        let budget = json_budget(&header).unwrap();
        for (payload, cells, json) in [
            (0, usize::MAX, budget),
            (0, 0, MAX_DECODE_BYTES + 1),
            (MAX_PACKET, 0, budget),
            (0, 0, 4096),
        ] {
            let bytes = wire(&header, RecordKind::Pane, payload, cells, json);
            let mut cursor = Cursor::new(bytes);
            assert!(Packet::read(&mut cursor).is_err());
            assert_eq!(
                cursor.position(),
                if payload == MAX_PACKET {
                    4
                } else {
                    RECORD_PREFIX_BYTES as u64
                }
            );
        }
        let mut value = serde_json::to_value(PaneHeader::default()).unwrap();
        value["notifications"] = serde_json::json!(vec![""; 1000]);
        let header = serde_json::to_vec(&value).unwrap();
        // Enough credit for input bytes, but not for Vec<String> expansion.
        let bytes = wire(&header, RecordKind::Pane, 0, 0, 4096 + 4 * header.len());
        let error = Packet::read(&mut bytes.as_slice()).err().unwrap();
        assert!(error.to_string().contains("decode allowance"));
        value["copy"] = serde_json::json!({"destination":"clipboard", "content":[{"mime":"text/plain", "len":usize::MAX}]});
        let header = serde_json::to_vec(&value).unwrap();
        for kind in [RecordKind::Pane, RecordKind::Clipboard] {
            let bytes = wire(&header, kind, 0, 0, json_budget(&header).unwrap());
            assert!(Packet::read(&mut bytes.as_slice()).is_err());
        }
        // Under-reporting cells cannot shrink admission below the allocation
        // required by the actual frame, even with a valid JSON allowance.
        let packet = Packet::Pane(PaneUpdate {
            header: PaneHeader {
                frame: Some(crate::Frame {
                    cols: 1,
                    rows: 1,
                    ..Default::default()
                }),
                rows: vec![0],
                ..Default::default()
            },
            cells: vec![crate::Cell::default()],
        });
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
        let mut reader = Cursor::new(bytes);
        assert!(Packet::read(&mut reader).is_err());
        assert_eq!(
            reader.position(),
            (RECORD_PREFIX_BYTES
                + serde_json::to_vec(&packet.as_pane().unwrap().header)
                    .unwrap()
                    .len()) as u64
        );
        // Even a huge escape/unknown subtree cannot bypass the scanner.
        assert!(json_budget(format!("{}0{}", "[".repeat(65), "]".repeat(65)).as_bytes()).is_err());
    }
    #[test]
    fn strings_cannot_change_the_structural_allocation_bound() {
        let mut header = PaneHeader::default();
        let empty = serde_json::to_vec(&header).unwrap();
        let structural = json_budget(&empty).unwrap() - 4 * empty.len();
        // Exercise even/odd backslash runs, adjacent escaped quotes, all JSON
        // punctuation and Unicode. None of these string bytes are containers.
        for slashes in 0..128 {
            header.meta.title = format!(
                "{}\"{{[,:]}}\0é🙂{}",
                "\\".repeat(slashes),
                "\\\"".repeat(slashes)
            );
            let bytes = serde_json::to_vec(&header).unwrap();
            assert_eq!(json_budget(&bytes).unwrap() - 4 * bytes.len(), structural);
        }
    }

    #[test]
    fn clipboard_seed_checks_escaped_keys_lengths_and_duplicate_fields() {
        let json =
            r#"{"destination":"clipboard","\u0063ontent":[{"mime":"text/plain","len":4194304}]}"#;
        let bytes = wire(
            json.as_bytes(),
            RecordKind::Clipboard,
            0,
            0,
            json_budget(json.as_bytes()).unwrap(),
        );
        let error = Packet::read(&mut bytes.as_slice())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("clipboard payload length"), "{error}");
        #[cfg(feature = "async")]
        assert!(
            futures_lite::future::block_on(Packet::read_message_async(&mut bytes.as_slice()))
                .err()
                .unwrap()
                .to_string()
                .contains("clipboard payload length")
        );
        let json = br#"{"destination":"clipboard","content":null,"\u0063ontent":null}"#;
        let bytes = wire(
            json,
            RecordKind::Clipboard,
            0,
            0,
            json_budget(json).unwrap(),
        );
        assert!(
            Packet::read(&mut bytes.as_slice())
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate field")
        );
        let value = serde_json::to_value(PaneHeader::default()).unwrap();
        for field in ["meta", "scroll_serial", "rows", "notifications"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<PaneHeader>(missing).is_err());
        }
    }

    #[test]
    fn allocation_bound_covers_capacity_growth_and_schema_types() {
        // Vector structs and boxed nodes are charged at each container opener.
        assert!(4 * size_of::<super::super::Metadata>() < 2048);
        assert!(2 * size_of::<crate::model::ExecutionTab>() < 2048);
        assert!(4 * size_of::<crate::model::PaneTree>() < 2048);
        for count in [0, 1, 4, 5, 127, 128, 129, 1025] {
            let packet = Packet::Pane(PaneUpdate {
                header: PaneHeader {
                    notifications: vec![String::new(); count],
                    find: Some(crate::search::FindState {
                        highlights: vec![(1, 2, 0); count],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cells: vec![],
            });
            let mut bytes = Vec::new();
            packet.encode_into(&mut bytes).unwrap();
            let record = Record::decode(bytes[..RECORD_PREFIX_BYTES].try_into().unwrap()).unwrap();
            let packet = Packet::read(&mut bytes.as_slice()).unwrap();
            assert!(packet.memory_size() + 256 < record.decode_bytes());
        }
    }
}

#[cfg(test)]
mod benchmark {
    use super::*;
    use std::{hint::black_box, time::Instant};

    #[test]
    #[ignore = "decode-stage measurement; run alone in release mode"]
    fn header_decode_cost() {
        for (name, text, count, iterations) in [
            ("small", "interactive".into(), 1, 10000),
            ("plain", "x".repeat(8 * 1024 * 1024), 1, 64),
            ("escaped", "\\\"".repeat(2 * 1024 * 1024), 1, 32),
            ("strings", String::new(), 20000, 100),
        ] {
            let header = PaneHeader {
                notifications: vec![text; count],
                ..Default::default()
            };
            let bytes = serde_json::to_vec(&header).unwrap();
            let record = Record::new(
                RECORD_PREFIX_BYTES - 4 + bytes.len(),
                bytes.len(),
                RecordKind::Pane,
                json_budget(&bytes).unwrap(),
                0,
            )
            .unwrap();
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(json_budget(black_box(&bytes)).unwrap());
            }
            let scan = start.elapsed().as_nanos() / iterations;
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(serde_json::from_slice::<PaneHeader>(black_box(&bytes)).unwrap());
            }
            let typed = start.elapsed().as_nanos() / iterations;
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(record.parse_header(black_box(&bytes)).unwrap());
            }
            let full = start.elapsed().as_nanos() / iterations;
            println!(
                "{}",
                serde_json::json!({"case":name,"header_bytes":bytes.len(),"scan_ns":scan,"typed_ns":typed,"full_ns":full})
            );
        }
    }
}
