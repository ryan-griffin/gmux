//! Versioned viewport packets: JSON metadata followed by fixed-width cell rows.
use crate::{Cell, Frame};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, ErrorKind, Write},
    path::PathBuf,
};

mod memory;
mod requests;
pub use requests::{
    KeyAction, KeyLayout, PasteOptions, SelectionMode, StreamCommand, StreamRequest,
};

pub const VERSION: u32 = 35;
/// A zero-length record acknowledges a connection heartbeat without a viewport.
pub const HEARTBEAT_REPLY: &[u8; 4] = &[0; 4];
pub const MAX_PASTE: usize = 4 * 1024 * 1024;
pub const PASTE_CHUNK: usize = 16 * 1024;
pub const MAX_PACKET: usize = 16 * 1024 * 1024;
pub const CELL_BYTES: usize = 75;
pub const DEFAULT_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub id: u64,
    pub directory: PathBuf,
    pub title: String,
    pub size: (u16, u16),
    pub exited: bool,
    /// Sequence of the most recent unread alert; zero after acknowledgement.
    pub unread_sequence: u64,
    pub busy: bool,
    pub command: crate::shell::CommandState,
    pub progress: crate::progress::Progress,
    pub program_status: Option<crate::program_status::Summary>,
    pub modes: [bool; 2],
}

/// Optional metadata for a pane requested with git-subscribe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMetadata {
    pub id: u64,
    pub directory: PathBuf,
    pub branch: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CachedRows {
    pub start: u64,
    pub row_flags: Vec<u8>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Header {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::control::CommandReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitMetadata>,
    pub meta: Metadata,
    pub frame: Option<Frame>,
    pub scroll_serial: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub cache_origin: u64,
    /// Discard previously cached rows at or beyond this screen row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_valid_before: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub row_shift: i16,
    /// Extra history rows, stored after the viewport's changed cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachedRows>,
    /// Replacement map for this frame; absent keeps the displayed map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hyperlinks: Option<crate::hyperlinks::Hyperlinks>,
    pub rows: Vec<u16>,
    /// Flags corresponding to `rows`; empty means every changed row is unwrapped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_flags: Vec<u8>,
    pub notifications: Vec<String>,
    pub error: Option<String>,
    pub find: Option<crate::search::FindState>,
    pub selection: Option<crate::selection::Selection>,
    #[serde(default)]
    pub selection_serial: u64,
    #[serde(default)]
    pub selection_origin: Option<u64>,
    pub copy: Option<crate::clipboard::Write>,
}

fn is_zero(value: &i16) -> bool {
    *value == 0
}

pub struct Packet {
    pub header: Header,
    pub cells: Vec<Cell>,
}

/// JSON escaping can expand control text sixfold. Stop encoding at the packet
/// limit, before an oversized header can grow the frontend's transport buffer.
struct HeaderWriter<'a> {
    bytes: &'a mut Vec<u8>,
    end: usize,
}
impl Write for HeaderWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.end.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("Viewport header is too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Packet {
    pub fn encode_into(&self, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
        let start = bytes.len();
        // Serialize into the reusable output buffer, then fill the lengths.
        // A failed packet must leave previously queued packets untouched.
        bytes.extend_from_slice(&[0; 8]);
        let result = (|| {
            if self.header.copy.is_some() {
                self.encode_clipboard_header(bytes, start + 4 + MAX_PACKET)?;
            } else {
                // Keep ordinary viewport traffic on serde's direct Vec writer.
                serde_json::to_writer(&mut *bytes, &self.header)?;
            }
            let header_size = bytes.len() - start - 8;
            let size = 4 + header_size + self.cells.len() * CELL_BYTES;
            anyhow::ensure!(size <= MAX_PACKET, "Viewport packet is too large");
            bytes[start..start + 4].copy_from_slice(&(size as u32).to_le_bytes());
            bytes[start + 4..start + 8].copy_from_slice(&(header_size as u32).to_le_bytes());
            bytes.reserve(self.cells.len() * CELL_BYTES);
            for cell in &self.cells {
                bytes.extend_from_slice(&cell.text);
                bytes.extend_from_slice(&cell.fg);
                bytes.extend_from_slice(&cell.bg);
                bytes.push(cell.flags);
                bytes.push(cell.decoration);
                bytes.extend_from_slice(&cell.underline_color);
            }
            Ok(())
        })();
        if result.is_err() {
            bytes.truncate(start);
        }
        result
    }

    #[cold]
    fn encode_clipboard_header(&self, bytes: &mut Vec<u8>, end: usize) -> anyhow::Result<()> {
        self.header.copy.as_ref().unwrap().validate()?;
        serde_json::to_writer(HeaderWriter { bytes, end }, &self.header)?;
        Ok(())
    }

    /// Decode one packet, skipping heartbeat replies and leaving following data buffered.
    pub fn read(reader: &mut impl BufRead) -> anyhow::Result<Self> {
        loop {
            if let Some(packet) = Self::read_message(reader)? {
                return Ok(packet);
            }
        }
    }

    /// Read a viewport packet or a heartbeat acknowledgement, represented by None.
    pub fn read_message(reader: &mut impl BufRead) -> anyhow::Result<Option<Self>> {
        let mut size = [0; 4];
        reader.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        if size == 0 {
            return Ok(None);
        }
        anyhow::ensure!(
            (4..=MAX_PACKET).contains(&size),
            "Invalid viewport packet size"
        );
        let mut header_size = [0; 4];
        reader.read_exact(&mut header_size)?;
        let header_size = u32::from_le_bytes(header_size) as usize;
        anyhow::ensure!(header_size <= size - 4, "Invalid viewport header size");
        let header = {
            let mut bytes = vec![0; header_size];
            reader.read_exact(&mut bytes)?;
            serde_json::from_slice::<Header>(&bytes)?
        };
        if let Some(write) = &header.copy {
            write.validate()?;
        }
        if let Some(end) = header.cache_valid_before {
            anyhow::ensure!(
                header.generation != 0
                    && header.cache_origin.checked_add(end).is_some()
                    && header
                        .frame
                        .is_none_or(|frame| end <= frame.scrollback.total),
                "Invalid history cache boundary"
            );
        }
        anyhow::ensure!(
            header.row_flags.is_empty()
                || (header.row_flags.len() == header.rows.len()
                    && header.row_flags.iter().all(|flags| flags & !1 == 0)),
            "Invalid viewport row flags"
        );
        anyhow::ensure!(
            header.hyperlinks.is_none() || header.frame.is_some(),
            "Hyperlinks require a frame"
        );
        let expected = if let Some(frame) = header.frame {
            let cols = frame.cols as usize;
            let rows = frame.rows as usize;
            anyhow::ensure!(
                header
                    .cache_origin
                    .checked_add(frame.scrollback.total)
                    .is_some(),
                "Invalid history origin"
            );
            anyhow::ensure!(
                crate::viewport::valid(cols as u64, rows as u64),
                "Invalid viewport dimensions"
            );
            anyhow::ensure!(
                header.rows.len() <= rows
                    && header.rows.windows(2).all(|r| r[0] < r[1])
                    && header.rows.iter().all(|r| (*r as usize) < rows),
                "Invalid viewport row indices"
            );
            anyhow::ensure!(
                (header.row_shift.unsigned_abs() as usize) < rows,
                "Invalid viewport row shift"
            );
            let exposed = if header.row_shift > 0 {
                rows - header.row_shift as usize..rows
            } else {
                0..header.row_shift.unsigned_abs() as usize
            };
            anyhow::ensure!(
                exposed
                    .into_iter()
                    .all(|row| header.rows.binary_search(&(row as u16)).is_ok()),
                "Missing exposed viewport rows"
            );
            let cached = if let Some(cache) = &header.cache {
                anyhow::ensure!(
                    cache.row_flags.len() <= crate::scroll_cache::capacity(cols)
                        && cache.row_flags.iter().all(|flag| flag & !1 == 0)
                        && cache.start <= frame.scrollback.total
                        && cache.row_flags.len() as u64 <= frame.scrollback.total - cache.start,
                    "Invalid cached rows"
                );
                cache.row_flags.len()
            } else {
                0
            };
            if let Some(links) = &header.hyperlinks {
                links.validate(cols * rows)?;
            }
            (header.rows.len() + cached) * cols
        } else {
            anyhow::ensure!(
                header.rows.is_empty() && header.cache.is_none() && header.row_shift == 0,
                "Rows require a frame"
            );
            0
        };
        anyhow::ensure!(
            size - 4 - header_size == expected * CELL_BYTES,
            "Invalid viewport cell data"
        );
        let mut cells = Vec::with_capacity(expected);
        while cells.len() < expected {
            let bytes = match reader.fill_buf() {
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                result => result?,
            };
            let count = (bytes.len() / CELL_BYTES).min(expected - cells.len());
            if count == 0 {
                // A cell can straddle buffered reads. read_exact also handles
                // short reads, interrupted reads, and EOF within this cell.
                let mut bytes = [0; CELL_BYTES];
                reader.read_exact(&mut bytes)?;
                cells.push(decode_cell(&bytes));
            } else {
                // Decode from the caller's buffer without copying the payload
                // into another allocation or consuming the following packet.
                cells.extend(
                    bytes[..count * CELL_BYTES]
                        .as_chunks::<CELL_BYTES>()
                        .0
                        .iter()
                        .map(decode_cell),
                );
                reader.consume(count * CELL_BYTES);
            }
        }
        Ok(Some(Self { header, cells }))
    }
}

fn decode_cell(bytes: &[u8; CELL_BYTES]) -> Cell {
    Cell {
        text: bytes[..64].try_into().unwrap(),
        fg: bytes[64..67].try_into().unwrap(),
        bg: bytes[67..70].try_into().unwrap(),
        flags: bytes[70],
        decoration: bytes[71],
        underline_color: bytes[72..75].try_into().unwrap(),
    }
}

pub fn command_bytes(request: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(request)?;
    anyhow::ensure!(bytes.len() <= 1_048_576, "Frontend command is too large");
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn command(writer: &mut impl Write, request: &impl Serialize) -> anyhow::Result<()> {
    writer.write_all(&command_bytes(request)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::{decoration, mouse_shape, underline};
    use std::io::{self, BufReader, Read};
    fn packet() -> Packet {
        let mut cell = Cell::default();
        cell.text[..5].copy_from_slice("界é".as_bytes());
        cell.fg = [1, 2, 3];
        cell.flags = 65 | crate::cells::flags::BLINK;
        cell.decoration = underline::CURLY | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
        cell.underline_color = [6, 7, 8];
        Packet {
            header: Header {
                meta: Metadata {
                    id: 1,
                    directory: PathBuf::from("/tmp"),
                    title: "shell".into(),
                    size: (2, 3),
                    exited: false,
                    unread_sequence: 0,
                    busy: false,
                    command: Default::default(),
                    progress: Default::default(),
                    program_status: None,
                    modes: [false, true],
                },
                scroll_serial: 42,
                frame: Some(Frame {
                    cols: 2,
                    rows: 3,
                    cursor_blink: 1,
                    mouse_shape: mouse_shape::ZOOM_OUT,
                    scrollback: crate::Scrollback {
                        total: 500,
                        offset: 200,
                        len: 3,
                    },
                    ..Frame::default()
                }),
                rows: vec![1],
                row_flags: vec![1],
                notifications: vec!["alert".into()],
                ..Header::default()
            },
            cells: vec![cell, Cell::default()],
        }
    }
    #[test]
    fn cached_rows_round_trip_and_invalid_scroll_storage_is_rejected() {
        let mut packet = packet();
        packet.header.generation = 1;
        packet.header.cache_origin = 42;
        packet.header.cache_valid_before = Some(200);
        packet.header.row_shift = -1;
        packet.header.rows = vec![0];
        packet.header.cache = Some(CachedRows {
            start: 196,
            row_flags: vec![0, 1, 0],
        });
        packet.cells.resize(8, Cell::default());
        packet.cells[2] = packet.cells[0];
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut &bytes[..]).unwrap();
        assert_eq!(decoded.cells.len(), 8);
        assert!(decoded.cells[0].blink() && decoded.cells[2].blink());
        assert_eq!(decoded.header.cache_origin, 42);
        assert_eq!(decoded.header.cache_valid_before, Some(200));
        assert_eq!(decoded.header.cache.unwrap().row_flags, [0, 1, 0]);
        for (shift, start, flags) in [
            (3, 196, vec![0]),
            (-3, 196, vec![0]),
            (0, u64::MAX, vec![0]),
            (0, 499, vec![0, 0]),
            (0, 0, vec![2]),
            (0, 0, vec![0; crate::scroll_cache::MAX_ROWS + 1]),
        ] {
            packet.header.row_shift = shift;
            packet.header.cache = Some(CachedRows {
                start,
                row_flags: flags,
            });
            bytes.clear();
            packet.encode_into(&mut bytes).unwrap();
            assert!(Packet::read(&mut &bytes[..]).is_err());
        }
    }

    #[test]
    fn binary_rows_round_trip_without_losing_cell_flags_or_unicode() {
        let packet = packet();
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        let decoded = Packet::read(&mut &encoded[..]).unwrap();
        assert_eq!(decoded.header.meta, packet.header.meta);
        assert_eq!(decoded.header.rows, vec![1]);
        assert_eq!(decoded.header.row_flags, vec![1]);
        assert_eq!(decoded.header.scroll_serial, 42);
        assert_eq!(
            decoded.header.frame.unwrap().scrollback,
            packet.header.frame.unwrap().scrollback
        );
        assert!(decoded.cells == packet.cells);
        assert_eq!(decoded.header.notifications, vec!["alert"]);
        packet.encode_into(&mut encoded).unwrap();
        let mut reader = &encoded[..];
        for _ in 0..2 {
            assert!(Packet::read(&mut reader).unwrap().cells == packet.cells);
        }
        assert!(reader.is_empty());
    }
    #[test]
    fn incoming_budget_counts_retained_capacity_and_rejects_oversized_packets() {
        let mut oversized = packet();
        oversized.cells.reserve_exact(4096);
        let mut encoded = Vec::new();
        oversized.encode_into(&mut encoded).unwrap();
        let size = oversized.memory_size();
        assert!(size > encoded.len());
        let (sender, receiver) = crate::incoming::channel(size - 1, 8);
        assert!(
            sender
                .send(Ok(oversized))
                .unwrap_err()
                .to_string()
                .contains("memory budget")
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        sender.send(Ok(packet())).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Ok(packet)) if packet.header.scroll_serial == 42));
    }
    #[test]
    fn heartbeat_records_are_separate_from_viewports_and_do_not_consume_following_data() {
        let mut bytes = HEARTBEAT_REPLY.to_vec();
        packet().encode_into(&mut bytes).unwrap();
        bytes.extend_from_slice(HEARTBEAT_REPLY);
        let mut reader = &bytes[..];
        assert!(Packet::read_message(&mut reader).unwrap().is_none());
        assert_eq!(
            Packet::read_message(&mut reader)
                .unwrap()
                .unwrap()
                .header
                .scroll_serial,
            42
        );
        assert!(Packet::read_message(&mut reader).unwrap().is_none());
        assert!(reader.is_empty());
        let mut reader = &bytes[..];
        assert_eq!(Packet::read(&mut reader).unwrap().header.scroll_serial, 42);
        assert_eq!(reader, HEARTBEAT_REPLY);
    }

    #[test]
    fn progress_is_required_and_validated_in_stream_and_control_metadata() {
        use serde_json::json;
        let mut header = serde_json::to_value(Header::default()).unwrap();
        for progress in [
            json!({"state":"error","percentage":101}),
            json!({"state":2,"percentage":0}),
            json!({"state":"inactive","percentage":42}),
            json!({"state":"paused"}),
            serde_json::Value::Null,
        ] {
            header["meta"]["progress"] = progress;
            let json = serde_json::to_vec(&header).unwrap();
            let mut bytes = ((json.len() + 4) as u32).to_le_bytes().to_vec();
            bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&json);
            assert!(Packet::read(&mut bytes.as_slice()).is_err());
            assert!(serde_json::from_value::<Metadata>(header["meta"].clone()).is_err());
        }
        header["meta"].as_object_mut().unwrap().remove("progress");
        assert!(serde_json::from_value::<Header>(header).is_err());
    }

    #[test]
    fn malformed_headers_and_payload_lengths_are_rejected() {
        let mut packet = packet();
        for flags in [vec![0, 1], vec![2]] {
            packet.header.row_flags = flags;
            let mut encoded = Vec::new();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut &encoded[..]).is_err());
        }
        packet.header.row_flags.clear();
        let mut encoded = Vec::new();
        for rows in [vec![3], vec![1, 1], vec![2, 1], vec![0, 1, 2, 3]] {
            packet.cells.resize(rows.len() * 2, Cell::default());
            packet.header.rows = rows;
            encoded.clear();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        // Keep the payload valid so only the grid limits can reject these frames.
        packet.header.rows.clear();
        packet.cells.clear();
        for (cols, rows) in [(0, 3), (2, 0), (1001, 3), (2, 1001), (512, 257)] {
            let frame = packet.header.frame.as_mut().unwrap();
            frame.cols = cols;
            frame.rows = rows;
            encoded.clear();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        packet = self::packet();
        packet.header.frame = None;
        packet.cells.clear();
        encoded.clear();
        packet.encode_into(&mut encoded).unwrap();
        assert!(Packet::read(&mut encoded.as_slice()).is_err());

        for size in [3, MAX_PACKET as u32 + 1, u32::MAX] {
            let mut bytes = size.to_le_bytes().to_vec();
            bytes.extend_from_slice(&encoded);
            let mut reader = io::Cursor::new(bytes);
            assert!(Packet::read(&mut reader).is_err());
            assert_eq!(reader.position(), 4, "read beyond an invalid packet size");
        }
        // A claimed header that exceeds the packet must fail before reading it.
        let mut bytes = vec![4, 0, 0, 0, 1, 0, 0, 0];
        bytes.extend_from_slice(&encoded);
        let mut reader = io::Cursor::new(bytes);
        assert!(Packet::read(&mut reader).is_err());
        assert_eq!(reader.position(), 8, "read beyond an invalid header size");

        packet = self::packet();
        encoded.clear();
        packet.encode_into(&mut encoded).unwrap();
        let size = u32::from_le_bytes(encoded[..4].try_into().unwrap());
        for claimed_size in [size - 1, size + 1, size - CELL_BYTES as u32] {
            encoded[..4].copy_from_slice(&claimed_size.to_le_bytes());
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        encoded[..4].copy_from_slice(&size.to_le_bytes());
        encoded[8] = b'!';
        assert!(Packet::read(&mut encoded.as_slice()).is_err());
    }

    #[test]
    fn malformed_link_indexes_ranges_and_budgets_are_rejected_before_cells() {
        use crate::hyperlinks::{Hyperlinks, MAX_SPANS, MAX_TARGET_BYTES, MAX_URI_BYTES};
        let mut packet = packet();
        for spans in [
            vec![[0, 0, 0]],
            vec![[2, 1, 0]],
            vec![[0, 7, 0]],
            vec![[0, 2, 1]],
            vec![[1, 3, 0], [2, 4, 0]],
            vec![[3, 4, 0], [0, 1, 0]],
            vec![[0, 1, 0]; MAX_SPANS + 1],
        ] {
            packet.header.hyperlinks = Some(Hyperlinks {
                targets: vec!["test:target".into()],
                spans,
            });
            let mut bytes = Vec::new();
            packet.encode_into(&mut bytes).unwrap();
            assert!(Packet::read(&mut bytes.as_slice()).is_err());
        }
        for targets in [
            vec![String::new()],
            vec!["test:bad\nuri".into()],
            vec!["x".repeat(MAX_URI_BYTES + 1)],
            vec!["x".repeat(MAX_URI_BYTES); MAX_TARGET_BYTES / MAX_URI_BYTES + 1],
        ] {
            packet.header.hyperlinks = Some(Hyperlinks {
                spans: (0..targets.len())
                    .map(|i| [i as u32, i as u32 + 1, i as u32])
                    .collect(),
                targets,
            });
            let mut bytes = Vec::new();
            packet.encode_into(&mut bytes).unwrap();
            assert!(Packet::read(&mut bytes.as_slice()).is_err());
        }
        packet.header.hyperlinks = Some(Hyperlinks {
            targets: vec!["test:actual".into()],
            spans: vec![[0, 6, 0]],
        });
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.header.hyperlinks, packet.header.hyperlinks);
        packet.header.frame = None;
        packet.header.rows.clear();
        packet.header.row_flags.clear();
        packet.cells.clear();
        bytes.clear();
        packet.encode_into(&mut bytes).unwrap();
        assert!(Packet::read(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn link_ranges_address_the_viewport_and_not_appended_cached_rows() {
        use crate::hyperlinks::Hyperlinks;
        let mut packet = packet();
        packet.header.generation = 1;
        packet.header.cache = Some(CachedRows {
            start: 0,
            row_flags: vec![0; 10],
        });
        packet.cells.extend(vec![Cell::default(); 20]);
        packet.header.hyperlinks = Some(Hyperlinks {
            targets: vec!["test:viewport".into()],
            spans: vec![[0, 6, 0]],
        });
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.cells.len(), 22);
        assert_eq!(
            decoded.header.hyperlinks.unwrap().target_at(5),
            Some("test:viewport")
        );
        packet.header.hyperlinks.as_mut().unwrap().spans[0][1] = 7;
        bytes.clear();
        packet.encode_into(&mut bytes).unwrap();
        assert!(Packet::read(&mut bytes.as_slice()).is_err());
    }

    struct Fragmented<'a> {
        bytes: &'a [u8],
        chunk: usize,
        reads: usize,
    }
    impl Read for Fragmented<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads.is_multiple_of(5) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let len = out.len().min(self.chunk);
            self.bytes.read(&mut out[..len])
        }
    }

    #[test]
    fn fragmented_and_interrupted_reads_preserve_cells_and_following_packets() {
        let mut packet = packet();
        packet.header.frame.as_mut().unwrap().cols = 73;
        packet.header.frame.as_mut().unwrap().mouse_shape = u8::MAX;
        packet.header.rows = vec![0, 2];
        packet.header.row_flags = vec![1, 0];
        packet.cells = (0..146)
            .map(|i| {
                let mut cell = Cell::default();
                cell.text[..8].copy_from_slice("界ée\u{301}".as_bytes());
                cell.fg = [i, 2, 3];
                cell.bg = [4, i, 6];
                cell.flags = i;
                cell.decoration = i | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
                cell.underline_color = [i, 20, 30];
                cell
            })
            .collect();
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        // Put an empty delta between two payloads to catch packet overconsumption.
        let unchanged = Packet {
            header: Header::default(),
            cells: vec![],
        };
        unchanged.encode_into(&mut encoded).unwrap();
        packet.encode_into(&mut encoded).unwrap();
        for (capacity, chunk) in [
            (1, 1),
            (CELL_BYTES - 1, 13),
            (CELL_BYTES, CELL_BYTES),
            (CELL_BYTES + 1, 137),
            (8192, 8192),
        ] {
            let mut reader = BufReader::with_capacity(
                capacity,
                Fragmented {
                    bytes: &encoded,
                    chunk,
                    reads: 0,
                },
            );
            for index in 0..3 {
                let decoded = Packet::read(&mut reader).unwrap();
                if index == 1 {
                    assert!(decoded.cells.is_empty());
                    assert!(decoded.header.frame.is_none());
                } else {
                    assert!(decoded.cells == packet.cells);
                    assert!(decoded.header.frame == packet.header.frame);
                    assert_eq!(decoded.header.rows, packet.header.rows);
                    assert_eq!(decoded.header.row_flags, packet.header.row_flags);
                    assert_eq!(decoded.header.meta, packet.header.meta);
                }
            }
            let mut trailing = Vec::new();
            reader.read_to_end(&mut trailing).unwrap();
            assert!(trailing.is_empty());
        }
    }

    #[test]
    fn truncated_packets_fail_at_every_header_and_cell_boundary() {
        let mut encoded = Vec::new();
        packet().encode_into(&mut encoded).unwrap();
        for len in 0..encoded.len() {
            let mut reader = BufReader::with_capacity(CELL_BYTES, &encoded[..len]);
            assert!(Packet::read(&mut reader).is_err(), "accepted {len} bytes");
        }
    }
    #[test]
    fn failed_encoding_preserves_previously_queued_packets() {
        use std::os::unix::ffi::OsStringExt;
        let mut packet = packet();
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        let original = encoded.clone();
        packet.header.meta.directory = std::ffi::OsString::from_vec(vec![0xff]).into();
        assert!(packet.encode_into(&mut encoded).is_err());
        assert_eq!(encoded, original);

        packet.header.meta.directory = "/tmp".into();
        packet.header.copy = Some(crate::clipboard::Write {
            destination: crate::clipboard::Destination::Clipboard,
            content: Some("x".repeat(MAX_PACKET).into()),
        });
        assert!(packet.encode_into(&mut encoded).is_err());
        assert_eq!(encoded, original);
        let decoded = Packet::read(&mut &encoded[..]).unwrap();
        assert!(decoded.cells == packet.cells);
    }

    #[test]
    fn rich_clipboard_limits_and_json_fallback_preserve_the_plain_offer() {
        use crate::clipboard::{Content, Destination, MAX_HTML, Write};
        let mut content = Content {
            text: "\"".repeat(MAX_PASTE),
            html: Some("\\".repeat(MAX_HTML)),
        };
        content.bound_html();
        assert!(content.html.is_none());
        let mut packet = Packet {
            header: Header {
                copy: Some(Write {
                    destination: Destination::Clipboard,
                    content: Some(content),
                }),
                ..Header::default()
            },
            cells: vec![],
        };
        let mut wire = vec![];
        packet.encode_into(&mut wire).unwrap();
        let decoded = Packet::read(&mut &wire[..]).unwrap();
        assert_eq!(decoded.header.copy, packet.header.copy);
        for html in ["x".repeat(MAX_HTML + 1), "bad\0html".into()] {
            packet
                .header
                .copy
                .as_mut()
                .unwrap()
                .content
                .as_mut()
                .unwrap()
                .html = Some(html);
            wire.clear();
            assert!(packet.encode_into(&mut wire).is_err());
            assert!(wire.is_empty());
            let header = serde_json::to_vec(&packet.header).unwrap();
            wire.extend_from_slice(&((header.len() + 4) as u32).to_le_bytes());
            wire.extend_from_slice(&(header.len() as u32).to_le_bytes());
            wire.extend_from_slice(&header);
            assert!(Packet::read(&mut &wire[..]).is_err());
        }
    }

    #[test]
    fn clipboard_text_limits_apply_to_decode_and_bound_json_escape_growth() {
        use crate::clipboard::{Destination, Write};
        let mut packet = Packet {
            header: Header {
                copy: Some(Write {
                    destination: Destination::Clipboard,
                    content: Some("\u{1}".repeat(MAX_PASTE).into()),
                }),
                ..Header::default()
            },
            cells: vec![],
        };
        let mut encoded = vec![0; 16];
        assert!(packet.encode_into(&mut encoded).is_err());
        assert_eq!(encoded, [0; 16]);
        assert!(encoded.capacity() <= 2 * (MAX_PACKET + 20));
        for text in ["x".repeat(MAX_PASTE + 1), "has\0nul".into()] {
            packet.header.copy.as_mut().unwrap().content = Some(text.into());
            // Bypass the encoder's checks to test untrusted remote packets.
            let header = serde_json::to_vec(&packet.header).unwrap();
            let mut wire = ((header.len() + 4) as u32).to_le_bytes().to_vec();
            wire.extend_from_slice(&(header.len() as u32).to_le_bytes());
            wire.extend_from_slice(&header);
            assert!(Packet::read(&mut &wire[..]).is_err());
        }
    }
}
