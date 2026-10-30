use crate::incoming;
use gmux_protocol::{
    control::{CommandOutput, CommandReply},
    model::{ExecutionSnapshot, ExecutionTab, MachineKind, PaneTree},
    transport::{Header, Metadata, Packet},
};
use std::collections::HashMap;

fn state() -> CommandReply {
    CommandReply {
        daemon: serde_json::from_str("\"00000000000000000000000000000001\"").unwrap(),
        home_directory: "/home/test".into(),
        machine_kind: MachineKind::Desktop,
        hostname: "host".into(),
        revision: 1,
        result: CommandOutput::Done { ok: true }.into(),
        execution: ExecutionSnapshot {
            tabs: vec![ExecutionTab {
                id: 1,
                created_at: 1,
                name: "Terminal".into(),
                custom_name: false,
                layout: PaneTree::Pane(2),
                directories: HashMap::from([(2, "/home/test".into())]),
            }],
        },
        metadata: vec![Metadata::default()],
    }
}

#[test]
fn state_capacity_consumes_queue_budget_even_when_json_is_unchanged() {
    const LIMIT: usize = 32 * 1024;
    type Reserve = fn(&mut CommandReply);
    let cases: &[(&str, Reserve)] = &[
        ("home", |s| s.home_directory.reserve(LIMIT * 2)),
        ("hostname", |s| s.hostname.reserve(LIMIT * 2)),
        ("warning", |s| {
            s.result.warning = Some(String::with_capacity(LIMIT * 2));
        }),
        ("metadata vector", |s| s.metadata.reserve(LIMIT)),
        ("metadata path", |s| {
            s.metadata[0].directory.reserve(LIMIT * 2)
        }),
        ("metadata title", |s| s.metadata[0].title.reserve(LIMIT * 2)),
        ("metadata command", |s| {
            s.metadata[0]
                .command
                .command
                .as_mut()
                .unwrap()
                .reserve(LIMIT * 2)
        }),
        ("tab vector", |s| s.execution.tabs.reserve(LIMIT)),
        ("tab name", |s| s.execution.tabs[0].name.reserve(LIMIT * 2)),
        ("directory table", |s| {
            s.execution.tabs[0].directories.reserve(LIMIT)
        }),
        ("pane path", |s| {
            s.execution.tabs[0]
                .directories
                .get_mut(&2)
                .unwrap()
                .reserve(LIMIT * 2);
        }),
    ];
    for (name, reserve) in cases {
        let mut state = state();
        // An empty optional warning also serializes independently of capacity.
        state.result.warning = Some(String::new());
        state.metadata[0].command.command = Some(String::new());
        let encoded = serde_json::to_vec(&state).unwrap();
        reserve(&mut state);
        assert_eq!(serde_json::to_vec(&state).unwrap(), encoded, "{name}");
        let packet = Packet {
            header: Header {
                state: Some(state),
                ..Header::default()
            },
            cells: vec![],
        };
        let (sender, receiver) = incoming::channel(LIMIT, 8);
        assert_eq!(
            sender.send(Ok(packet)),
            Err(incoming::SendError::TooLarge),
            "{name}"
        );
        // Rejection must not reserve bytes or prevent a later error reaching the UI.
        sender
            .send(Err("closed after oversized state".into()))
            .unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Err(_))), "{name}");
    }
}

#[test]
fn rich_clipboard_capacity_counts_toward_incoming_queue_admission() {
    use gmux_protocol::clipboard::{Content, Destination, Write};
    let packet = Packet {
        header: Header {
            copy: Some(Write {
                destination: Destination::Clipboard,
                content: Some(
                    Content {
                        text: String::with_capacity(8192),
                        html: Some(String::with_capacity(8192)),
                    }
                    .into(),
                ),
            }),
            ..Header::default()
        },
        cells: vec![],
    };
    assert!(packet.memory_size() >= 16384);
    let (sender, _) = incoming::channel(12000, 8);
    assert_eq!(sender.send(Ok(packet)), Err(incoming::SendError::TooLarge));
}

#[test]
fn nested_pane_tree_cannot_exceed_the_queue_budget() {
    fn tree(first: u64, panes: u64) -> PaneTree {
        if panes == 1 {
            PaneTree::Pane(first)
        } else {
            PaneTree::Split {
                vertical: false,
                first: Box::new(tree(first, panes / 2)),
                second: Box::new(tree(first + panes / 2, panes - panes / 2)),
            }
        }
    }
    let packet = |state| Packet {
        header: Header {
            state: Some(state),
            ..Header::default()
        },
        cells: vec![],
    };
    let (sender, receiver) = incoming::channel(4096, 8);
    sender.send(Ok(packet(state()))).unwrap();
    assert!(matches!(receiver.try_recv(), Ok(Ok(_))));

    let mut nested = state();
    nested.execution.tabs[0].layout = tree(2, 256);
    assert_eq!(
        sender.send(Ok(packet(nested))),
        Err(incoming::SendError::TooLarge)
    );
}
