use super::*;
use gmux_client::retention::{Limit, LimitMode, ScrollbackLimits};

fn limits(bytes: Option<usize>, lines: Option<usize>) -> ScrollbackLimits {
    let limit =
        |value: Option<usize>| value.map_or(Limit::Mode(LimitMode::Unlimited), Limit::Count);
    ScrollbackLimits {
        max_bytes: limit(bytes),
        max_lines: limit(lines),
    }
}
fn history() -> Engine {
    let mut engine = Engine::new().unwrap();
    engine.set_scrollback_limits(limits(None, None)).unwrap();
    engine.resize(80, 6, 0, 0).unwrap();
    for row in 0..1800 {
        engine.write(format!("\x1b]133;A\x07prompt{row:04}\x1b]133;B\x07cmd\r\n\x1b]133;C\x07\x1b[31mrow{row:04} 界é\x1b[0m\r\n\x1b]133;D;0\x07").as_bytes());
    }
    engine.snapshot().unwrap();
    engine
}
fn finish_find(engine: &mut Engine) -> usize {
    while engine.find_pending() {
        engine.poll_find();
    }
    engine.find_state().total
}

#[test]
fn immediate_pruning_updates_views_search_selection_prompts_and_history_identity() {
    let mut engine = history();
    let mut old = engine.new_view().unwrap();
    let mut retained = engine.new_view().unwrap();
    let total = engine.frame.scrollback.total;
    let origin = engine.history.origin;
    let generation = engine.history.generation;
    let revision = engine.revision;
    engine.with_view(&mut old, |e| {
        e.scroll_to(0);
        e.snapshot().unwrap();
        e.select((0, 0), (15, 1), SelectionMode::Cell).unwrap();
        e.anchor_output((0, 1)).unwrap();
        e.find("row");
        // Leave a search in flight to catch scanning against the old origin.
        e.poll_find();
        assert!(e.find_pending());
    });
    let (selected, cells) = engine.with_view(&mut retained, |e| {
        e.scroll_to(total - 40);
        e.snapshot().unwrap();
        e.select(
            (0, total as u32 - 39),
            (15, total as u32 - 37),
            SelectionMode::Rectangle,
        )
        .unwrap();
        e.find("row179");
        assert!(finish_find(e) > 0);
        e.anchor_output((0, total as u32 - 38)).unwrap();
        (e.copy().unwrap(), e.cells.clone())
    });
    assert!(selected.contains("界é"));
    let active = engine.text().unwrap();
    engine
        .set_scrollback_limits(limits(None, Some(160)))
        .unwrap();
    engine.snapshot().unwrap();
    let remaining = engine.frame.scrollback.total;
    let removed = total - remaining;
    assert!(
        removed > 3000 && remaining > 6 && remaining < 400,
        "remaining={remaining}, removed={removed}"
    );
    assert_eq!(engine.history.generation, generation);
    assert_eq!(engine.history.origin, origin + removed);
    assert_ne!(engine.revision, revision);
    assert_eq!(engine.text().unwrap(), active);
    engine.with_view(&mut old, |e| {
        e.snapshot().unwrap();
        assert!(e.copy().unwrap().is_empty());
        assert_eq!(e.frame.scrollback.offset, 0);
        assert!(e.select_anchored_output().is_err());
        assert!(e.find_state().highlights.is_empty());
        assert!(finish_find(e) as u64 <= remaining / 2 + 1);
        e.scroll_to(0);
        assert!(e.prompt(true).is_err());
        e.scroll(0, true);
        e.prompt(true).unwrap();
        e.snapshot().unwrap();
        assert!(e.frame.scrollback.offset < remaining - 6);
    });
    engine.with_view(&mut retained, |e| {
        e.snapshot().unwrap();
        assert_eq!(e.copy().unwrap(), selected);
        assert!(
            e.cells == cells,
            "Pruning must preserve retained text and rendition"
        );
        assert_eq!(e.frame.scrollback.offset, total - 40 - removed);
        assert!(finish_find(e) > 0);
        assert!(matches!(e.selection().unwrap().shape, Shape::Rectangle));
        e.select_anchored_output().unwrap();
        assert!(e.copy().unwrap().contains("row"));
    });
}

#[test]
fn byte_and_line_limits_are_independent_zero_is_native_and_active_application_survives() {
    let mut engine = history();
    engine
        .set_scrollback_limits(limits(Some(128 * 1024), None))
        .unwrap();
    engine.snapshot().unwrap();
    let byte_rows = engine.frame.scrollback.total;
    assert!(byte_rows < 3600);
    engine.set_scrollback_limits(limits(None, Some(0))).unwrap();
    engine.snapshot().unwrap();
    let line_rows = engine.frame.scrollback.total;
    assert!(line_rows > 6 && line_rows < byte_rows);
    // Byte zero erases even a partial page. Keep active-area selections valid.
    let mut retained = engine.new_view().unwrap();
    let mut erased = engine.new_view().unwrap();
    engine.with_view(&mut retained, |e| {
        e.select(
            (0, line_rows as u32 - 2),
            (12, line_rows as u32 - 2),
            SelectionMode::Cell,
        )
        .unwrap();
    });
    engine.with_view(&mut erased, |e| {
        e.select((0, 0), (12, 0), SelectionMode::Rectangle).unwrap();
    });
    let selected = engine.with_view(&mut retained, |e| e.copy().unwrap());
    engine.write(b"\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[2;3Happ\x1b[38;5;42m");
    let alternate = engine.text().unwrap();
    engine.set_scrollback_limits(limits(Some(0), None)).unwrap();
    assert_eq!(engine.text().unwrap(), alternate);
    assert!(engine.mode(2004) && engine.mode(1004));
    engine.write(b"\x1b[?1049l");
    engine.snapshot().unwrap();
    assert_eq!(engine.frame.scrollback.total, 6);
    engine.with_view(&mut retained, |e| assert_eq!(e.copy().unwrap(), selected));
    engine.with_view(&mut erased, |e| assert!(e.copy().unwrap().is_empty()));
    engine.write(b"more\r\n".repeat(400).as_slice());
    engine.snapshot().unwrap();
    assert_eq!(engine.frame.scrollback.total, 6);
    engine.set_scrollback_limits(limits(None, None)).unwrap();
    engine.write(b"again\r\n".repeat(400).as_slice());
    engine.snapshot().unwrap();
    assert!(engine.frame.scrollback.total >= 400);
}

#[test]
fn restored_history_obeys_limits_before_replay_and_keeps_fresh_shell_state() {
    let source = history();
    let checkpoint = source.checkpoint().unwrap();
    for policy in [limits(None, Some(80)), limits(Some(0), None)] {
        let mut fresh = Engine::new().unwrap();
        fresh.set_scrollback_limits(policy).unwrap();
        fresh.restore_history(&checkpoint).unwrap();
        fresh.snapshot().unwrap();
        assert!(fresh.frame.scrollback.total < 350);
        if policy.max_bytes == Limit::Count(0) {
            assert_eq!(fresh.frame.scrollback.total, 6);
        }
        assert!(!fresh.mode(2004));
        assert_eq!(
            fresh.command_state().phase,
            gmux_client::shell::CommandPhase::Unknown
        );
        assert_eq!(fresh.memory_usage().unwrap().prompt_anchors, 0);
    }
}

#[test]
fn pruning_during_a_render_hold_refreshes_each_view_after_expiry_without_output() {
    for policy in [limits(None, Some(160)), limits(Some(0), None)] {
        let mut engine = history();
        let mut first = engine.new_view().unwrap();
        let mut second = engine.new_view().unwrap();
        let total = engine.frame.scrollback.total;
        let original = [(0, &mut first), (total - 40, &mut second)]
            .into_iter()
            .map(|(offset, view)| {
                engine.with_view(view, |e| {
                    e.scroll_to(offset);
                    e.snapshot().unwrap();
                    (e.frame, e.cells.clone(), e.history)
                })
            })
            .collect::<Vec<_>>();
        let pages = engine.memory_usage().unwrap().pages;
        engine.write(b"\x1b[?2026h");
        engine.set_scrollback_limits(policy).unwrap();
        assert!(
            engine.mode(2026),
            "Retention must preserve the application's hold"
        );
        assert!(engine.memory_usage().unwrap().pages < pages);
        for (view, (frame, cells, history)) in [&mut first, &mut second].into_iter().zip(&original)
        {
            engine.with_view(view, |e| {
                e.snapshot().unwrap();
                assert!(e.frame == *frame && e.cells == *cells);
                assert!(e.history == *history);
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(1050));
        assert!(engine.expire_render_hold());
        for (view, (frame, _, _)) in [&mut first, &mut second].into_iter().zip(&original) {
            engine.with_view(view, |e| {
                e.snapshot().unwrap();
                let remaining = e.frame.scrollback.total;
                assert!(remaining < 400);
                let removed = total - remaining;
                assert_eq!(
                    e.frame.scrollback.offset,
                    frame.scrollback.offset.saturating_sub(removed)
                );
                assert!(e.history != original[0].2);
            });
        }
    }
}
