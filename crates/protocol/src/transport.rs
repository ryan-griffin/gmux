//! Typed stream events: kind-selected JSON followed by cell rows, image pixels, or clipboard bytes.
use crate::{Cell, Frame};
#[cfg(feature = "async")]
use futures_lite::io::{AsyncBufReadExt, AsyncReadExt};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, ErrorKind, Read, Write},
    path::PathBuf,
};

mod memory;
mod record;
pub use record::{MAX_DECODE_BYTES, RECORD_PREFIX_BYTES, Record, RecordKind};
mod requests;
pub use requests::{
    KeyAction, KeyLayout, PasteOptions, SelectionMode, StreamCommand, StreamRequest,
};

pub const VERSION: u32 = 48;
/// A zero-length record acknowledges a connection heartbeat without a viewport.
pub const HEARTBEAT_REPLY: &[u8; 4] = &[0; 4];
pub const MAX_PASTE: usize = 4 * 1024 * 1024;
pub const PASTE_CHUNK: usize = 16 * 1024;
pub const MAX_PACKET: usize = 64 * 1024 * 1024;
/// Per-cell prefix: UTF-8 length, colors and rendition. Text follows the prefix.
pub const CELL_PREFIX_BYTES: usize = 13;
/// Maximum supplemental history rows in one viewport packet.
pub const MAX_CACHE_ROWS: usize = 512;
/// Cell-data bytes reserved for supplemental history, excluding JSON metadata.
/// Admission reserves the maximum encoded grapheme length for every cell, even
/// when the actual text is shorter. This also bounds allocations before decoding.
pub const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
pub const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const MAX_IMAGE_TRANSFER_TIME: std::time::Duration =
    RESPONSE_TIMEOUT.saturating_add(image_transfer_allowance(crate::graphics::MAX_PIXEL_BYTES));

/// Extra time for image payloads to arrive at 128 KiB/s.
pub const fn image_transfer_allowance(bytes: usize) -> std::time::Duration {
    std::time::Duration::from_millis(bytes as u64 * 1000 / (128 * 1024))
}

/// Wire row limit for a supported width. Independent of Rust cell layout and
/// any client's choice of history-cache storage or retention budget.
pub fn cache_row_limit(cols: usize) -> usize {
    if !(1..=crate::viewport::MAX_AXIS).contains(&cols) {
        return 0;
    }
    (MAX_CACHE_BYTES / (cols * (CELL_PREFIX_BYTES + crate::cells::MAX_GRAPHEME_BYTES)))
        .min(MAX_CACHE_ROWS)
}

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct PaneHeader {
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
    /// Glyph state delivered with this viewport.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glyphs: Option<crate::glyphs::GlyphUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graphics: Option<crate::graphics::GraphicsUpdate>,
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
}

fn is_zero(value: &i16) -> bool {
    *value == 0
}

pub struct PaneUpdate {
    pub header: PaneHeader,
    pub cells: Vec<Cell>,
}

/// One framed stream event. Only pane updates can carry terminal cells.
// Pane updates dominate traffic. Keep their metadata inline to avoid an extra
// allocation on every update; retained accounting charges the full enum size
// for every variant. The stream-codec probe measures this choice.
#[allow(clippy::large_enum_variant)]
pub enum Packet {
    Pane(PaneUpdate),
    Execution(crate::control::CommandReply),
    Git(GitMetadata),
    Clipboard(crate::clipboard::Write),
    ClipboardRead(ClipboardRead),
    WindowResize(WindowResize),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowResize {
    pub pane: u64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardRead {
    pub pane: u64,
    pub event: crate::clipboard::ReadEvent,
}

/// Bound metadata while serializing, before growing the daemon output queue.
struct HeaderWriter<'a> {
    bytes: &'a mut Vec<u8>,
    end: usize,
}
impl Write for HeaderWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.end.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("Stream event header is too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Keep validation and allocation behavior identical for blocking and async I/O.
macro_rules! packet_reader {
    ($name:ident, $body:ident, [$($async:tt)*], $reader:ty, [$($wait:tt)*], $take:path, [$($yield:tt)*]) => {
        /// Read a stream event or a heartbeat acknowledgement, represented by None.
        pub $($async)* fn $name(reader: &mut $reader) -> anyhow::Result<Option<Self>> {
            let mut prefix = [0; RECORD_PREFIX_BYTES];
            reader.read_exact(&mut prefix[..4])$($wait)*?;
            if Record::is_heartbeat(prefix[..4].try_into().unwrap())? { return Ok(None); }
            reader.read_exact(&mut prefix[4..])$($wait)*?;
            let record = Record::decode(prefix)?;
            let mut bytes = vec![0; record.header_size()];
            reader.read_exact(&mut bytes)$($wait)*?;
            let packet = record.parse_header(&bytes)?;
            drop(bytes);
            let packet = Self::$body(reader, record, packet)$($wait)*?;
            record.validate_packet(&packet)?;
            Ok(Some(packet))
        }

        pub $($async)* fn $body(reader: &mut $reader, record: Record, mut packet: Packet) -> anyhow::Result<Self> {
            anyhow::ensure!(packet.kind() == record.kind(), "Incorrect record kind");
            let payload = record.payload_size();
            let update = match &mut packet {
                Packet::Clipboard(write) => {
                    anyhow::ensure!(
                        write.content.as_ref().map_or(0, |content| content.byte_len())
                            == payload,
                        "Invalid clipboard payload length"
                    );
                    if let Some(content) = &mut write.content {
                        for representation in &mut content.0 {
                            reader.read_exact(&mut representation.data)$($wait)*?;
                        }
                    }
                    return Ok(packet);
                }
                Packet::Pane(update) => update,
                _ => return Ok(packet),
            };
            let header = &update.header;
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
                        cache.row_flags.len() <= cache_row_limit(cols)
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
                if let Some(glyphs) = &header.glyphs {
                    glyphs.validate()?;
                }
                (header.rows.len() + cached) * cols
            } else {
                anyhow::ensure!(
                    header.rows.is_empty() && header.cache.is_none() && header.row_shift == 0
                        && header.glyphs.is_none() && header.graphics.is_none(),
                    "Rows, glyphs, and graphics require a frame"
                );
                0
            };
            anyhow::ensure!(expected == record.cell_count(), "Incorrect cell count");
            let image_bytes = header.graphics.as_ref().map_or(Ok(0), |graphics| graphics.payload_len())?;
            let cell_bytes = payload.checked_sub(image_bytes)
                .ok_or_else(|| anyhow::anyhow!("Invalid terminal image payload length"))?;
            anyhow::ensure!(
                (expected * CELL_PREFIX_BYTES
                    ..=expected * (CELL_PREFIX_BYTES + crate::cells::MAX_GRAPHEME_BYTES))
                    .contains(&cell_bytes),
                "Invalid viewport cell data"
            );
            // Bound reads to this record even when a length claims the next packet.
            let mut cell_reader = $take(&mut *reader, cell_bytes as u64);
            let mut remaining = cell_bytes;
            let mut cells = Vec::with_capacity(expected);
            while cells.len() < expected {
                $($yield)*
                let bytes = match cell_reader.fill_buf()$($wait)* {
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    result => result?,
                };
                // Bound synchronous work even for readers exposing a whole
                // record at once. Client sockets normally expose only 8 KiB.
                let bytes = &bytes[..bytes.len().min(8192)];
                let mut consumed = 0;
                while bytes.len() - consumed >= CELL_PREFIX_BYTES && cells.len() < expected {
                    let len = cell_length(&bytes[consumed..])?;
                    if bytes.len() - consumed < len {
                        break;
                    }
                    cells.push(decode_cell(&bytes[consumed..consumed + len])?);
                    consumed += len;
                }
                if consumed != 0 {
                    remaining -= consumed;
                    cell_reader.consume(consumed);
                    continue;
                }
                let mut bytes = [0; CELL_PREFIX_BYTES + crate::cells::MAX_GRAPHEME_BYTES];
                cell_reader.read_exact(&mut bytes[..CELL_PREFIX_BYTES])$($wait)*?;
                let len = cell_length(&bytes)?;
                cell_reader.read_exact(&mut bytes[CELL_PREFIX_BYTES..len])$($wait)*?;
                remaining -= len;
                cells.push(decode_cell(&bytes[..len])?);
            }
            anyhow::ensure!(remaining == 0, "Trailing viewport cell data");
            update.cells = cells;
            if let Some(graphics) = &mut update.header.graphics {
                for image in &mut graphics.images {
                    let image = std::sync::Arc::get_mut(image)
                        .ok_or_else(|| anyhow::anyhow!("Shared terminal image before decoding"))?;
                    image.pixels = vec![0; image.pixel_len()?];
                    for chunk in image.pixels.chunks_mut(64 * 1024) {
                        $($yield)*
                        reader.read_exact(chunk)$($wait)*?;
                    }
                }
            }
            Ok(packet)
        }
    };
}

impl Packet {
    pub fn encode_into(&self, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
        self.encode_into_limit(bytes, MAX_PACKET + 4)
    }

    /// Limit the complete record before copying clipboard payloads to the queue.
    pub fn encode_into_limit(&self, bytes: &mut Vec<u8>, limit: usize) -> anyhow::Result<()> {
        self.encode_record(bytes, limit, true)
    }

    /// Encode a record prefix and return its image payloads in wire order.
    pub fn encode_into_parts(
        mut self,
        bytes: &mut Vec<u8>,
        limit: usize,
    ) -> anyhow::Result<Vec<std::sync::Arc<crate::graphics::Image>>> {
        self.encode_record(bytes, limit, false)?;
        Ok(self
            .as_pane_mut()
            .and_then(|pane| pane.header.graphics.as_mut())
            .map_or_else(Vec::new, |graphics| std::mem::take(&mut graphics.images)))
    }

    fn encode_record(
        &self,
        bytes: &mut Vec<u8>,
        limit: usize,
        copy_images: bool,
    ) -> anyhow::Result<()> {
        let start = bytes.len();
        let limit = limit.min(MAX_PACKET + 4);
        let (cells, payload) = match self {
            Self::ClipboardRead(read) => {
                read.event.validate()?;
                (&[][..], 0)
            }
            Self::Clipboard(write) => {
                let payload = write.content.as_ref().map_or(0, |c| c.byte_len());
                // Reject backpressure before scanning text or growing the queue.
                anyhow::ensure!(
                    RECORD_PREFIX_BYTES + payload <= limit,
                    "Frontend is too slow"
                );
                write.validate()?;
                (&[][..], payload)
            }
            Self::Pane(update) => {
                let payload = if let Some(graphics) = &update.header.graphics {
                    let len = graphics.payload_len()?;
                    for image in &graphics.images {
                        anyhow::ensure!(
                            image.pixels.len() == image.pixel_len()?,
                            "Invalid terminal image pixels"
                        );
                    }
                    len
                } else {
                    0
                };
                (update.cells.as_slice(), payload)
            }
            _ => (&[][..], 0),
        };
        anyhow::ensure!(
            limit >= RECORD_PREFIX_BYTES + payload,
            "Frontend is too slow"
        );
        // Serialize into the reusable output buffer, then fill the lengths.
        // A failed packet must leave previously queued packets untouched.
        bytes.extend_from_slice(&[0; RECORD_PREFIX_BYTES]);
        let result = (|| {
            let kind = self.kind();
            self.write_header(HeaderWriter {
                bytes,
                end: start
                    + RECORD_PREFIX_BYTES
                    + (limit - RECORD_PREFIX_BYTES - payload).min(kind.header_limit()),
            })?;
            let header_size = bytes.len() - start - RECORD_PREFIX_BYTES;
            let json_budget = record::json_budget(&bytes[start + RECORD_PREFIX_BYTES..])?;
            let cell_bytes: usize = cells
                .iter()
                .map(|cell| CELL_PREFIX_BYTES + cell.text().len())
                .sum();
            let size = bytes.len() - start - 4 + payload + cell_bytes;
            anyhow::ensure!(size <= MAX_PACKET, "Stream packet is too large");
            anyhow::ensure!(size + 4 <= limit, "Frontend is too slow");
            let record = Record::new(size, header_size, kind, json_budget, cells.len())?;
            bytes[start..start + RECORD_PREFIX_BYTES].copy_from_slice(&record.encode());
            if payload != 0 && (copy_images || !matches!(self, Self::Pane(_))) {
                bytes.reserve(payload + cell_bytes);
            }
            if let Self::Clipboard(crate::clipboard::Write {
                content: Some(content),
                ..
            }) = self
            {
                for representation in &content.0 {
                    bytes.extend_from_slice(&representation.data);
                }
            }
            encode_cells(cells, bytes, cell_bytes);
            if copy_images
                && let Self::Pane(update) = self
                && let Some(graphics) = &update.header.graphics
            {
                for image in &graphics.images {
                    bytes.extend_from_slice(&image.pixels);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            bytes.truncate(start);
        }
        result
    }

    pub fn kind(&self) -> RecordKind {
        match self {
            Self::Pane(_) => RecordKind::Pane,
            Self::Execution(_) => RecordKind::Execution,
            Self::Git(_) => RecordKind::Git,
            Self::Clipboard(_) => RecordKind::Clipboard,
            Self::ClipboardRead(_) => RecordKind::ClipboardRead,
            Self::WindowResize(_) => RecordKind::WindowResize,
        }
    }

    fn write_header(&self, writer: impl Write) -> serde_json::Result<()> {
        match self {
            Self::Pane(update) => serde_json::to_writer(writer, &update.header),
            Self::Execution(state) => serde_json::to_writer(writer, state),
            Self::Git(git) => serde_json::to_writer(writer, git),
            Self::Clipboard(write) => {
                serde_json::to_writer(writer, &crate::clipboard::wire::WriteRef(write))
            }
            Self::ClipboardRead(read) => serde_json::to_writer(writer, read),
            Self::WindowResize(size) => serde_json::to_writer(writer, size),
        }
    }

    pub fn as_pane(&self) -> Option<&PaneUpdate> {
        if let Self::Pane(update) = self {
            Some(update)
        } else {
            None
        }
    }
    pub fn as_pane_mut(&mut self) -> Option<&mut PaneUpdate> {
        if let Self::Pane(update) = self {
            Some(update)
        } else {
            None
        }
    }
    pub fn into_pane(self) -> Option<PaneUpdate> {
        if let Self::Pane(update) = self {
            Some(update)
        } else {
            None
        }
    }

    /// Decode one packet, skipping heartbeat replies and leaving following data buffered.
    pub fn read(reader: &mut impl BufRead) -> anyhow::Result<Self> {
        loop {
            if let Some(packet) = Self::read_message(reader)? {
                return Ok(packet);
            }
        }
    }

    packet_reader!(
        read_message,
        read_body,
        [],
        impl BufRead,
        [],
        std::io::Read::take,
        []
    );
    #[cfg(feature = "async")]
    packet_reader!(read_message_async, read_body_async, [async], (impl futures_lite::io::AsyncBufRead + Unpin), [.await], futures_lite::io::AsyncReadExt::take, [futures_lite::future::yield_now().await;]);
}

// Keep the hot binary loop independent of JSON serialization and event dispatch.
#[inline(never)]
fn encode_cells(cells: &[Cell], bytes: &mut Vec<u8>, cell_bytes: usize) {
    bytes.reserve(cell_bytes);
    for cell in cells {
        let len = (cell.text().len() as u16).to_le_bytes();
        bytes.extend_from_slice(&[
            len[0],
            len[1],
            cell.fg[0],
            cell.fg[1],
            cell.fg[2],
            cell.bg[0],
            cell.bg[1],
            cell.bg[2],
            cell.flags,
            cell.decoration,
            cell.underline_color[0],
            cell.underline_color[1],
            cell.underline_color[2],
        ]);
        bytes.extend_from_slice(cell.text().as_bytes());
    }
}

fn cell_length(bytes: &[u8]) -> anyhow::Result<usize> {
    let len = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as usize;
    anyhow::ensure!(
        len <= crate::cells::MAX_GRAPHEME_BYTES,
        "Invalid grapheme length"
    );
    Ok(CELL_PREFIX_BYTES + len)
}
fn decode_cell(bytes: &[u8]) -> anyhow::Result<Cell> {
    Ok(Cell {
        text: crate::cells::Grapheme::from_utf8(&bytes[CELL_PREFIX_BYTES..])?,
        fg: bytes[2..5].try_into().unwrap(),
        bg: bytes[5..8].try_into().unwrap(),
        flags: bytes[8],
        decoration: bytes[9],
        underline_color: bytes[10..13].try_into().unwrap(),
    })
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
        cell.set_text("界é").unwrap();
        cell.fg = [1, 2, 3];
        cell.flags = 65 | crate::cells::flags::BLINK;
        cell.decoration = underline::CURLY | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
        cell.underline_color = [6, 7, 8];
        Packet::Pane(PaneUpdate {
            header: PaneHeader {
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
                    cursor_password_input: true,
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
                ..PaneHeader::default()
            },
            cells: vec![cell, Cell::default()],
        })
    }
    #[test]
    fn wire_cache_limits_reserve_encoded_bytes_and_reject_before_cells() {
        assert_eq!(cache_row_limit(1), 512);
        assert_eq!(cache_row_limit(1000), 61);
        for cols in [0, 1001, usize::MAX] {
            assert_eq!(cache_row_limit(cols), 0);
        }
        for cols in [1, 80, 512, 1000] {
            let count = cache_row_limit(cols);
            assert!(count * cols * (CELL_PREFIX_BYTES + 260) <= MAX_CACHE_BYTES);
            let mut packet = packet();
            packet.as_pane_mut().unwrap().header.frame = Some(Frame {
                cols: cols as u16,
                rows: 1,
                scrollback: crate::Scrollback {
                    total: 1000,
                    offset: 0,
                    len: 1,
                },
                ..Frame::default()
            });
            packet.as_pane_mut().unwrap().header.rows.clear();
            packet.as_pane_mut().unwrap().header.row_flags.clear();
            packet.as_pane_mut().unwrap().header.cache = Some(CachedRows {
                start: 0,
                row_flags: vec![0; count],
            });
            packet.as_pane_mut().unwrap().cells = vec![Cell::default(); count * cols];
            let mut bytes = Vec::new();
            packet.encode_into(&mut bytes).unwrap();
            packet.encode_into(&mut bytes).unwrap();
            let mut reader = bytes.as_slice();
            for _ in 0..2 {
                let decoded = Packet::read(&mut reader).unwrap();
                assert!(decoded.as_pane().unwrap().cells == packet.as_pane().unwrap().cells);
                assert_eq!(
                    decoded
                        .as_pane()
                        .unwrap()
                        .header
                        .cache
                        .as_ref()
                        .unwrap()
                        .row_flags
                        .len(),
                    count
                );
            }
            assert!(reader.is_empty());

            // Short text cannot bypass the reservation for worst-case text.
            // Omit the cell data entirely to prove rejection precedes its read.
            packet
                .as_pane_mut()
                .unwrap()
                .header
                .cache
                .as_mut()
                .unwrap()
                .row_flags
                .push(0);
            let header = serde_json::to_vec(&packet.as_pane().unwrap().header).unwrap();
            let size =
                RECORD_PREFIX_BYTES - 4 + header.len() + (count + 1) * cols * CELL_PREFIX_BYTES;
            let record = Record::new(
                size,
                header.len(),
                RecordKind::Pane,
                record::json_budget(&header).unwrap(),
                (count + 1) * cols,
            )
            .unwrap();
            let mut bytes = record.encode().to_vec();
            bytes.extend_from_slice(&header);
            let mut reader = io::Cursor::new(&bytes);
            assert_eq!(
                Packet::read(&mut reader).err().unwrap().to_string(),
                "Invalid cached rows"
            );
            assert_eq!(reader.position() as usize, bytes.len());
            #[cfg(feature = "async")]
            futures_lite::future::block_on(async {
                let mut reader = futures_lite::io::Cursor::new(&bytes);
                assert_eq!(
                    Packet::read_message_async(&mut reader)
                        .await
                        .err()
                        .unwrap()
                        .to_string(),
                    "Invalid cached rows"
                );
                assert_eq!(reader.position() as usize, bytes.len());
            });
        }
    }

    #[test]
    fn cached_rows_round_trip_and_invalid_scroll_storage_is_rejected() {
        let mut packet = packet();
        packet.as_pane_mut().unwrap().header.generation = 1;
        packet.as_pane_mut().unwrap().header.cache_origin = 42;
        packet.as_pane_mut().unwrap().header.cache_valid_before = Some(200);
        packet.as_pane_mut().unwrap().header.row_shift = -1;
        packet.as_pane_mut().unwrap().header.rows = vec![0];
        packet.as_pane_mut().unwrap().header.cache = Some(CachedRows {
            start: 196,
            row_flags: vec![0, 1, 0],
        });
        packet
            .as_pane_mut()
            .unwrap()
            .cells
            .resize(8, Cell::default());
        packet.as_pane_mut().unwrap().cells[2] = packet.as_pane().unwrap().cells[0].clone();
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut &bytes[..]).unwrap();
        assert_eq!(decoded.as_pane().unwrap().cells.len(), 8);
        assert!(
            decoded.as_pane().unwrap().cells[0].blink()
                && decoded.as_pane().unwrap().cells[2].blink()
        );
        assert_eq!(decoded.as_pane().unwrap().header.cache_origin, 42);
        assert_eq!(
            decoded.as_pane().unwrap().header.cache_valid_before,
            Some(200)
        );
        assert_eq!(
            decoded
                .as_pane()
                .unwrap()
                .header
                .cache
                .as_ref()
                .unwrap()
                .row_flags,
            [0, 1, 0]
        );
        for (shift, start, flags) in [
            (3, 196, vec![0]),
            (-3, 196, vec![0]),
            (0, u64::MAX, vec![0]),
            (0, 499, vec![0, 0]),
            (0, 0, vec![2]),
            (0, 0, vec![0; MAX_CACHE_ROWS + 1]),
        ] {
            packet.as_pane_mut().unwrap().header.row_shift = shift;
            packet.as_pane_mut().unwrap().header.cache = Some(CachedRows {
                start,
                row_flags: flags,
            });
            bytes.clear();
            packet.encode_into(&mut bytes).unwrap();
            assert!(Packet::read(&mut &bytes[..]).is_err());
        }
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
                .as_pane()
                .unwrap()
                .header
                .scroll_serial,
            42
        );
        assert!(Packet::read_message(&mut reader).unwrap().is_none());
        assert!(reader.is_empty());
        let mut reader = &bytes[..];
        assert_eq!(
            Packet::read(&mut reader)
                .unwrap()
                .as_pane()
                .unwrap()
                .header
                .scroll_serial,
            42
        );
        assert_eq!(reader, HEARTBEAT_REPLY);
    }

    #[test]
    fn pane_records_require_progress_metadata() {
        let mut header = serde_json::to_value(PaneHeader::default()).unwrap();
        header["meta"].as_object_mut().unwrap().remove("progress");
        let json = serde_json::to_vec(&header).unwrap();
        let record = Record::new(
            RECORD_PREFIX_BYTES - 4 + json.len(),
            json.len(),
            RecordKind::Pane,
            record::json_budget(&json).unwrap(),
            0,
        )
        .unwrap();
        let mut bytes = record.encode().to_vec();
        bytes.extend_from_slice(&json);
        let error = Packet::read(&mut bytes.as_slice()).err().unwrap();
        assert!(
            error.to_string().contains("missing field `progress`"),
            "{error:#}"
        );
    }

    #[test]
    fn malformed_headers_and_payload_lengths_are_rejected() {
        let mut packet = packet();
        for flags in [vec![0, 1], vec![2]] {
            packet.as_pane_mut().unwrap().header.row_flags = flags;
            let mut encoded = Vec::new();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut &encoded[..]).is_err());
        }
        packet.as_pane_mut().unwrap().header.row_flags.clear();
        let mut encoded = Vec::new();
        for rows in [vec![3], vec![1, 1], vec![2, 1], vec![0, 1, 2, 3]] {
            packet
                .as_pane_mut()
                .unwrap()
                .cells
                .resize(rows.len() * 2, Cell::default());
            packet.as_pane_mut().unwrap().header.rows = rows;
            encoded.clear();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        // Keep the payload valid so only the grid limits can reject these frames.
        packet.as_pane_mut().unwrap().header.rows.clear();
        packet.as_pane_mut().unwrap().cells.clear();
        for (cols, rows) in [(0, 3), (2, 0), (1001, 3), (2, 1001), (512, 257)] {
            let frame = packet.as_pane_mut().unwrap().header.frame.as_mut().unwrap();
            frame.cols = cols;
            frame.rows = rows;
            encoded.clear();
            packet.encode_into(&mut encoded).unwrap();
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        packet = self::packet();
        packet.as_pane_mut().unwrap().header.frame = None;
        packet.as_pane_mut().unwrap().cells.clear();
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
        let mut bytes = vec![16, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&encoded);
        let mut reader = io::Cursor::new(bytes);
        assert!(Packet::read(&mut reader).is_err());
        assert_eq!(
            reader.position(),
            RECORD_PREFIX_BYTES as u64,
            "read beyond an invalid header size"
        );

        packet = self::packet();
        encoded.clear();
        packet.encode_into(&mut encoded).unwrap();
        let size = u32::from_le_bytes(encoded[..4].try_into().unwrap());
        for claimed_size in [size - 1, size + 1, size - CELL_PREFIX_BYTES as u32] {
            encoded[..4].copy_from_slice(&claimed_size.to_le_bytes());
            assert!(Packet::read(&mut encoded.as_slice()).is_err());
        }
        encoded[..4].copy_from_slice(&size.to_le_bytes());
        encoded[RECORD_PREFIX_BYTES] = b'!';
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
            packet.as_pane_mut().unwrap().header.hyperlinks = Some(Hyperlinks {
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
            packet.as_pane_mut().unwrap().header.hyperlinks = Some(Hyperlinks {
                spans: (0..targets.len())
                    .map(|i| [i as u32, i as u32 + 1, i as u32])
                    .collect(),
                targets,
            });
            let mut bytes = Vec::new();
            packet.encode_into(&mut bytes).unwrap();
            assert!(Packet::read(&mut bytes.as_slice()).is_err());
        }
        packet.as_pane_mut().unwrap().header.hyperlinks = Some(Hyperlinks {
            targets: vec!["test:actual".into()],
            spans: vec![[0, 6, 0]],
        });
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(
            decoded.as_pane().unwrap().header.hyperlinks,
            packet.as_pane().unwrap().header.hyperlinks
        );
        packet.as_pane_mut().unwrap().header.frame = None;
        packet.as_pane_mut().unwrap().header.rows.clear();
        packet.as_pane_mut().unwrap().header.row_flags.clear();
        packet.as_pane_mut().unwrap().cells.clear();
        bytes.clear();
        packet.encode_into(&mut bytes).unwrap();
        assert!(Packet::read(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn link_ranges_address_the_viewport_and_not_appended_cached_rows() {
        use crate::hyperlinks::Hyperlinks;
        let mut packet = packet();
        packet.as_pane_mut().unwrap().header.generation = 1;
        packet.as_pane_mut().unwrap().header.cache = Some(CachedRows {
            start: 0,
            row_flags: vec![0; 10],
        });
        packet
            .as_pane_mut()
            .unwrap()
            .cells
            .extend(vec![Cell::default(); 20]);
        packet.as_pane_mut().unwrap().header.hyperlinks = Some(Hyperlinks {
            targets: vec!["test:viewport".into()],
            spans: vec![[0, 6, 0]],
        });
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let decoded = Packet::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.as_pane().unwrap().cells.len(), 22);
        assert_eq!(
            decoded
                .as_pane()
                .unwrap()
                .header
                .hyperlinks
                .as_ref()
                .unwrap()
                .target_at(5),
            Some("test:viewport")
        );
        packet
            .as_pane_mut()
            .unwrap()
            .header
            .hyperlinks
            .as_mut()
            .unwrap()
            .spans[0][1] = 7;
        bytes.clear();
        packet.encode_into(&mut bytes).unwrap();
        assert!(Packet::read(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn invalid_grapheme_lengths_utf8_and_trailing_data_are_rejected() {
        let mut packet = packet();
        packet.as_pane_mut().unwrap().cells[0]
            .set_text("X")
            .unwrap();
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        let start =
            RECORD_PREFIX_BYTES + u32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize;
        for length in [261u16, u16::MAX] {
            let mut bad = encoded.clone();
            bad[start..start + 2].copy_from_slice(&length.to_le_bytes());
            assert!(Packet::read(&mut bad.as_slice()).is_err());
        }
        for invalid in [0, 0x80, 0xff] {
            let mut bad = encoded.clone();
            bad[start + CELL_PREFIX_BYTES] = invalid;
            assert!(Packet::read(&mut bad.as_slice()).is_err());
        }
        // An inner length cannot consume the next record, even if it is valid UTF-8.
        let mut bad = encoded.clone();
        bad[start..start + 2].copy_from_slice(&260u16.to_le_bytes());
        let boundary = bad.len();
        bad.extend_from_slice(&encoded);
        let mut reader = io::Cursor::new(bad);
        assert!(Packet::read(&mut reader).is_err());
        assert!(reader.position() <= boundary as u64);
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
            std::io::Read::read(&mut self.bytes, &mut out[..len])
        }
    }

    #[cfg(feature = "async")]
    impl futures_lite::io::AsyncRead for Fragmented<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            out: &mut [u8],
        ) -> std::task::Poll<io::Result<usize>> {
            // Exercise suspension as well as fragmentation and Interrupted reads.
            if self.reads.is_multiple_of(7) {
                self.reads += 1;
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            match Read::read(&mut *self, out) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    // AsyncRead handles EINTR below the codec.
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
                result => std::task::Poll::Ready(result),
            }
        }
    }

    #[test]
    fn fragmented_and_interrupted_reads_preserve_cells_and_following_packets() {
        let mut packet = packet();
        packet
            .as_pane_mut()
            .unwrap()
            .header
            .frame
            .as_mut()
            .unwrap()
            .cols = 73;
        packet
            .as_pane_mut()
            .unwrap()
            .header
            .frame
            .as_mut()
            .unwrap()
            .mouse_shape = u8::MAX;
        packet.as_pane_mut().unwrap().header.rows = vec![0, 2];
        packet.as_pane_mut().unwrap().header.row_flags = vec![1, 0];
        packet.as_pane_mut().unwrap().cells = (0..146)
            .map(|i| {
                let mut cell = Cell::default();
                cell.set_text(&if i % 2 == 0 {
                    "界ée\u{301}".into()
                } else {
                    format!("🧑{}", "\u{1d185}".repeat(64))
                })
                .unwrap();
                cell.fg = [i, 2, 3];
                cell.bg = [4, i, 6];
                cell.flags = i;
                cell.decoration = i | decoration::OVERLINE | decoration::UNDERLINE_COLOR;
                cell.underline_color = [i, 20, 30];
                cell
            })
            .collect();
        packet.as_pane_mut().unwrap().header.cache = Some(CachedRows {
            start: 10,
            row_flags: vec![1, 0],
        });
        packet.as_pane_mut().unwrap().cells.extend_from_within(..);
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        // Put an empty delta between two payloads to catch packet overconsumption.
        let unchanged = Packet::Pane(PaneUpdate {
            header: PaneHeader::default(),
            cells: vec![],
        });
        unchanged.encode_into(&mut encoded).unwrap();
        packet.encode_into(&mut encoded).unwrap();
        let expected_pane = packet.as_pane().unwrap();
        for (capacity, chunk) in [
            (1, 1),
            (CELL_PREFIX_BYTES - 1, 13),
            (CELL_PREFIX_BYTES, CELL_PREFIX_BYTES),
            (CELL_PREFIX_BYTES + 1, 137),
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
                let decoded = Packet::read(&mut reader).unwrap().into_pane().unwrap();
                if index == 1 {
                    assert!(decoded.cells.is_empty());
                    assert!(decoded.header.frame.is_none());
                } else {
                    assert!(decoded.cells == expected_pane.cells);
                    assert!(decoded.header.frame == expected_pane.header.frame);
                    assert_eq!(decoded.header.rows, expected_pane.header.rows);
                    assert_eq!(decoded.header.row_flags, expected_pane.header.row_flags);
                    assert_eq!(decoded.header.meta, expected_pane.header.meta);
                    assert_eq!(
                        decoded.header.scroll_serial,
                        expected_pane.header.scroll_serial
                    );
                    assert_eq!(
                        decoded.header.notifications,
                        expected_pane.header.notifications
                    );
                }
            }
            let mut trailing = Vec::new();
            reader.read_to_end(&mut trailing).unwrap();
            assert!(trailing.is_empty());
            #[cfg(feature = "async")]
            futures_lite::future::block_on(async {
                let mut reader = futures_lite::io::BufReader::with_capacity(
                    capacity,
                    Fragmented {
                        bytes: &encoded,
                        chunk,
                        reads: 0,
                    },
                );
                for expected in [&packet, &unchanged, &packet] {
                    let decoded = Packet::read_message_async(&mut reader)
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(decoded.as_pane().unwrap().cells == expected.as_pane().unwrap().cells);
                    assert_eq!(
                        serde_json::to_value(&decoded.as_pane().unwrap().header).unwrap(),
                        serde_json::to_value(&expected.as_pane().unwrap().header).unwrap()
                    );
                }
            });
        }
    }

    #[test]
    fn truncated_packets_fail_at_every_header_and_cell_boundary() {
        let mut encoded = Vec::new();
        packet().encode_into(&mut encoded).unwrap();
        for len in 0..encoded.len() {
            let mut reader = BufReader::with_capacity(CELL_PREFIX_BYTES, &encoded[..len]);
            assert!(Packet::read(&mut reader).is_err(), "accepted {len} bytes");
        }
    }
    #[test]
    fn failed_encoding_preserves_previously_queued_packets() {
        let mut packet = packet();
        let mut encoded = Vec::new();
        packet.encode_into(&mut encoded).unwrap();
        let original = encoded.clone();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            packet.as_pane_mut().unwrap().header.meta.directory =
                std::ffi::OsString::from_vec(vec![0xff]).into();
            assert!(packet.encode_into(&mut encoded).is_err());
            assert_eq!(encoded, original);
        }

        packet.as_pane_mut().unwrap().header.meta.directory = "/tmp".into();
        let invalid = offer(&[("text/plain", &vec![b'x'; MAX_PACKET])]);
        assert!(invalid.encode_into(&mut encoded).is_err());
        assert_eq!(encoded, original);
        let decoded = Packet::read(&mut &encoded[..]).unwrap();
        assert!(decoded.as_pane().unwrap().cells == packet.as_pane().unwrap().cells);
    }

    fn offer(contents: &[(&str, &[u8])]) -> Packet {
        use crate::clipboard::{Destination, Offer, Representation, Write};
        Packet::Clipboard(Write {
            destination: Destination::Primary,
            content: Some(Offer(
                contents
                    .iter()
                    .map(|(mime, data)| Representation {
                        mime: (*mime).into(),
                        data: data.to_vec(),
                    })
                    .collect(),
            )),
        })
    }

    #[test]
    fn clipboard_binary_alternatives_roundtrip_without_json_amplification() {
        let png = include_bytes!("../../../scripts/fixtures/clipboard.png");
        let packet = offer(&[
            ("image/png", png),
            ("text/html", b"<b>html</b>\0"),
            ("text/plain;charset=utf-8", "plain 界🙂".as_bytes()),
            ("text/plain", "plain 界🙂".as_bytes()),
        ]);
        let mut bytes = vec![];
        packet.encode_into(&mut bytes).unwrap();
        let header_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let header: serde_json::Value =
            serde_json::from_slice(&bytes[RECORD_PREFIX_BYTES..RECORD_PREFIX_BYTES + header_len])
                .unwrap();
        assert_eq!(
            header["content"][0],
            serde_json::json!({"mime":"image/png", "len":png.len()})
        );
        assert_eq!(&bytes[RECORD_PREFIX_BYTES + header_len..][..png.len()], png);
        // Read through very small buffers to cross header and binary boundaries.
        for capacity in [1, 3, 79, 65536] {
            let decoded =
                Packet::read(&mut BufReader::with_capacity(capacity, bytes.as_slice())).unwrap();
            let (Packet::Clipboard(decoded), Packet::Clipboard(expected)) = (&decoded, &packet)
            else {
                panic!("expected clipboard events");
            };
            assert_eq!(decoded, expected);
        }
        let packet = offer(&[(
            "image/png",
            &vec![0; crate::clipboard::MAX_APPLICATION_BYTES],
        )]);
        bytes.clear();
        packet.encode_into(&mut bytes).unwrap();
        assert!(bytes.len() < crate::clipboard::MAX_APPLICATION_BYTES + 1024);
        assert!(Packet::read(&mut bytes.as_slice()).is_ok());
        let mut queue = vec![1; 20];
        let capacity = queue.capacity();
        assert!(packet.encode_into_limit(&mut queue, 4096).is_err());
        assert_eq!(queue, [1; 20]);
        assert_eq!(queue.capacity(), capacity); // No queue growth on rejection.
    }

    fn clipboard_header(packet: &Packet) -> serde_json::Value {
        let mut bytes = Vec::new();
        packet.write_header(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn clipboard_header_and_queue_limits_apply_at_the_exact_boundary() {
        let packet = offer(&[("text/plain", b"value")]);
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let mut queue = bytes.clone();
        packet.encode_into_limit(&mut queue, bytes.len()).unwrap();
        assert_eq!(queue.len(), 2 * bytes.len());
        assert!(
            packet
                .encode_into_limit(&mut queue, bytes.len() - 1)
                .is_err()
        );
        assert_eq!(queue.len(), 2 * bytes.len());
        // Whitespace is valid JSON metadata and counts against the header limit.
        for extra in [0, 1] {
            let mut header = serde_json::to_vec(&clipboard_header(&packet)).unwrap();
            header.resize(crate::clipboard::MAX_HEADER_BYTES + extra, b' ');
            let mut wire: Vec<_> = [
                RECORD_PREFIX_BYTES - 4 + header.len() + 5,
                header.len(),
                RecordKind::Clipboard as usize,
                record::json_budget(&header).unwrap(),
                0,
            ]
            .into_iter()
            .flat_map(|v| (v as u32).to_le_bytes())
            .collect();
            wire.extend_from_slice(&header);
            wire.extend_from_slice(b"value");
            let mut reader = io::Cursor::new(wire);
            assert_eq!(Packet::read(&mut reader).is_ok(), extra == 0);
            if extra != 0 {
                assert_eq!(reader.position(), RECORD_PREFIX_BYTES as u64);
            }
        }
    }

    #[test]
    fn clipboard_rejects_invalid_descriptors_payloads_and_mixed_packets() {
        let valid_packet = offer(&[("text/plain", b"ok"), ("text/html", b"<b>ok</b>")]);
        let header = clipboard_header(&valid_packet);
        let mut cases = Vec::new();
        for content in [
            serde_json::json!([]),
            serde_json::json!([{"mime":"image/jpeg","len":0}]),
            serde_json::json!([{"mime":"image/png","len":usize::MAX}]),
            serde_json::json!([
                {"mime":"image/png","len":0}, {"mime":"text/html","len":0},
                {"mime":"text/plain","len":0}, {"mime":"text/plain;charset=utf-8","len":0},
                {"mime":"image/png","len":0}
            ]),
            serde_json::json!([
                {"mime":"image/png","len":crate::clipboard::MAX_HTML},
                {"mime":"text/html","len":crate::clipboard::MAX_HTML}
            ]),
            serde_json::json!([{"mime":"text/plain","len":0},{"mime":"text/plain","len":0}]),
            serde_json::json!([{"mime":"text/plain","len":1}]), // NUL text.
            serde_json::json!([{"mime":"text/plain","len":2}]), // Truncated.
        ] {
            let mut h = header.clone();
            h["content"] = content;
            cases.push(h);
        }
        let mut mixed = header.clone();
        mixed["frame"] = serde_json::to_value(packet().as_pane().unwrap().header.frame).unwrap();
        cases.push(mixed);
        for header in cases {
            let header = serde_json::to_vec(&header).unwrap();
            let record = Record::new(
                RECORD_PREFIX_BYTES - 4 + header.len() + 1,
                header.len(),
                RecordKind::Clipboard,
                record::json_budget(&header).unwrap(),
                0,
            )
            .unwrap();
            let mut wire = record.encode().to_vec();
            wire.extend_from_slice(&header);
            wire.push(0);
            assert!(Packet::read(&mut wire.as_slice()).is_err());
        }
        let mut valid = vec![];
        valid_packet.encode_into(&mut valid).unwrap();
        for cut in [1, 4, 7, valid.len() - 1] {
            assert!(Packet::read(&mut &valid[..cut]).is_err());
        }
        for contents in [
            vec![("text/plain", b"bad\0".as_slice())],
            vec![
                ("text/plain", b"one".as_slice()),
                ("text/plain;charset=utf-8", b"two".as_slice()),
            ],
            vec![
                ("text/html", b"ok".as_slice()),
                ("application/octet-stream", b"no".as_slice()),
            ],
        ] {
            let invalid = offer(&contents);
            let mut queue = valid.clone();
            assert!(invalid.encode_into(&mut queue).is_err());
            assert_eq!(queue, valid);
        }
    }
}

#[cfg(test)]
mod event_tests;
