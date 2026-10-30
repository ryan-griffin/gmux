use gmux_protocol::cells::{decoration, mouse_shape, underline};
use gmux_protocol::{
    Cell, Frame,
    transport::{CachedRows, Header, Metadata, Packet},
};
use std::{mem::size_of, path::PathBuf};
fn packet() -> Packet {
    let mut cell = Cell::default();
    cell.set_text("界é").unwrap();
    cell.fg = [1, 2, 3];
    cell.flags = 65 | gmux_protocol::cells::flags::BLINK;
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
                scrollback: gmux_protocol::Scrollback {
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
fn maximum_native_text_grid_and_prefetch_fit_packet_and_queue_bounds() {
    let glyph = format!("🧑{}", "\u{1d185}".repeat(64));
    let mut cell = Cell::default();
    cell.set_text(&glyph).unwrap();
    let cols = 512;
    let rows = gmux_protocol::viewport::MAX_CELLS / cols;
    let cached = gmux_protocol::scroll_cache::capacity(cols);
    let packet = Packet {
        header: Header {
            frame: Some(Frame {
                cols: cols as u16,
                rows: rows as u16,
                scrollback: gmux_protocol::Scrollback {
                    total: 1000,
                    offset: 0,
                    len: rows as u64,
                },
                ..Frame::default()
            }),
            rows: (0..rows as u16).collect(),
            cache: Some(CachedRows {
                start: 0,
                row_flags: vec![0; cached],
            }),
            ..Header::default()
        },
        cells: vec![cell; (rows + cached) * cols],
    };
    let (sender, receiver) = crate::incoming::channel(crate::incoming::MAX_BYTES, 1);
    sender.send(Ok(packet)).unwrap();
    let packet = receiver.try_recv().unwrap().unwrap();
    let mut bytes = Vec::new();
    packet.encode_into(&mut bytes).unwrap();
    let decoded = Packet::read(&mut bytes.as_slice()).unwrap();
    assert_eq!(decoded.cells.len(), packet.cells.len());
    assert_eq!(decoded.cells.last().unwrap().text(), glyph);
    // Heap text must consume admission budget even when Arc clones share it.
    let (sender, _) = crate::incoming::channel(
        decoded.cells.capacity() * size_of::<Cell>() + 1024 * 1024,
        1,
    );
    assert_eq!(
        sender.send(Ok(decoded)),
        Err(crate::incoming::SendError::TooLarge)
    );
}
