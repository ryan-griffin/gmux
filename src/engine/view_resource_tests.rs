use super::*;

#[test]
fn admission_and_native_capture_failure_release_reservations_with_either_owner_lifetime() {
    let resources = Rc::new(ViewResources::new(1024 * 1024, 1));
    let mut engine = Engine::new().unwrap();
    let mut view = engine.new_view_in(&resources).unwrap();
    assert!(engine.new_view_in(&resources).is_err());
    assert_eq!(resources.count(), 1);
    engine.with_view(&mut view, |e| e.find("needle")).unwrap();
    engine.write(b"\x1b]8;;https://example.com/needle\x07needle\x1b]8;;\x07\x1b[?2026h");
    assert!(view.check_resources().is_ok());
    assert!(resources.render_bytes() > 0);
    assert!(resources.native_reserved() > resources.render_bytes());
    view.set_active(false);
    assert_eq!(resources.native_reserved(), 0);
    assert!(view.find_pending());
    drop(engine);
    drop(view);
    assert_eq!(resources.count(), 0);
    assert_eq!(resources.search.used(), 0);

    // Enough for link reservation but no render state. The failure is sticky
    // and cannot make the frontend publish the live, incomplete sync frame.
    let resources = Rc::new(ViewResources::new(720_896, 2));
    let mut engine = Engine::new().unwrap();
    let mut view = engine.new_view_in(&resources).unwrap();
    engine.write(b"\x1b[?2026h");
    assert!(view.check_resources().is_err());
    assert!(
        engine
            .with_view(&mut view, |_| panic!("failed capture admitted"))
            .is_err()
    );
    drop(view);
    assert_eq!(resources.native_reserved(), 0);
    assert_eq!(resources.count(), 0);
    assert!(engine.new_view_in(&resources).is_ok());
}

#[test]
fn link_free_views_do_not_hold_the_full_hyperlink_reservation() {
    let resources = Rc::new(ViewResources::new(8 * 1024 * 1024, 32));
    let mut engine = Engine::new().unwrap();
    let views: Vec<_> = (0..32)
        .map(|_| engine.new_view_in(&resources).unwrap())
        .collect();
    engine.write(b"before\x1b[?2026hafter");
    assert!(views.iter().all(|view| view.check_resources().is_ok()));
    assert_eq!(resources.native_reserved(), resources.render_bytes());
    drop(views);
    assert_eq!(resources.native_reserved(), 0);
}

#[test]
fn scopes_reject_foreign_and_detached_views_without_entering_or_changing_appearance() {
    let mut owner = Engine::new().unwrap();
    let mut other = Engine::new().unwrap();
    let resources = Rc::default();
    let mut view = owner.new_view_in(&resources).unwrap();
    view.set_theme(&Theme::adwaita(false));
    owner.write(b"owner");
    other.write(b"other");
    assert!(
        other
            .with_view(&mut view, |_| panic!("foreign scope admitted"))
            .is_err()
    );
    assert!(other.use_view_appearance(&view).is_err());
    assert_eq!(other.text().unwrap(), "other");
    assert_eq!(other.frame.background, Theme::adwaita(true).background);
    owner
        .with_view(&mut view, |active| {
            // Native callers must also reject nested entry without disturbing
            // the outer scope's presentation or its eventual leave.
            assert!(!unsafe {
                gmux_engine_view_enter(active.engine.raw.as_ptr(), active.view.raw.as_ptr())
            });
            assert!(active.engine.use_view_appearance(active.view).is_err());
            assert_eq!(active.text().unwrap(), "owner");
            assert_eq!(active.frame().background, Theme::adwaita(false).background);
        })
        .unwrap();
    drop(owner);
    let mut replacement = Engine::new().unwrap();
    assert!(
        replacement
            .with_view(&mut view, |_| panic!("detached scope admitted"))
            .is_err()
    );
    assert!(replacement.use_view_appearance(&view).is_err());
    drop(view);
    assert_eq!(resources.count(), 0);
    assert_eq!(resources.native_reserved(), 0);
}

#[test]
fn unwinding_restores_native_view_search_and_normal_or_held_snapshot_scratch() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    for held in [false, true] {
        let mut engine = Engine::new().unwrap();
        engine.resize(20, 2, 0, 0).unwrap();
        engine.write(b"\x1b]8;;test:alpha\x07alpha\x1b]8;;\x07\r\nbeta\r\n\x1b]8;;test:gamma\x07gamma\x1b]8;;\x07");
        let resources = Rc::default();
        let mut first = engine.new_view_in(&resources).unwrap();
        let mut second = engine.new_view_in(&resources).unwrap();
        first.set_theme(&Theme::adwaita(false));
        engine
            .with_view(&mut first, |active| active.scroll_to(0))
            .unwrap();
        engine.find("a");
        while engine.find_pending() {
            engine.poll_find();
        }
        engine.snapshot().unwrap();
        let root_cells = engine.cells.as_ptr();
        let root_frame = engine.frame;
        let root_links = engine.hyperlinks.clone();
        if held {
            engine.write(b"\x1b[?2026h\r\x1b[2Khidden");
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            engine
                .with_view(&mut first, |active| {
                    active.snapshot().unwrap();
                    assert!(active.text().unwrap().starts_with("alpha"));
                    active.select((0, 0), (4, 0), SelectionMode::Cell).unwrap();
                    active.find("alpha");
                    while active.find_pending() {
                        active.poll_find();
                    }
                    panic!("unwind an entered view");
                })
                .unwrap();
        }));
        assert!(result.is_err());
        assert_eq!(engine.cells.as_ptr(), root_cells);
        assert!(engine.selection().is_none());
        assert_eq!(engine.find_state().total, 5);
        if held {
            assert!(engine.frame == root_frame);
            assert_eq!(engine.hyperlinks, root_links);
        }
        engine
            .with_view(&mut second, |active| {
                assert!(active.selection().is_none());
                assert_eq!(active.find_state().total, 0);
                assert!(active.text().unwrap().contains("gamma"));
                assert_eq!(active.frame().background, Theme::adwaita(true).background);
            })
            .unwrap();
        engine
            .with_view(&mut first, |active| {
                assert_eq!(active.copy().unwrap(), "alpha");
                assert_eq!(active.find_state().total, 1);
                assert!(active.text().unwrap().starts_with("alpha"));
                assert_eq!(active.frame().background, Theme::adwaita(false).background);
            })
            .unwrap();
        if held {
            engine.write(b"\x1b[?2026l");
        }
        assert!(
            engine
                .text()
                .unwrap()
                .contains(if held { "hidden" } else { "gamma" })
        );
        assert_eq!(engine.frame.background, Theme::adwaita(true).background);
        drop(engine);
        drop(first);
        drop(second);
        assert_eq!(resources.count(), 0);
        assert_eq!(resources.native_reserved(), 0);
        assert_eq!(resources.search.used(), 0);
    }
}
