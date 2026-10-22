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
                text: Some("héllo 界🙂".into())
            },
            Write {
                destination: Destination::Primary,
                text: None
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
            .text
            .as_ref()
            .unwrap()
            .len(),
        text.len()
    );
    assert_eq!(admit(0), GMUX_CLIPBOARD_SUCCESS as i32);
    assert_eq!(engine.take_clipboard_writes()[0].text.as_deref(), Some(""));
}
