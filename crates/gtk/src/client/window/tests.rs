use super::*;
use crate::client::{
    model::{Layout, TabState, WorkspaceDefinition},
    request::Target,
};
use gmux_protocol::control as execution;

fn daemon(host: u8) -> DaemonId {
    DaemonId::from_bytes([host; 16])
}
fn tab(host: u8, id: u64) -> TabRef {
    TabRef {
        daemon: daemon(host),
        id,
    }
}
fn pane(host: u8, id: u64) -> PaneRef {
    PaneRef {
        daemon: daemon(host),
        id,
    }
}
fn saved(host: u8, id: u64) -> TabState {
    TabState {
        id: tab(host, id),
        created_at: id,
        name: format!("Tab {id}"),
        custom_name: false,
        workspace: None,
        layout: Layout::Pane(pane(host, id + 1)),
        active: pane(host, id + 1),
        directories: HashMap::new(),
    }
}
fn selection(tabs: &[TabState], active: Option<TabRef>) -> Selection {
    Selection {
        active,
        filter: TabFilter::All,
        tabs: tabs
            .iter()
            .map(|t| {
                (
                    t.id,
                    TabSelection {
                        active: t.active,
                        workspace: t.workspace,
                    },
                )
            })
            .collect(),
    }
}
fn reply(revision: u64, tabs: Vec<TabState>, output: CommandOutput) -> CommandReply {
    CommandReply {
        daemon: Some(daemon(1)),
        revision,
        result: output.into(),
        session: Session {
            tabs,
            ..Session::default()
        },
        catalog: WorkspaceCatalog::default(),
        desktop: None,
    }
}
fn done(revision: u64, tabs: Vec<TabState>) -> CommandReply {
    reply(revision, tabs, CommandOutput::Done { ok: true })
}
fn created(revision: u64, tabs: Vec<TabState>, host: u8, id: u64) -> CommandReply {
    reply(
        revision,
        tabs,
        CommandOutput::New {
            tab: tab(host, id),
            pane: pane(host, id + 1),
            workspace: None,
        },
    )
}
fn begin(c: &mut Controller, host: u8, create: bool) -> CommandId {
    c.begin(Route {
        request: RoutedRequest::Execution {
            target: Target::Local(daemon(host)),
            request: execution::Request::Internal(execution::InternalRequest::Command {
                command: if create {
                    execution::Command::New {
                        directory: None,
                        name: None,
                    }
                } else {
                    execution::Command::Tab(execution::TabCommand::Rename {
                        tab: 1,
                        name: "renamed".into(),
                    })
                },
            }),
        },
        assign: create.then_some(Assignment::CreatedTab { workspace: Some(7) }),
    })
    .unwrap()
    .0
}
fn complete(
    c: &mut Controller,
    id: CommandId,
    reply: anyhow::Result<CommandReply>,
    current: Selection,
) -> Update {
    let effect = c.reply(id, reply, current);
    let update = finish(effect);
    c.settled(id);
    update
}
fn finish(effect: Option<Effect>) -> Update {
    match effect {
        Some(Effect::Finished { update, .. }) => update.unwrap(),
        _ => panic!("Expected completion"),
    }
}
fn assign(effect: Option<Effect>) -> DesktopTab {
    match effect {
        Some(Effect::Assign { tab, .. }) => tab,
        _ => panic!("Expected assignment"),
    }
}

#[test]
fn stream_announcements_wait_for_assignment_and_only_the_latest_creation_reveals() {
    // Independent daemon lanes can finish in either order. IDs deliberately overlap.
    for newest_first in [false, true] {
        let mut c = Controller::new(0);
        let local = vec![saved(1, 1)];
        let current = || selection(&local, Some(tab(1, 1)));
        let earlier = begin(&mut c, 1, true);
        let newer = begin(&mut c, 2, true);
        let tabs = vec![saved(1, 1), saved(1, 3), saved(2, 3), saved(3, 3)];
        let state = c
            .shared(done(1, tabs.clone()), current())
            .unwrap()
            .state
            .unwrap()
            .0;
        assert_eq!(
            state.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            [tab(1, 1), tab(3, 3)]
        );
        let a = assign(c.reply(earlier, Ok(created(2, tabs.clone(), 1, 3)), current()));
        let b = assign(c.reply(newer, Ok(created(3, tabs.clone(), 2, 3)), current()));
        assert_eq!((a.reference, a.workspace), (tab(1, 3), Some(7)));
        assert_eq!((b.reference, b.workspace), (tab(2, 3), Some(7)));
        let (first, second) = if newest_first {
            (newer, earlier)
        } else {
            (earlier, newer)
        };
        let update = complete(&mut c, first, Ok(done(4, tabs.clone())), current());
        assert_eq!(update.reveal, newest_first);
        if !newest_first {
            assert!(
                !update
                    .state
                    .as_ref()
                    .unwrap()
                    .0
                    .tabs
                    .iter()
                    .any(|t| t.id == tab(2, 3))
            );
        }
        let update = complete(&mut c, second, Ok(done(5, tabs)), current());
        assert_eq!(update.reveal, !newest_first);
        assert!(c.pending.is_empty() && c.creating.is_empty());
    }
}

#[test]
fn navigation_during_execution_or_assignment_cancels_reveal_even_after_navigating_back() {
    for navigate_before_execution in [false, true] {
        let mut c = Controller::new(0);
        let local = vec![saved(1, 1), saved(1, 3)];
        let current = || selection(&local, Some(tab(1, 1)));
        let id = begin(&mut c, 1, true);
        if navigate_before_execution {
            c.navigated();
        }
        let tabs = vec![saved(1, 1), saved(1, 3), saved(1, 5)];
        assign(c.reply(id, Ok(created(1, tabs.clone(), 1, 5)), current()));
        if !navigate_before_execution {
            c.navigated();
        }
        c.navigated(); // Return to the original selection. Equality alone cannot detect this.
        let update = complete(&mut c, id, Ok(done(2, tabs)), current());
        assert!(!update.reveal);
        assert_eq!(update.state.unwrap().0.active, Some(tab(1, 1)));
    }
}

#[test]
fn exits_before_attachment_survive_streams_assignment_and_other_outstanding_commands() {
    for exit_before_reply in [false, true] {
        let mut c = Controller::new(0);
        let local = vec![saved(1, 1)];
        let current = || selection(&local, Some(tab(1, 1)));
        let new = begin(&mut c, 1, true);
        let other = begin(&mut c, 2, false);
        let tabs = vec![saved(1, 1), saved(1, 3), saved(2, 3)];
        if exit_before_reply {
            c.exited(pane(1, 4));
        }
        assign(c.reply(new, Ok(created(1, tabs.clone(), 1, 3)), current()));
        if !exit_before_reply {
            c.exited(pane(1, 4));
        }
        let update = complete(&mut c, new, Ok(done(2, tabs.clone())), current());
        assert!(
            !update
                .state
                .unwrap()
                .0
                .tabs
                .iter()
                .any(|t| t.id == tab(1, 3))
        );
        let state = c
            .shared(done(3, tabs.clone()), current())
            .unwrap()
            .state
            .unwrap()
            .0;
        assert!(!state.tabs.iter().any(|t| t.id == tab(1, 3)));
        assert!(state.tabs.iter().any(|t| t.id == tab(2, 3)));
        let update = complete(&mut c, other, Ok(done(4, tabs)), current());
        assert!(
            !update
                .state
                .unwrap()
                .0
                .tabs
                .iter()
                .any(|t| t.id == tab(1, 3))
        );
        assert!(c.exits.is_empty());
    }
}

#[test]
fn failed_dispatch_or_assignment_releases_deferrals_and_preserves_newer_execution() {
    for execution_succeeded in [false, true] {
        let mut c = Controller::new(0);
        let local = vec![saved(1, 1)];
        let current = || selection(&local, Some(tab(1, 1)));
        let id = begin(&mut c, 1, true);
        let tabs = vec![saved(1, 1), saved(1, 3)];
        c.shared(done(2, tabs.clone()), current()).unwrap();
        if execution_succeeded {
            assign(c.reply(id, Ok(created(1, tabs.clone(), 1, 3)), current()));
        }
        match c.reply(
            id,
            Err(anyhow::anyhow!("save or dispatch failed")),
            current(),
        ) {
            Some(Effect::Finished {
                update,
                refresh_on_failure,
                ..
            }) => {
                assert!(update.is_err());
                assert!(refresh_on_failure);
            }
            _ => panic!("Expected failure"),
        }
        assert!(c.creating.is_empty());
        let update = c.shared(done(3, tabs), current()).unwrap();
        assert_eq!(update.state.unwrap().0.tabs.len(), 2);
        c.settled(id);
        assert!(c.pending.is_empty());
        assert!(c.reply(id, Ok(done(4, vec![])), current()).is_none());
    }
}

#[test]
fn newer_stream_state_wins_over_delayed_completion_but_retains_the_command_result() {
    let mut c = Controller::new(0);
    let local = vec![saved(1, 1), saved(1, 3)];
    let current = || selection(&local, Some(tab(1, 3)));
    let id = begin(&mut c, 1, false);
    let state = c
        .shared(done(10, local.clone()), current())
        .unwrap()
        .state
        .unwrap()
        .0;
    assert_eq!(state.active, Some(tab(1, 3)));
    let update = complete(&mut c, id, Ok(done(9, vec![saved(1, 1)])), current());
    assert!(update.state.is_none());
    assert!(matches!(
        update.result.output,
        CommandOutput::Done { ok: true }
    ));
    assert!(c.shared(done(10, vec![]), current()).is_none());
    assert_eq!(c.revision, 10);
}

#[test]
fn cached_selection_restores_after_reconnection_until_the_user_navigates() {
    for navigated in [false, true] {
        let mut c = Controller::new(0);
        let local = vec![saved(1, 1)];
        let current = || selection(&local, Some(tab(1, 1)));
        c.restore_selection(tab(2, 1), Some(tab(1, 1)), TabFilter::All);
        c.shared(done(1, local.clone()), current()).unwrap();
        assert!(c.restore.is_some());
        if navigated {
            c.navigated();
        }
        let update = c
            .shared(done(2, vec![saved(1, 1), saved(2, 1)]), current())
            .unwrap();
        assert_eq!(
            update.state.unwrap().0.active,
            Some(tab(if navigated { 1 } else { 2 }, 1))
        );
        assert!(c.restore.is_none());
    }
}

#[test]
fn startup_selection_survives_fallback_tabs_arriving_before_the_saved_tab() {
    for navigated in [false, true] {
        let mut c = Controller::new(0);
        c.restore_selection(tab(2, 1), None, TabFilter::All);
        let state = c
            .shared(done(1, vec![]), selection(&[], None))
            .unwrap()
            .state
            .unwrap()
            .0;
        let state = c
            .shared(
                done(2, vec![saved(1, 1)]),
                selection(&state.tabs, state.active),
            )
            .unwrap()
            .state
            .unwrap()
            .0;
        assert_eq!(state.active, Some(tab(1, 1)));
        // A later snapshot of the fallback must not cancel the pending restore.
        let state = c
            .shared(
                done(3, state.tabs.clone()),
                selection(&state.tabs, state.active),
            )
            .unwrap()
            .state
            .unwrap()
            .0;
        if navigated {
            c.navigated();
        }
        let restored = c
            .shared(
                done(4, vec![saved(1, 1), saved(2, 1)]),
                selection(&state.tabs, state.active),
            )
            .unwrap()
            .state
            .unwrap()
            .0;
        assert_eq!(restored.active, Some(tab(if navigated { 1 } else { 2 }, 1)));
        assert!(c.restore.is_none());
    }
}

#[test]
fn window_close_discards_both_phases_and_ignores_late_events() {
    let mut c = Controller::new(0);
    let current = || selection(&[], None);
    let executing = begin(&mut c, 1, true);
    let assigning = begin(&mut c, 2, true);
    assign(c.reply(
        assigning,
        Ok(created(1, vec![saved(2, 1)], 2, 1)),
        current(),
    ));
    c.exited(pane(1, 2));
    c.close();
    assert!(c.pending.is_empty() && c.creating.is_empty() && c.exits.is_empty());
    for id in [executing, assigning] {
        assert!(c.reply(id, Ok(done(2, vec![])), current()).is_none());
    }
    assert!(c.shared(done(3, vec![]), current()).is_none());
    assert!(
        c.begin(Route {
            request: RoutedRequest::Organization(
                super::super::request::OrganizationRequest::Command(Command::WorkspaceRemove {
                    workspace: 7
                })
            ),
            assign: None
        })
        .is_err()
    );
}

#[test]
fn warnings_survive_assignment_and_window_targets_remain_qualified() {
    let local = vec![saved(2, 1)];
    let mut current = selection(&local, Some(tab(2, 1)));
    current.filter = TabFilter::Workspace(7);
    let mut split = Command::Tab(TabCommand::Split {
        pane: None,
        down: false,
    });
    Controller::resolve(
        &mut split,
        current.active,
        Some(local[0].active),
        current.filter,
    )
    .unwrap();
    assert!(
        matches!(split, Command::Tab(TabCommand::Split { pane: Some(p), .. }) if p == pane(2, 2))
    );
    let mut c = Controller::new(0);
    let id = begin(&mut c, 2, true);
    let mut incoming = created(1, vec![saved(2, 1), saved(2, 3)], 2, 3);
    incoming.result.warning = Some("Execution could not be saved".into());
    assign(c.reply(id, Ok(incoming), current.clone()));
    let mut incoming = done(2, vec![saved(2, 1), saved(2, 3)]);
    incoming.catalog.workspaces.push(WorkspaceDefinition {
        id: 7,
        name: "Project".into(),
        directories: Default::default(),
    });
    let update = complete(&mut c, id, Ok(incoming), current);
    assert_eq!(
        update.result.warning.as_deref(),
        Some("Execution could not be saved")
    );
    assert!(matches!(
        update.result.output,
        CommandOutput::New {
            workspace: Some(7),
            ..
        }
    ));
}

#[test]
fn failed_final_assignment_keeps_exits_through_the_failure_refresh() {
    let mut c = Controller::new(0);
    let local = vec![saved(1, 1)];
    let current = || selection(&local, Some(tab(1, 1)));
    let id = begin(&mut c, 1, true);
    let tabs = vec![saved(1, 1), saved(1, 3)];
    assign(c.reply(id, Ok(created(1, tabs.clone(), 1, 3)), current()));
    c.exited(pane(1, 4));
    assert!(matches!(
        c.reply(id, Err(anyhow::anyhow!("disk failed")), current()),
        Some(Effect::Finished {
            refresh_on_failure: true,
            ..
        })
    ));
    let update = c.shared(done(2, tabs), current()).unwrap();
    assert_eq!(update.state.unwrap().0.tabs.len(), 1);
    c.settled(id);
    assert!(c.exits.is_empty());
}

#[test]
fn split_completion_respects_stream_revisions_and_later_pane_navigation() {
    for navigated in [false, true] {
        let mut c = Controller::new(0);
        let mut tabs = vec![saved(1, 1)];
        tabs[0].layout.split(pane(1, 2), pane(1, 4), false);
        let current = || selection(&tabs, Some(tab(1, 1)));
        let id = c
            .begin(Route {
                request: RoutedRequest::Execution {
                    target: Target::Local(daemon(1)),
                    request: execution::Request::Internal(execution::InternalRequest::Command {
                        command: execution::Command::Tab(execution::TabCommand::Split {
                            pane: Some(2),
                            down: false,
                        }),
                    }),
                },
                assign: None,
            })
            .unwrap()
            .0;
        let streamed = c.shared(done(3, tabs.clone()), current()).unwrap();
        assert_eq!(streamed.state.unwrap().0.tabs[0].active, pane(1, 2));
        if navigated {
            c.navigated();
        }
        let update = complete(
            &mut c,
            id,
            Ok(reply(
                2,
                tabs.clone(),
                CommandOutput::Split { pane: pane(1, 4) },
            )),
            current(),
        );
        assert!(update.state.is_none());
        assert_eq!(update.reveal, !navigated);
    }
}

#[test]
fn a_tab_that_closed_before_its_reply_finishes_without_an_assignment() {
    let mut c = Controller::new(0);
    let current = || selection(&[], None);
    let id = begin(&mut c, 1, true);
    c.exited(pane(1, 4));
    let update = complete(&mut c, id, Ok(created(1, vec![], 1, 3)), current());
    assert!(update.state.unwrap().0.tabs.is_empty());
    assert!(matches!(update.result.output, CommandOutput::New { tab: t, .. } if t == tab(1, 3)));
    assert!(c.creating.is_empty());
}

#[test]
fn exits_remain_until_every_completed_update_is_applied() {
    let mut c = Controller::new(0);
    let local = vec![saved(1, 1)];
    let current = || selection(&local, Some(tab(1, 1)));
    let first = begin(&mut c, 1, false);
    let second = begin(&mut c, 2, false);
    c.settled(first); // An acknowledgment cannot retire a command still executing.
    finish(c.reply(first, Ok(done(1, local.clone())), current()));
    finish(c.reply(second, Ok(done(2, local.clone())), current()));
    assert!(c.reply(first, Ok(done(3, vec![])), current()).is_none());
    c.exited(pane(1, 2));
    c.settled(second);
    let update = c.shared(done(4, local), selection(&[], None)).unwrap();
    assert!(update.state.unwrap().0.tabs.is_empty());
    c.settled(first);
    assert!(c.pending.is_empty() && c.exits.is_empty());
}

#[test]
#[ignore = "implicit target resolution measurement; run alone with --nocapture"]
fn measure_implicit_target_resolution() {
    let tabs: Vec<_> = (0..512).map(|i| saved(1, i * 2 + 1)).collect();
    let active = tabs.last().unwrap().id;
    let start = std::time::Instant::now();
    for _ in 0..10_000 {
        let active_pane = tabs
            .iter()
            .find(|tab| tab.id == active)
            .map(|tab| tab.active);
        let mut command = Command::Tab(TabCommand::Split {
            pane: None,
            down: false,
        });
        Controller::resolve(&mut command, Some(active), active_pane, TabFilter::All).unwrap();
        std::hint::black_box(command);
    }
    eprintln!(
        "implicit_target_resolution tabs=512 iterations=10000 elapsed_us={}",
        start.elapsed().as_micros()
    );
}
