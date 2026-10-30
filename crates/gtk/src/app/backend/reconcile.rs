//! Merge daemon snapshots with frontend selections and observed pane exits.
use crate::client::control::CommandReply;
use crate::model::{Session, TabFilter, WorkspaceCatalog};
use std::collections::{HashMap, HashSet};

pub(in crate::app) struct Selection {
    pub(super) active: u64,
    pub(super) filter: TabFilter,
    pub(super) tabs: HashMap<u64, TabSelection>,
}

pub(super) struct TabSelection {
    pub(super) active: u64,
    pub(super) workspace: Option<u64>,
}

pub(super) fn remove_exited_panes(reply: &mut CommandReply, exited: &HashSet<u64>) {
    if exited.is_empty() {
        return;
    }
    reply.session.tabs.retain_mut(|saved| {
        let old_ids = saved.layout.ids();
        let removed: Vec<_> = old_ids
            .iter()
            .filter(|id| exited.contains(id))
            .copied()
            .collect();
        if removed.is_empty() {
            return true;
        }
        let mut layout = Some(saved.layout.clone());
        for id in removed {
            layout = layout.and_then(|layout| layout.remove(id));
            saved.directories.remove(&id);
        }
        let Some(layout) = layout else {
            return false;
        };
        let ids = layout.ids();
        if !ids.contains(&saved.active) {
            let index = old_ids
                .iter()
                .position(|id| *id == saved.active)
                .unwrap_or(0);
            saved.active = old_ids[index..]
                .iter()
                .find(|id| ids.contains(id))
                .copied()
                .unwrap_or_else(|| *ids.last().unwrap());
        }
        saved.layout = layout;
        true
    });
}

/// A daemon snapshot carries shared objects; each window keeps its own selection.
pub(super) fn preserve_window_state(
    state: &mut Session,
    catalog: &WorkspaceCatalog,
    current: &Selection,
) {
    state.active = current.active;
    state.filter = match current.filter {
        TabFilter::Workspace(id)
            if !catalog
                .workspaces
                .iter()
                .any(|workspace| workspace.id == id) =>
        {
            TabFilter::All
        }
        filter => filter,
    };
    for saved in &mut state.tabs {
        if let Some(tab) = current.tabs.get(&saved.id)
            && saved.layout.contains(tab.active)
        {
            saved.active = tab.active;
        }
    }
    repair_active_tab(state, current);
}

pub(super) fn reconcile_selection(
    state: &mut Session,
    catalog: &WorkspaceCatalog,
    requested: &Selection,
    current: &Selection,
    reveal: bool,
) {
    if !reveal {
        // A reply can wait for persistence while the user navigates. Keep
        // later choices when their targets survived the command.
        let filter_survived = match current.filter {
            TabFilter::Workspace(id) => catalog
                .workspaces
                .iter()
                .any(|workspace| workspace.id == id),
            _ => true,
        };
        if current.active != requested.active
            && (current.active != 0 || filter_survived)
            && (current.active == 0 || state.tabs.iter().any(|tab| tab.id == current.active))
        {
            state.active = current.active;
        }
        if filter_survived && current.filter != requested.filter {
            state.filter = current.filter;
        }
        for saved in &mut state.tabs {
            if let Some(tab) = current.tabs.get(&saved.id)
                && requested
                    .tabs
                    .get(&saved.id)
                    .is_some_and(|before| before.active != tab.active)
                && saved.layout.contains(tab.active)
            {
                saved.active = tab.active;
            }
        }
    }
    repair_active_tab(state, current);
}

fn repair_active_tab(state: &mut Session, current: &Selection) {
    let active_matches = state
        .tabs
        .iter()
        .any(|tab| tab.id == state.active && state.filter.matches(tab.workspace));
    // A move can populate an empty view or move its selected tab away. Keep
    // views emptied by sidebar text when the workspace hasn't gained tabs.
    let workspace_gained_tab = state.active == 0
        && state.tabs.iter().any(|saved| {
            state.filter.matches(saved.workspace)
                && current
                    .tabs
                    .get(&saved.id)
                    .is_none_or(|tab| !state.filter.matches(tab.workspace))
        });
    if !active_matches && (state.active != 0 || workspace_gained_tab) {
        let mut matching = state
            .tabs
            .iter()
            .filter(|tab| state.filter.matches(tab.workspace));
        let first = matching.next();
        state.active = first
            .filter(|tab| tab.id == current.active)
            .or_else(|| matching.find(|tab| tab.id == current.active))
            .or(first)
            .map_or(0, |tab| tab.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::control::CommandOutput;
    use crate::model::{Layout, TabState, WorkspaceDefinition};

    fn tab(id: u64, pane: u64, workspace: Option<u64>) -> TabState {
        TabState {
            daemon: gmux_platform::unix::identity::daemon_id().unwrap(),
            id,
            created_at: id,
            name: format!("Tab {id}"),
            custom_name: false,
            workspace,
            layout: Layout::Pane(pane),
            directories: HashMap::from([(pane, ".".into())]),
            active: pane,
        }
    }

    fn session(tabs: Vec<TabState>, active: u64, filter: TabFilter) -> Session {
        Session {
            tabs,
            active,
            filter,
            ..Session::default()
        }
    }
    #[test]
    fn workspace_filter_repairs_selection_across_connections() {
        let local = gmux_platform::unix::identity::daemon_id().unwrap();
        let remote = gmux_platform::unix::identity::daemon_id().unwrap();
        let mut own = tab(1, 2, None);
        own.daemon = local;
        let mut foreign = tab(4, 5, Some(7));
        foreign.daemon = remote;
        let mut state = session(vec![own, foreign], 1, TabFilter::Workspace(7));
        let catalog = WorkspaceCatalog {
            last_id: 7,
            workspaces: vec![WorkspaceDefinition {
                id: 7,
                name: "Shared".into(),
                directories: Default::default(),
            }],
        };
        let current = Selection {
            active: 1,
            filter: state.filter,
            tabs: HashMap::new(),
        };
        preserve_window_state(&mut state, &catalog, &current);
        assert_eq!(state.active, 4);
        state.tabs.pop();
        preserve_window_state(&mut state, &catalog, &current);
        assert_eq!(state.active, 0);
    }

    fn selection(state: &Session) -> Selection {
        Selection {
            active: state.active,
            filter: state.filter,
            tabs: state
                .tabs
                .iter()
                .map(|tab| {
                    (
                        tab.id,
                        TabSelection {
                            active: tab.active,
                            workspace: tab.workspace,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn shared_snapshots_preserve_the_windows_existing_tab_pane_and_filter() {
        let mut split = tab(1, 101, Some(10));
        split.layout.split(101, 102, false);
        let local = session(
            vec![split.clone(), tab(2, 201, None)],
            1,
            TabFilter::Workspace(10),
        );
        let current = selection(&local);
        let catalog = WorkspaceCatalog {
            last_id: 0,
            workspaces: vec![WorkspaceDefinition {
                id: 10,
                name: "Project".into(),
                directories: Default::default(),
            }],
        };
        split.active = 102;
        let mut incoming = session(
            vec![tab(3, 301, None), split, tab(2, 201, None)],
            3,
            TabFilter::All,
        );
        preserve_window_state(&mut incoming, &catalog, &current);
        assert_eq!(incoming.active, 1);
        assert!(incoming.filter == TabFilter::Workspace(10));
        assert_eq!(incoming.tabs[1].active, 101);
        let empty = selection(&session(local.tabs, 0, TabFilter::All));
        preserve_window_state(&mut incoming, &catalog, &empty);
        // A new shared tab populates a previously empty view without changing its filter.
        assert!(incoming.filter == TabFilter::All);
        assert_eq!(incoming.active, 3);
    }

    fn reconcile(state: &mut Session, requested: &Selection, current: &Selection, reveal: bool) {
        let catalog = WorkspaceCatalog {
            last_id: 0,
            workspaces: [10, 20]
                .into_iter()
                .map(|id| WorkspaceDefinition {
                    id,
                    name: format!("Workspace {id}"),
                    directories: Default::default(),
                })
                .collect(),
        };
        reconcile_selection(state, &catalog, requested, current, reveal);
    }

    #[test]
    fn later_navigation_survives_replies_but_new_panes_reveal_the_command_result() {
        let mut state = session(
            vec![tab(1, 101, Some(10)), tab(2, 201, Some(20))],
            1,
            TabFilter::All,
        );
        state.tabs[1].layout.split(201, 202, false);
        let requested = selection(&state);
        let mut current = selection(&state);
        current.active = 2;
        current.filter = TabFilter::Workspace(20);
        current.tabs.get_mut(&2).unwrap().active = 202;

        let mut reply = state.clone();
        reconcile(&mut reply, &requested, &current, false);
        assert_eq!(reply.active, 2);
        assert!(reply.filter == TabFilter::Workspace(20));
        assert_eq!(reply.tabs[1].active, 202);

        state.tabs.push(tab(3, 301, None));
        state.active = 3;
        reconcile(&mut state, &requested, &current, true);
        assert_eq!(state.active, 3);
        assert!(state.filter == TabFilter::All);
        assert_eq!(state.tabs[1].active, 201);
    }

    #[test]
    fn moves_populate_empty_views_and_replace_selections_outside_the_workspace() {
        for populated in [false, true] {
            let mut state = session(
                vec![tab(1, 101, Some(10)), tab(2, 201, Some(10))],
                2,
                TabFilter::All,
            );
            if populated {
                state.tabs.push(tab(3, 301, Some(20)));
            }
            let requested = selection(&state);
            let mut current = selection(&state);
            current.active = 0;
            current.filter = TabFilter::Workspace(20);
            state.tabs[0].workspace = Some(20);
            reconcile(&mut state, &requested, &current, false);
            assert_eq!(state.active, 1);
            assert!(state.filter == TabFilter::Workspace(20));
        }

        let mut state = session(
            vec![tab(1, 101, Some(10)), tab(2, 201, Some(10))],
            2,
            TabFilter::Workspace(10),
        );
        let requested = selection(&state);
        let mut current = selection(&state);
        current.active = 1;
        state.tabs[0].workspace = Some(20);
        reconcile(&mut state, &requested, &current, false);
        assert_eq!(state.active, 2);
        assert!(state.filter == TabFilter::Workspace(10));
    }

    #[test]
    fn sidebar_search_can_keep_a_view_empty_when_workspace_membership_is_unchanged() {
        let mut state = session(vec![tab(1, 101, Some(20))], 1, TabFilter::Workspace(20));
        let requested = selection(&state);
        let mut current = selection(&state);
        current.active = 0;
        reconcile(&mut state, &requested, &current, false);
        assert_eq!(state.active, 0);
    }

    #[test]
    fn deleted_filters_and_closed_tabs_cannot_preserve_invalid_selections() {
        for active in [0, 2] {
            let mut state = session(
                vec![tab(1, 101, None), tab(2, 201, None)],
                1,
                TabFilter::All,
            );
            let requested = selection(&state);
            let mut current = selection(&state);
            current.active = active;
            current.filter = TabFilter::Workspace(30);
            reconcile(&mut state, &requested, &current, false);
            assert_eq!(state.active, if active == 0 { 1 } else { 2 });
            assert!(state.filter == TabFilter::All);
        }

        let mut state = session(
            vec![tab(1, 101, None), tab(2, 201, None)],
            2,
            TabFilter::All,
        );
        let requested = selection(&state);
        let mut current = selection(&state);
        current.active = 1;
        state.tabs.remove(0);
        reconcile(&mut state, &requested, &current, false);
        assert_eq!(state.active, 2);
    }

    #[test]
    fn observed_exits_prune_layouts_and_focus_the_next_or_last_surviving_pane() {
        for (active, exiting, expected) in
            [(102, [101, 102, 301], 103), (104, [103, 104, 301], 102)]
        {
            let mut split = tab(1, 101, None);
            for id in 102..=104 {
                split.layout.split(id - 1, id, false);
                split.directories.insert(id, ".".into());
            }
            split.active = active;
            let mut reply = CommandReply {
                desktop: None,
                daemon: Some(gmux_platform::unix::identity::daemon_id().unwrap()),
                revision: 0,
                result: CommandOutput::Done { ok: true }.into(),
                session: session(vec![split, tab(3, 301, None)], 1, TabFilter::All),
                catalog: WorkspaceCatalog::default(),
            };
            let original = serde_json::to_value(&reply).unwrap();
            remove_exited_panes(&mut reply, &HashSet::new());
            assert_eq!(serde_json::to_value(&reply).unwrap(), original);

            let exited = HashSet::from(exiting);
            remove_exited_panes(&mut reply, &exited);
            assert_eq!(reply.session.tabs.len(), 1);
            assert_eq!(reply.session.tabs[0].active, expected);
            assert_eq!(reply.session.tabs[0].layout.ids().len(), 2);
            assert_eq!(reply.session.tabs[0].directories.len(), 2);
        }
    }
}
