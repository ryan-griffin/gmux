use super::*;
use gmux_client::program_status::{Kind, MAX_RECORDS, State};

fn report(engine: &mut Engine, body: &str) -> bool {
    engine.write(format!("\x1b]7501;{body}\x1b\\").as_bytes())
}
fn flush(engine: &mut Engine) -> bool {
    engine.program_status_mut().take_changed()
}
fn ids(engine: &Engine) -> Vec<&str> {
    engine
        .program_status()
        .records()
        .into_iter()
        .map(|r| r.id.as_str())
        .collect()
}

#[test]
fn native_detection_borrowed_decoding_replacement_and_rejection() {
    let mut engine = Engine::new().unwrap();
    engine.write(b"\x1b]7501;?\x07");
    let reply = String::from_utf8(engine.take_events().0).unwrap();
    assert!(reply.starts_with("\x1b]7501;?"), "{reply:?}");
    let mut body = b"\x1b]7501;state=blocked:kind=permission:progress=42:app=cargo:title=QnVpbGQ=:msg=QXBwcm92ZT8=\x07".to_vec();
    assert!(!engine.write(&body));
    body.fill(0);
    assert!(flush(&mut engine));
    let summary = engine.program_status().summary().unwrap();
    assert_eq!(summary.record.title, "Build");
    assert_eq!(summary.record.message, "Approve?");
    assert_eq!(summary.record.kind, Some(Kind::Permission));
    assert_eq!(summary.record.progress, Some(42));
    for body in [
        "state=future",
        "state=done:id=bad//id",
        "state=done:msg=Cg==",
        "state=done:msg=a",
    ] {
        report(&mut engine, body);
        assert!(!flush(&mut engine), "{body}");
    }
    report(&mut engine, "state=working");
    assert!(flush(&mut engine));
    let r = &engine.program_status().summary().unwrap().record;
    assert_eq!(r.state, State::Working);
    assert_eq!((r.kind, r.progress, r.app.as_deref()), (None, None, None));
    assert!(r.title.is_empty() && r.message.is_empty());
}

#[test]
fn child_priority_nearest_app_and_segment_clears() {
    let mut e = Engine::new().unwrap();
    for body in [
        "state=idle:app=root",
        "state=working:id=build:app=cargo",
        "state=blocked:id=build/test:kind=question",
        "state=error:id=builder/test",
    ] {
        report(&mut e, body);
    }
    flush(&mut e);
    let r = &e.program_status().summary().unwrap().record;
    assert_eq!(r.id, "build/test");
    assert_eq!(r.app.as_deref(), Some("cargo"));
    report(&mut e, "state=working:id=build");
    flush(&mut e);
    assert_eq!(
        e.program_status().summary().unwrap().record.app.as_deref(),
        Some("root")
    );
    report(&mut e, "state=clear:id=build");
    flush(&mut e);
    assert_eq!(ids(&e), ["", "builder/test"]);
    report(&mut e, "state=blocked:id=missing/child");
    flush(&mut e);
    assert_eq!(
        e.program_status().summary().unwrap().record.app.as_deref(),
        Some("root")
    );
    report(&mut e, "state=working:id=background");
    flush(&mut e);
    report(&mut e, "state=working:id=background:progress=17");
    assert!(
        !flush(&mut e),
        "A background update must not republish the same summary"
    );
    report(&mut e, "state=clear");
    assert!(flush(&mut e));
    assert!(ids(&e).is_empty());
}

#[test]
fn eviction_refreshes_identical_reports_and_bounds_retained_text() {
    let mut e = Engine::new().unwrap();
    let title = "eHh4".repeat(64);
    let message = format!("{}eHg=", "eHh4".repeat(682));
    for n in 0..MAX_RECORDS {
        report(
            &mut e,
            &format!("state=working:id=task{n:02}:app=cargo:title={title}:msg={message}"),
        );
    }
    flush(&mut e);
    let bytes = e.program_status().memory_bytes();
    assert!(bytes > 64 * 2048 && bytes < 180_000, "{bytes}");
    report(
        &mut e,
        &format!("state=working:id=task00:app=cargo:title={title}:msg={message}"),
    );
    assert!(!flush(&mut e));
    report(&mut e, "state=done:id=new");
    flush(&mut e);
    let ids = ids(&e);
    assert_eq!(ids.len(), MAX_RECORDS);
    assert!(ids.contains(&"task00") && !ids.contains(&"task01"));
    let wire = serde_json::to_vec(&e.program_status().records()).unwrap();
    assert!(wire.len() < 180_000);
    assert!(
        serde_json::to_vec(&e.program_status().summary())
            .unwrap()
            .len()
            < 3000
    );
}

#[test]
fn maximum_text_and_json_escaping_stay_bounded() {
    let mut e = Engine::new().unwrap();
    // Base64 for quotation marks; JSON must escape every decoded byte.
    let title = "IiIi".repeat(64);
    let message = format!("{}IiI=", "IiIi".repeat(682));
    for n in 0..MAX_RECORDS {
        let id = format!("{}/{}{}", "a".repeat(32), "b".repeat(30), n % 10);
        let id = format!("{id}/{}/task{n:02}{}", "c".repeat(32), "d".repeat(24));
        assert_eq!(id.len(), 128);
        report(
            &mut e,
            &format!(
                "state=blocked:id={id}:app={}:title={title}:msg={message}",
                "p".repeat(32)
            ),
        );
    }
    flush(&mut e);
    assert_eq!(ids(&e).len(), MAX_RECORDS);
    let memory = e.program_status().memory_bytes();
    let detail = serde_json::to_vec(&e.program_status().records())
        .unwrap()
        .len();
    let summary = serde_json::to_vec(&e.program_status().summary())
        .unwrap()
        .len();
    assert!(memory < 180_000, "{memory}");
    assert!(detail < 320 * 1024, "{detail}");
    assert!(summary < 5 * 1024, "{summary}");
    eprintln!(
        "program status maximum: memory={memory}, detail_json={detail}, summary_json={summary}"
    );
}

#[test]
fn prompt_exit_ack_reset_screen_and_restore_lifetimes() {
    let mut e = Engine::new().unwrap();
    for body in [
        "state=done:id=result",
        "state=error:id=failure",
        "state=blocked:id=worker",
        "state=idle",
    ] {
        report(&mut e, body);
    }
    e.write(b"\x1b[?1049h\x1b[!p\x1b[?1049l");
    assert_eq!(ids(&e).len(), 4);
    e.write(b"\x1b]133;P;k=r\x07");
    assert_eq!(ids(&e).len(), 4);
    e.write(b"\x1b]133;A\x07");
    assert_eq!(ids(&e), ["", "failure", "result"]);
    report(&mut e, "state=working:id=worker");
    e.program_status_mut().end_execution();
    assert_eq!(ids(&e), ["", "failure", "result"]);
    e.program_status_mut().acknowledge();
    assert_eq!(ids(&e), [""]);
    report(&mut e, "state=blocked:id=worker");
    e.program_status_mut().acknowledge();
    assert_eq!(ids(&e), ["", "worker"]);
    e.write(b"\x1b[?1049h\x1b]133;A\x07");
    assert_eq!(ids(&e), [""]);
    report(&mut e, "state=done");
    let checkpoint = e.checkpoint().unwrap();
    let mut fresh = Engine::new().unwrap();
    fresh.restore_history(&checkpoint).unwrap();
    assert!(ids(&fresh).is_empty());
    e.write(b"\x1bc");
    flush(&mut e);
    assert!(ids(&e).is_empty() && e.program_status().summary().is_none());
}

#[test]
fn status_only_chunks_preserve_view_search_and_compression_and_coalesce() {
    let mut e = Engine::new().unwrap();
    e.write(b"searchable text\r\n");
    e.snapshot().unwrap();
    let revision = e.revision;
    let token = e.compression_activity().unwrap();
    assert!(!e.write(b"\x1b]7501;state=blocked:id=worker:msg=SGVsbG8="));
    assert!(ids(&e).is_empty());
    assert!(!e.write(b"\x1b\\\x1b]7501;state=working\x07"));
    assert!(flush(&mut e));
    for _ in 0..100 {
        assert!(!report(&mut e, "state=blocked:id=worker:msg=SGVsbG8="));
        assert!(!flush(&mut e));
    }
    let fragmented = b"\x1b]7501;state=blocked:id=worker:msg=SGVsbG8=\x1b\\";
    for split in 0..=fragmented.len() {
        assert!(!e.write(&fragmented[..split]), "split {split}");
        assert!(!e.write(&fragmented[split..]), "split {split}");
        assert!(!flush(&mut e));
    }
    for byte in fragmented {
        assert!(!e.write(std::slice::from_ref(byte)));
    }
    assert!(!flush(&mut e));
    assert_eq!(e.revision, revision);
    assert_eq!(e.compression_activity().unwrap(), token);
    e.write(b"\x1b]9;4;1;53\x07");
    assert_eq!(e.progress().percentage(), 53);
    assert_eq!(
        e.program_status().summary().unwrap().record.state,
        State::Blocked
    );
    assert!(e.write(b"\x1b]7501;state=done\x07visible"));
    assert!(e.text().unwrap().contains("visible"));
    // Aborted OSC followed by a full reset must still invalidate the grid.
    assert!(e.write(b"\x1b]7501;state=working\x1bc"));
    assert!(ids(&e).is_empty());
    // The deferred ESC also has to resume ordinary VT operations correctly.
    assert!(!e.write(b"\x1b"));
    assert!(e.write(b"[31mred"));
    assert!(e.text().unwrap().contains("red"));
    assert!(!e.write(b"\x1b"));
    assert!(!e.write(b"]9;4;1;73\x07"));
    assert_eq!(e.progress().percentage(), 73);
    assert!(!e.write(b"\x1b"));
    assert!(e.write(b"c"));
    assert!(ids(&e).is_empty());
}

#[test]
#[ignore = "manual release benchmark"]
fn program_status_measurements() {
    let mut e = Engine::new().unwrap();
    for n in 0..MAX_RECORDS {
        report(&mut e, &format!("state=working:id=task{n:02}:msg=SGVsbG8="));
    }
    flush(&mut e);
    eprintln!(
        "program status inline bytes: Records={}, Summary={}, Metadata={}",
        std::mem::size_of::<crate::program_status::Records>(),
        std::mem::size_of::<gmux_client::program_status::Summary>(),
        std::mem::size_of::<gmux_client::transport::Metadata>()
    );
    let iterations = 100_000;
    let report = b"\x1b]7501;state=working:id=task00:msg=SGVsbG8=\x07";
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        e.write(report);
        assert!(!flush(&mut e));
    }
    eprintln!(
        "program status unchanged, cap={MAX_RECORDS}, ns/report={:.1}, retained_bytes={}",
        start.elapsed().as_nanos() as f64 / f64::from(iterations),
        e.program_status().memory_bytes()
    );
    let report = b"\x1b]7501;state=working:id=task00:msg=V29ya2luZw==\x07";
    let other = b"\x1b]7501;state=working:id=task00:msg=SGVsbG8=\x07";
    let start = std::time::Instant::now();
    for _ in 0..iterations / 2 {
        e.write(report);
        assert!(flush(&mut e));
        e.write(other);
        assert!(flush(&mut e));
    }
    eprintln!(
        "program status changed, ns/report={:.1}",
        start.elapsed().as_nanos() as f64 / f64::from(iterations)
    );
    let mut summary = e.program_status().summary().unwrap().clone();
    summary.record.message = "x".repeat(2048);
    summary.record.title = "t".repeat(192);
    for count in [1, 16, 64, 256] {
        let panes = vec![summary.clone(); count];
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(gmux_client::program_status::representative(
                panes
                    .iter()
                    .enumerate()
                    .map(|(id, status)| (id as u64, Some(status))),
            ));
        }
        eprintln!(
            "program status aggregation, panes={count}, ns/selection={:.1}",
            start.elapsed().as_nanos() as f64 / f64::from(iterations)
        );
        let mut legacy = gmux_client::progress::Progress::default();
        legacy.apply(gmux_client::progress::ProgressState::Error, Some(42));
        for mixed in [false, true] {
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(gmux_client::activity::representative(
                    std::hint::black_box(panes.as_slice())
                        .iter()
                        .enumerate()
                        .map(|(id, status)| {
                            (id as u64, (!mixed || id % 2 == 0).then_some(status), legacy)
                        }),
                ));
            }
            eprintln!(
                "combined activity aggregation, panes={count}, mixed={mixed}, ns/selection={:.1}",
                start.elapsed().as_nanos() as f64 / f64::from(iterations)
            );
        }
    }
}
