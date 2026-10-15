use crate::engine::Engine;
use gmux_client::search::*;
use gmux_client::theme::Theme;

fn screen(text: &str, cols: u16, rows: u16) -> Engine {
    let mut engine = Engine::new().unwrap();
    engine.set_theme(&Theme::adwaita(true)).unwrap();
    engine.resize(cols, rows, 8, 16).unwrap();
    engine.write(text.as_bytes());
    engine.snapshot().unwrap();
    engine
}
fn history_find(engine: &mut Engine, query: &str) -> FindState {
    engine.find(query);
    for _ in 0..1000 {
        if !engine.find_pending() {
            engine.snapshot().unwrap();
            return engine.find_state();
        }
        engine.poll_find();
    }
    panic!("Find did not finish");
}
fn status(engine: &Engine) -> (usize, usize) {
    let state = engine.find_state();
    (state.current, state.total)
}
#[test]
fn history_find_joins_soft_wraps_but_preserves_hard_line_breaks_and_wide_spacers() {
    let mut engine = screen("123456needle\r\nneed\r\nle", 8, 5);
    let found = history_find(&mut engine, "needle");
    assert_eq!(found.total, 1);
    assert_eq!(found.highlights, vec![(6, 12, 2)]);
    engine.resize(32, 5, 8, 16).unwrap();
    assert_eq!(history_find(&mut engine, "needle").total, 1);
    let mut engine = screen("1234567界É", 8, 3);
    let found = history_find(&mut engine, "7界é");
    assert_eq!(found.total, 1);
    assert_eq!(found.highlights, vec![(6, 11, 2)]);
}
#[test]
fn wrapped_matches_cross_scan_batches_without_duplicate_or_overlapping_matches() {
    let mut engine = screen(&("\r\n".repeat(63) + "123456needle"), 8, 3);
    assert_eq!(history_find(&mut engine, "needle").total, 1);
    let mut engine = screen("aaaaaaaa", 3, 4);
    assert_eq!(history_find(&mut engine, "aa").total, 4);
    assert_eq!(history_find(&mut engine, "aaaaaaaa").total, 1);
    engine.write(b"\x1b[3J\x1b[2J\x1b[Hnew");
    assert_eq!(history_find(&mut engine, "aaaaaaaa").total, 0);
}

#[test]
fn counts_case_insensitive_matches_and_wraps_in_both_directions() {
    let mut engine = screen("one ONE one", 20, 2);
    history_find(&mut engine, "One");
    assert_eq!(status(&engine), (1, 3));
    engine.find_advance(true);
    assert_eq!(status(&engine), (3, 3));
    engine.find_advance(false);
    assert_eq!(status(&engine), (1, 3));
    engine.find_advance(false);
    history_find(&mut engine, "one");
    assert_eq!(status(&engine), (2, 3));
    history_find(&mut engine, "missing");
    engine.find_advance(true);
    assert_eq!(status(&engine), (0, 0));
}

#[test]
fn handles_unicode_and_highlights_the_whole_wide_character() {
    let mut engine = screen("界 É e\u{301} İ", 20, 2);
    let state = history_find(&mut engine, "界");
    assert_eq!(status(&engine), (1, 1));
    assert_eq!(state.highlights, vec![(0, 2, 2)]);
    for query in ["é", "e\u{301}", "i\u{307}"] {
        history_find(&mut engine, query);
        assert_eq!(status(&engine), (1, 1), "{query}");
    }
}

#[test]
fn updates_when_output_changes_and_does_not_join_separate_rows() {
    let mut engine = screen("one one\r\none", 12, 3);
    history_find(&mut engine, "one");
    engine.find_advance(false);
    engine.write(b"\x1b[1;1Hxxx");
    let state = history_find(&mut engine, "one");
    assert_eq!(status(&engine), (1, 2));
    assert_eq!(state.highlights, vec![(4, 7, 2), (12, 15, 1)]);
    history_find(&mut engine, "one one");
    assert_eq!(status(&engine), (0, 0));
    assert!(history_find(&mut engine, "").highlights.is_empty());
}
#[test]
fn highlights_follow_navigation_query_changes_and_viewport_updates() {
    let mut engine = screen("one one", 12, 2);
    assert_eq!(
        history_find(&mut engine, "one").highlights,
        vec![(0, 3, 2), (4, 7, 1)]
    );
    engine.find_advance(false);
    assert_eq!(engine.find_state().highlights, vec![(0, 3, 1), (4, 7, 2)]);
    engine.write(b"\x1b[1;1Hxxx");
    assert_eq!(history_find(&mut engine, "one").highlights, vec![(4, 7, 2)]);
    assert!(history_find(&mut engine, "").highlights.is_empty());
    assert_eq!(history_find(&mut engine, "xxx").highlights, vec![(0, 3, 2)]);
}
