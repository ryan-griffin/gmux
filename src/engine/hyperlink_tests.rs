use super::*;

fn show(engine: &mut Engine, text: &str) {
    engine.write(text.as_bytes());
    engine.snapshot().unwrap();
}
fn link(uri: &str, text: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
}

#[test]
fn osc8_targets_follow_wide_wrapped_text_target_only_edits_clear_and_resize() {
    let mut e = Engine::new().unwrap();
    e.resize(6, 4, 0, 0).unwrap();
    show(&mut e, &link("https://actual.example/a", "ab界cdef"));
    assert_eq!(e.hyperlinks.targets, ["https://actual.example/a"]);
    for i in 0..8 {
        assert_eq!(
            e.hyperlinks.target_at(i),
            Some("https://actual.example/a"),
            "cell {i}"
        );
    }
    assert_eq!(e.hyperlinks.target_at(8), None);
    let cells = e.cells.clone();
    show(
        &mut e,
        &format!("\x1b[H{}", link("https://different.example/b", "ab界cdef")),
    );
    assert!(e.cells == cells, "Only the OSC 8 target changed");
    assert_eq!(
        e.hyperlinks.target_at(3),
        Some("https://different.example/b")
    );
    e.resize(4, 5, 0, 0).unwrap();
    e.snapshot().unwrap();
    assert!(e.hyperlinks.spans.iter().all(|span| span[1] <= 20));
    assert!(
        e.hyperlinks
            .targets
            .iter()
            .all(|uri| uri == "https://different.example/b")
    );
    show(&mut e, "\x1b[H\x1b[2Jordinary https://unlinked.example/");
    assert_eq!(e.hyperlinks.memory_size(), 0);
    assert!(e.hyperlinks.targets.is_empty());
}

#[test]
fn long_repeated_links_are_interned_and_over_budget_snapshots_have_no_links() {
    let mut e = Engine::new().unwrap();
    e.resize(80, 24, 0, 0).unwrap();
    let uri = format!("https://example/{}", "x".repeat(1500));
    show(&mut e, &link(&uri, &"x".repeat(80 * 20)));
    assert_eq!(e.hyperlinks.targets, [uri]);
    assert_eq!(e.hyperlinks.spans, [[0, 1600, 0]]);
    let mut many = String::from("\x1b[H\x1b[2J");
    for i in 0..200 {
        many.push_str(&link(
            &format!("https://example/{i}/{}", "x".repeat(1500)),
            "x",
        ));
    }
    show(&mut e, &many);
    assert!(e.hyperlinks.spans.is_empty());
    show(
        &mut e,
        &format!("\x1b[H\x1b[2J{}", link("https://fresh/", "x")),
    );
    assert_eq!(e.hyperlinks.target_at(0), Some("https://fresh/"));
}

#[test]
fn independent_scrolled_views_and_hold_start_keep_their_own_targets() {
    let mut e = Engine::new().unwrap();
    e.resize(12, 3, 0, 0).unwrap();
    for i in 0..8 {
        e.write(
            format!(
                "{}\r\n",
                link(&format!("https://row/{i}"), &format!("row {i}"))
            )
            .as_bytes(),
        );
    }
    let mut first = e.new_view().unwrap();
    let mut second = e.new_view().unwrap();
    let read = |e: &mut Engine| {
        e.snapshot().unwrap();
        e.hyperlinks.clone()
    };
    let top = e.with_view(&mut first, |e| {
        e.scroll_to(0);
        read(e)
    });
    let bottom = e.with_view(&mut second, read);
    assert_ne!(top, bottom);
    assert_eq!(top.target_at(0), Some("https://row/0"));
    // Leave the shared normal cache on a different viewport before the hold.
    assert_eq!(e.with_view(&mut first, read), top);
    // Capture at the hold boundary, before a same-chunk live target rewrite.
    e.write(format!("\x1b[?2026h\x1b[H{}", link("https://hidden/", "row 6")).as_bytes());
    assert_eq!(e.with_view(&mut first, read), top);
    assert_eq!(e.with_view(&mut second, read), bottom);
    assert_eq!(e.with_view(&mut first, read), top);
    e.write(b"\x1b[?2026l");
    let updated = e.with_view(&mut second, read);
    assert_ne!(updated, bottom);
    assert_eq!(
        updated.target_at(12),
        bottom.target_at(12),
        "Clean rows after a hold must use the bottom view"
    );
    assert_eq!(e.with_view(&mut first, read), top);
    // A fresh client receives a fresh bottom snapshot, including its links.
    drop(second);
    let mut reconnected = e.new_view().unwrap();
    let latest = e.with_view(&mut reconnected, read);
    assert_eq!(latest.target_at(0), Some("https://hidden/"));
}

#[test]
fn variable_rust_hold_scratch_releases_after_release_expiry_and_host_clear() {
    for ending in 0..3 {
        let mut e = Engine::new().unwrap();
        let mut view = e.new_view().unwrap();
        show(&mut e, &link("test:held", "visible"));
        e.write(b"\x1b[?2026h\x1b[H\x1b[2Jplain");
        e.with_view(&mut view, |e| {
            e.snapshot().unwrap();
            assert_eq!(e.hyperlinks.target_at(0), Some("test:held"));
        });
        assert!(e.held_hyperlinks.memory_size() > 0);
        match ending {
            0 => {
                e.write(b"\x1b[?2026l");
            }
            1 => {
                std::thread::sleep(std::time::Duration::from_millis(1050));
            }
            _ => e.clear_screen().unwrap(),
        }
        e.snapshot().unwrap();
        assert_eq!(e.held_hyperlinks.memory_size(), 0);
        assert_eq!(e.hyperlinks.memory_size(), 0);
    }
}

#[test]
fn partial_link_rows_match_full_capture_after_edits_erase_and_row_remapping() {
    let mut e = Engine::new().unwrap();
    e.resize(8, 4, 0, 0).unwrap();
    for row in 0..4 {
        e.write(
            format!(
                "\x1b[{};1H{}",
                row + 1,
                link(&format!("test:row/{row}"), "same界")
            )
            .as_bytes(),
        );
    }
    e.snapshot().unwrap();
    let mut dark = true;
    let mut verify_full = |e: &mut Engine| {
        e.snapshot().unwrap();
        let incremental = e.hyperlinks.clone();
        dark = !dark;
        e.set_theme(&Theme::adwaita(dark)).unwrap();
        e.snapshot().unwrap();
        assert_eq!(
            e.hyperlinks, incremental,
            "Partial capture disagreed with a full palette-driven capture"
        );
    };
    let before = e.cells.clone();
    e.write(format!("\x1b[2;1H{}", link("test:changed-target", "same界")).as_bytes());
    verify_full(&mut e);
    assert_eq!(e.hyperlinks.target_at(8), Some("test:changed-target"));
    // Cell text/rendition before the palette change was identical; this row
    // must still resolve its new target rather than comparing raw cell bits.
    assert!(before.iter().zip(&e.cells).all(|(a, b)| a.text_eq(b)));
    for sequence in [
        "\x1b[1;1H\x1b[2K",
        "\x1b[2;4r\x1b[2;1H\x1b[L",
        "\x1b[3;1H\x1b[M",
        "\x1b[r\x1b[4;1H\r\nnext",
        "\x1b[?1049h",
        "\x1b[?1049l",
    ] {
        e.write(sequence.as_bytes());
        verify_full(&mut e);
    }
    for width in [5, 11, 8] {
        e.resize(width, 4, 0, 0).unwrap();
        verify_full(&mut e);
    }
}

#[test]
fn invalid_utf8_targets_discard_the_complete_map_before_transport() {
    let mut e = Engine::new().unwrap();
    e.write(b"\x1b]8;;test:invalid/\xff\x07bad\x1b]8;;\x07");
    e.write(link("test:valid", "good").as_bytes());
    e.snapshot().unwrap();
    assert!(e.hyperlinks.spans.is_empty());
    assert_eq!(e.hyperlinks.memory_size(), 0);
    show(
        &mut e,
        &format!("\x1b[H\x1b[2J{}", link("test:valid", "good")),
    );
    assert_eq!(e.hyperlinks.target_at(0), Some("test:valid"));
}

#[test]
fn span_limit_discards_the_whole_map_and_recovers_after_a_partial_erase() {
    use gmux_client::hyperlinks::MAX_SPANS;
    let mut e = Engine::new().unwrap();
    e.resize(1000, 33, 0, 0).unwrap();
    // Keep one Ghostty link open; repeated OSC starts have their own upstream
    // per-page entry budget and do not exercise our span limit.
    let mut text = String::from("\x1b]8;;test:one\x07");
    for index in 0..MAX_SPANS {
        text.push_str(&format!(
            "\x1b[{};{}Hx",
            index * 2 / 1000 + 1,
            index * 2 % 1000 + 1
        ));
    }
    text.push_str("\x1b]8;;\x07");
    show(&mut e, &text);
    assert_eq!(e.hyperlinks.spans.len(), MAX_SPANS);
    show(&mut e, &format!("\x1b[33;769H{}", link("test:one", "x")));
    assert!(e.hyperlinks.spans.is_empty());
    assert_eq!(e.hyperlinks.memory_size(), 0);
    // An invalid prior map must not suppress clean-row targets on recovery.
    show(&mut e, "\x1b[33;1H\x1b[2K");
    assert_eq!(e.hyperlinks.spans.len(), 16_000);
    assert_eq!(e.hyperlinks.target_at(0), Some("test:one"));
}

#[test]
fn history_prefetch_preserves_viewport_targets_and_cannot_read_held_output() {
    let mut e = Engine::new().unwrap();
    e.resize(8, 3, 0, 0).unwrap();
    for row in 0..10 {
        e.write(format!("{}\r\n", link(&format!("test:row/{row}"), "wide界wrap")).as_bytes());
    }
    e.scroll_to(0);
    e.snapshot().unwrap();
    let frame = e.frame;
    let cells = e.cells.clone();
    let links = e.hyperlinks.clone();
    let revision = e.link_revision;
    for start in [0, 3, 5] {
        let (prefetched, _) = e.cache_rows(start..start + 3).unwrap();
        assert_eq!(prefetched.len(), 24);
        e.snapshot().unwrap();
        assert!(e.frame == frame);
        assert!(e.cells == cells);
        assert_eq!(e.hyperlinks, links);
        assert_eq!(e.link_revision, revision);
    }
    e.write(format!("\x1b[?2026h\x1b[H{}", link("test:hidden", "new")).as_bytes());
    e.snapshot().unwrap();
    assert!(e.cache_rows(0..3).is_err());
    assert_eq!(e.hyperlinks, links);
    e.write(b"\x1b[?2026l");
    e.scroll(0, true);
    e.snapshot().unwrap();
    assert_eq!(e.hyperlinks.target_at(0), Some("test:hidden"));
}
