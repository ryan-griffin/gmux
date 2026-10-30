use super::*;
use gmux_protocol::shell::CommandPhase::*;

fn terminal() -> Engine {
    let mut engine = Engine::new().unwrap();
    engine.resize(12, 3, 0, 0).unwrap();
    engine
}

fn command(engine: &mut Engine, text: &str) {
    engine.write(b"\x1b]133;A\x07$ \x1b]133;B\x07run\r\n\x1b]133;C\x07");
    engine.write(text.as_bytes());
    engine.write(b"\r\n\x1b]133;D;0\x07");
}

#[test]
fn lifecycle_copies_optional_bounded_metadata_and_handles_incomplete_events() {
    let mut e = terminal();
    assert_eq!(e.command_state().phase, Unknown);
    e.write(b"\x1b]133;B\x07");
    assert_eq!(e.command_state().phase, Input);
    e.write(b"\x1b]133;C;cmdline_url=echo%20hello\x07");
    assert!(e.semantic_busy());
    assert_eq!(e.command_state().command.as_deref(), Some("echo hello"));
    e.write(b"\x1b]133;C\x07");
    assert_eq!(e.command_state().command.as_deref(), Some("echo hello"));
    e.write(b"\x1b]133;D;-1\x07");
    assert_eq!(e.command_state().last_exit_code, Some(-1));
    assert_eq!(e.command_state().phase, Complete);
    e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(e.command_state().last_exit_code, Some(-1));
    assert_eq!(e.command_state().command, None);
    e.write(b"\x1b]133;C\x07\x1b]133;D\x07");
    assert_eq!(e.command_state().last_exit_code, None);
    assert_eq!(e.command_state().command, None);
    e.write(b"\x1b]133;C\x07\x1b]133;A\x07");
    assert_eq!(e.command_state().phase, Prompt);
    assert!(!e.semantic_busy());
    // Exercise the bridge's own bound independently of the parser's OSC cap.
    let text = format!("{}界end", "a".repeat(4095));
    let report = GmuxCommandEvent {
        kind: 3,
        has_exit_code: false,
        exit_code: 0,
        command: text.as_ptr(),
        command_len: text.len(),
    };
    unsafe {
        event(
            (&mut *e.events as *mut Events).cast(),
            5,
            (&report as *const GmuxCommandEvent).cast(),
            std::mem::size_of_val(&report),
        );
    }
    assert_eq!(e.command_state().command.unwrap().len(), 4095);
    assert!(e.command_state().command_truncated);
    e.write(b"\x1b]133;D;4\x07\x1b]133;C\x07");
    assert_eq!(e.command_state().command, None);
    assert!(!e.command_state().command_truncated);
}

#[test]
fn auxiliary_prompts_and_redraws_do_not_create_commands_or_primary_anchors() {
    let mut e = terminal();
    e.write(b"\x1b]133;A\x07\x1b]133;P;k=i\x07$ \x1b]133;B\x07");
    assert_eq!(e.memory_usage().unwrap().prompt_anchors, 1);
    assert!(e.memory_usage().unwrap().prompt_anchor_bytes <= 32 * 16);
    for kind in ["r", "c", "s"] {
        e.write(format!("\x1b]133;P;k={kind}\x07> \x1b]133;B\x07").as_bytes());
        assert_eq!(e.command_state().phase, Input);
        assert_eq!(e.memory_usage().unwrap().prompt_anchors, 1);
    }
    e.write(b"\x1b]133;C\x07");
    e.write(b"\x1b]133;P;k=r\x07\x1b]133;B\x07");
    assert_eq!(e.command_state().phase, Running);
    e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(e.command_state().phase, Input);
}

#[test]
fn output_selection_is_linear_unwraps_and_excludes_prompt_input_and_next_prompt() {
    let mut e = terminal();
    command(&mut e, "1234567890abcdef 界🙂");
    e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    e.select_output(None).unwrap();
    assert_eq!(
        e.selection().unwrap().shape,
        gmux_protocol::selection::Shape::Linear
    );
    assert_eq!(e.copy().unwrap(), "1234567890abcdef 界🙂");
    e.resize(8, 4, 0, 0).unwrap();
    assert_eq!(e.copy().unwrap(), "1234567890abcdef 界🙂");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "1234567890abcdef 界🙂");
    e.clear_selection();
    assert!(
        e.select_output(Some((0, 0))).is_err(),
        "Input is not output"
    );
    command(&mut e, "second");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "second");
}

#[test]
fn empty_completed_output_falls_back_to_current_output_only_for_default_target() {
    let mut e = terminal();
    e.resize(40, 10, 0, 0).unwrap();
    command(&mut e, "older");
    e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(e.select_output(None).is_err());
    e.anchor_output((0, 3)).unwrap();
    e.write(b"run\r\n\x1b]133;C\x07current output\r\n");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "current output");
    assert!(e.select_output(Some((0, 3))).is_err());
    assert!(e.select_anchored_output().is_err());
    e.write(b"\x1b]133;D\x07\x1b]133;A\x07$ \x1b]133;B\x07next\r\n\x1b]133;C\x07newer output");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "current output");
}

#[test]
fn prompt_navigation_and_output_selection_belong_to_each_view() {
    let mut e = terminal();
    for text in ["one", "two", "three", "four"] {
        command(&mut e, text);
    }
    e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    let mut first = e.new_view().unwrap();
    let mut second = e.new_view().unwrap();
    e.with_view(&mut first, |e| {
        e.scroll_to(0);
        e.prompt(false).unwrap();
        e.snapshot().unwrap();
        assert_eq!(e.frame.scrollback.offset, 2);
        e.select_output(Some((0, 3))).unwrap();
        assert_eq!(e.copy().unwrap(), "two");
    });
    e.with_view(&mut second, |e| {
        e.snapshot().unwrap();
        assert!(e.frame.scrollback.offset > 2);
        assert_eq!(e.selection(), None);
        e.scroll_to(0);
    });
    e.resize(8, 3, 0, 0).unwrap();
    e.with_view(&mut first, |e| {
        assert_eq!(e.copy().unwrap(), "two");
        e.prompt(true).unwrap();
        e.snapshot().unwrap();
        assert_eq!(e.frame.scrollback.offset, 0);
    });
    assert_eq!(e.command_state().phase, Input);
}

#[test]
fn alternate_screen_and_reset_do_not_leak_semantic_state_or_selection() {
    let mut e = terminal();
    command(&mut e, "primary");
    let before = e.command_state();
    e.write(b"\x1b[?1049h\x1b]133;A\x07other\x1b]133;C\x07");
    assert_eq!(e.command_state().phase, before.phase);
    assert!(e.prompt(true).is_err());
    assert!(e.select_output(None).is_err());
    e.write(b"\x1b[?1049l");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "primary");
    e.write(b"\x1bc");
    assert_eq!(e.command_state().phase, Unknown);
    assert_eq!(e.command_state().last_exit_code, None);
    assert_eq!(e.memory_usage().unwrap().prompt_anchors, 0);
    assert_eq!(e.memory_usage().unwrap().prompt_anchor_bytes, 0);
    assert!(e.select_output(None).is_err());
    assert!(e.prompt(true).is_err());
}

#[test]
fn retained_anchors_are_bounded_pruned_and_do_not_restart_idle_compression() {
    let mut e = terminal();
    for _ in 0..3000 {
        command(&mut e, "x");
    }
    assert!(e.memory_usage().unwrap().prompt_anchors <= 2048);
    e.write(&b"output\r\n".repeat(20000));
    assert_eq!(e.memory_usage().unwrap().prompt_anchors, 0);
    assert_eq!(e.memory_usage().unwrap().prompt_anchor_bytes, 0);
    assert!(e.select_output(None).is_err());
    command(&mut e, "retained");
    let activity = e.compression_activity().unwrap();
    for _ in 0..30 {
        let _ = e.command_state();
        let _ = e.memory_usage().unwrap();
    }
    assert_eq!(activity, e.compression_activity().unwrap());
    e.clear_screen().unwrap();
    assert_eq!(e.memory_usage().unwrap().prompt_anchors, 0);
    assert!(e.prompt(true).is_err());
}

#[test]
fn restoration_has_text_but_no_old_commands_or_semantic_anchors() {
    let mut e = terminal();
    command(&mut e, "saved output");
    let history = e.checkpoint().unwrap();
    let mut fresh = terminal();
    fresh.restore_history(&history).unwrap();
    assert_eq!(fresh.command_state().phase, Unknown);
    assert_eq!(fresh.memory_usage().unwrap().prompt_anchors, 0);
    assert!(fresh.select_output(None).is_err());
}

#[test]
fn captured_output_survives_reflow_and_cannot_select_unrelated_pruned_text() {
    let mut e = terminal();
    command(&mut e, "1234567890abcdef");
    let mut view = e.new_view().unwrap();
    let mut other = e.new_view().unwrap();
    e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
    e.resize(8, 3, 0, 0).unwrap();
    e.with_view(&mut view, |e| {
        e.select_anchored_output().unwrap();
        assert_eq!(e.copy().unwrap(), "1234567890abcdef");
    });
    e.with_view(&mut other, |e| assert!(e.select_anchored_output().is_err()));
    // The pinned API can clamp partial-page erasures instead of invalidating
    // their pins. Captured targets in that discarded prefix must be released.
    for _ in 0..20 {
        command(&mut e, "x");
    }
    e.write(b"\x1b[2J\x1b[3J\x1b[Hunrelated\r\n\x1b]133;A\x07$ ");
    e.with_view(&mut view, |e| assert!(e.select_anchored_output().is_err()));
}

#[test]
fn erasing_the_only_command_cannot_retarget_its_output_to_new_text() {
    let mut e = terminal();
    command(&mut e, &"old output\r\n".repeat(20));
    let mut view = e.new_view().unwrap();
    e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
    e.write(b"\x1b[2J\x1b[3J\x1b[Hunrelated\r\n\x1b]133;A\x07$ ");
    e.with_view(&mut view, |e| assert!(e.select_anchored_output().is_err()));
    assert!(e.select_output(None).is_err());
}

#[test]
fn captured_output_rejects_reused_active_cells_with_or_without_a_following_prompt() {
    // A non-prompt on the bottom row forces destructive ED 2. Since Ghostty
    // 1.4, trailing blank rows allow prompt-preserving scroll-clear instead.
    for following_prompt in [false, true] {
        let mut e = terminal();
        e.resize(40, 10, 0, 0).unwrap();
        command(&mut e, "original");
        if following_prompt {
            e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        }
        let mut view = e.new_view().unwrap();
        e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
        e.write(b"\x1b[10;1H.\x1b[2J\x1b[H");
        command(&mut e, "replacement");
        e.with_view(&mut view, |e| assert!(e.select_anchored_output().is_err()));
        e.select_output(None).unwrap();
        assert_eq!(e.copy().unwrap(), "replacement");
    }
}

#[test]
fn captured_output_rejects_cells_erased_inside_the_next_command() {
    for lines in [2, 3] {
        let mut e = terminal();
        e.resize(40, 10, 0, 0).unwrap();
        command(&mut e, "original");
        e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let mut view = e.new_view().unwrap();
        let mut other = e.new_view().unwrap();
        e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
        e.write(b"clear\r\n\x1b]133;C\x07\x1b[10;1H.\x1b[2J\x1b[H");
        e.write("replacement\r\n".repeat(lines).as_bytes());
        e.write(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        // The new prompt is on or below the former prompt's row. A retained
        // prompt ID alone cannot prove that its original row still exists.
        e.with_view(&mut view, |e| assert!(e.select_anchored_output().is_err()));
        e.with_view(&mut other, |e| {
            e.anchor_output((0, 0)).unwrap();
            e.select_anchored_output().unwrap();
            assert_eq!(e.copy().unwrap(), vec!["replacement"; lines].join("\n"));
        });
    }
}

#[test]
fn screen_clear_preserves_captured_output_moved_into_native_history() {
    // Filled screens and partially filled prompt screens both scroll-clear.
    for (rows, commands) in [(3, 8), (10, 2)] {
        let mut e = terminal();
        e.resize(40, rows, 0, 0).unwrap();
        for i in 0..commands {
            command(&mut e, &format!("old{i}"));
        }
        e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let mut history = e.new_view().unwrap();
        let mut active = e.new_view().unwrap();
        e.with_view(&mut history, |e| e.anchor_output((0, 1)).unwrap());
        e.with_view(&mut active, |e| {
            e.anchor_output((0, commands * 2 - 1)).unwrap()
        });
        e.write(b"\x1b[2J\x1b[H");
        command(&mut e, "replacement");
        e.with_view(&mut active, |e| {
            e.select_anchored_output().unwrap();
            assert_eq!(e.copy().unwrap(), format!("old{}", commands - 1));
        });
        e.with_view(&mut history, |e| {
            e.select_anchored_output().unwrap();
            assert_eq!(e.copy().unwrap(), "old0");
        });
    }
}

#[test]
fn captured_output_outlives_the_prompt_index_but_rejects_later_rewrites() {
    for forward_prompt in [false, true] {
        let mut e = terminal();
        // Keep all these commands in the grid while the navigation index wraps.
        e.resize(4, 5000, 0, 0).unwrap();
        e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07r\r\n\x1b]133;C\x07keep\r\n\x1b]133;D;0\x07");
        let mut view = e.new_view().unwrap();
        e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
        for _ in 0..2100 {
            e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07r\r\n\x1b]133;C\x07x\r\n\x1b]133;D;0\x07");
        }
        assert_eq!(e.memory_usage().unwrap().prompt_anchors, 2048);
        e.with_view(&mut view, |e| {
            e.select_anchored_output().unwrap();
            assert_eq!(e.copy().unwrap(), "keep");
        });
        if forward_prompt {
            e.write(b"\x1b]133;A\x07$ \x1b]133;B\x07r\r\n\x1b]133;C\x07\x1b[5000;1H.\x1b[2J\x1b[Hnew\r\n\x1b]133;D;0\x07\x1b[4500H\x1b]133;A\x07$ \x1b]133;B\x07");
        } else {
            e.write(b"\x1b[5000;1H.\x1b[2J\x1b[H\x1b]133;A\x07$ \x1b]133;B\x07r\r\n\x1b]133;C\x07new\r\n\x1b]133;D;0\x07");
        }
        e.with_view(&mut view, |e| assert!(e.select_anchored_output().is_err()));
    }
}

#[test]
fn output_targets_survive_narrow_reflow_and_wide_first_cells() {
    for text in ["x", "界🙂", "1234567890abcdef"] {
        let mut e = terminal();
        command(&mut e, text);
        let mut view = e.new_view().unwrap();
        let mut tail = e.new_view().unwrap();
        e.with_view(&mut view, |e| e.anchor_output((0, 1)).unwrap());
        e.with_view(&mut tail, |e| {
            e.anchor_output((u16::from(!text.is_ascii()), 1)).unwrap()
        });
        // A grid narrower than a wide glyph cannot retain that glyph.
        for cols in if text.is_ascii() {
            [2, 1, 12]
        } else {
            [2, 3, 12]
        } {
            e.resize(cols, 4, 0, 0).unwrap();
            e.select_output(None)
                .unwrap_or_else(|error| panic!("{text:?} at {cols} columns: {error}"));
            assert_eq!(e.copy().unwrap(), text, "{cols} columns");
            for view in [&mut view, &mut tail] {
                e.with_view(view, |e| {
                    e.select_anchored_output().unwrap();
                    assert_eq!(e.copy().unwrap(), text, "{cols} columns");
                });
            }
        }
    }
    let mut e = terminal();
    e.resize(1, 3, 0, 0).unwrap();
    command(&mut e, "x");
    e.select_output(None).unwrap();
    assert_eq!(e.copy().unwrap(), "x");
}
