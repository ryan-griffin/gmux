use super::*;

#[test]
fn rectangles_copy_physical_rows_blanks_and_complete_graphemes_in_every_direction() {
    let mut engine = Engine::new().unwrap();
    engine.resize(10, 6, 8, 16).unwrap();
    engine.write("01界e\u{301}XYZ\r\nabcdefgh\r\nq\r\n\r\nlast".as_bytes());
    for (left, right, top, bottom, expected) in [
        (2, 4, 0, 1, "界e\u{301}\ncde"),
        (3, 3, 0, 1, "界\nd"),
        (2, 2, 0, 1, "界\nc"),
        (3, 4, 0, 0, "界e\u{301}"),
        (1, 3, 2, 4, "   \n   \nast"),
        (8, 9, 0, 5, "  \n  \n  \n  \n  \n  "),
    ] {
        for (start, end) in [
            ((left, top), (right, bottom)),
            ((right, bottom), (left, top)),
            ((right, top), (left, bottom)),
            ((left, bottom), (right, top)),
        ] {
            engine.clear_selection();
            engine.select(start, end, SelectionMode::Rectangle).unwrap();
            assert_eq!(
                engine.selection(),
                Some(Selection::rectangle(
                    u64::from(top) * 10 + u64::from(left),
                    u64::from(bottom) * 10 + u64::from(right),
                    10
                ))
            );
            assert_eq!(engine.copy().unwrap(), expected, "{start:?}..{end:?}");
        }
    }
    engine.clear_selection();
    engine.select((0, 0), (2, 1), SelectionMode::Cell).unwrap();
    assert_eq!(engine.selection().unwrap().shape, Shape::Linear);
    assert_eq!(engine.copy().unwrap(), "01界e\u{301}XYZ\nabc");

    engine.clear_screen().unwrap();
    engine.resize(8, 4, 8, 16).unwrap();
    engine.write("1234567界Z".as_bytes());
    engine
        .select((0, 0), (7, 1), SelectionMode::Rectangle)
        .unwrap();
    assert_eq!(engine.copy().unwrap(), "1234567 \n界Z     ");
    engine.clear_selection();
    engine
        .select((7, 0), (7, 1), SelectionMode::Rectangle)
        .unwrap();
    assert_eq!(engine.copy().unwrap(), " \n ");
    let mut guarded = [0xee; 4];
    let mut written = 0;
    let result = unsafe {
        gmux_engine_copy(
            engine.raw.as_ptr(),
            guarded.as_mut_ptr().add(1),
            1,
            &mut written,
        )
    };
    assert_eq!((result, written), (1, 3));
    assert_eq!(guarded, [0xee, b' ', 0xee, 0xee]);
}

#[test]
fn rectangle_wraps_reflow_mutation_and_screen_changes_use_the_displayed_bounds() {
    let mut engine = Engine::new().unwrap();
    engine.resize(8, 6, 8, 16).unwrap();
    engine.write(b"abcdefghABCDEFGH12");
    engine
        .select((1, 0), (3, 2), SelectionMode::Rectangle)
        .unwrap();
    assert_eq!(engine.copy().unwrap(), "bcd\nBCD\n2  ");
    engine.clear_selection();
    engine
        .select((2, 0), (4, 1), SelectionMode::Rectangle)
        .unwrap();
    engine.resize(4, 6, 8, 16).unwrap();
    assert_eq!(engine.selection(), Some(Selection::rectangle(2, 12, 4)));
    assert_eq!(engine.copy().unwrap(), "abc\nefg\nABC\nEFG");
    engine.write(b"\x1b[HXYZ");
    assert_eq!(engine.copy().unwrap(), "XYZ\nefg\nABC\nEFG");
    engine.write(b"\x1b[?1049hOTHER");
    assert_eq!(engine.selection(), None);
    assert_eq!(engine.copy().unwrap(), "");
    engine.write(b"\x1b[?1049l");
    assert_eq!(engine.copy().unwrap(), "XYZ\nefg\nABC\nEFG");
    engine.clear_screen().unwrap();
    assert_eq!(engine.selection(), None);
    assert_eq!(engine.copy().unwrap(), "");
}

#[test]
fn rectangles_belong_to_views_and_follow_retained_rows_until_pruned() {
    let mut engine = Engine::new().unwrap();
    engine.resize(80, 4, 8, 16).unwrap();
    engine.write(b"01234567\r\nabcdefgh\r\n");
    let mut first = engine.new_view().unwrap();
    let mut second = engine.new_view().unwrap();
    engine.with_view(&mut first, |view| {
        view.select((2, 0), (4, 1), SelectionMode::Rectangle)
            .unwrap();
        assert_eq!(view.copy().unwrap(), "234\ncde");
    });
    engine.with_view(&mut second, |view| {
        assert_eq!(view.selection(), None);
        view.select((0, 0), (7, 0), SelectionMode::Cell).unwrap();
    });
    engine.write("retained row\r\n".repeat(512).as_bytes());
    engine.with_view(&mut first, |view| {
        view.scroll(0, true);
        view.snapshot().unwrap();
        assert_eq!(view.copy().unwrap(), "234\ncde");
        assert_eq!(view.selection().unwrap().shape, Shape::Rectangle);
    });
    engine.with_view(&mut second, |view| {
        assert_eq!(view.copy().unwrap(), "01234567");
        assert_eq!(view.selection().unwrap().shape, Shape::Linear);
    });
    for _ in 0..128 {
        engine.write("pruning row\r\n".repeat(512).as_bytes());
        if engine.with_view(&mut first, |view| view.selection().is_none()) {
            break;
        }
    }
    engine.with_view(&mut first, |view| {
        assert_eq!(view.selection(), None);
        assert_eq!(view.copy().unwrap(), "");
    });
}
