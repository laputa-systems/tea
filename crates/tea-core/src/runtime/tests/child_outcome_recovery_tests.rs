//! Explicit root continuation after a crash between a committed child fact
//! and the ordinary root tool-result entry that reports it.

use super::*;

fn committed_tool_result<'a>(
    snapshot: &'a tea_session::SessionSnapshot,
    tool_call_id: &str,
) -> Option<&'a ToolResultEntry> {
    snapshot.entries().iter().find_map(|entry| match &entry.body {
        SessionEntry::ToolResult(result) if result.tool_call_id == tool_call_id => Some(result),
        _ => None,
    })
}

fn inline_result_text(result: &ToolResultEntry) -> String {
    match &result.full_result {
        PayloadRef::Inline(value) => value.to_json_string().expect("inline result encodes"),
        PayloadRef::Artifact { .. } => panic!("fixture child result stays inline"),
    }
}

#[test]
fn resume_restores_an_accepted_spawn_result_without_new_child_work() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            requests,
            _reopens,
            _cleanup,
            _finalizations,
            _outcomes,
            _apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture_with_options(false, false, false, None);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child assignment commits before the root result is lost");
        let accepted_tasks = *tasks.accepted.lock().expect("fixture task count mutex");
        let prepared_workspaces = requests.lock().expect("fixture request mutex").len();
        assert!(
            committed_tool_result(
                &runtime.snapshot().expect("snapshot reads"),
                "fixture-root-replay-call"
            )
            .is_none(),
            "the fixture models a crash before the root result entry"
        );

        let _ = runtime.resume().await;

        let snapshot = runtime.snapshot().expect("recovered snapshot reads");
        let result = committed_tool_result(&snapshot, "fixture-root-replay-call")
            .expect("explicit continuation restores the committed spawn outcome");
        assert_eq!(result.tool_name, "spawn_agent");
        assert!(!result.is_error);
        assert!(
            inline_result_text(result).contains(&spawned.agent_id.to_string()),
            "the restored result names the already accepted child"
        );
        assert_eq!(
            *tasks.accepted.lock().expect("fixture task count mutex"),
            accepted_tasks,
            "restoring the result never starts another child task"
        );
        assert_eq!(
            requests.lock().expect("fixture request mutex").len(),
            prepared_workspaces,
            "restoring the result never prepares another child workspace"
        );
        let graph = reduce_agent_graph(&snapshot).expect("recovered graph reduces");
        assert_eq!(graph.agents.len(), 1, "the old spawn identity is not duplicated");
    });
}

#[test]
fn resume_restores_a_committed_apply_result_without_reapplying_the_delta() {
    smol::block_on(async {
        let (
            runtime,
            _tasks,
            _requests,
            _reopens,
            _cleanup,
            finalizations,
            apply_outcomes,
            apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture_with_options(false, false, true, None);
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("interruption retains the configured workspace delta");
        let delta_id = WorkspaceDeltaId::derive(
            &WorkspaceLeaseId::derive(&spawned.agent_id),
            "fixture-child-base",
            "fixture-child-result",
        );
        apply_outcomes
            .lock()
            .expect("fixture apply outcome mutex")
            .push_back(Ok(WorkspaceApplyOutcome::Applied {
                changed_paths: vec!["src/fixture.rs".into()],
            }));
        coordinator
            .apply(
                fixture_apply_call(&spawned.agent_id),
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                delta_id.clone(),
            )
            .await
            .expect("host application commits its durable fact");
        assert_eq!(apply_requests.lock().expect("apply request mutex").len(), 1);

        let _ = runtime.resume().await;

        let snapshot = runtime.snapshot().expect("recovered snapshot reads");
        let applied = committed_tool_result(&snapshot, "fixture-root-replay-apply-call")
            .expect("explicit continuation restores the committed apply outcome");
        assert_eq!(applied.tool_name, "apply_agent_changes");
        assert!(!applied.is_error);
        let text = inline_result_text(applied);
        assert!(text.contains(delta_id.as_str()), "{text}");
        assert!(text.contains("src/fixture.rs"), "{text}");
        assert!(
            committed_tool_result(&snapshot, "fixture-root-replay-call").is_some(),
            "the same continuation restores the sibling spawn result"
        );
        assert_eq!(
            apply_requests.lock().expect("apply request mutex").len(),
            1,
            "a committed WorkspaceDeltaApplied fact never re-enters host mutation"
        );
    });
}
