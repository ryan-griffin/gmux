use super::*;
use gmux_client::transport::{Header, Packet};
use std::{hint::black_box, time::Instant};

#[test]
#[ignore]
fn grapheme_measurements() {
    for (name, glyph) in [
        ("ascii", "a".to_owned()),
        ("common", "e\u{301}".to_owned()),
        ("long", format!("a{}", "\u{1ab0}".repeat(64))),
    ] {
        let mut engine = Engine::new().unwrap();
        engine.resize(120, 40, 8, 16).unwrap();
        let input = format!("\x1b[H{}", glyph.repeat(120 * 40));
        engine.write(input.as_bytes());
        engine.snapshot().unwrap();
        let n = 500;
        let start = Instant::now();
        for _ in 0..n {
            engine.write(input.as_bytes());
            black_box(engine.snapshot().unwrap());
        }
        let snapshot_us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        let packet = Packet {
            header: Header {
                frame: Some(engine.frame),
                rows: (0..40).collect(),
                ..Header::default()
            },
            cells: engine.cells.clone(),
        };
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes).unwrap();
        let wire_bytes = bytes.len();
        let start = Instant::now();
        for _ in 0..n {
            bytes.clear();
            packet.encode_into(&mut bytes).unwrap();
            black_box(Packet::read(&mut bytes.as_slice()).unwrap());
        }
        let transport_us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        let start = Instant::now();
        for _ in 0..n {
            bytes.clear();
            packet.encode_into(&mut bytes).unwrap();
        }
        let encode_us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        let start = Instant::now();
        for _ in 0..n {
            black_box(Packet::read(&mut bytes.as_slice()).unwrap());
        }
        let decode_us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!(
            "{name}: cell_bytes={} grid_bytes={} packet_memory={} wire_bytes={wire_bytes} snapshot_us={snapshot_us:.2} transport_us={transport_us:.2} encode_us={encode_us:.2} decode_us={decode_us:.2} exported_glyph_bytes={}",
            size_of::<Cell>(),
            engine.cells.capacity() * size_of::<Cell>(),
            packet.memory_size(),
            engine.cells[0].text().len()
        );
    }
}

#[test]
fn complete_native_clusters_survive_views_history_transport_copy_search_and_reflow() {
    for (base, mark, count, width) in [
        ("a", "\u{301}", 31, 1), // 63 bytes, the old boundary
        ("a", "\u{301}", 32, 1),
        ("a", "\u{1ab0}", 21, 1), // 64 bytes, including a three-byte suffix
        ("a", "\u{1ab0}", 64, 1),
        ("🧑", "\u{1d185}", 64, 2), // 260 bytes, the native maximum
        ("🧑", "\u{1d185}", 80, 2), // The engine itself retains only 64 suffixes.
    ] {
        let expected = format!("{base}{}", mark.repeat(count.min(64)));
        let mut engine = Engine::new().unwrap();
        engine.set_theme(&Theme::adwaita(true)).unwrap();
        engine.resize(8, 3, 8, 16).unwrap();
        engine.write(
            format!(
                "\x1b[5;4:3;38;2;12;34;56m{base}{}\x1b[0mX",
                mark.repeat(count)
            )
            .as_bytes(),
        );
        assert_eq!(engine.text().unwrap(), format!("{expected}X"));
        let original = engine.cells[..8].to_vec();
        assert!(original[0].blink());
        assert_eq!(
            original[0].underline(),
            gmux_client::cells::underline::CURLY
        );
        assert_eq!(original[0].fg, [12, 34, 56]);
        assert_eq!(original[width].text(), "X");
        if width == 2 {
            assert_ne!(original[1].flags & 64, 0);
        }
        // Dirty extraction must retain overflow in untouched rows and held frames.
        engine.write(b"\x1b[3;1Hbefore");
        engine.snapshot().unwrap();
        assert!(engine.cells[..8] == original);
        engine.write(b"\x1b[?2026h\x1b[3;1Hafter");
        engine.snapshot().unwrap();
        assert_eq!(engine.cells[0].text(), expected);
        assert_eq!(engine.cells[16].text(), "b");
        engine.write(b"\x1b[?2026l");
        engine.snapshot().unwrap();
        assert_eq!(engine.cells[16].text(), "a");
        // Move the actual cluster into history, then read both styled prefetch
        // and the scrolled native render cache.
        engine.write(b"\r\nnext\r\nlast\r\n");
        engine.snapshot().unwrap();
        let (history, flags) = engine.cache_rows(0..1).unwrap();
        assert!(history == original);
        engine.scroll_to(0);
        engine.snapshot().unwrap();
        assert!(engine.cells[..8] == original);
        let packet = Packet {
            header: Header {
                frame: Some(engine.frame),
                rows: (0..3).collect(),
                cache: Some(gmux_client::transport::CachedRows {
                    start: 0,
                    row_flags: flags,
                }),
                ..Header::default()
            },
            cells: engine.cells.iter().chain(&history).cloned().collect(),
        };
        let mut wire = Vec::new();
        packet.encode_into(&mut wire).unwrap();
        let decoded = Packet::read(&mut wire.as_slice()).unwrap();
        assert!(decoded.cells == packet.cells);
        engine.select((0, 0), (0, 0), SelectionMode::Cell).unwrap();
        let content = engine
            .copy_content(gmux_client::clipboard::Formats::TextHtml)
            .unwrap();
        assert_eq!(content.text, expected);
        let escaped: String = expected
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    c.to_string()
                } else {
                    format!("&#{};", c as u32)
                }
            })
            .collect();
        assert!(content.html.as_ref().unwrap().contains(&escaped));
        engine.find(&format!("{expected}X"));
        while engine.find_pending() {
            engine.poll_find();
        }
        assert_eq!(engine.find_state().total, 1);
        // Saved state uses Ghostty's checkpoint and formatter, with no cell cap.
        let saved = engine.checkpoint().unwrap();
        let mut restored = Engine::new().unwrap();
        restored.restore_history(&saved).unwrap();
        restored.set_theme(&Theme::adwaita(true)).unwrap();
        for cols in [4, 12, 8] {
            restored.resize(cols, 3, 8, 16).unwrap();
            restored.scroll_to(0);
            restored.snapshot().unwrap();
            assert_eq!(restored.cells[0].text(), expected);
            assert_eq!(restored.cells[width].text(), "X");
            assert!(restored.cells[0].blink());
        }
    }
}
