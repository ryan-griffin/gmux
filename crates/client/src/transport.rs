//! Versioned viewport packets: JSON metadata followed by fixed-width cell rows.
use crate::{Cell, Frame};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, ErrorKind, Write},
    path::PathBuf,
};

mod memory;
mod requests;
pub use requests::{PasteOptions, SelectionMode, StreamCommand, StreamRequest};

pub const VERSION: u32 = 22;
pub const MAX_PASTE: usize = 4 * 1024 * 1024;
pub const PASTE_CHUNK: usize = 16 * 1024;
pub const MAX_PACKET: usize = 16 * 1024 * 1024;
const CELL_BYTES: usize = 71;

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
pub struct Header {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::control::CommandReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitMetadata>,
    pub meta: Metadata,
    pub frame: Option<Frame>,
    pub scroll_serial: u64,
    pub rows: Vec<u16>,
    /// Flags corresponding to `rows`; empty means every changed row is unwrapped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_flags: Vec<u8>,
    pub notifications: Vec<String>,
    pub error: Option<String>,
    pub find: Option<crate::search::FindState>,
    pub selection: Option<(u64, u64)>,
    #[serde(default)]
    pub selection_serial: u64,
    #[serde(default)]
    pub selection_origin: Option<u64>,
    pub copy: Option<String>,
}

pub struct Packet {
    pub header: Header,
    pub cells: Vec<Cell>,
}

impl Packet {
    pub fn encode_into(&self, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
        let start = bytes.len();
        // Serialize into the reusable output buffer, then fill the lengths.
        // A failed packet must leave previously queued packets untouched.
        bytes.extend_from_slice(&[0; 8]);
        let result = (|| {
            serde_json::to_writer(&mut *bytes, &self.header)?;
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
            }
            Ok(())
        })();
        if result.is_err() {
            bytes.truncate(start);
        }
        result
    }

    /// Decode one packet, leaving any following packet in the reader's buffer.
    pub fn read(reader: &mut impl BufRead) -> anyhow::Result<Self> {
        let mut size = [0; 4];
        reader.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
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
        anyhow::ensure!(
            header.row_flags.is_empty()
                || (header.row_flags.len() == header.rows.len()
                    && header.row_flags.iter().all(|flags| flags & !1 == 0)),
            "Invalid viewport row flags"
        );
        let expected = if let Some(frame) = header.frame {
            let cols = frame.cols as usize;
            let rows = frame.rows as usize;
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
            header.rows.len() * cols
        } else {
            anyhow::ensure!(header.rows.is_empty(), "Rows require a frame");
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
        Ok(Self { header, cells })
    }
}

fn decode_cell(bytes: &[u8; CELL_BYTES]) -> Cell {
    Cell {
        text: bytes[..64].try_into().unwrap(),
        fg: bytes[64..67].try_into().unwrap(),
        bg: bytes[67..70].try_into().unwrap(),
        flags: bytes[70],
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
    use std::io::{self, BufReader, Read};
    fn packet() -> Packet {
        let mut cell = Cell::default();
        cell.text[..5].copy_from_slice("界é".as_bytes());
        cell.fg = [1, 2, 3];
        cell.flags = 65;
        Packet {
            header: Header {
                git: None,
                state: None,
                meta: Metadata {
                    id: 1,
                    directory: PathBuf::from("/tmp"),
                    title: "shell".into(),
                    size: (2, 3),
                    exited: false,
                    unread_sequence: 0,
                    busy: false,
                    modes: [false, true],
                },
                scroll_serial: 42,
                frame: Some(Frame {
                    cols: 2,
                    rows: 3,
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
                error: None,
                find: None,
                selection: None,
                copy: None,
                ..Header::default()
            },
            cells: vec![cell, Cell::default()],
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

        for size in [0, 3, MAX_PACKET as u32 + 1, u32::MAX] {
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
        packet.header.rows = vec![0, 2];
        packet.header.row_flags = vec![1, 0];
        packet.cells = (0..146)
            .map(|i| {
                let mut cell = Cell::default();
                cell.text[..8].copy_from_slice("界ée\u{301}".as_bytes());
                cell.fg = [i, 2, 3];
                cell.bg = [4, i, 6];
                cell.flags = i;
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
        for (capacity, chunk) in [(1, 1), (70, 13), (71, 71), (72, 137), (8192, 8192)] {
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
            let mut reader = BufReader::with_capacity(71, &encoded[..len]);
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
        packet.header.copy = Some("x".repeat(MAX_PACKET));
        assert!(packet.encode_into(&mut encoded).is_err());
        assert_eq!(encoded, original);
        let decoded = Packet::read(&mut &encoded[..]).unwrap();
        assert!(decoded.cells == packet.cells);
    }
}
