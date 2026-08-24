//! Deterministic cacheability baseline for adjacent core model requests.

use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::compaction::{CompactionContext, CompactionFuture, CompactionResult, Compactor};
use tea_core::error::HookError;
use tea_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet};
use tea_core::measurement::measure_prompt_cacheability;
use tea_core::measurement::{
    ExpectedPromptLayoutTransition, PromptCacheScope, PromptContinuity, PromptLayoutLedger,
    PromptLayoutPolicy,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason, ThinkingLevel};
use tea_core::tool::{AgentToolResult, ToolCall};

#[derive(Clone, Default)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

struct RewritingHooks;

struct KeepFirstMessage;

impl Compactor for KeepFirstMessage {
    fn compact<'a>(
        &'a self,
        mut context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        context.messages.truncate(1);
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }
}

impl HookSet for RewritingHooks {
    fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        Ok(format!("rewritten:{}", context.messages.len()))
    }
}

impl ModelProvider for RecordingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("recording provider mutex poisoned")
            .push(request);
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("fixture response".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

/// Mirrors task-mode child projection: its stable system/tool surface and
/// logical workspace precede the assignment, which is the final variable
/// context item. The fixture leaves tools empty only because their ordered
/// definitions are constant across the two requests under comparison.
fn task_mode_child_request_with_assignment_last(task: &str, model: &str) -> ModelRequest {
    let mut system_prompt = "stable Tea v2 child system prompt".to_owned();
    tea_core::runtime::append_child_subagent_instruction_suffix(&mut system_prompt);
    ModelRequest {
        system_prompt,
        tools: Vec::new(),
        model: Some(ModelDescriptor {
            provider: "fixture".into(),
            model: model.into(),
            revision: None,
        }),
        thinking_level: ThinkingLevel::Off,
        context: format!(
            "stable child context\nlogical workspace: /workspace/logical\nassignment: {task}"
        ),
    }
}

#[test]
fn adjacent_text_turns_keep_the_prior_context_prefix() {
    let provider = RecordingProvider::default();
    let agent = Agent::builder()
        .system_prompt("stable system prompt")
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "cache-baseline".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .model_provider(Arc::new(provider.clone()))
        .build();

    for prompt in ["first turn", "second turn", "third turn"] {
        smol::block_on(
            agent
                .start_prompt(prompt)
                .expect("idle fixture agent")
                .drive(),
        )
        .expect("fixture run settles");
    }

    let requests = provider
        .requests
        .lock()
        .expect("recording provider mutex poisoned")
        .clone();
    assert_eq!(requests.len(), 3);
    let measurements = requests
        .windows(2)
        .map(|pair| measure_prompt_cacheability(Some(&pair[0]), &pair[1]))
        .collect::<Vec<_>>();
    assert!(
        measurements
            .iter()
            .all(|measurement| !measurement.cache_domain_changed)
    );
    assert!(
        measurements
            .iter()
            .all(|measurement| measurement.common_context_prefix_bytes > 0)
    );
    eprintln!(
        "cache baseline: requests={}, context_bytes={:?}, common_prefix_bytes={:?}, ratios_ppm={:?}",
        requests.len(),
        requests
            .iter()
            .map(|request| request.context.len())
            .collect::<Vec<_>>(),
        measurements
            .iter()
            .map(|measurement| measurement.common_context_prefix_bytes)
            .collect::<Vec<_>>(),
        measurements
            .iter()
            .map(|measurement| measurement.common_context_prefix_ratio_millionths)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn lane_ledgers_never_compare_sibling_child_requests_as_adjacent() {
    let root = PromptLayoutLedger::default();
    let first_child = PromptLayoutLedger::default();
    let second_child = PromptLayoutLedger::default();
    let root_request = task_mode_child_request_with_assignment_last("root task", "root-model");
    let first_request =
        task_mode_child_request_with_assignment_last("first child task", "child-model");
    let second_request =
        task_mode_child_request_with_assignment_last("second child task", "child-model");

    assert_eq!(
        root.observe(&root_request).continuity,
        PromptContinuity::FirstRequest
    );
    assert_eq!(
        first_child.observe(&first_request).continuity,
        PromptContinuity::FirstRequest
    );
    assert_eq!(
        second_child.observe(&second_request).continuity,
        PromptContinuity::FirstRequest,
        "a sibling starts a distinct logical conversation, not after the first child"
    );

    let mut first_follow_up = first_request.clone();
    first_follow_up.context.push_str("\nfollow-up: continue");
    assert_eq!(
        first_child.observe(&first_follow_up).continuity,
        PromptContinuity::ExactExtension
    );
    let mut second_follow_up = second_request.clone();
    second_follow_up.context.push_str("\nfollow-up: continue");
    assert_eq!(
        second_child.observe(&second_follow_up).continuity,
        PromptContinuity::ExactExtension
    );
}

#[test]
fn same_child_model_and_context_keep_a_stable_task_last_prefix() {
    let first =
        task_mode_child_request_with_assignment_last("A: inspect the first concern", "child-model");
    let second = task_mode_child_request_with_assignment_last(
        "B: inspect the second concern",
        "child-model",
    );
    let stable_context =
        "stable child context\nlogical workspace: /workspace/logical\nassignment: ";
    let measurement = measure_prompt_cacheability(Some(&first), &second);

    assert!(first.context.ends_with("A: inspect the first concern"));
    assert!(second.context.ends_with("B: inspect the second concern"));
    assert_eq!(
        measurement.common_context_prefix_bytes,
        stable_context.len(),
        "only the final assignment varies between otherwise identical child requests"
    );
    assert!(!measurement.cache_domain_changed);
}

#[test]
fn model_change_is_a_domain_change_even_with_identical_child_context() {
    let ledger = PromptLayoutLedger::default();
    let first = task_mode_child_request_with_assignment_last("same assignment", "child-model-a");
    let second = task_mode_child_request_with_assignment_last("same assignment", "child-model-b");
    let _ = ledger.observe(&first);
    let changed = ledger.observe(&second);

    assert_eq!(changed.continuity, PromptContinuity::DomainChanged);
    assert!(changed.cache_domain_changed);
    assert!(
        changed
            .changed_cache_domain_components
            .iter()
            .any(|component| component == "model")
    );
}

#[test]
fn child_compaction_changes_only_its_own_history_and_layout_ledger() {
    smol::block_on(async {
        let root_ledger = Arc::new(PromptLayoutLedger::default());
        let compacted_child_ledger = Arc::new(PromptLayoutLedger::default());
        let sibling_ledger = Arc::new(PromptLayoutLedger::default());
        let root = Agent::builder()
            .model_provider(Arc::new(RecordingProvider::default()))
            .prompt_layout_ledger(Arc::clone(&root_ledger))
            .build();
        let compacted_child = Agent::builder()
            .model_provider(Arc::new(RecordingProvider::default()))
            .compactor(Arc::new(KeepFirstMessage))
            .prompt_layout_ledger(Arc::clone(&compacted_child_ledger))
            .build();
        let sibling = Agent::builder()
            .model_provider(Arc::new(RecordingProvider::default()))
            .prompt_layout_ledger(Arc::clone(&sibling_ledger))
            .build();

        for (agent, prompt) in [
            (&root, "root history"),
            (&compacted_child, "child history"),
            (&sibling, "sibling history"),
        ] {
            agent
                .start_prompt(prompt)
                .expect("lane prompt starts")
                .drive()
                .await
                .expect("lane prompt settles");
        }
        let root_before = root.snapshot().messages;
        let sibling_before = sibling.snapshot().messages;

        compacted_child
            .start_compaction()
            .expect("child compaction starts")
            .drive()
            .await
            .expect("child compaction settles");

        assert_eq!(compacted_child.snapshot().messages.len(), 1);
        assert_eq!(root.snapshot().messages, root_before);
        assert_eq!(sibling.snapshot().messages, sibling_before);
        assert!(
            !Arc::ptr_eq(&root_ledger, &compacted_child_ledger)
                && !Arc::ptr_eq(&compacted_child_ledger, &sibling_ledger)
        );
    });
}

#[test]
fn require_exact_extension_rejects_domain_change_before_provider_call() {
    let provider = RecordingProvider::default();
    let agent = Agent::builder()
        .system_prompt("stable system prompt")
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "cache-policy".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .prompt_layout_ledger(Arc::new(
            PromptLayoutLedger::new(PromptCacheScope::new(7))
                .policy(PromptLayoutPolicy::RequireExactExtension),
        ))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(
        agent
            .start_prompt("first")
            .expect("first run starts")
            .drive(),
    )
    .expect("first run settles");
    let mut configuration = agent.configuration();
    configuration.system_prompt = "changed system prompt".into();
    agent
        .replace_configuration(configuration)
        .expect("idle configuration replacement");
    let error = smol::block_on(
        agent
            .start_prompt("second")
            .expect("second run starts")
            .drive(),
    )
    .expect_err("domain change is rejected before provider dispatch");
    assert_eq!(
        error,
        tea_core::error::CoreError::PromptLayoutRejected {
            continuity: PromptContinuity::DomainChanged,
        }
    );
    assert_eq!(
        provider
            .requests
            .lock()
            .expect("recording provider mutex poisoned")
            .len(),
        1,
    );
}

#[test]
fn explicit_host_domain_transition_permit_is_one_use_and_does_not_weaken_hook_rebases() {
    let provider = RecordingProvider::default();
    let agent = Agent::builder()
        .system_prompt("stable system prompt")
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "cache-policy".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .prompt_layout_ledger(Arc::new(
            PromptLayoutLedger::default().policy(PromptLayoutPolicy::RequireExactExtension),
        ))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(
        agent
            .start_prompt("first")
            .expect("first run starts")
            .drive(),
    )
    .expect("first run settles");
    let mut configuration = agent.configuration();
    configuration.system_prompt = "host-selected next prompt".into();
    agent
        .replace_configuration(configuration)
        .expect("idle configuration replacement");
    agent
        .expect_next_prompt_layout_transition(ExpectedPromptLayoutTransition::DomainChanged)
        .expect("idle host may authorize its known transition");
    smol::block_on(
        agent
            .start_prompt("second")
            .expect("second run starts")
            .drive(),
    )
    .expect("one matching host transition is permitted");

    let mut configuration = agent.configuration();
    configuration.hooks = Arc::new(RewritingHooks);
    agent
        .replace_configuration(configuration)
        .expect("idle hook replacement");
    let error = smol::block_on(
        agent
            .start_prompt("third")
            .expect("third run starts")
            .drive(),
    )
    .expect_err("a later hook rewrite has no remaining permit");
    assert_eq!(
        error,
        tea_core::error::CoreError::PromptLayoutRejected {
            continuity: PromptContinuity::Discontinuous,
        }
    );
    assert_eq!(
        provider
            .requests
            .lock()
            .expect("recording provider mutex poisoned")
            .len(),
        2,
    );
}

#[test]
fn require_exact_extension_rejects_same_domain_rewrite_before_provider_call() {
    let provider = RecordingProvider::default();
    let agent = Agent::builder()
        .system_prompt("stable system prompt")
        .hooks(Arc::new(RewritingHooks))
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "cache-policy".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .prompt_layout_ledger(Arc::new(
            PromptLayoutLedger::default().policy(PromptLayoutPolicy::RequireExactExtension),
        ))
        .model_provider(Arc::new(provider.clone()))
        .build();
    for prompt in ["first", "second"] {
        let run = agent.start_prompt(prompt).expect("run starts");
        let result = smol::block_on(run.drive());
        if prompt == "first" {
            result.expect("first run settles");
        } else {
            assert_eq!(
                result.expect_err("same-domain rewrite is rejected"),
                tea_core::error::CoreError::PromptLayoutRejected {
                    continuity: PromptContinuity::Rebased,
                }
            );
        }
    }
    assert_eq!(
        provider
            .requests
            .lock()
            .expect("recording provider mutex poisoned")
            .len(),
        1,
    );
}
