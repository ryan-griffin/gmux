//! Opt-in, owner-thread timings. No clocks or allocations on the disabled path.
//! Scopes are inclusive; nested stages must not be added together.
use std::{cell::RefCell, path::PathBuf, sync::OnceLock, time::Instant};

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Readiness,
    Deadlines,
    NextUpdate,
    Input,
    Control,
    Pty,
    Fanout,
    FramePlan,
    PaneVisit,
    Snapshot,
    Encode,
    Flush,
    Compression,
}
const NAMES: [&str; 13] = [
    "readiness",
    "deadlines",
    "next_update",
    "input",
    "control",
    "pty",
    "fanout",
    "frame_plan",
    "pane_visit",
    "snapshot",
    "encode",
    "flush",
    "compression",
];
#[derive(Clone, Copy, Default, serde::Serialize)]
struct Cost {
    calls: u64,
    units: u64,
    nanos: u64,
    max_nanos: u64,
}
thread_local! {
    static COSTS: RefCell<[Cost; NAMES.len()]> = const { RefCell::new([Cost {
        calls: 0, units: 0, nanos: 0, max_nanos: 0,
    }; NAMES.len()]) };
}
static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

pub(super) struct Scope(Option<(Instant, Stage, u64)>);
#[inline]
pub(super) fn scope(stage: Stage, units: usize) -> Scope {
    Scope(
        PATH.get_or_init(|| std::env::var_os("GMUX_OWNER_PROFILE").map(PathBuf::from))
            .as_ref()
            .map(|_| (Instant::now(), stage, units as u64)),
    )
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((started, stage, units)) = self.0 {
            let nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            COSTS.with_borrow_mut(|costs| {
                let cost = &mut costs[stage as usize];
                cost.calls += 1;
                cost.units += units;
                cost.nanos += nanos;
                cost.max_nanos = cost.max_nanos.max(nanos);
            });
        }
    }
}

/// Write once on orderly owner-loop shutdown, never during a measured turn.
pub(super) fn finish() {
    if let Some(Some(path)) = PATH.get() {
        let report = COSTS.with_borrow(|costs| {
            NAMES
                .into_iter()
                .zip(costs.iter().copied())
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        let result = serde_json::to_vec_pretty(&report)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(path, bytes));
        if let Err(error) = result {
            eprintln!("Could not write owner profile: {error}");
        }
    }
}
