//! Retained packet storage, independent of JSON encoding and string lengths.
use super::{Metadata, Packet};
use crate::{
    control::CommandReply,
    model::{ExecutionState, ExecutionTab, PaneTree},
};
use std::{collections::HashMap, mem::size_of};

impl Packet {
    /// Estimate retained bytes for queue admission, including unused capacity.
    /// Hash tables include a conservative allowance for spare buckets and controls;
    /// allocator bookkeeping is excluded, as it is for the other allocations.
    pub fn memory_size(&self) -> usize {
        let header = &self.header;
        size_of::<Self>()
            + header.state.as_ref().map_or(0, state_size)
            + self.cells.capacity() * size_of::<crate::Cell>()
            + metadata_size(&header.meta)
            + header
                .git
                .as_ref()
                .map_or(0, |git| git.directory.capacity() + git.branch.capacity())
            + header.rows.capacity() * size_of::<u16>()
            + header
                .hyperlinks
                .as_ref()
                .map_or(0, crate::hyperlinks::Hyperlinks::memory_size)
            + header.row_flags.capacity()
            + header
                .cache
                .as_ref()
                .map_or(0, |cache| cache.row_flags.capacity())
            + header.notifications.capacity() * size_of::<String>()
            + header
                .notifications
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + header.error.as_ref().map_or(0, String::capacity)
            + header
                .copy
                .as_ref()
                .and_then(|write| write.text.as_ref())
                .map_or(0, String::capacity)
            + header.find.as_ref().map_or(0, |find| {
                find.highlights.capacity() * size_of::<(usize, usize, u8)>()
            })
    }
}

fn metadata_size(meta: &Metadata) -> usize {
    meta.directory.capacity() + meta.title.capacity()
}

fn state_size(state: &CommandReply) -> usize {
    state.home_directory.capacity()
        + state.hostname.capacity()
        + state.result.warning.as_ref().map_or(0, String::capacity)
        + state.metadata.capacity() * size_of::<Metadata>()
        + state.metadata.iter().map(metadata_size).sum::<usize>()
        + execution_size(&state.execution)
}

fn execution_size(state: &ExecutionState) -> usize {
    state.tabs.capacity() * size_of::<ExecutionTab>()
        + state.tabs.iter().map(tab_size).sum::<usize>()
        + map_storage(&state.scrollback)
        + state
            .scrollback
            .values()
            .map(String::capacity)
            .sum::<usize>()
}

fn tab_size(tab: &ExecutionTab) -> usize {
    tab.name.capacity()
        + tree_size(&tab.layout)
        + map_storage(&tab.directories)
        + tab
            .directories
            .values()
            .map(|path| path.capacity())
            .sum::<usize>()
}

fn tree_size(tree: &PaneTree) -> usize {
    match tree {
        PaneTree::Pane(_) => 0,
        PaneTree::Split { first, second, .. } => {
            2 * size_of::<PaneTree>() + tree_size(first) + tree_size(second)
        }
    }
}

fn map_storage<K, V>(map: &HashMap<K, V>) -> usize {
    if map.capacity() == 0 {
        return 0;
    }
    // std does not expose the allocation size. Allow two buckets per usable
    // entry for load-factor/rounding slack, one control byte per bucket, and
    // space for the trailing SIMD control group and allocation alignment.
    2 * map.capacity() * (size_of::<(K, V)>() + 1) + 16 + std::mem::align_of::<(K, V)>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control::CommandOutput, incoming, model::MachineKind, transport::Header};

    fn state() -> CommandReply {
        CommandReply {
            daemon: serde_json::from_str("\"00000000000000000000000000000001\"").unwrap(),
            home_directory: "/home/test".into(),
            machine_kind: MachineKind::Desktop,
            hostname: "host".into(),
            revision: 1,
            result: CommandOutput::Done { ok: true }.into(),
            execution: ExecutionState {
                tabs: vec![ExecutionTab {
                    id: 1,
                    created_at: 1,
                    name: "Terminal".into(),
                    custom_name: false,
                    layout: PaneTree::Pane(2),
                    directories: HashMap::from([(2, "/home/test".into())]),
                }],
                scrollback: HashMap::from([(2, String::new())]),
                ..ExecutionState::default()
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
            ("scrollback table", |s| {
                s.execution.scrollback.reserve(LIMIT)
            }),
            ("scrollback string", |s| {
                s.execution
                    .scrollback
                    .get_mut(&2)
                    .unwrap()
                    .reserve(LIMIT * 2);
            }),
        ];
        for (name, reserve) in cases {
            let mut state = state();
            // An empty optional warning also serializes independently of capacity.
            state.result.warning = Some(String::new());
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
}
