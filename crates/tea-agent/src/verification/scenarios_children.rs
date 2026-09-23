//! Durable isolated-child verification driven through the injected live host.
//!
//! The public entry point never discovers a provider or credentials. It needs
//! separately constructed root and child [`RestrictedZenConsumer`] values so
//! the durable root and child requests retain their real verification roles
//! even though the restricted factory intentionally pins both to one model.

use super::{
    is_exact_zen_descriptor, LiveVerificationError, RestrictedZenConsumer, VerificationConsumer,
};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tea_core::scheduler::ModelProvider;
use tea_core::state::ModelDescriptor;
use tea_session::{
    LaneId, LaneRecord, OperationOutcome, SessionFact, SessionMutationRef,
    SessionSnapshot, reduce_agent_graph,
};

/// Explicit disposable inputs for one live root/child orchestration exercise.
///
/// `workspace` must be a clean Git repository containing only public or
/// synthetic fixture material. The prompt must instruct the root to delegate
/// one isolated edit, wait for its report, and explicitly apply the reported
/// delta in the same root operation.
pub struct LiveChildScenario<'a> {
    /// Existing caller-owned Tea home for this one case.
    pub tea_home: &'a Path,
    /// Existing clean public or synthetic Git workspace.
    pub workspace: &'a Path,
    /// Guarded root-role consumer from the one restricted factory.
    pub root: &'a RestrictedZenConsumer,
    /// Guarded child-role consumer from the one restricted factory.
    pub child: &'a RestrictedZenConsumer,
    /// Public or synthetic root prompt for the child lifecycle.
    pub prompt: &'a str,
}

/// Content-free durable evidence from one live isolated-child exercise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveChildScenarioOutcome {
    /// At least one durable child-lane provider request was admitted.
    pub child_request_observed: bool,
    /// A child delta and terminal report were durable before its parent apply.
    pub isolated_workspace_verified: bool,
    /// The child produced a normal terminal report with a retained final entry.
    pub report_verified: bool,
    /// The parent durably applied the exact child delta to the clean workspace.
    pub parent_apply_verified: bool,
    /// The complete resulting session passed the durable verifier.
    pub durable_state_verified: bool,
}

/// Drive one actual root/child isolated-workspace scenario through the
/// feature-only terminal composition seam.
///
/// The two consumers are checked independently before any filesystem or
/// provider work. The child provider is injected into the child host directly,
/// rather than reusing a root-role wrapper or consulting a provider factory.
/// Returned evidence deliberately contains only booleans; no model response,
/// child report, patch, request, credential, or workspace path escapes this
/// boundary.
pub fn run_live_child_scenario(
    scenario: LiveChildScenario<'_>,
) -> Result<LiveChildScenarioOutcome, LiveVerificationError> {
    if scenario.root.role() != VerificationConsumer::Root
        || !is_exact_zen_descriptor(scenario.root.model())
    {
        return Err(LiveVerificationError::new(
            "live child verification requires the restricted canonical root consumer",
        ));
    }
    if scenario.child.role() != VerificationConsumer::Child
        || !is_exact_zen_descriptor(scenario.child.model())
    {
        return Err(LiveVerificationError::new(
            "live child verification requires the restricted canonical child consumer",
        ));
    }
    if scenario.root.model() != scenario.child.model() {
        return Err(LiveVerificationError::new(
            "live child verification requires matching restricted root and child descriptors",
        ));
    }
    run_child_scenario(
        scenario.tea_home,
        scenario.workspace,
        scenario.root.model().clone(),
        scenario.root.provider(),
        scenario.child.model().clone(),
        scenario.child.provider(),
        scenario.prompt,
    )
}

fn run_child_scenario(
    tea_home: &Path,
    workspace: &Path,
    root_model: ModelDescriptor,
    root_provider: Arc<dyn ModelProvider>,
    child_model: ModelDescriptor,
    child_provider: Arc<dyn ModelProvider>,
    prompt: &str,
) -> Result<LiveChildScenarioOutcome, LiveVerificationError> {
    if !tea_home.is_dir() || !workspace.is_dir() {
        return Err(LiveVerificationError::new(
            "live child verification requires explicit existing temporary home and workspace directories",
        ));
    }
    if prompt.trim().is_empty() {
        return Err(LiveVerificationError::new(
            "live child verification refuses an empty public or synthetic prompt",
        ));
    }
    if !workspace_is_clean(workspace)? {
        return Err(LiveVerificationError::new(
            "live child verification requires a clean public or synthetic Git workspace",
        ));
    }

    let protocol_prompt = format!(
        "{prompt}\n\nRequired protocol: delegate exactly one isolated task with `spawn_agent` using model `{}`, wait for its terminal report with `wait_agent`, then call `apply_agent_changes` for the reported delta in this same root operation. Do not access paths outside the public fixture workspace.",
        child_model.model,
    );
    let harness = crate::app::create_live_verification_child_harness(
        tea_home,
        workspace,
        root_model,
        root_provider,
        child_model,
        child_provider,
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let operation = smol::block_on(harness.run_root_prompt(protocol_prompt));
    let snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()));
    let durable_verification = harness
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()));
    let parent_clean_after = workspace_is_clean(workspace);
    let closed = smol::block_on(harness.close()).map_err(|error| LiveVerificationError::new(error.to_string()));

    operation.map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let snapshot = snapshot?;
    durable_verification?;
    let parent_clean_after = parent_clean_after?;
    closed?;

    let graph = reduce_agent_graph(&snapshot)
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let child_request_observed = graph.agents.values().any(|child| {
        child.operation_id.as_ref().is_some_and(|operation_id| {
            snapshot.records().iter().any(|record| {
                matches!(
                    &record.record,
                    LaneRecord::ProviderRequestStarted(request)
                        if request.operation_id == *operation_id
                )
            })
        })
    });
    let child = graph.agents.values().find(|child| {
        child.workspace_delta.is_some()
            || child.terminal.is_some()
            || child.operation_id.is_some()
    });
    let report_verified = child.is_some_and(|child| {
        child.terminal.as_ref().is_some_and(|terminal| {
            terminal.outcome == OperationOutcome::Completed && terminal.final_entry_id.is_some()
        })
    });
    let workspace_delta_verified = child.is_some_and(|child| {
        child.workspace_delta.as_ref().is_some_and(|delta| {
            child.terminal.as_ref().is_some_and(|terminal| {
                terminal.workspace_delta_id.as_ref() == Some(&delta.delta_id)
            })
        })
    });
    let parent_apply_verified = child.is_some_and(|child| {
        child.workspace_delta.as_ref().is_some_and(|delta| {
            child.applied.as_ref().is_some_and(|applied| {
                applied.delta_id == delta.delta_id
                    && applied.target_lane_id == LaneId::main()
                    && applied.changed_paths == delta.changed_paths
            })
        })
    }) && !parent_clean_after;
    let isolated_workspace_verified = child.is_some_and(|child| {
        child.workspace_delta.as_ref().is_some_and(|delta| {
            child_delta_precedes_parent_apply(&snapshot, &child.spawned.agent_id, &delta.delta_id)
        })
    }) && workspace_delta_verified
        && report_verified
        && parent_apply_verified;

    Ok(LiveChildScenarioOutcome {
        child_request_observed,
        isolated_workspace_verified,
        report_verified,
        parent_apply_verified,
        durable_state_verified: true,
    })
}

fn workspace_is_clean(workspace: &Path) -> Result<bool, LiveVerificationError> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| {
            LiveVerificationError::new(format!(
                "live child verification could not inspect its public Git workspace: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(LiveVerificationError::new(
            "live child verification workspace must be an accessible Git repository",
        ));
    }
    Ok(output.stdout.is_empty())
}

fn child_delta_precedes_parent_apply(
    snapshot: &SessionSnapshot,
    agent_id: &tea_session::AgentId,
    delta_id: &tea_session::WorkspaceDeltaId,
) -> bool {
    let mut delta_ready = false;
    let mut terminal = false;
    for mutation in snapshot.mutations() {
        match mutation.mutation {
            SessionMutationRef::Fact(fact) => match &fact.fact {
                SessionFact::WorkspaceDelta(delta)
                    if delta.agent_id == *agent_id && delta.delta_id == *delta_id =>
                {
                    delta_ready = true;
                }
                SessionFact::AgentTaskFinished(result)
                    if result.agent_id == *agent_id
                        && result.workspace_delta_id.as_ref() == Some(delta_id) =>
                {
                    terminal = delta_ready;
                }
                SessionFact::WorkspaceDeltaApplied(applied) if applied.delta_id == *delta_id => {
                    return delta_ready && terminal;
                }
                _ => {}
            },
            SessionMutationRef::Entry(_) | SessionMutationRef::Record(_) | SessionMutationRef::Lane(_) => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verification::{ZEN_FREE_MODEL_ID, ZEN_PROVIDER_ID};
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tea_core::scheduler::{
        CancellationToken, ModelFuture, ModelRequest, ModelStream,
        ModelStreamEvent,
    };
    use tea_core::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId};
    use tea_protocol::JsonValue;

    struct TemporaryRepository {
        directory: std::path::PathBuf,
        tea_home: std::path::PathBuf,
        workspace: std::path::PathBuf,
    }

    impl TemporaryRepository {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(1);
            let directory = std::env::temp_dir().join(format!(
                "tea-live-child-scenario-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            let tea_home = directory.join("tea-home");
            let workspace = directory.join("workspace");
            fs::create_dir_all(&tea_home).expect("temporary Tea home creates");
            fs::create_dir(&workspace).expect("temporary workspace creates");
            git(&workspace, &["init"]);
            git(&workspace, &["config", "user.name", "Tea Verification"]);
            git(
                &workspace,
                &["config", "user.email", "verification@example.invalid"],
            );
            fs::write(workspace.join("fixture.txt"), "before\n")
                .expect("public fixture writes");
            git(&workspace, &["add", "fixture.txt"]);
            git(&workspace, &["commit", "-m", "public fixture"]);
            Self {
                directory,
                tea_home,
                workspace,
            }
        }
    }

    impl Drop for TemporaryRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("Git fixture command starts");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(Default)]
    struct ScriptedRootProvider {
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ModelProvider for ScriptedRootProvider {
        fn stream<'a>(
            &'a self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let call = {
                let mut requests = self.requests.lock().expect("root request mutex");
                requests.push(request.clone());
                requests.len() - 1
            };
            let events = match call {
                0 => tool_turn(
                    "root-spawn-child",
                    "spawn_agent",
                    JsonValue::object([
                        ("task_name", JsonValue::String("fixture_child".into())),
                        (
                            "task",
                            JsonValue::String(
                                "Edit fixture.txt from before to after, then report completion."
                                    .into(),
                            ),
                        ),
                        ("model", JsonValue::String(ZEN_FREE_MODEL_ID.into())),
                        ("context", JsonValue::String("task".into())),
                    ]),
                ),
                1 => tool_turn(
                    "root-wait-child",
                    "wait_agent",
                    JsonValue::object([
                        (
                            "targets",
                            JsonValue::Array(vec![JsonValue::String("fixture_child".into())]),
                        ),
                        ("return_when", JsonValue::String("all".into())),
                        ("timeout_ms", JsonValue::from(5_000_u64)),
                    ]),
                ),
                2 => {
                    let delta_id = JsonValue::parse(&request.context)
                        .ok()
                        .and_then(|value| find_delta_id(&value, 0))
                        .expect("wait result exposes the durable delta ID");
                    tool_turn(
                        "root-apply-child",
                        "apply_agent_changes",
                        JsonValue::object([("delta_id", JsonValue::String(delta_id))]),
                    )
                }
                3 => vec![
                    ModelStreamEvent::TextDelta("public child fixture applied".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
                _ => vec![ModelStreamEvent::Error {
                    message: "scripted root received an unexpected request".into(),
                }],
            };
            Box::pin(std::future::ready(Ok(Box::new(ModelStream { events }) as _)))
        }
    }

    struct ScriptedChildProvider {
        requests: AtomicUsize,
    }

    impl ScriptedChildProvider {
        fn new() -> Self {
            Self {
                requests: AtomicUsize::new(0),
            }
        }
    }

    impl ModelProvider for ScriptedChildProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let call = self.requests.fetch_add(1, Ordering::SeqCst);
            let events = match call {
                0 => tool_turn(
                    "child-edit-fixture",
                    "edit",
                    JsonValue::object([(
                        "files",
                        JsonValue::Array(vec![JsonValue::object([
                            ("path", JsonValue::String("fixture.txt".into())),
                            (
                                "edits",
                                JsonValue::Array(vec![JsonValue::object([
                                    ("oldText", JsonValue::String("before\n".into())),
                                    ("newText", JsonValue::String("after\n".into())),
                                ])]),
                            ),
                        ])]),
                    )]),
                ),
                1 => vec![
                    ModelStreamEvent::TextDelta("child completed the public fixture edit".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
                _ => vec![ModelStreamEvent::Error {
                    message: "scripted child received an unexpected request".into(),
                }],
            };
            Box::pin(std::future::ready(Ok(Box::new(ModelStream { events }) as _)))
        }
    }

    fn tool_turn(call_id: &str, name: &str, arguments: JsonValue) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new(call_id).expect("fixture tool call ID"),
                name: name.into(),
                arguments: SerializedJson::new(
                    arguments.to_json_string().expect("fixture arguments encode"),
                ),
            }),
            ModelStreamEvent::End(StopReason::ToolUse),
        ]
    }

    fn find_delta_id(value: &JsonValue, depth: usize) -> Option<String> {
        if depth > 8 {
            return None;
        }
        if let Some(object) = value.as_object() {
            if let Some(delta_id) = object.get("delta_id").and_then(JsonValue::as_str) {
                return Some(delta_id.to_owned());
            }
            return object
                .values()
                .find_map(|value| find_delta_id(value, depth + 1));
        }
        if let Some(values) = value.as_array() {
            return values
                .iter()
                .find_map(|value| find_delta_id(value, depth + 1));
        }
        value
            .as_str()
            .and_then(|text| JsonValue::parse(text).ok())
            .and_then(|value| find_delta_id(&value, depth + 1))
    }

    #[test]
    fn scripted_counterpart_drives_distinct_injected_root_and_child_roles() {
        let repository = TemporaryRepository::new();
        let root_provider = Arc::new(ScriptedRootProvider::default());
        let child_provider = Arc::new(ScriptedChildProvider::new());
        let descriptor = ModelDescriptor {
            provider: ZEN_PROVIDER_ID.into(),
            model: ZEN_FREE_MODEL_ID.into(),
            revision: None,
        };
        let root = RestrictedZenConsumer {
            model: descriptor.clone(),
            provider: Arc::clone(&root_provider) as Arc<dyn ModelProvider>,
            role: VerificationConsumer::Root,
        };
        let child = RestrictedZenConsumer {
            model: descriptor,
            provider: Arc::clone(&child_provider) as Arc<dyn ModelProvider>,
            role: VerificationConsumer::Child,
        };

        let outcome = run_live_child_scenario(LiveChildScenario {
            tea_home: &repository.tea_home,
            workspace: &repository.workspace,
            root: &root,
            child: &child,
            prompt: "This public fixture must delegate one isolated edit, wait for its report, and apply the exact reported delta.",
        })
        .expect("scripted counterpart settles through the live-child host seam");

        assert!(outcome.child_request_observed);
        assert!(outcome.isolated_workspace_verified);
        assert!(outcome.report_verified);
        assert!(outcome.parent_apply_verified);
        assert!(outcome.durable_state_verified);
        assert_eq!(
            child_provider.requests.load(Ordering::SeqCst),
            2,
            "the injected child-role provider drives the edit and terminal report",
        );
        assert_eq!(
            root_provider.requests.lock().expect("root request mutex").len(),
            4,
            "the injected root-role provider owns spawn, wait, apply, and final turns",
        );
        assert_eq!(
            fs::read_to_string(repository.workspace.join("fixture.txt"))
                .expect("parent fixture reads"),
            "after\n",
            "the parent changes only through the explicit durable apply",
        );
    }
}
