use super::*;
use crate::clipboard::{Destination, ReadEvent, Write};

fn events() -> Vec<Packet> {
    vec![
        Packet::Execution(
            serde_json::from_value(serde_json::json!({
                "daemon":"01010101010101010101010101010101", "home_directory":"/home/test",
                "machine_kind":"server", "hostname":"host", "revision":1, "result":{"ok":true},
                "execution":{"tabs":[]}, "metadata":[]
            }))
            .unwrap(),
        ),
        Packet::Pane(PaneUpdate {
            header: PaneHeader {
                meta: Metadata {
                    id: 7,
                    ..Default::default()
                },
                frame: Some(Frame {
                    cols: 1,
                    rows: 2,
                    ..Default::default()
                }),
                rows: vec![1],
                row_shift: 1,
                generation: 3,
                cache_origin: 20,
                find: Some(crate::search::FindState {
                    resource_limited: true,
                    ..Default::default()
                }),
                selection: Some(crate::selection::Selection::linear(0, 1)),
                ..Default::default()
            },
            cells: vec![Cell::default()],
        }),
        Packet::Clipboard(Write {
            destination: Destination::Clipboard,
            content: Some("界🙂".to_string().into()),
        }),
        Packet::Git(GitMetadata {
            id: 7,
            directory: "/tmp".into(),
            branch: "main".into(),
        }),
        Packet::ClipboardRead(ClipboardRead {
            pane: 7,
            event: ReadEvent::Cancel { id: 8 },
        }),
        Packet::Clipboard(Write {
            destination: Destination::Primary,
            content: None,
        }),
    ]
}

fn header(packet: &Packet) -> Vec<u8> {
    let mut bytes = Vec::new();
    packet.write_header(&mut bytes).unwrap();
    bytes
}

#[test]
fn mixed_typed_events_and_heartbeats_keep_binary_data_and_order() {
    let events = events();
    let mut bytes = Vec::new();
    for packet in &events {
        bytes.extend_from_slice(HEARTBEAT_REPLY);
        packet.encode_into(&mut bytes).unwrap();
    }
    let mut reader = std::io::BufReader::with_capacity(1, bytes.as_slice());
    for expected in &events {
        assert!(Packet::read_message(&mut reader).unwrap().is_none());
        let packet = Packet::read_message(&mut reader).unwrap().unwrap();
        assert_eq!(packet.kind(), expected.kind());
        assert_eq!(header(&packet), header(expected));
        if let (Some(actual), Some(expected)) = (packet.as_pane(), expected.as_pane()) {
            assert!(actual.cells == expected.cells);
        }
        if let (Packet::Clipboard(actual), Packet::Clipboard(expected)) = (&packet, expected) {
            assert_eq!(actual, expected);
        }
    }
    #[cfg(feature = "async")]
    futures_lite::future::block_on(async {
        let mut reader = futures_lite::io::BufReader::with_capacity(1, bytes.as_slice());
        for expected in &events {
            assert!(
                Packet::read_message_async(&mut reader)
                    .await
                    .unwrap()
                    .is_none()
            );
            let packet = Packet::read_message_async(&mut reader)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(packet.kind(), expected.kind());
            assert_eq!(header(&packet), header(expected));
            if let (Packet::Clipboard(actual), Packet::Clipboard(expected)) = (&packet, expected) {
                assert_eq!(actual, expected);
            }
        }
    });
}

fn rejects(bytes: &[u8]) {
    let mut reader = bytes;
    assert!(Packet::read_message(&mut reader).is_err());
    #[cfg(feature = "async")]
    {
        let mut reader = bytes;
        assert!(futures_lite::future::block_on(Packet::read_message_async(&mut reader)).is_err());
    }
}

#[test]
fn framing_selects_one_schema_and_rejects_cross_kind_or_extra_binary_data() {
    for packet in events() {
        for limit in [0, RECORD_PREFIX_BYTES - 1] {
            let mut queue = Vec::new();
            assert!(packet.encode_into_limit(&mut queue, limit).is_err());
            assert_eq!(queue.capacity(), 0, "rejection must not grow the queue");
        }
        let mut valid = Vec::new();
        packet.encode_into(&mut valid).unwrap();
        for kind in [0, 1, 2, 3, 4, 5, 6, u32::MAX] {
            if kind == packet.kind() as u32 {
                continue;
            }
            let mut wrong = valid.clone();
            wrong[8..12].copy_from_slice(&kind.to_le_bytes());
            rejects(&wrong);
        }
        // Even null cross-kind fields are invalid, rather than ignored.
        let mut json: serde_json::Value = serde_json::from_slice(&header(&packet)).unwrap();
        json[if packet.kind() == RecordKind::Pane {
            "content"
        } else {
            "frame"
        }] = serde_json::Value::Null;
        let json = serde_json::to_vec(&json).unwrap();
        let original_header = u32::from_le_bytes(valid[4..8].try_into().unwrap()) as usize;
        let payload = &valid[RECORD_PREFIX_BYTES + original_header..];
        let record = Record::new(
            RECORD_PREFIX_BYTES - 4 + json.len() + payload.len(),
            json.len(),
            packet.kind(),
            record::json_budget(&json).unwrap(),
            packet.as_pane().map_or(0, |p| p.cells.len()),
        )
        .unwrap();
        let mut wrong = record.encode().to_vec();
        wrong.extend_from_slice(&json);
        wrong.extend_from_slice(payload);
        rejects(&wrong);
        if matches!(
            packet.kind(),
            RecordKind::Execution | RecordKind::Git | RecordKind::ClipboardRead
        ) {
            let mut wrong = valid.clone();
            let size = u32::from_le_bytes(wrong[..4].try_into().unwrap()) + 1;
            wrong[..4].copy_from_slice(&size.to_le_bytes());
            wrong.push(0);
            let mut reader = std::io::Cursor::new(&wrong);
            assert!(Packet::read(&mut reader).is_err());
            assert_eq!(reader.position(), RECORD_PREFIX_BYTES as u64);
            rejects(&wrong);
        }
    }
}
