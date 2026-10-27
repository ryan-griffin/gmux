use super::*;
use gmux_client::clipboard::{Destination, Write};

#[test]
fn fragmented_clipboard_writes_own_unicode_and_preserve_destinations_and_clear() {
    let mut engine = Engine::new().unwrap();
    engine.set_clipboard_enabled(true);
    engine.write(b"\x1b]52;c;aMOpbGxvIOeVjPCf");
    assert!(engine.take_clipboard_writes().is_empty());
    let mut tail = b"mYI=\x1b\\\x1b]52;p;\x07".to_vec();
    engine.write(&tail);
    tail.fill(0); // The library's borrowed data has already expired.
    assert_eq!(
        engine.take_clipboard_writes(),
        [
            Write {
                destination: Destination::Clipboard,
                content: Some("héllo 界🙂".to_owned().into())
            },
            Write {
                destination: Destination::Primary,
                content: None
            },
        ]
    );
    engine.write(b"\x1b]52;s;dGV4dA==\x07\x1b]52;c;/w==\x07\x1b]52;c;YQBi\x07after");
    assert!(engine.take_clipboard_writes().is_empty());
    assert!(engine.take_events().0.is_empty());
    engine.snapshot().unwrap();
    assert!(engine.text().unwrap().contains("after"));
    let checkpoint = engine.checkpoint().unwrap();
    let mut restored = Engine::new().unwrap();
    restored.restore_history(&checkpoint).unwrap();
    assert!(restored.take_clipboard_writes().is_empty());
    engine.set_clipboard_enabled(false);
    engine.write(b"\x1b]52;c;dGV4dA==\x07");
    engine.set_clipboard_enabled(true);
    assert!(engine.take_clipboard_writes().is_empty());
}

#[test]
fn clipboard_admission_bounds_bytes_and_event_count_and_recovers_after_drain() {
    let mut engine = Engine::new().unwrap();
    engine.set_clipboard_enabled(true);
    for _ in 0..MAX_CLIPBOARD_EVENTS + 10 {
        engine.write(b"\x1b]52;c;\x07");
    }
    assert_eq!(engine.take_clipboard_writes().len(), MAX_CLIPBOARD_EVENTS);
    let text = vec![b'x'; gmux_client::transport::MAX_PASTE];
    let ud = (&mut *engine.events as *mut Events).cast();
    let admit =
        |len| unsafe { clipboard_write(ud, GMUX_CLIPBOARD_STANDARD as u8, text.as_ptr(), len) };
    assert_eq!(admit(text.len()), GMUX_CLIPBOARD_SUCCESS as i32);
    assert_eq!(admit(1), GMUX_CLIPBOARD_BUSY as i32);
    assert_eq!(admit(text.len() + 1), GMUX_CLIPBOARD_INVALID as i32);
    assert_eq!(
        engine.take_clipboard_writes()[0]
            .content
            .as_ref()
            .unwrap()
            .text
            .len(),
        text.len()
    );
    assert_eq!(admit(0), GMUX_CLIPBOARD_SUCCESS as i32);
    assert_eq!(
        engine.take_clipboard_writes()[0]
            .content
            .as_ref()
            .map(|c| c.text.as_str()),
        Some("")
    );
}

#[test]
fn user_copy_uses_formatter_colors_escaping_and_styles_without_selection_colors() {
    use gmux_client::clipboard::Formats;
    let mut engine = Engine::new().unwrap();
    engine.resize(80, 6, 0, 0).unwrap();
    engine.write(b"\x1b]10;#123456\x07\x1b]11;#654321\x07\x1b]4;1;#abcdef\x07");
    engine.write("<>&\"'界🙂e\u{301} \x1b[1;3;4:3;9;31;48;2;11;22;33mstyled\x1b[0m".as_bytes());
    engine.select((0, 0), (40, 0), SelectionMode::Cell).unwrap();
    let plain = engine.copy_content(Formats::Text).unwrap();
    assert!(plain.html.is_none());
    let rich = engine.copy_content(Formats::TextHtml).unwrap();
    assert_eq!(plain.text, rich.text);
    let html = rich.html.unwrap();
    for fragment in [
        "&lt;&gt;&amp;&quot;&#39;",
        "&#30028;",
        "&#128578;",
        "&#769;",
        "color:#123456",
        "background-color:#654321",
        "color: #abcdef",
        "rgb(11, 22, 33)",
        "font-weight: bold",
        "font-style: italic",
        "text-decoration-style: wavy",
        "line-through",
    ] {
        assert!(html.contains(fragment), "missing {fragment}: {html}");
    }
    assert!(!html.contains("var("));
    // Limits stop the writer, rather than first sizing unbounded styled output.
    assert!(engine.format_copy(true, 32).is_err());
    assert!(engine.format_copy(false, plain.text.len() - 1).is_err());
    assert_eq!(
        engine.format_copy(false, plain.text.len()).unwrap().0,
        plain.text
    );
}

#[test]
fn rich_copy_tracks_screen_reverse_colors_without_changing_explicit_colors() {
    use gmux_client::clipboard::Formats;
    let mut engine = Engine::new().unwrap();
    engine.resize(80, 6, 0, 0).unwrap();
    engine.write(b"\x1b]10;#123456\x07\x1b]11;#654321\x07\x1b]4;1;#abcdef\x07");
    engine.write(b"plain\x1b[31;48;2;11;22;33mstyled\x1b[0m");
    engine.select((0, 0), (10, 0), SelectionMode::Cell).unwrap();
    for (mode, foreground, background) in [
        (b"\x1b[?5l", [0x12, 0x34, 0x56], [0x65, 0x43, 0x21]),
        (b"\x1b[?5h", [0x65, 0x43, 0x21], [0x12, 0x34, 0x56]),
        (b"\x1b[?5l", [0x12, 0x34, 0x56], [0x65, 0x43, 0x21]),
    ] {
        engine.write(mode);
        engine.snapshot().unwrap();
        assert_eq!(engine.frame.foreground, foreground);
        assert_eq!(engine.frame.background, background);
        let content = engine.copy_content(Formats::TextHtml).unwrap();
        assert_eq!(content.text, "plainstyled");
        assert_eq!(engine.copy().unwrap(), content.text);
        let html = content.html.unwrap();
        assert!(
            html.contains(&format!(
                "color:#{:02x}{:02x}{:02x};background-color:#{:02x}{:02x}{:02x}",
                foreground[0],
                foreground[1],
                foreground[2],
                background[0],
                background[1],
                background[2],
            )),
            "{html}"
        );
        assert!(html.contains("color: #abcdef"), "{html}");
        assert!(html.contains("background-color: rgb(11, 22, 33)"), "{html}");
    }
}

#[test]
fn rich_copy_falls_back_after_bounded_styled_expansion_and_keeps_view_selection() {
    use gmux_client::clipboard::Formats;
    let mut engine = Engine::new().unwrap();
    engine.resize(1000, 100, 0, 0).unwrap();
    engine.write(
        "\x1b[1;3;4:3;9;38;2;11;22;33;48;2;44;55;66mx\x1b[38;2;77;88;99mx"
            .repeat(50_000)
            .as_bytes(),
    );
    let mut first = engine.new_view().unwrap();
    let mut second = engine.new_view().unwrap();
    engine.with_view(&mut first, |e| {
        e.select((0, 0), (999, 99), SelectionMode::Cell).unwrap();
        let content = e.copy_content(Formats::TextHtml).unwrap();
        assert_eq!(content.text.len(), 100_000);
        assert!(content.html.is_none());
    });
    engine.with_view(&mut second, |e| {
        assert!(e.selection().is_none());
        e.select((1, 0), (2, 0), SelectionMode::Cell).unwrap();
        let content = e.copy_content(Formats::TextHtml).unwrap();
        assert_eq!(content.text, "xx");
        assert!(content.html.is_some());
    });
    engine.with_view(&mut first, |e| assert_eq!(e.copy().unwrap().len(), 100_000));
}

#[test]
#[ignore = "copy timing and transient memory measurements"]
fn copy_measurements() {
    use gmux_client::clipboard::Formats;
    let case = std::env::var("GMUX_COPY_CASE").unwrap_or_else(|_| "uniform".into());
    let cells = std::env::var("GMUX_COPY_CELLS")
        .ok()
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(100_000);
    let formats = if std::env::var("GMUX_COPY_FORMATS").as_deref() == Ok("text-html") {
        Formats::TextHtml
    } else {
        Formats::Text
    };
    assert!(cells.is_multiple_of(1000) && cells <= 131_000);
    let mut engine = Engine::new().unwrap();
    engine.resize(1000, (cells / 1000) as u16, 0, 0).unwrap();
    let sequence = match case.as_str() {
        "uniform" => "x",
        "styled" => "\x1b[31mx\x1b[32mx",
        "heavy" => "\x1b[1;3;4:3;9;38;2;11;22;33;48;2;44;55;66mx\x1b[38;2;77;88;99mx",
        _ => panic!("unknown case"),
    };
    let characters = if case == "uniform" { 1 } else { 2 };
    engine.write(sequence.repeat(cells / characters).as_bytes());
    engine
        .select(
            (0, 0),
            (999, (cells / 1000 - 1) as u32),
            SelectionMode::Cell,
        )
        .unwrap();
    let memory = || {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let value = |key| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        };
        (value("VmRSS:"), value("VmHWM:"))
    };
    // Exclude fixture ingestion from the copy high-water mark on Linux.
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let before = memory();
    let content = engine.copy_content(formats).unwrap();
    let during = memory();
    let text_bytes = content.text.len();
    let html_bytes = content.html.as_ref().map_or(0, String::len);
    drop(content);
    let started = std::time::Instant::now();
    let iterations = 50;
    for _ in 0..iterations {
        let content = engine.copy_content(formats).unwrap();
        assert_eq!(content.text.len(), text_bytes);
        assert_eq!(content.html.as_ref().map_or(0, String::len), html_bytes);
        std::hint::black_box(content);
    }
    let copy_us = started.elapsed().as_secs_f64() * 1e6 / f64::from(iterations);
    let after = memory();
    eprintln!(
        "{}",
        serde_json::json!({"case":case, "formats":formats, "cells":cells, "copy_us":copy_us, "text_bytes":text_bytes, "html_bytes":html_bytes, "before_rss_kib":before.0, "offer_rss_kib":during.0, "after_rss_kib":after.0, "peak_growth_kib":after.1.saturating_sub(before.1)})
    );
}
