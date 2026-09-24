use super::*;

#[test]
fn busy_root_resume_cannot_reconcile_a_live_child_before_rejecting_the_claim() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            _requests,
            reopen_count,
            _cleanup_fail,
            _finalizations,
            _apply_outcomes,
            _apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture_with_options(false, true, false, None);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let child = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation is durably accepted and locally live");
        let before = runtime.snapshot().expect("pre-resume snapshot reads");
        assert_eq!(tasks.owned_task_count(), 1, "child drive remains live");

        runtime
            .claim_root_before_acceptance_for_test()
            .expect("fixture holds the root drive claim");
        let error = runtime
            .resume()
            .await
            .expect_err("a concurrent root resume must reject before child reconciliation");
        assert!(
            error
                .to_string()
                .contains("durable harness already has an active drive")
        );
        assert_eq!(
            runtime.snapshot().expect("rejected resume snapshot reads"),
            before,
            "a busy root claim prevents every recovery-side session mutation"
        );
        assert_eq!(
            reduce_agent_graph(&runtime.snapshot().expect("child graph reads"))
                .expect("child graph reduces")
                .agents[&child.agent_id]
                .state,
            AgentState::Running,
            "the rejected root continuation cannot abort or finalize its live child"
        );
        assert_eq!(
            tasks.owned_task_count(),
            1,
            "rejected resume cannot reap child work"
        );
        assert_eq!(
            *reopen_count.lock().expect("fixture reopen mutex"),
            0,
            "rejected resume cannot contact the child recovery host"
        );
    });
}
