use super::*;

#[test]
fn admission_and_native_capture_failure_release_reservations_with_either_owner_lifetime() {
    let resources = Rc::new(ViewResources::new(1024 * 1024, 1));
    let mut engine = Engine::new().unwrap();
    let mut view = engine.new_view_in(&resources).unwrap();
    assert!(engine.new_view_in(&resources).is_err());
    assert_eq!(resources.count(), 1);
    engine.with_view(&mut view, |e| e.find("needle"));
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
    let view = engine.new_view_in(&resources).unwrap();
    engine.write(b"\x1b[?2026h");
    assert!(view.check_resources().is_err());
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
