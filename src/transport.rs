//! Versioned viewport packets: JSON metadata followed by fixed-width cell rows.
use crate::{Cell, Frame};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
};

mod requests;
pub(crate) use requests::{PasteOptions, StreamCommand, StreamRequest};

pub(crate) const VERSION: u32 = 6;
pub(crate) const MAX_PASTE: usize = 4 * 1024 * 1024;
pub(crate) const PASTE_CHUNK: usize = 16 * 1024;
pub(crate) const MAX_PACKET: usize = 16 * 1024 * 1024;
const CELL_BYTES: usize = 71;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub id: u64,
    pub directory: PathBuf,
    pub title: String,
    pub size: (u16, u16),
    pub exited: bool,
    /// Sequence of the most recent unread alert; zero after acknowledgement.
    pub unread: u64,
    pub busy: bool,
    pub modes: [bool; 2],
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Header {
    pub meta: Metadata,
    pub frame: Option<Frame>,
    pub scroll_serial: u64,
    pub rows: Vec<u16>,
    pub notifications: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub find: Option<crate::search::FindState>,
    #[serde(default)]
    pub selection: Option<(u64, u64)>,
    #[serde(default)]
    pub copy: Option<String>,
}

pub(crate) struct Packet {
    pub header: Header,
    pub cells: Vec<Cell>,
}

impl Packet {
    pub(crate) fn memory_size(&self) -> usize {
        let header = &self.header;
        std::mem::size_of::<Self>()
            + self.cells.capacity() * std::mem::size_of::<Cell>()
            + header.meta.directory.capacity()
            + header.meta.title.capacity()
            + header.rows.capacity() * std::mem::size_of::<u16>()
            + header.notifications.capacity() * std::mem::size_of::<String>()
            + header
                .notifications
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + header.error.as_ref().map_or(0, String::capacity)
            + header.copy.as_ref().map_or(0, String::capacity)
            + header.find.as_ref().map_or(0, |find| {
                find.highlights.capacity() * std::mem::size_of::<(usize, usize, u8)>()
            })
    }

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

    pub fn read(reader: &mut impl Read) -> anyhow::Result<Self> {
        let mut size = [0; 4];
        reader.read_exact(&mut size)?;
        let size = u32::from_le_bytes(size) as usize;
        anyhow::ensure!(
            (4..=MAX_PACKET).contains(&size),
            "Invalid viewport packet size"
        );
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes)?;
        let header_size = u32::from_le_bytes(bytes[..4].try_into()?) as usize;
        anyhow::ensure!(header_size <= size - 4, "Invalid viewport header size");
        let header: Header = serde_json::from_slice(&bytes[4..4 + header_size])?;
        let data = &bytes[4 + header_size..];
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
            data.len() == expected * CELL_BYTES,
            "Invalid viewport cell data"
        );
        let cells = data
            .as_chunks::<CELL_BYTES>()
            .0
            .iter()
            .map(|bytes| Cell {
                text: bytes[..64].try_into().unwrap(),
                fg: bytes[64..67].try_into().unwrap(),
                bg: bytes[67..70].try_into().unwrap(),
                flags: bytes[70],
            })
            .collect();
        Ok(Self { header, cells })
    }
}

pub(crate) fn command_bytes(request: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(request)?;
    anyhow::ensure!(bytes.len() <= 1_048_576, "Frontend command is too large");
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn command(writer: &mut impl Write, request: &impl Serialize) -> anyhow::Result<()> {
    writer.write_all(&command_bytes(request)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet() -> Packet {
        let mut cell = Cell::default();
        cell.text[..5].copy_from_slice("界é".as_bytes());
        cell.fg = [1, 2, 3];
        cell.flags = 65;
        Packet {
            header: Header {
                meta: Metadata {
                    id: 1,
                    directory: PathBuf::from("/tmp"),
                    title: "shell".into(),
                    size: (2, 3),
                    exited: false,
                    unread: 0,
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
                notifications: vec!["alert".into()],
                error: None,
                find: None,
                selection: None,
                copy: None,
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
        let size = crate::incoming::packet_size(&oversized);
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
    fn malformed_packets_are_rejected_before_applying_rows() {
        let mut packet = packet();
        packet.header.rows = vec![3];
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        assert!(Packet::read(&mut &encoded[..]).is_err());
        packet.header.rows = vec![1, 1];
        encoded.clear();
        packet.encode_into(&mut encoded).unwrap();
        assert!(Packet::read(&mut &encoded[..]).is_err());
        assert!(Packet::read(&mut &u32::MAX.to_le_bytes()[..]).is_err());
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
