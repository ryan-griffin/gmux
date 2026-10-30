//! Retained packet storage, independent of JSON encoding and string lengths.
use super::{Metadata, Packet};
use crate::{
    control::CommandReply,
    model::{ExecutionSnapshot, ExecutionTab, PaneTree},
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
            + self
                .cells
                .iter()
                .map(crate::Cell::heap_bytes)
                .sum::<usize>()
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
                .and_then(|write| write.content.as_ref())
                .map_or(0, crate::clipboard::Offer::memory_size)
            + header
                .clipboard_read
                .as_ref()
                .map_or(0, crate::clipboard::ReadEvent::memory_size)
            + header.find.as_ref().map_or(0, |find| {
                find.highlights.capacity() * size_of::<(usize, usize, u8)>()
            })
    }
}

fn metadata_size(meta: &Metadata) -> usize {
    meta.directory.capacity()
        + meta.title.capacity()
        + meta.command.command.as_ref().map_or(0, String::capacity)
        + meta
            .program_status
            .as_ref()
            .map_or(0, |s| s.record.heap_bytes())
}

fn state_size(state: &CommandReply) -> usize {
    state.home_directory.capacity()
        + state.hostname.capacity()
        + state.result.warning.as_ref().map_or(0, String::capacity)
        + state.metadata.capacity() * size_of::<Metadata>()
        + state.metadata.iter().map(metadata_size).sum::<usize>()
        + execution_size(&state.execution)
}

fn execution_size(state: &ExecutionSnapshot) -> usize {
    state.tabs.capacity() * size_of::<ExecutionTab>()
        + state.tabs.iter().map(tab_size).sum::<usize>()
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
