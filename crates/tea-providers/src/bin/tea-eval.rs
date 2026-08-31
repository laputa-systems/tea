//! Provider-backed durable Tea adapter for the explicit Pi shootout.
//!
//! The executable accepts a credential only from its final `vault ... --`
//! process boundary. Coding-tool children receive the separate `--shell-env`
//! allowlist and never inherit that credential.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::future::Future;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use tea_core::agent::AgentConfiguration;
use tea_core::coding::{
    CodingHost, CodingOperations, CommandEnvironment, CommandOutput, EditTransaction,
    EditTransactionOutcome, EntryMetadata, FileSnapshot, LocalCodingOperations, OperationFuture,
    SearchResult, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
    WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
};
use tea_core::error::HookError;
use tea_core::event::AgentEventKind;
use tea_core::harness::extension::{
    ExtensionCapability, ExtensionCapabilityBindings, ExtensionCapabilityFuture,
    ExtensionCapabilityRequest, ExtensionEngine, ExtensionLimits,
    ExtensionMemoryCollector, ExtensionPromptSection, ExtensionToolLimits,
};
use tea_core::harness::{
    HarnessActor, HarnessRepository, HarnessResolver, HarnessResourceLimits, HarnessSnapshotSpec,
    ModelHarnessProfile, SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode,
    ToolPresentationDescriptor,
};
use tea_core::hooks::{
    AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookFuture, HookSet,
};
use tea_core::runtime::{
    HarnessEvent, HarnessIdentity, RuntimePolicyIdentities, RuntimeServices, SessionSupervisor,
    SessionSupervisorInput, TeaEvent,
};
use tea_core::scheduler::CancellationToken;
use tea_core::state::{AgentMessage, ModelDescriptor, ThinkingLevel};
use tea_core::tool::{AgentToolResult, ToolCall, ToolExecutionMode, ToolRegistry, ToolUpdateSink};
use tea_luau::{LuauExtensionEngine, builtins};
use tea_protocol::{JsonNumber, JsonValue};
use tea_providers::openai::OpenAiContextHook;
use tea_providers::openrouter::{OpenRouterConfig, OpenRouterProvider, OpenRouterRequestCapture};
use tea_providers::RetryPolicy;
use tea_session::{
    CanonicalHashWriter, Digest, DurabilityMode, EntryId, HarnessRevisionChangedEntry,
    JsonlSession, LaneId, LaneRecord, ModelChangedEntry, ProvisionedEntry, SessionEntry,
    SessionHeader, SessionId, SessionSnapshot, SessionWriter, StepAttemptedRecord, StepKind,
    ThinkingChangedEntry,
};

const RESULT_SCHEMA: &str = "tea-coding-eval-result/v4";
const REQUIRED_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
const REQUIRED_THINKING: &str = "high";
const SHOOTOUT_TEMPERATURE: f64 = 0.0;
const SHOOTOUT_SEED: u64 = 20260829;
// Paired static runs use zero replay retries. This constant controls both the
// executable OpenRouter retry policy and the emitted controlled-policy witness.
const SHOOTOUT_PROVIDER_MAX_RETRIES: u32 = 0;

/// The exact controlled-policy witness emitted with each paired shootout
/// result. Keep it coupled to the executable provider policy below.
fn shootout_provider_retry_evidence() -> JsonValue {
    JsonValue::object([
        ("enabled", JsonValue::Bool(true)),
        (
            "max_retries",
            JsonValue::from(u64::from(SHOOTOUT_PROVIDER_MAX_RETRIES)),
        ),
    ])
}

/// Keep the provider request from expiring before the outer shootout budget.
/// A zero outer budget is an uncapped diagnostic, but the HTTP transport still
/// needs a finite deadline to avoid an OS-level socket hanging forever.
const DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS: u64 = 86_400;
/// Evaluation-only guidance that mirrors the concise parts of Pi's native
/// coding prompt. It makes completion—not repository-history exploration—the
/// static agent's next action after it has identified a bounded code fix. It
/// does not change the closed capability set or any core runtime policy.
const STATIC_CODING_GUIDELINES: &str = "You are an expert coding assistant operating inside Tea, a coding agent harness. You help users by reading files, executing bounded commands, and editing code.\n\nGuidelines:\n- Work directly toward the requested code change. Do not finish after inspection when the requested code change is still unmade. A code-change task is not complete with an empty diff.\n- Treat the stated task as the specification. Do not inspect Git history, branches, tags, reflogs, remotes, or upstream references. Do not repeat an inspection that did not change the repair decision.\n- When a task names a source file, inspect it and then make the smallest safe edit before giving a long explanation. If the next response would only restate the same hypothesis, edit or run the focused check instead.\n- After inspecting the named target and its immediate dependencies, form the smallest root-cause hypothesis, make the edit, and run a focused reproduction or validator.\n- Be concise in responses and show file paths clearly.\n- Use `read` to inspect known files instead of using `bash` with cat or sed.\n- Use `find` for workspace discovery; never search from `/`, inspect home/cache directories, or inspect outside the workspace.\n- Keep bash commands bounded and workspace-local; do not run network, package-install, or upstream/repository-history probes.\n- Honor the requested null/undefined semantics; when an optional nullish value should be ignored, guard it before validation or calculation.\n- Batch independent inspections in one tool turn when practical.\n- Use `edit` for precise changes, then run the relevant validator and stop once the fix is verified.\n- For targeted edits, use `files[].edits[]` with exact `oldText` and `newText`; batch independent edits in one call.\n- Keep edit matches small and unique; do not repeat unchanged probes.\n- Once the relevant check passes, finish without further exploratory inspection.";
const STATIC_BASH_GIT_HISTORY_INVITATION: &str = "Git, builds, and ordinary directory inspection.";
const STATIC_BASH_NO_HISTORY_GUIDANCE: &str = "workspace-local builds, and focused local validation. Use `find` for workspace discovery.";
/// Task-specific diagnostic guidance, not a general coding policy. It is
/// intentionally available only through the evidence-marked Tea-only profile
/// so a successful screen cannot be mistaken for a paired Pi/Tea result.
const STATIC_PREFIX_GUARD_DIAGNOSTIC_GUIDANCE: &str = "Routing tasks: a RegExp substring match is not a mount prefix. Only trim a `layerPath` that equals the start of `path`; otherwise continue to the next layer unchanged. Put the guard at the existing trim boundary; do not expand `layerPath` or modify matching internals.";
/// A more prescriptive follow-up screen after `prefix-guard-v1` established
/// that the semantic reminder alone still allowed long repro-file loops.
/// This remains task-specific Tea-only diagnostic guidance, never a paired
/// candidate or a replacement for the generic static coding prompt.
const STATIC_PREFIX_GUARD_FOCUSED_DIAGNOSTIC_GUIDANCE: &str = "Routing-task diagnostic: after reading `lib/router/index.js`, edit only that file. In `trim_prefix`, before the existing path-separator validation, reject a `layerPath` that is not a prefix of `path`; then run the focused validator. Do not create reproduction files or modify matching internals.";
const EDIT_RECOVERY_PROJECTION_HINT: &str = "The preceding edit was rejected before execution. Reissue it with one top-level `files` array; each `files[]` entry contains `path` and exactly one of `edits` or `content`. Do not use top-level `path` or `edits`.";
const EDIT_RECOVERY_PROJECTION_IDENTITY: &str = "tea-eval-edit-recovery-projection-canonical-v1";
const PRE_EDIT_TOOL_GATE_BLOCK_REASON: &str = "Pre-edit direct workflow policy: before a successful edit result, bash and find are unavailable. Read the named source and make the smallest edit to the named target; after a successful edit, use bash or find only for focused validation.";
const SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON: &str = "Pre-edit source-local workflow policy: before a successful edit to a declared task target, only read and edit calls whose paths are declared task targets are available. Bash, find, and non-target read/edit calls are unavailable; after a successful target-local edit, use other tools only for focused validation.";
const POST_EDIT_VALIDATION_BLOCK_REASON: &str = "Validation evidence requires a direct foreground command whose exit status is visible. Avoid pipelines and status-suppression wrappers; choose an appropriate workspace-local check.";
const POST_EDIT_VALIDATION_REMINDER: &str = "Before finalizing, run an appropriate workspace-local check after the most recent successful edit. Run it directly so its exit status is visible; avoid pipelines and status-suppression wrappers. Choose the check from the task and workspace, address any failure, then finish.";
const PRE_EDIT_TOOL_GATE_IDENTITY_PREFIX: &str = "pre-edit-direct-workflow-policy-v1";
const JIT_ADDENDUM: &str = "Task-local harness adaptation is available but optional.\n\nFirst inspect the task and repository using the normal coding tools. Use NoChange unless you have concrete evidence that one bounded harness change is likely to improve this task.\n\nYou may stage at most one task-local harness candidate. It may alter only currently supported prompt, tool-presentation, hook, context, memory-selection, failure-policy, or compaction-policy surfaces. It cannot grant new authority, change the provider or model, access hidden validators, use subagents, or add a web-research tool.\n\nA candidate must include observed task or failure evidence, a root-cause hypothesis, expected effect, regression risk, and the harness surfaces changed. If it activates, continue solving the same task under the new immutable harness revision. All adaptation time and model usage count toward the task result.";

/// Evaluation-only recovery projection for the known invalid Tea edit envelope.
///
/// The core has already rejected and durably retained the raw model call when
/// this runs. The cloned model context gains a concise retry hint only for the
/// immediate recovery request; no arguments are accepted or rewritten.
#[derive(Clone, Copy, Debug, Default)]
struct EditRecoveryProjectionHook;

impl HookSet for EditRecoveryProjectionHook {
    fn identity(&self) -> Digest {
        Digest::from_bytes(EDIT_RECOVERY_PROJECTION_IDENTITY)
    }

    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        HookSet::before_tool_call(&OpenAiContextHook, call)
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        HookSet::after_tool_call(&OpenAiContextHook, call, result)
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        let context = HookSet::transform_context(&OpenAiContextHook, context)?;
        Ok(project_invalid_edit_recovery(context))
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        HookSet::convert_to_llm(&OpenAiContextHook, context)
    }

    fn should_stop_after_turn(&self, context: &ContextEnvelope) -> Result<bool, HookError> {
        HookSet::should_stop_after_turn(&OpenAiContextHook, context)
    }

    fn prepare_next_turn(
        &self,
        context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        HookSet::prepare_next_turn(&OpenAiContextHook, context)
    }
}

/// Static paired workflow gate used after prompt-only screens repeatedly chose
/// unbounded exploration over a named, source-local repair.
///
/// The gate never rewrites tool arguments. Its pre-edit state is derived from
/// durable context on every decision. `source-local-v1` additionally permits
/// `read` and `edit` only for paths explicitly declared by the versioned task
/// metadata until a successful result is correlated to one admitted edit ID.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PreEditToolGate {
    mode: PreEditToolGateMode,
    source_local_targets: Vec<String>,
}

impl PreEditToolGate {
    fn from_task(mode: PreEditToolGateMode, task: &JsonValue, prompt: &str, workspace: &Path) -> Result<Self, String> {
        let source_local_targets = match mode {
            PreEditToolGateMode::SourceLocalV1 => source_local_task_targets(task, prompt, workspace)?,
            PreEditToolGateMode::None | PreEditToolGateMode::DirectEditV1 => Vec::new(),
        };
        Ok(Self { mode, source_local_targets })
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            mode: PreEditToolGateMode::None,
            source_local_targets: Vec::new(),
        }
    }

    #[cfg(test)]
    fn direct_edit_v1() -> Self {
        Self {
            mode: PreEditToolGateMode::DirectEditV1,
            source_local_targets: Vec::new(),
        }
    }

    fn source_local_target_set(&self) -> BTreeSet<&str> {
        self.source_local_targets.iter().map(String::as_str).collect()
    }

    fn policy_identity(&self) -> String {
        self.surface()
            .to_json_string()
            .expect("pre-edit policy surface encodes")
    }

    fn surface(&self) -> JsonValue {
        let mode = self.mode;
        JsonValue::object([
            ("mode", JsonValue::from(mode.name())),
            (
                "blocked_tools",
                JsonValue::Array(
                    mode.blocked_tools()
                        .iter()
                        .map(|tool| JsonValue::from(*tool))
                        .collect(),
                ),
            ),
            (
                "target_restricted_tools",
                JsonValue::Array(
                    mode.target_restricted_tools()
                        .iter()
                        .map(|tool| JsonValue::from(*tool))
                        .collect(),
                ),
            ),
            (
                "source_local_targets",
                JsonValue::Array(
                    self.source_local_targets
                        .iter()
                        .cloned()
                        .map(JsonValue::from)
                        .collect(),
                ),
            ),
            (
                "unlocks_after",
                mode.unlocks_after()
                    .map(JsonValue::from)
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "same_batch_rule",
                mode.same_batch_rule()
                    .map(JsonValue::from)
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "block_reason_sha256",
                mode.block_reason_sha256()
                    .map(JsonValue::from)
                    .unwrap_or(JsonValue::Null),
            ),
        ])
    }
}

/// The optional paired-static post-edit sidecar. It never identifies a test,
/// invokes a validator, changes tool presentation, or interprets command
/// output. Its narrow evidence claim is limited to an unmasked foreground
/// `bash` result after a declared target edit has settled successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostEditValidationGateMode {
    None,
    UnmaskedEvidenceV1,
}

impl PostEditValidationGateMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "unmasked-evidence-v1" => Ok(Self::UnmaskedEvidenceV1),
            _ => Err("--post-edit-validation-gate must be none or unmasked-evidence-v1".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UnmaskedEvidenceV1 => "unmasked-evidence-v1",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PostEditValidationGate {
    mode: PostEditValidationGateMode,
    source_local_targets: BTreeSet<String>,
}

impl PostEditValidationGate {
    fn from_pre_edit(mode: PostEditValidationGateMode, pre_edit: &PreEditToolGate) -> Result<Self, String> {
        if mode == PostEditValidationGateMode::UnmaskedEvidenceV1
            && pre_edit.mode != PreEditToolGateMode::SourceLocalV1
        {
            return Err("unmasked-evidence-v1 requires --pre-edit-tool-gate source-local-v1".into());
        }
        let source_local_targets: BTreeSet<String> =
            pre_edit.source_local_targets.iter().cloned().collect();
        if mode == PostEditValidationGateMode::UnmaskedEvidenceV1 && source_local_targets.is_empty() {
            return Err("unmasked-evidence-v1 requires declared source-local targets".into());
        }
        Ok(Self { mode, source_local_targets })
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self { mode: PostEditValidationGateMode::None, source_local_targets: BTreeSet::new() }
    }

    fn enabled(&self) -> bool {
        self.mode == PostEditValidationGateMode::UnmaskedEvidenceV1
    }

    fn policy_identity(&self) -> String {
        self.surface().to_json_string().expect("post-edit policy surface encodes")
    }

    fn surface(&self) -> JsonValue {
        if !self.enabled() {
            return JsonValue::object([
                ("mode", JsonValue::from("none")),
                ("applies_after", JsonValue::Null),
                ("qualifies_with", JsonValue::Null),
                ("resets_after", JsonValue::Null),
                ("same_batch_rule", JsonValue::Null),
                ("command_profile", JsonValue::Null),
                ("completion_reminder_limit", JsonValue::from(0_u64)),
                ("block_reason_sha256", JsonValue::Null),
                ("reminder_sha256", JsonValue::Null),
            ]);
        }
        JsonValue::object([
            ("mode", JsonValue::from(self.mode.name())),
            ("applies_after", JsonValue::from("prior-successful-declared-target-edit-result")),
            ("qualifies_with", JsonValue::from("prior-successful-unmasked-direct-foreground-bash-result")),
            ("resets_after", JsonValue::from("later-successful-edit-result")),
            ("same_batch_rule", JsonValue::from("evidence-requires-prior-successful-bash-result")),
            ("command_profile", JsonValue::from("unmasked-direct-foreground-bash/v1")),
            ("completion_reminder_limit", JsonValue::from(1_u64)),
            ("block_reason_sha256", JsonValue::from(sha256(POST_EDIT_VALIDATION_BLOCK_REASON.as_bytes()))),
            ("reminder_sha256", JsonValue::from(sha256(POST_EDIT_VALIDATION_REMINDER.as_bytes()))),
        ])
    }

    fn target_edit(&self, arguments: &JsonValue) -> bool {
        arguments
            .get("files")
            .and_then(JsonValue::as_array)
            .is_some_and(|files| {
                !files.is_empty()
                    && files.iter().all(|file| {
                        file.get("path")
                            .and_then(JsonValue::as_str)
                            .is_some_and(|path| self.source_local_targets.contains(path))
                    })
            })
    }

    fn validation_violation(
        &self,
        call: &ToolCall,
        context: &ContextEnvelope,
        exit_status_observations: &BTreeMap<String, bool>,
    ) -> Option<&'static str> {
        if !self.enabled()
            || call.name != "bash"
            || !validation_evidence_from_context(self, context, exit_status_observations).pending()
        {
            return None;
        }
        let arguments = JsonValue::parse(call.arguments.as_str()).ok();
        let command = arguments
            .as_ref()
            .and_then(|arguments| arguments.get("command"))
            .and_then(JsonValue::as_str);
        (!direct_foreground_shell_v1(command)).then_some(POST_EDIT_VALIDATION_BLOCK_REASON)
    }
}

/// `unmasked-direct-foreground-bash/v1` is intentionally syntactic and
/// conservative. It does not look for test names, expected output, package
/// scripts, or validator conventions; it only rejects syntax that can mask or
/// redirect the outer command's exit status.
fn direct_foreground_shell_v1(command: Option<&str>) -> bool {
    let Some(command) = command.filter(|command| !command.trim().is_empty()) else {
        return false;
    };
    if command.contains('\0') || command.contains('\n') || command.contains('\r') {
        return false;
    }
    if command.chars().any(|character| matches!(character, ';' | '&' | '|' | '$' | '`' | '(' | ')' | '<' | '>' | '\'' | '"' | '\\')) {
        return false;
    }
    !command.split_whitespace().any(|word| {
        let evaluator = word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase();
        matches!(evaluator.as_str(), "sh" | "bash" | "zsh" | "dash" | "fish" | "." | "source" | "eval" | "exec")
    })
}

/// The adapter records only the trusted process receipt boolean, keyed by the
/// core-owned call ID. It never stores a command, stdout, stderr, validator
/// identity, or nonzero status value. A missing in-memory receipt is
/// deliberately non-qualifying after a fresh static run is interrupted.
#[derive(Clone, Default)]
struct DirectExitStatusWitness {
    outcomes: Arc<Mutex<BTreeMap<String, bool>>>,
}

impl DirectExitStatusWitness {
    fn record(&self, tool_call_id: String, exit_zero: bool) {
        self.outcomes
            .lock()
            .expect("exit witness outcome map is not poisoned")
            .insert(tool_call_id, exit_zero);
    }

    fn take(&self, tool_call_id: &str) -> Option<bool> {
        self.outcomes
            .lock()
            .expect("exit witness outcome map is not poisoned")
            .remove(tool_call_id)
    }
}

/// Results observed by the post-tool hook. The durable reducer still derives
/// edit ordering and candidate identity from assistant/result pairs; this
/// sidecar contributes only the explicit process-zero fact, and lost state is
/// conservatively treated as missing evidence.
#[derive(Clone, Default)]
struct ExitStatusObservations {
    outcomes: Arc<Mutex<BTreeMap<String, bool>>>,
}

impl ExitStatusObservations {
    fn record(&self, tool_call_id: String, exit_zero: bool) {
        self.outcomes
            .lock()
            .expect("exit observation map is not poisoned")
            .insert(tool_call_id, exit_zero);
    }

    fn snapshot(&self) -> BTreeMap<String, bool> {
        self.outcomes
            .lock()
            .expect("exit observation map is not poisoned")
            .clone()
    }
}

/// Wrap only the already-granted process capability. The decorator leaves the
/// response and capability authority unchanged while binding its typed process
/// receipt to the immutable tool-call ID supplied by the core.
#[derive(Clone)]
struct ExitWitnessProcessCapability {
    delegate: Arc<dyn ExtensionCapability>,
    witness: DirectExitStatusWitness,
}

impl ExitWitnessProcessCapability {
    fn new(delegate: Arc<dyn ExtensionCapability>, witness: DirectExitStatusWitness) -> Self {
        Self { delegate, witness }
    }
}

impl ExtensionCapability for ExitWitnessProcessCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let delegate = Arc::clone(&self.delegate);
        let witness = self.witness.clone();
        Box::pin(async move {
            let tool_call_id = request.call_id.to_string();
            let response = delegate.invoke(request, cancellation).await?;
            let exit_zero = response.value.get("termination").and_then(JsonValue::as_str) == Some("exited")
                && matches!(response.value.get("exitCode"), Some(JsonValue::Number(JsonNumber::Signed(0))) | Some(JsonValue::Number(JsonNumber::Unsigned(0))));
            witness.record(tool_call_id, exit_zero);
            Ok(response)
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationCandidate {
    generation: u64,
    call_id_sha256: String,
    arguments_sha256: String,
    eligible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationTransition {
    transition: &'static str,
    generation: u64,
    qualifying_call_id_sha256: Option<String>,
    qualifying_arguments_sha256: Option<String>,
    process_exit: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationEvidence {
    enabled: bool,
    generation: u64,
    qualifying: Option<ValidationCandidate>,
    candidate_failures: u64,
    masked_call_blocks: u64,
    reminders_issued: u64,
    transitions: Vec<ValidationTransition>,
    admitted_edit_targets: BTreeMap<String, bool>,
    bash_candidates: BTreeMap<String, ValidationCandidate>,
}

impl ValidationEvidence {
    fn disabled() -> Self {
        Self {
            enabled: false,
            generation: 0,
            qualifying: None,
            candidate_failures: 0,
            masked_call_blocks: 0,
            reminders_issued: 0,
            transitions: Vec::new(),
            admitted_edit_targets: BTreeMap::new(),
            bash_candidates: BTreeMap::new(),
        }
    }

    fn enabled() -> Self {
        Self { enabled: true, ..Self::disabled() }
    }

    fn pending(&self) -> bool {
        self.enabled && self.generation > 0 && self.qualifying.is_none()
    }

    fn observe_assistant(&mut self, gate: &PostEditValidationGate, calls: Vec<(&str, &str, &JsonValue)>) {
        let batch_has_edit = calls.iter().any(|(_, name, _)| *name == "edit");
        for (id, name, arguments) in calls {
            if name == "edit" {
                self.admitted_edit_targets.insert(id.to_owned(), gate.target_edit(arguments));
                continue;
            }
            if name != "bash" || !self.pending() {
                continue;
            }
            let command = arguments.get("command").and_then(JsonValue::as_str);
            if !direct_foreground_shell_v1(command) {
                self.masked_call_blocks = self.masked_call_blocks.saturating_add(1);
                self.transitions.push(ValidationTransition {
                    transition: "masked-bash-blocked", generation: self.generation,
                    qualifying_call_id_sha256: None, qualifying_arguments_sha256: None, process_exit: None,
                });
                continue;
            }
            self.bash_candidates.insert(id.to_owned(), ValidationCandidate {
                generation: self.generation,
                call_id_sha256: sha256(id.as_bytes()),
                arguments_sha256: sha256(arguments.to_json_string().expect("tool arguments encode").as_bytes()),
                eligible: !batch_has_edit,
            });
        }
    }

    fn observe_result(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        is_error: bool,
        process_exit_zero: bool,
    ) {
        if tool_name == "edit" {
            let declared_target = self.admitted_edit_targets.remove(tool_call_id).unwrap_or(false);
            if !is_error && (declared_target || self.generation > 0) {
                self.generation = self.generation.saturating_add(1);
                self.qualifying = None;
                self.transitions.push(ValidationTransition {
                    transition: "edit-pending", generation: self.generation,
                    qualifying_call_id_sha256: None, qualifying_arguments_sha256: None, process_exit: None,
                });
            }
            return;
        }
        if tool_name != "bash" {
            return;
        }
        let Some(candidate) = self.bash_candidates.remove(tool_call_id) else {
            return;
        };
        if !candidate.eligible || candidate.generation != self.generation || !self.pending() {
            return;
        }
        if is_error || !process_exit_zero {
            self.candidate_failures = self.candidate_failures.saturating_add(1);
            self.transitions.push(ValidationTransition {
                transition: "candidate-failed", generation: self.generation,
                qualifying_call_id_sha256: None, qualifying_arguments_sha256: None, process_exit: None,
            });
            return;
        }
        self.qualifying = Some(candidate.clone());
        self.transitions.push(ValidationTransition {
            transition: "evidence-satisfied", generation: self.generation,
            qualifying_call_id_sha256: Some(candidate.call_id_sha256),
            qualifying_arguments_sha256: Some(candidate.arguments_sha256),
            process_exit: Some("exited-zero"),
        });
    }

    fn issue_reminder(&mut self) -> bool {
        if !self.pending() || self.reminders_issued != 0 {
            return false;
        }
        self.reminders_issued = 1;
        self.transitions.push(ValidationTransition {
            transition: "completion-reminder-issued", generation: self.generation,
            qualifying_call_id_sha256: None, qualifying_arguments_sha256: None, process_exit: None,
        });
        true
    }

    fn mark_missing(&mut self) {
        if !self.pending() || self.reminders_issued != 1 {
            return;
        }
        self.transitions.push(ValidationTransition {
            transition: "evidence-missing", generation: self.generation,
            qualifying_call_id_sha256: None, qualifying_arguments_sha256: None, process_exit: None,
        });
    }

    fn json(&self) -> JsonValue {
        let (state, generation, qualifying_call_id_sha256, qualifying_arguments_sha256, candidate_failures, masked_call_blocks, reminders_issued) =
            if !self.enabled || self.generation == 0 {
                ("not_required", JsonValue::Null, JsonValue::Null, JsonValue::Null, 0_u64, 0_u64, 0_u64)
            } else if let Some(candidate) = &self.qualifying {
                ("satisfied", JsonValue::from(self.generation), JsonValue::from(candidate.call_id_sha256.clone()), JsonValue::from(candidate.arguments_sha256.clone()), self.candidate_failures, self.masked_call_blocks, self.reminders_issued)
            } else {
                ("missing", JsonValue::from(self.generation), JsonValue::Null, JsonValue::Null, self.candidate_failures, self.masked_call_blocks, self.reminders_issued)
            };
        let transitions = JsonValue::Array(self.transitions.iter().map(ValidationTransition::json).collect());
        JsonValue::object([
            ("state", JsonValue::from(state)),
            ("edit_generation", generation),
            ("qualifying_call_id_sha256", qualifying_call_id_sha256),
            ("qualifying_arguments_sha256", qualifying_arguments_sha256),
            (
                "qualifying_process_exit",
                if self.qualifying.is_some() {
                    JsonValue::from("exited-zero")
                } else {
                    JsonValue::Null
                },
            ),
            ("candidate_failures", JsonValue::from(candidate_failures)),
            ("masked_call_blocks", JsonValue::from(masked_call_blocks)),
            ("reminders_issued", JsonValue::from(reminders_issued)),
            ("transitions_sha256", JsonValue::from(sha256(transitions.to_json_string().expect("validation transitions encode").as_bytes()))),
        ])
    }

    fn trace(&self) -> Vec<JsonValue> {
        self.transitions.iter().map(|transition| {
            let mut value = transition.json();
            value.as_object_mut().expect("transition is an object").insert("type".into(), JsonValue::from("post_edit_validation_transition"));
            value
        }).collect()
    }
}

impl ValidationTransition {
    fn json(&self) -> JsonValue {
        JsonValue::object([
            ("transition", JsonValue::from(self.transition)),
            ("generation", JsonValue::from(self.generation)),
            ("qualifying_call_id_sha256", self.qualifying_call_id_sha256.clone().map(JsonValue::from).unwrap_or(JsonValue::Null)),
            ("qualifying_arguments_sha256", self.qualifying_arguments_sha256.clone().map(JsonValue::from).unwrap_or(JsonValue::Null)),
            ("process_exit", self.process_exit.map(JsonValue::from).unwrap_or(JsonValue::Null)),
        ])
    }
}

fn validation_evidence_from_context(
    gate: &PostEditValidationGate,
    context: &ContextEnvelope,
    exit_status_observations: &BTreeMap<String, bool>,
) -> ValidationEvidence {
    if !gate.enabled() {
        return ValidationEvidence::disabled();
    }
    let mut evidence = ValidationEvidence::enabled();
    for message in &context.messages {
        match message {
            AgentMessage::Assistant { tool_calls, .. } => {
                let parsed = tool_calls.iter().map(|call| {
                    let arguments = JsonValue::parse(call.arguments.as_str()).unwrap_or(JsonValue::Null);
                    (call.id.to_string(), call.name.as_str(), arguments)
                }).collect::<Vec<_>>();
                evidence.observe_assistant(gate, parsed.iter().map(|(id, name, arguments)| (id.as_str(), *name, arguments)).collect());
            }
            AgentMessage::ToolResult { tool_call_id, tool_name, is_error, .. } => {
                let tool_call_id = tool_call_id.to_string();
                let exit_zero = exit_status_observations.get(&tool_call_id).copied().unwrap_or(false);
                evidence.observe_result(&tool_call_id, tool_name, *is_error, exit_zero);
            }
            _ => {}
        }
    }
    evidence
}

fn validation_evidence_from_snapshot(
    gate: &PostEditValidationGate,
    snapshot: &SessionSnapshot,
    exit_status_observations: &BTreeMap<String, bool>,
) -> ValidationEvidence {
    if !gate.enabled() {
        return ValidationEvidence::disabled();
    }
    let mut evidence = ValidationEvidence::enabled();
    for entry in snapshot.entries() {
        match &entry.body {
            SessionEntry::AssistantMessage(message) => {
                evidence.observe_assistant(gate, message.tool_calls.iter().map(|call| (call.id.as_str(), call.name.as_str(), &call.arguments)).collect());
            }
            SessionEntry::ToolResult(result) => {
                let exit_zero = exit_status_observations
                    .get(&result.tool_call_id)
                    .copied()
                    .unwrap_or(false);
                evidence.observe_result(&result.tool_call_id, &result.tool_name, result.is_error, exit_zero);
            }
            _ => {}
        }
    }
    evidence
}

struct PreEditToolGateHook {
    edit_recovery_projection: EditRecoveryProjectionMode,
    gate: PreEditToolGate,
    post_edit_validation_gate: PostEditValidationGate,
    direct_exit_status_witness: DirectExitStatusWitness,
    exit_status_observations: ExitStatusObservations,
}

impl HookSet for PreEditToolGateHook {
    fn identity(&self) -> Digest {
        Digest::from_bytes(format!(
            "{PRE_EDIT_TOOL_GATE_IDENTITY_PREFIX}:{}:post-edit={}:edit-recovery={}",
            self.gate.policy_identity(),
            self.post_edit_validation_gate.policy_identity(),
            self.edit_recovery_projection.name(),
        ))
    }

    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        // The runtime uses the asynchronous form below because it carries the
        // durable context needed for the state-derived decision.
        HookSet::before_tool_call(&OpenAiContextHook, call)
    }

    fn before_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, BeforeToolCall> {
        Box::pin(async move {
            if let Some(reason) = pre_edit_tool_gate_violation(&self.gate, call, &context) {
                return Ok(BeforeToolCall::Block {
                    reason: reason.to_owned(),
                });
            }
            let exit_status_observations = self.exit_status_observations.snapshot();
            if let Some(reason) = self.post_edit_validation_gate.validation_violation(
                call,
                &context,
                &exit_status_observations,
            ) {
                return Ok(BeforeToolCall::Block {
                    reason: reason.to_owned(),
                });
            }
            HookSet::before_tool_call_async(&OpenAiContextHook, call, context, cancellation).await
        })
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        if call.name == "bash" {
            let exit_zero = self
                .direct_exit_status_witness
                .take(&call.id.to_string())
                .unwrap_or(false);
            self.exit_status_observations
                .record(call.id.to_string(), !result.is_error && exit_zero);
        }
        HookSet::after_tool_call(&OpenAiContextHook, call, result)
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        let context = HookSet::transform_context(&OpenAiContextHook, context)?;
        Ok(match self.edit_recovery_projection {
            EditRecoveryProjectionMode::None => context,
            EditRecoveryProjectionMode::CanonicalV1 => project_invalid_edit_recovery(context),
        })
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        HookSet::convert_to_llm(&OpenAiContextHook, context)
    }

    fn should_stop_after_turn(&self, context: &ContextEnvelope) -> Result<bool, HookError> {
        HookSet::should_stop_after_turn(&OpenAiContextHook, context)
    }

    fn prepare_next_turn(
        &self,
        context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        HookSet::prepare_next_turn(&OpenAiContextHook, context)
    }
}

fn project_invalid_edit_recovery(mut context: ContextEnvelope) -> ContextEnvelope {
    let trailing_start = context
        .messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::Assistant { .. }))
        .map_or(0, |index| index.saturating_add(1));
    // A tool batch can contain several rejected edit calls. Keep raw evidence
    // for each one, but attach the correction once to the latest matching
    // result so the next provider request has one unambiguous retry target.
    for message in context.messages[trailing_start..].iter_mut().rev() {
        let AgentMessage::ToolResult {
            tool_name,
            content,
            is_error,
            ..
        } = message
        else {
            continue;
        };
        if *is_error
            && tool_name == "edit"
            && is_recoverable_malformed_edit_error(content)
        {
            if !content.contains(EDIT_RECOVERY_PROJECTION_HINT) {
                content.push_str("\n\n");
                content.push_str(EDIT_RECOVERY_PROJECTION_HINT);
            }
            break;
        }
    }
    context
}

fn is_recoverable_malformed_edit_error(content: &str) -> bool {
    [
        "Validation failed for tool \"edit\":",
        "files: must have required properties files",
        "path: must not have additional properties",
        "edits: must not have additional properties",
    ]
    .into_iter()
    .all(|marker| content.contains(marker))
}

fn pre_edit_tool_gate_violation(
    gate: &PreEditToolGate,
    call: &ToolCall,
    context: &ContextEnvelope,
) -> Option<&'static str> {
    let target_edit_succeeded = match gate.mode {
        PreEditToolGateMode::SourceLocalV1 => successful_target_local_edit_result(context, gate),
        PreEditToolGateMode::None | PreEditToolGateMode::DirectEditV1 => successful_edit_result(context),
    };
    if target_edit_succeeded {
        return None;
    }
    match gate.mode {
        PreEditToolGateMode::None => None,
        PreEditToolGateMode::DirectEditV1 if matches!(call.name.as_str(), "bash" | "find") => {
            Some(PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        }
        PreEditToolGateMode::DirectEditV1 => None,
        PreEditToolGateMode::SourceLocalV1 if matches!(call.name.as_str(), "bash" | "find") => {
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        }
        PreEditToolGateMode::SourceLocalV1 if call.name == "read" && !call_is_target_read(call, gate) => {
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        }
        PreEditToolGateMode::SourceLocalV1 if call.name == "edit" && !call_is_target_edit(call, gate) => {
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        }
        PreEditToolGateMode::SourceLocalV1 => None,
    }
}

fn successful_edit_result(context: &ContextEnvelope) -> bool {
    context.messages.iter().any(|message| {
        matches!(
            message,
            AgentMessage::ToolResult {
                tool_name,
                is_error,
                ..
            } if tool_name == "edit" && !*is_error
        )
    })
}

/// A source-local unlock requires both durable halves of the same permitted
/// edit: an assistant call whose exact parsed paths are declared targets and a
/// later successful result carrying that call's ID. This avoids inferring
/// target locality from untrusted result text or a different edit in history.
fn successful_target_local_edit_result(context: &ContextEnvelope, gate: &PreEditToolGate) -> bool {
    let mut admitted_target_edit_ids = BTreeSet::new();
    for message in &context.messages {
        match message {
            AgentMessage::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    let admitted = ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    };
                    if admitted.name == "edit" && call_is_target_edit(&admitted, gate) {
                        admitted_target_edit_ids.insert(admitted.id);
                    }
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
                ..
            } if tool_name == "edit" && !*is_error && admitted_target_edit_ids.contains(tool_call_id) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn call_is_target_read(call: &ToolCall, gate: &PreEditToolGate) -> bool {
    JsonValue::parse(call.arguments.as_str())
        .ok()
        .and_then(|arguments| arguments.get("path").and_then(JsonValue::as_str).map(str::to_owned))
        .is_some_and(|path| gate.source_local_target_set().contains(path.as_str()))
}

fn call_is_target_edit(call: &ToolCall, gate: &PreEditToolGate) -> bool {
    let Ok(arguments) = JsonValue::parse(call.arguments.as_str()) else {
        return false;
    };
    let Some(files) = arguments.get("files").and_then(JsonValue::as_array) else {
        return false;
    };
    !files.is_empty()
        && files.iter().all(|file| {
            file.get("path")
                .and_then(JsonValue::as_str)
                .is_some_and(|path| gate.source_local_target_set().contains(path))
        })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum HarnessMode {
    Static,
    Jit,
}

impl HarnessMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "static" => Ok(Self::Static),
            "jit" => Ok(Self::Jit),
            _ => Err("--harness-mode must be static or jit".into()),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Jit => "jit",
        }
    }
    fn extension_mode(self) -> SelfExtensionMode {
        match self {
            Self::Static => SelfExtensionMode::Off,
            Self::Jit => SelfExtensionMode::Adaptive,
        }
    }
}

/// The explicit static-only profile for model-visible builtin prompt sections.
///
/// This leaves the resolved tool definitions and capabilities unchanged. A
/// non-default profile is evidence-bearing so a screen cannot silently reuse
/// the contradictory generic Bash guidance that prompted history probes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticPromptProfile {
    BuiltinV1,
    NoHistoryV1,
    PrefixGuardV1,
    PrefixGuardFocusedV1,
}

impl StaticPromptProfile {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "builtin-v1" => Ok(Self::BuiltinV1),
            "no-history-v1" => Ok(Self::NoHistoryV1),
            "prefix-guard-v1" => Ok(Self::PrefixGuardV1),
            "prefix-guard-focused-v1" => Ok(Self::PrefixGuardFocusedV1),
            _ => Err(
                "--static-prompt-profile must be builtin-v1, no-history-v1, prefix-guard-v1, or prefix-guard-focused-v1".into(),
            ),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::BuiltinV1 => "builtin-v1",
            Self::NoHistoryV1 => "no-history-v1",
            Self::PrefixGuardV1 => "prefix-guard-v1",
            Self::PrefixGuardFocusedV1 => "prefix-guard-focused-v1",
        }
    }

    fn uses_no_history_bash_projection(self) -> bool {
        matches!(
            self,
            Self::NoHistoryV1 | Self::PrefixGuardV1 | Self::PrefixGuardFocusedV1
        )
    }

    fn additional_static_guidance(self) -> Option<&'static str> {
        match self {
            Self::PrefixGuardV1 => Some(STATIC_PREFIX_GUARD_DIAGNOSTIC_GUIDANCE),
            Self::PrefixGuardFocusedV1 => Some(STATIC_PREFIX_GUARD_FOCUSED_DIAGNOSTIC_GUIDANCE),
            Self::BuiltinV1 | Self::NoHistoryV1 => None,
        }
    }

    fn project_prompt_sections(
        self,
        extension_id: &str,
        mut sections: Vec<ExtensionPromptSection>,
    ) -> Result<Vec<ExtensionPromptSection>, String> {
        if !self.uses_no_history_bash_projection() || extension_id != "bash" {
            return Ok(sections);
        }
        let section = sections
            .iter_mut()
            .find(|section| section.id == "bash")
            .ok_or_else(|| "bash builtin must declare its bash prompt section".to_owned())?;
        if section
            .content
            .matches(STATIC_BASH_GIT_HISTORY_INVITATION)
            .count()
            != 1
        {
            return Err("bash builtin prompt no longer contains its expected Git-history invitation".into());
        }
        section.content = section.content.replacen(
            STATIC_BASH_GIT_HISTORY_INVITATION,
            STATIC_BASH_NO_HISTORY_GUIDANCE,
            1,
        );
        Ok(sections)
    }
}

struct Args {
    model: String,
    task_json: PathBuf,
    workspace: PathBuf,
    capabilities_json: PathBuf,
    result_json: PathBuf,
    evidence_dir: PathBuf,
    attempt_id: String,
    baseline_id: String,
    harness_mode: HarnessMode,
    static_prompt_profile: StaticPromptProfile,
    thinking: ThinkingLevel,
    max_output_tokens: Option<u64>,
    outer_timeout_seconds: u64,
    provider_routing: JsonValue,
    tool_child_sandbox: Option<ToolChildSandbox>,
    edit_recovery_projection: EditRecoveryProjectionMode,
    pre_edit_tool_gate: PreEditToolGateMode,
    post_edit_validation_gate: PostEditValidationGateMode,
    shell_environment: CommandEnvironment,
    shell_environment_json: JsonValue,
    attempt_path_replacements: Vec<(String, String)>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut values = BTreeMap::<String, String>::new();
        let mut shell = BTreeMap::<String, String>::new();
        let mut arguments = env::args().skip(1);
        while let Some(flag) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            if flag == "--shell-env" {
                let (name, value) = value
                    .split_once('=')
                    .filter(|(name, _)| !name.is_empty())
                    .ok_or_else(|| "--shell-env must be NAME=VALUE".to_owned())?;
                if shell.insert(name.into(), value.into()).is_some() {
                    return Err(format!("duplicate shell environment variable {name}"));
                }
            } else if values.insert(flag.clone(), value).is_some() {
                return Err(format!("duplicate argument {flag}"));
            }
        }
        let take = |name: &str| {
            values
                .get(name)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| format!("missing required argument {name}"))
        };
        let supported = [
            "--provider",
            "--model",
            "--task-json",
            "--workspace",
            "--capabilities-json",
            "--result-json",
            "--evidence-dir",
            "--attempt-id",
            "--baseline-id",
            "--harness-mode",
            "--static-prompt-profile",
            "--thinking-level",
            "--max-output-tokens",
            "--outer-timeout-seconds",
            "--provider-routing-json",
            "--tool-child-sandbox",
            "--edit-recovery-projection",
            "--pre-edit-tool-gate",
            "--post-edit-validation-gate",
        ];
        if values
            .keys()
            .any(|flag| !supported.contains(&flag.as_str()))
        {
            return Err("unsupported evaluation adapter argument".into());
        }
        if take("--provider")? != "openrouter" {
            return Err("tea shootout supports only openrouter".into());
        }
        let model = take("--model")?;
        if model != REQUIRED_MODEL {
            return Err(format!("tea shootout requires {REQUIRED_MODEL}"));
        }
        if take("--thinking-level")? != REQUIRED_THINKING {
            return Err(format!(
                "tea shootout requires thinking level {REQUIRED_THINKING}"
            ));
        }
        let harness_mode = HarnessMode::parse(&take("--harness-mode")?)?;
        let baseline_id = take("--baseline-id")?;
        if !matches!(
            (harness_mode, baseline_id.as_str()),
            (HarnessMode::Static, "tea-static") | (HarnessMode::Jit, "tea-jit")
        ) {
            return Err("baseline ID must agree with harness mode".into());
        }
        let static_prompt_profile = values
            .get("--static-prompt-profile")
            .map(String::as_str)
            .map(StaticPromptProfile::parse)
            .transpose()?
            .unwrap_or(StaticPromptProfile::BuiltinV1);
        if static_prompt_profile != StaticPromptProfile::BuiltinV1
            && harness_mode != HarnessMode::Static
        {
            return Err("non-default static prompt profile is available only to tea-static".into());
        }
        let tool_child_sandbox_mode = values
            .get("--tool-child-sandbox")
            .map(String::as_str)
            .map(ToolChildSandboxMode::parse)
            .transpose()?
            .unwrap_or(ToolChildSandboxMode::None);
        if tool_child_sandbox_mode != ToolChildSandboxMode::None
            && harness_mode != HarnessMode::Static
        {
            return Err("tool-child sandbox is available only to tea-static".into());
        }
        let edit_recovery_projection = values
            .get("--edit-recovery-projection")
            .map(String::as_str)
            .map(EditRecoveryProjectionMode::parse)
            .transpose()?
            .unwrap_or(EditRecoveryProjectionMode::None);
        if edit_recovery_projection != EditRecoveryProjectionMode::None
            && harness_mode != HarnessMode::Static
        {
            return Err("edit recovery projection is available only to tea-static".into());
        }
        let pre_edit_tool_gate = values
            .get("--pre-edit-tool-gate")
            .map(String::as_str)
            .map(PreEditToolGateMode::parse)
            .transpose()?
            .unwrap_or(PreEditToolGateMode::None);
        if pre_edit_tool_gate != PreEditToolGateMode::None
            && harness_mode != HarnessMode::Static
        {
            return Err("pre-edit tool gate is available only to tea-static".into());
        }
        let post_edit_validation_gate = values
            .get("--post-edit-validation-gate")
            .map(String::as_str)
            .map(PostEditValidationGateMode::parse)
            .transpose()?
            .unwrap_or(PostEditValidationGateMode::None);
        if post_edit_validation_gate != PostEditValidationGateMode::None
            && harness_mode != HarnessMode::Static
        {
            return Err("post-edit validation gate is available only to tea-static".into());
        }
        if post_edit_validation_gate == PostEditValidationGateMode::UnmaskedEvidenceV1
            && pre_edit_tool_gate != PreEditToolGateMode::SourceLocalV1
        {
            return Err("unmasked-evidence-v1 requires --pre-edit-tool-gate source-local-v1".into());
        }
        let maximum = take("--max-output-tokens")?;
        if maximum != "unlimited" {
            return Err("tea shootout requires unlimited max output tokens".into());
        }
        let max_output_tokens = None;
        let outer_timeout_seconds = take("--outer-timeout-seconds")?
            .parse::<u64>()
            .ok()
            .ok_or_else(|| "--outer-timeout-seconds must be a non-negative integer".to_owned())?;
        let provider_routing = JsonValue::parse(&take("--provider-routing-json")?)
            .map_err(|_| "--provider-routing-json must be JSON".to_owned())?;
        if !provider_routing.is_object() {
            return Err("--provider-routing-json must be an object".into());
        }
        let workspace = PathBuf::from(take("--workspace")?);
        let sandbox_attempt_paths = shell
            .iter()
            .filter_map(|(name, value)| match name.as_str() {
                "HOME" | "TMPDIR" | "npm_config_cache" | "NODE_PATH" => {
                    Some(PathBuf::from(value))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let tool_child_sandbox = match tool_child_sandbox_mode {
            ToolChildSandboxMode::None => None,
            mode @ (ToolChildSandboxMode::MacosSeatbeltV1 | ToolChildSandboxMode::MacosSeatbeltV2) => Some(
                ToolChildSandbox::macos_seatbelt(mode, &workspace, &sandbox_attempt_paths)?,
            ),
        };
        let attempt_path_replacements =
            std::iter::once((
                workspace.to_string_lossy().into_owned(),
                "{WORKSPACE}".to_owned(),
            ))
                .chain(
                    shell
                        .iter()
                        .filter_map(|(name, value)| match name.as_str() {
                            "HOME" => Some((value.clone(), "{HOME}".to_owned())),
                            "TMPDIR" => Some((value.clone(), "{TMPDIR}".to_owned())),
                            "npm_config_cache" => Some((value.clone(), "{NPM_CACHE}".to_owned())),
                            "NODE_PATH" => Some((value.clone(), "{NODE_PATH}".to_owned())),
                            _ => None,
                        }),
                )
                .collect::<Vec<_>>();
        let shell_environment = shell
            .iter()
            .fold(CommandEnvironment::empty(), |current, (name, value)| {
                current.with(name.clone(), value.clone())
            });
        let normalized_shell = shell
            .into_iter()
            .map(|(name, value)| {
                let normalized = match name.as_str() {
                    "HOME" => "{HOME}".into(),
                    "TMPDIR" => "{TMPDIR}".into(),
                    "npm_config_cache" => "{NPM_CACHE}".into(),
                    "NODE_PATH" => "{NODE_PATH}".into(),
                    _ => value,
                };
                (name, JsonValue::String(normalized))
            })
            .collect();
        Ok(Self {
            model,
            task_json: PathBuf::from(take("--task-json")?),
            workspace,
            capabilities_json: PathBuf::from(take("--capabilities-json")?),
            result_json: PathBuf::from(take("--result-json")?),
            evidence_dir: PathBuf::from(take("--evidence-dir")?),
            attempt_id: take("--attempt-id")?,
            baseline_id,
            harness_mode,
            static_prompt_profile,
            thinking: ThinkingLevel::High,
            max_output_tokens,
            outer_timeout_seconds,
            provider_routing,
            tool_child_sandbox,
            edit_recovery_projection,
            pre_edit_tool_gate,
            post_edit_validation_gate,
            shell_environment,
            shell_environment_json: JsonValue::Object(normalized_shell),
            attempt_path_replacements,
        })
    }
}

fn read_json(path: &Path, label: &str) -> Result<JsonValue, String> {
    let source = fs::read_to_string(path).map_err(|_| format!("cannot read evaluation {label}"))?;
    JsonValue::parse(&source).map_err(|_| format!("evaluation {label} is not JSON"))
}

/// Validate the task-owned source-local declaration before the model sees the
/// prompt. The runner separately proves the checkout is clean immediately
/// before launch; this adapter confirms its locally supplied task and regular
/// workspace paths still agree with that witness.
fn source_local_task_targets(
    task: &JsonValue,
    prompt: &str,
    workspace: &Path,
) -> Result<Vec<String>, String> {
    let metadata = task
        .get("source_local_v1")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "source-local-v1 requires versioned task metadata".to_owned())?;
    if metadata
        .get("schema_version")
        .and_then(JsonValue::as_str)
        != Some("tea-coding-eval-source-local/v1")
    {
        return Err("source-local-v1 task metadata schema is unsupported".into());
    }
    let targets = metadata
        .get("targets")
        .and_then(JsonValue::as_array)
        .filter(|targets| !targets.is_empty())
        .ok_or_else(|| "source-local-v1 task targets are invalid".to_owned())?;
    let mut unique = BTreeSet::new();
    let mut parsed = Vec::with_capacity(targets.len());
    for target in targets {
        let target = target
            .as_str()
            .filter(|target| safe_source_local_target(target))
            .ok_or_else(|| "source-local-v1 task targets are invalid".to_owned())?;
        if !unique.insert(target) {
            return Err("source-local-v1 task targets must be unique".into());
        }
        if !prompt.contains(target) {
            return Err("source-local-v1 task target must occur literally in the prompt".into());
        }
        let entry = fs::symlink_metadata(workspace.join(target))
            .map_err(|_| format!("source-local-v1 target is not a regular workspace file: {target}"))?;
        if entry.file_type().is_symlink() || !entry.file_type().is_file() {
            return Err(format!("source-local-v1 target is not a regular workspace file: {target}"));
        }
        parsed.push(target.to_owned());
    }
    Ok(parsed)
}

fn safe_source_local_target(target: &str) -> bool {
    !target.is_empty()
        && !target.contains('\0')
        && !target.starts_with('/')
        && !target.contains('\\')
        && target
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn request_timeout_seconds(outer_timeout_seconds: u64) -> u64 {
    if outer_timeout_seconds == 0 {
        DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS
    } else {
        outer_timeout_seconds
    }
}

/// Drive one durable operation to settlement, cancelling it at the evaluator's
/// finite wall-clock boundary. Provider request timeouts are intentionally
/// per-request; a coding run can span multiple requests, so this boundary must
/// cover the whole durable operation instead. When the deadline wins, the
/// caller's cancellation action runs before the same drive future is awaited,
/// preserving the normal durable terminal record and evaluator epilogue.
async fn settle_with_outer_deadline<T>(
    deadline: Option<Duration>,
    drive: impl Future<Output = T>,
    on_deadline: impl FnOnce(),
) -> (T, bool) {
    let Some(deadline) = deadline else {
        return (drive.await, false);
    };
    let mut drive = Box::pin(drive);
    let mut timer = Box::pin(smol::Timer::after(deadline));
    let mut completed = None;
    let deadline_fired = std::future::poll_fn(|context| {
        if let Poll::Ready(outcome) = drive.as_mut().poll(context) {
            completed = Some(outcome);
            return Poll::Ready(false);
        }
        if timer.as_mut().poll(context).is_ready() {
            Poll::Ready(true)
        } else {
            Poll::Pending
        }
    })
    .await;
    if deadline_fired {
        on_deadline();
        (drive.await, true)
    } else {
        (
            completed.expect("completed operation remains available after deadline race"),
            false,
        )
    }
}

fn thinking_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn sha256(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut message = bytes.to_vec();
    let bit_length = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());
    let mut state = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in message.as_chunks::<64>().0 {
        let mut words = [0_u32; 64];
        for (index, word) in words[..16].iter_mut().enumerate() {
            *word = u32::from_be_bytes(
                block[index * 4..index * 4 + 4]
                    .try_into()
                    .expect("SHA block"),
            );
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (target, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *target = target.wrapping_add(value);
        }
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

fn model_profile(model: &ModelDescriptor) -> Result<ModelHarnessProfile, String> {
    ModelHarnessProfile::new(
        model.provider.clone(),
        model.model.clone(),
        model.revision.clone(),
        "tea-pi-shootout-v0",
        "tea-core-canonical-v1",
        "tea-provider-summary-v1",
        "tea-recoverable-projection-v1",
    )
    .map_err(|error| error.to_string())
}

fn host_profile_digest(configuration: &AgentConfiguration) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-pi-shootout-host-profile", 1, 1);
    writer.string("system_prompt", &configuration.system_prompt);
    // A context hook can change the next provider request without changing a
    // prompt-visible tool definition. Bind that executable policy to the
    // durable host profile so diagnostic recovery runs remain distinguishable.
    writer.string("hook_identity", &configuration.hooks.identity().to_hex());
    let definitions = configuration.tools.definitions();
    writer.u64("tool_count", definitions.len() as u64);
    for tool in definitions {
        writer.string("tool_name", &tool.name);
        writer.string("tool_description", &tool.description);
        writer.string(
            "tool_schema",
            &tool
                .schema
                .to_json_string()
                .expect("registered schema encodes"),
        );
        writer.string(
            "tool_execution_mode",
            match tool.execution_mode {
                ToolExecutionMode::Sequential => "sequential",
                ToolExecutionMode::Parallel => "parallel",
            },
        );
    }
    writer.finish()
}

fn snapshot_spec(
    configuration: &AgentConfiguration,
    profile: &ModelHarnessProfile,
    mode: HarnessMode,
    identities: RuntimePolicyIdentities,
) -> HarnessSnapshotSpec {
    let tools = configuration
        .tools
        .definitions()
        .into_iter()
        .map(|tool| ToolPresentationDescriptor {
            name: tool.name,
            description: tool.description,
            schema: tool.schema,
            execution_mode: match tool.execution_mode {
                ToolExecutionMode::Sequential => "sequential".into(),
                ToolExecutionMode::Parallel => "parallel".into(),
            },
            requires_exclusive_batch: tool.requires_exclusive_batch,
            cancellation_settlement_mode: match tool.cancellation_settlement_mode {
                tea_core::tool::CancellationSettlementMode::DropFuture => "drop_future".into(),
                tea_core::tool::CancellationSettlementMode::AwaitFuture => "await_future".into(),
            },
        })
        .collect();
    HarnessSnapshotSpec {
        base_profile_digest: host_profile_digest(configuration),
        base_system_prompt: configuration.system_prompt.clone(),
        model_harness_profile: profile.profile_id.clone(),
        self_extension_addendum: (mode == HarnessMode::Jit).then(|| JIT_ADDENDUM.into()),
        ordered_global_plugins: Vec::new(),
        ordered_session_plugins: Vec::new(),
        prompt_sections: Vec::new(),
        plugin_prompt_sections: Vec::new(),
        tool_presentations: tools,
        plugin_tool_presentations: Vec::new(),
        // Snapshot policy identities must come from the exact RuntimeServices
        // instance that will resolve and execute this snapshot. Keeping these
        // values coupled prevents the resolver's executable-policy guard from
        // rejecting an otherwise valid evaluation before the provider runs.
        hook_bundle_digest: identities.hook_bundle_digest,
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits {
            source_bytes: 16 * 1024,
            ..HarnessResourceLimits::default()
        },
        compaction_policy_digest: identities.compaction_policy_digest,
        tool_projection_digest: identities.tool_projection_digest,
        failure_policy_digest: identities.failure_policy_digest,
    }
}

fn event_name(event: &TeaEvent) -> &'static str {
    match event {
        TeaEvent::Agent { event, .. } => match &event.kind {
            AgentEventKind::ProviderRequestObserved { .. } => "provider_request_observed",
            AgentEventKind::PromptLayoutObserved { .. } => "prompt_layout_observed",
            AgentEventKind::CompactionLifecycle { .. } => "compaction_lifecycle",
            AgentEventKind::AgentStart => "agent_start",
            AgentEventKind::TurnStart { .. } => "turn_start",
            AgentEventKind::TurnEnd { .. } => "turn_end",
            AgentEventKind::MessageStart { .. } => "message_start",
            AgentEventKind::MessageUpdate { .. } => "message_update",
            AgentEventKind::MessageEnd { .. } => "message_end",
            AgentEventKind::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEventKind::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEventKind::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEventKind::ToolFailureObserved { .. } => "tool_failure_observed",
            AgentEventKind::CompactionStart { .. } => "compaction_start",
            AgentEventKind::CompactionResult { .. } => "compaction_result",
            AgentEventKind::CompactionEnd { .. } => "compaction_end",
            AgentEventKind::AutomaticCompactionStart { .. } => "automatic_compaction_start",
            AgentEventKind::AutomaticCompactionEnd { .. } => "automatic_compaction_end",
            AgentEventKind::ContextEstimate { .. } => "context_estimate",
            AgentEventKind::ProviderRequestSkipped { .. } => "provider_request_skipped",
            AgentEventKind::ModelTurnUsage { .. } => "model_turn_usage",
            AgentEventKind::AgentEnd { .. } => "agent_end",
        },
        TeaEvent::Session(_) => "session_event",
        TeaEvent::Harness(_) => "harness_event",
        TeaEvent::Artifact(_) => "artifact_event",
    }
}

fn optional_u64(value: Option<u64>) -> JsonValue {
    value.map(JsonValue::from).unwrap_or(JsonValue::Null)
}

fn strings(values: impl IntoIterator<Item = String>) -> JsonValue {
    JsonValue::Array(values.into_iter().map(JsonValue::String).collect())
}

fn normalize_attempt_text(value: &str, replacements: &[(String, String)]) -> String {
    let mut normalized = value.to_owned();
    let mut ordered = replacements.to_vec();
    ordered.sort_by_key(|(source, _)| std::cmp::Reverse(source.len()));
    for (source, target) in ordered {
        if !source.is_empty() {
            normalized = normalized.replace(&source, &target);
        }
    }
    normalized
}

fn sensitive_wire_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "max_tokens" | "max_completion_tokens") {
        // These are numeric model controls, not credentials. Keep them in the
        // retained wire witness so parity checks can see the actual ceiling.
        return false;
    }
    if lower.split(['_', '-']).any(|part| part == "token") {
        return true;
    }
    [
        "authorization",
        "api_key",
        "apikey",
        "credential",
        "secret",
        "password",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Create the private persisted representation from the exact bytes handed to
/// the OpenRouter HTTP client. This copy never includes headers, and redacts
/// credential-shaped fields defensively before it reaches disk.
fn sanitize_wire_value(value: &JsonValue, replacements: &[(String, String)]) -> JsonValue {
    match value {
        JsonValue::String(text) => JsonValue::String(normalize_attempt_text(text, replacements)),
        JsonValue::Array(values) => JsonValue::Array(
            values
                .iter()
                .map(|entry| sanitize_wire_value(entry, replacements))
                .collect(),
        ),
        JsonValue::Object(values) => JsonValue::Object(
            values
                .iter()
                .map(|(name, entry)| {
                    (
                        name.clone(),
                        if sensitive_wire_field(name) {
                            JsonValue::String("[redacted]".into())
                        } else {
                            sanitize_wire_value(entry, replacements)
                        },
                    )
                })
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

fn wire_field(payload: &JsonValue, name: &str) -> JsonValue {
    JsonValue::object([
        ("present", JsonValue::Bool(payload.get(name).is_some())),
        (
            "value",
            payload.get(name).cloned().unwrap_or(JsonValue::Null),
        ),
    ])
}

fn wire_request_record(
    payload: JsonValue,
    ordinal: usize,
    replacements: &[(String, String)],
) -> Result<JsonValue, String> {
    let payload = sanitize_wire_value(&payload, replacements);
    let object = payload
        .as_object()
        .ok_or_else(|| "OpenRouter payload must be an object".to_owned())?;
    let messages = object
        .get("messages")
        .and_then(JsonValue::as_array)
        .unwrap_or(&[]);
    let roles = messages
        .iter()
        .map(|message| {
            message
                .get("role")
                .and_then(JsonValue::as_str)
                .unwrap_or("<missing>")
                .to_owned()
        })
        .collect::<Vec<_>>();
    let assistant_messages = messages
        .iter()
        .filter(|message| message.get("role").and_then(JsonValue::as_str) == Some("assistant"))
        .collect::<Vec<_>>();
    let message_records = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let content = message.get("content").cloned().unwrap_or(JsonValue::Null);
            let structural = message.to_json_string().expect("sanitized message encodes");
            let content = content.to_json_string().expect("sanitized content encodes");
            JsonValue::object([
                ("ordinal", JsonValue::from((index + 1) as u64)),
                ("role", JsonValue::String(roles[index].clone())),
                (
                    "structural_sha256",
                    JsonValue::String(sha256(structural.as_bytes())),
                ),
                (
                    "content_sha256",
                    JsonValue::String(sha256(content.as_bytes())),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let system = JsonValue::Array(
        messages
            .iter()
            .filter(|message| {
                matches!(
                    message.get("role").and_then(JsonValue::as_str),
                    Some("system") | Some("developer")
                )
            })
            .cloned()
            .collect(),
    );
    let system_prompt_sha256 = if system.as_array().is_some_and(|items| items.is_empty()) {
        JsonValue::Null
    } else {
        JsonValue::String(sha256(
            system
                .to_json_string()
                .expect("system messages encode")
                .as_bytes(),
        ))
    };
    let tools = object
        .get("tools")
        .and_then(JsonValue::as_array)
        .unwrap_or(&[]);
    let tool_names = tools
        .iter()
        .map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .or_else(|| tool.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or("<unnamed>")
                .to_owned()
        })
        .collect::<Vec<_>>();
    let tool_json = JsonValue::Array(tools.to_vec())
        .to_json_string()
        .expect("tools encode");
    let known = [
        "model",
        "messages",
        "tools",
        "reasoning",
        "reasoning_effort",
        "temperature",
        "seed",
        "max_tokens",
        "max_completion_tokens",
        "tool_choice",
        "parallel_tool_calls",
        "stream",
        "stream_options",
        "provider",
    ];
    let other = JsonValue::Object(
        object
            .iter()
            .filter(|(name, _)| !known.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    );
    let canonical_payload = payload
        .to_json_string()
        .map_err(|error| error.to_string())?;
    Ok(JsonValue::object([
        ("ordinal", JsonValue::from(ordinal as u64)),
        (
            "canonical_request_sha256",
            JsonValue::String(sha256(canonical_payload.as_bytes())),
        ),
        (
            "model",
            object.get("model").cloned().unwrap_or(JsonValue::Null),
        ),
        ("message_count", JsonValue::from(messages.len() as u64)),
        ("message_roles", strings(roles)),
        ("messages", JsonValue::Array(message_records)),
        ("system_prompt_sha256", system_prompt_sha256),
        (
            "assistant_reasoning_content",
            if assistant_messages.is_empty() {
                JsonValue::Null
            } else {
                JsonValue::Bool(
                    assistant_messages
                        .iter()
                        .all(|message| message.get("reasoning_content").is_some()),
                )
            },
        ),
        ("tool_count", JsonValue::from(tool_names.len() as u64)),
        ("tool_names", strings(tool_names)),
        (
            "tool_schema_sha256",
            JsonValue::String(sha256(tool_json.as_bytes())),
        ),
        (
            "reasoning",
            object
                .get("reasoning")
                .cloned()
                .or_else(|| object.get("reasoning_effort").cloned())
                .unwrap_or(JsonValue::Null),
        ),
        ("temperature", wire_field(&payload, "temperature")),
        ("seed", wire_field(&payload, "seed")),
        ("max_tokens", wire_field(&payload, "max_tokens")),
        (
            "max_completion_tokens",
            wire_field(&payload, "max_completion_tokens"),
        ),
        ("tool_choice", wire_field(&payload, "tool_choice")),
        (
            "parallel_tool_calls",
            wire_field(&payload, "parallel_tool_calls"),
        ),
        ("stream", wire_field(&payload, "stream")),
        ("stream_options", wire_field(&payload, "stream_options")),
        (
            "provider_routing",
            object.get("provider").cloned().unwrap_or(JsonValue::Null),
        ),
        ("other_model_affecting_top_level_fields", other),
        ("canonical_payload", payload),
    ]))
}

fn returned_route_evidence(capture: &OpenRouterRequestCapture) -> JsonValue {
    let route = capture.returned_route();
    let provenance = route
        .is_observed()
        .then(|| JsonValue::from("OpenRouter response header"))
        .unwrap_or(JsonValue::Null);
    JsonValue::object([
        ("model", route.model.map(JsonValue::String).unwrap_or(JsonValue::Null)),
        (
            "provider",
            route.provider.map(JsonValue::String).unwrap_or(JsonValue::Null),
        ),
        ("provenance", provenance),
    ])
}

fn write_wire_evidence(
    args: &Args,
    capture: &OpenRouterRequestCapture,
) -> Result<JsonValue, String> {
    let requests = capture
        .payloads()
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            let source = std::str::from_utf8(&bytes)
                .map_err(|_| "OpenRouter serialized a non-UTF-8 JSON payload".to_owned())?;
            let payload = JsonValue::parse(source)
                .map_err(|_| "OpenRouter serialized invalid JSON payload".to_owned())?;
            wire_request_record(payload, index + 1, &args.attempt_path_replacements)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let returned_route = returned_route_evidence(capture);
    let private = JsonValue::object([
        (
            "schema_version",
            JsonValue::from("tea-pi-wire-request-evidence/v1"),
        ),
        ("requests", JsonValue::Array(requests.clone())),
        ("returned_route", returned_route.clone()),
    ]);
    fs::create_dir_all(&args.evidence_dir).map_err(|error| error.to_string())?;
    fs::write(
        args.evidence_dir.join("wire-requests.json"),
        format!(
            "{}\n",
            private
                .to_json_string_pretty()
                .map_err(|error| error.to_string())?
        ),
    )
    .map_err(|error| error.to_string())?;
    let summaries: Vec<JsonValue> = requests
        .into_iter()
        .map(|request| {
            let mut object = request.as_object().expect("wire record is object").clone();
            object.remove("canonical_payload");
            JsonValue::Object(object)
        })
        .collect();
    Ok(JsonValue::object([
        (
            "source",
            JsonValue::from("direct-final-openrouter-boundary"),
        ),
        ("request_count", JsonValue::from(summaries.len() as u64)),
        ("requests", JsonValue::Array(summaries)),
        ("routing_policy", args.provider_routing.clone()),
        ("returned_route", returned_route),
    ]))
}

/// Export curated durable lineage next to the session log without copying
/// provider payloads, secrets, or unbounded model/tool transcripts.
fn write_jit_evidence(
    args: &Args,
    initial_snapshot_id: &str,
    final_snapshot_id: &str,
    candidate: Option<&tea_core::harness::HarnessCandidateV1>,
    activated: bool,
    changed_surfaces: &[String],
) -> Result<(), String> {
    if args.harness_mode != HarnessMode::Jit {
        return Ok(());
    }
    let root = args
        .evidence_dir
        .parent()
        .ok_or_else(|| "evidence directory needs a parent".to_owned())?
        .join("harness");
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    fs::write(
        root.join("lineage.json"),
        JsonValue::object([
            (
                "initial_snapshot_id",
                JsonValue::from(initial_snapshot_id.to_owned()),
            ),
            (
                "final_snapshot_id",
                JsonValue::from(final_snapshot_id.to_owned()),
            ),
            ("activated", JsonValue::Bool(activated)),
            (
                "changed_surfaces",
                strings(changed_surfaces.iter().cloned()),
            ),
        ])
        .to_json_string()
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if let Some(candidate) = candidate {
        let draft = &candidate.draft;
        let candidate_json = JsonValue::object([
            (
                "candidate_id",
                JsonValue::from(candidate.candidate_id.to_string()),
            ),
            (
                "parent_revision_id",
                JsonValue::from(draft.parent_revision_id.to_string()),
            ),
            (
                "proposed_snapshot_id",
                JsonValue::from(draft.proposed_snapshot_id.to_string()),
            ),
            (
                "changed_paths",
                strings(draft.changed_paths.iter().map(ToString::to_string)),
            ),
            (
                "changed_surfaces",
                strings(
                    draft
                        .changed_surfaces
                        .iter()
                        .map(|surface| format!("{surface:?}").to_ascii_lowercase()),
                ),
            ),
            (
                "targeted_failures",
                strings(draft.targeted_failures.clone()),
            ),
            ("evidence", strings(draft.evidence.clone())),
            ("expected_effects", strings(draft.expected_effects.clone())),
            ("regression_risks", strings(draft.regression_risks.clone())),
            (
                "capability_ceiling",
                strings(draft.capability_ceiling.iter().cloned().collect::<Vec<_>>()),
            ),
        ])
        .to_json_string()
        .map_err(|error| error.to_string())?;
        let validation_json = JsonValue::object([
            ("accepted", JsonValue::Bool(candidate.validation.accepted)),
            ("is_noop", JsonValue::Bool(candidate.validation.is_noop)),
            (
                "diagnostics",
                strings(candidate.validation.diagnostics.clone()),
            ),
        ])
        .to_json_string()
        .map_err(|error| error.to_string())?;
        fs::write(root.join("candidate.json"), candidate_json)
            .map_err(|error| error.to_string())?;
        fs::write(root.join("validation.json"), validation_json)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct ResultJsonInput<'a> {
    args: &'a Args,
    pre_edit_tool_gate: &'a PreEditToolGate,
    post_edit_validation_gate: &'a PostEditValidationGate,
    validation_evidence: &'a ValidationEvidence,
    provider: &'a OpenRouterProvider,
    surface: &'a CodingSurface,
    snapshot: &'a tea_core::harness::HarnessSnapshotV1,
    session_snapshot: &'a SessionSnapshot,
    final_snapshot_id: String,
    terminal: (&'a str, Option<&'a str>),
    agent_ms: u64,
    final_text: String,
    trace: Vec<JsonValue>,
    wire_evidence: JsonValue,
    candidate_count: u64,
    candidate_id: Option<String>,
    changed_surfaces: Vec<String>,
    hypothesis: Option<String>,
    activated: bool,
}

struct EventCollection {
    trace: Vec<JsonValue>,
    candidate_count: u64,
    candidate_id: Option<String>,
    changed_surfaces: Vec<String>,
    activated: bool,
    final_snapshot_id: String,
}

impl EventCollection {
    fn new(initial_snapshot_id: String) -> Self {
        Self {
            trace: Vec::new(),
            candidate_count: 0,
            candidate_id: None,
            changed_surfaces: Vec::new(),
            activated: false,
            final_snapshot_id: initial_snapshot_id,
        }
    }

    fn observe(&mut self, event: TeaEvent) {
        let name = event_name(&event);
        match &event {
            TeaEvent::Harness(HarnessEvent::CandidateStaged {
                candidate_id: staged,
                ..
            }) => {
                self.candidate_count = self.candidate_count.saturating_add(1);
                self.candidate_id = Some(staged.to_string());
            }
            TeaEvent::Harness(HarnessEvent::SnapshotActivated {
                snapshot_id: activated_snapshot,
                changed_surfaces: surfaces,
                ..
            }) => {
                self.activated = true;
                self.final_snapshot_id = activated_snapshot.to_string();
                self.changed_surfaces = surfaces
                    .iter()
                    .map(|surface| format!("{surface:?}").to_ascii_lowercase())
                    .collect();
            }
            _ => {}
        }
        self.trace.push(JsonValue::object([
            ("seq", JsonValue::from(self.trace.len() as u64 + 1)),
            ("type", JsonValue::from(name)),
        ]));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionCounts {
    /// Pi-compatible logical turns: durable user messages.
    user_messages: u64,
    /// Core model-loop turns, including turns that may not materialize an assistant entry.
    model_turns: u64,
    /// Physical provider request intents written before dispatch.
    provider_requests: u64,
    /// Provider-emitted assistant tool-call blocks.
    tool_calls: u64,
    /// Durable step attempts carrying an explicit retry reason.
    retries: u64,
    /// Pi-compatible completed compaction lifecycle events.
    compactions: u64,
}

fn session_counts(snapshot: &SessionSnapshot) -> SessionCounts {
    let user_messages = snapshot
        .entries()
        .iter()
        .filter(|entry| matches!(&entry.body, SessionEntry::UserMessage(_)))
        .count() as u64;
    let tool_calls = snapshot
        .entries()
        .iter()
        .filter_map(|entry| match &entry.body {
            SessionEntry::AssistantMessage(message) => Some(message.tool_calls.len() as u64),
            _ => None,
        })
        .sum();
    let mut model_turns = 0_u64;
    let mut provider_requests = 0_u64;
    let mut retries = 0_u64;
    for stored in snapshot.records() {
        match &stored.record {
            LaneRecord::StepAttempted(StepAttemptedRecord {
                kind: StepKind::Assistant,
                reason,
                ..
            }) => {
                model_turns = model_turns.saturating_add(1);
                retries = retries.saturating_add(u64::from(reason.is_some()));
            }
            LaneRecord::StepAttempted(StepAttemptedRecord { reason, .. }) => {
                retries = retries.saturating_add(u64::from(reason.is_some()));
            }
            LaneRecord::ProviderRequestStarted(_) => {
                provider_requests = provider_requests.saturating_add(1);
            }
            _ => {}
        }
    }
    SessionCounts {
        user_messages,
        model_turns,
        provider_requests,
        tool_calls,
        retries,
        compactions: 0,
    }
}

fn uncached_input_tokens(
    prompt_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
) -> Option<u64> {
    prompt_tokens.map(|prompt| {
        prompt
            .saturating_sub(cache_read_tokens.unwrap_or(0))
            .saturating_sub(cache_write_tokens.unwrap_or(0))
    })
}

/// OpenRouter's `prompt_tokens` already includes cache reads and writes.
/// Preserve that provider-reported total instead of reconstructing it from a
/// saturated uncached-input subtraction. The latter is lossy when a provider
/// reports cache components that exceed its prompt total.
fn prompt_total_tokens(prompt_tokens: Option<u64>) -> Option<u64> {
    prompt_tokens
}

/// An explicit direct-edit workflow policy.
///
/// `direct-edit-v1` makes the pre-edit state explicit without mutating model
/// arguments: the named target can be read and edited, while exploratory
/// `bash`/`find` calls are returned as ordinary blocked tool results. A
/// successful durable `edit` result reopens those tools for validation. Calls
/// batched with that edit remain blocked because only a *prior* successful
/// result changes the durable context-derived state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreEditToolGateMode {
    None,
    DirectEditV1,
    SourceLocalV1,
}

impl PreEditToolGateMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "direct-edit-v1" => Ok(Self::DirectEditV1),
            "source-local-v1" => Ok(Self::SourceLocalV1),
            _ => Err("--pre-edit-tool-gate must be none, direct-edit-v1, or source-local-v1".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DirectEditV1 => "direct-edit-v1",
            Self::SourceLocalV1 => "source-local-v1",
        }
    }

    fn block_reason_sha256(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::DirectEditV1 => Some(sha256(PRE_EDIT_TOOL_GATE_BLOCK_REASON.as_bytes())),
            Self::SourceLocalV1 => Some(sha256(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON.as_bytes())),
        }
    }

    fn blocked_tools(self) -> &'static [&'static str] {
        match self {
            Self::None => &[],
            Self::DirectEditV1 => &["bash", "find"],
            Self::SourceLocalV1 => &["bash", "find"],
        }
    }

    fn target_restricted_tools(self) -> &'static [&'static str] {
        match self {
            Self::None | Self::DirectEditV1 => &[],
            Self::SourceLocalV1 => &["read", "edit"],
        }
    }

    fn unlocks_after(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::DirectEditV1 => Some("prior-successful-edit-result"),
            Self::SourceLocalV1 => Some("prior-successful-target-local-edit-result"),
        }
    }

    fn same_batch_rule(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::DirectEditV1 => Some("block-until-prior-successful-edit-result"),
            Self::SourceLocalV1 => Some("block-until-prior-successful-target-local-edit-result"),
        }
    }
}

/// The explicit model-context recovery mode available only to Tea-only diagnostics.
///
/// This changes only the immediate next model context after one precisely
/// identified rejected edit. It never accepts or normalizes model arguments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditRecoveryProjectionMode {
    None,
    CanonicalV1,
}

impl EditRecoveryProjectionMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "canonical-v1" => Ok(Self::CanonicalV1),
            _ => Err("--edit-recovery-projection must be none or canonical-v1".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CanonicalV1 => "canonical-v1",
        }
    }

    fn context_hook(
        self,
        pre_edit_tool_gate: PreEditToolGate,
        post_edit_validation_gate: PostEditValidationGate,
        direct_exit_status_witness: DirectExitStatusWitness,
        exit_status_observations: ExitStatusObservations,
    ) -> Arc<dyn HookSet> {
        match (self, pre_edit_tool_gate.mode, post_edit_validation_gate.mode) {
            (Self::None, PreEditToolGateMode::None, PostEditValidationGateMode::None) => Arc::new(OpenAiContextHook),
            (Self::CanonicalV1, PreEditToolGateMode::None, PostEditValidationGateMode::None) => Arc::new(EditRecoveryProjectionHook),
            (edit_recovery_projection, _, _) => {
                Arc::new(PreEditToolGateHook {
                    edit_recovery_projection,
                    gate: pre_edit_tool_gate,
                    post_edit_validation_gate,
                    direct_exit_status_witness,
                    exit_status_observations,
                })
            }
        }
    }

    fn correction_sha256(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::CanonicalV1 => Some(sha256(EDIT_RECOVERY_PROJECTION_HINT.as_bytes())),
        }
    }
}

/// The explicit shell-isolation mode available only to Tea-only diagnostics.
///
/// The paired shootout must not select this Tea-only policy: it would narrow
/// Tea's shell authority without an identical Pi policy. This mode exists to
/// keep exploratory parallel screens from granting model-issued shell commands
/// ambient host filesystem or network authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolChildSandboxMode {
    None,
    MacosSeatbeltV1,
    /// V2 preserves V1's filesystem/network boundary and also blocks
    /// model-issued shell reads and writes of workspace repository data.
    MacosSeatbeltV2,
}

impl ToolChildSandboxMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "macos-seatbelt-v1" => Ok(Self::MacosSeatbeltV1),
            "macos-seatbelt-v2" => Ok(Self::MacosSeatbeltV2),
            _ => Err(
                "--tool-child-sandbox must be none, macos-seatbelt-v1, or macos-seatbelt-v2"
                    .into(),
            ),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MacosSeatbeltV1 => "macos-seatbelt-v1",
            Self::MacosSeatbeltV2 => "macos-seatbelt-v2",
        }
    }
}

/// The concrete, invocation-local profile used for one shell child.
///
/// Seatbelt applies to the inner shell and all of its descendants. The
/// provider adapter remains outside it so OpenRouter transport retains its
/// independent credential/network boundary.
#[derive(Clone, Debug)]
struct ToolChildSandbox {
    mode: ToolChildSandboxMode,
    profile: String,
    profile_sha256: String,
}

impl ToolChildSandbox {
    fn macos_seatbelt(
        mode: ToolChildSandboxMode,
        workspace: &Path,
        attempt_paths: &[PathBuf],
    ) -> Result<Self, String> {
        if !matches!(
            mode,
            ToolChildSandboxMode::MacosSeatbeltV1 | ToolChildSandboxMode::MacosSeatbeltV2
        ) {
            return Err("only a macos Seatbelt mode can build a Seatbelt profile".into());
        }
        if !cfg!(target_os = "macos") {
            return Err(format!("{} requires macOS", mode.name()));
        }
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return Err(format!("{} requires /usr/bin/sandbox-exec", mode.name()));
        }
        let canonical_workspace = workspace.canonicalize().map_err(|error| {
            format!(
                "{} cannot canonicalize workspace {}: {error}",
                mode.name(),
                workspace.display()
            )
        })?;
        let mut allowed_paths = BTreeSet::new();
        allowed_paths.insert(canonical_workspace.clone());
        for path in attempt_paths {
            let canonical = path.canonicalize().map_err(|error| {
                format!(
                    "{} cannot canonicalize allowed path {}: {error}",
                    mode.name(),
                    path.display()
                )
            })?;
            allowed_paths.insert(canonical);
        }
        let profile = macos_seatbelt_profile(mode, &canonical_workspace, &allowed_paths)?;
        Ok(Self {
            mode,
            profile_sha256: sha256(profile.as_bytes()),
            profile,
        })
    }

    fn wrapped_command(&self, command: &str) -> String {
        // The outer trusted bash receives only a constant exec form plus
        // shell-quoted literals. It never evaluates model-supplied command
        // text itself; Seatbelt is established before the inner bash parses it.
        format!(
            "exec /usr/bin/sandbox-exec -p {} /bin/bash -c {}",
            shell_literal(&self.profile),
            shell_literal(command),
        )
    }
}

const SEATBELT_TOOL_CHILD_SYSTEM_READ_ROOTS: &[&str] = &[
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/dev",
    "/opt/homebrew",
    "/usr/local",
];

fn macos_seatbelt_profile(
    mode: ToolChildSandboxMode,
    workspace: &Path,
    allowed_paths: &BTreeSet<PathBuf>,
) -> Result<String, String> {
    let system_rules = SEATBELT_TOOL_CHILD_SYSTEM_READ_ROOTS
        .iter()
        .map(|path| seatbelt_subpath_rule(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let attempt_rules = allowed_paths
        .iter()
        .map(|path| seatbelt_subpath_rule(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut read_rules = system_rules;
    read_rules.extend(attempt_rules.iter().cloned());
    // Keep the narrower deny rules after the workspace allow rules. Seatbelt
    // gives this later, more-specific rule precedence, so V2 removes only
    // repository data rather than broadening or changing source access.
    let repository_metadata_denies = if mode == ToolChildSandboxMode::MacosSeatbeltV2 {
        let git_metadata = seatbelt_subpath_rule(&workspace.join(".git"))?;
        format!(
            "\n(deny file-read* {git_metadata})\n(deny file-write* {git_metadata})"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process-exec)\n(allow process-fork)\n(allow file-read* {})\n(allow file-write* {})\n(allow file-read-metadata (subpath \"/\"))\n(deny network-outbound){repository_metadata_denies}",
        read_rules.join(" "),
        attempt_rules.join(" "),
    ))
}

fn seatbelt_subpath_rule(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("macos-seatbelt-v1 path is not UTF-8: {}", path.display()))?;
    Ok(format!("(subpath \"{}\")", path.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Decorates only process operations with a direct Seatbelt child boundary.
///
/// Workspace read/edit/find retain their existing canonical-path authority and
/// delegate unchanged. This is deliberately an evaluation adapter wrapper,
/// not a default coding-profile policy.
#[derive(Clone)]
struct SeatbeltToolChildOperations {
    delegate: Arc<dyn CodingOperations>,
    sandbox: ToolChildSandbox,
}

impl SeatbeltToolChildOperations {
    fn new(delegate: Arc<dyn CodingOperations>, sandbox: ToolChildSandbox) -> Self {
        Self { delegate, sandbox }
    }
}

impl CodingOperations for SeatbeltToolChildOperations {
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        self.delegate.read_file(path)
    }

    fn read_file_snapshots<'a>(
        &'a self,
        paths: &'a [PathBuf],
        max_total_bytes: usize,
    ) -> OperationFuture<'a, Vec<FileSnapshot>> {
        self.delegate.read_file_snapshots(paths, max_total_bytes)
    }

    fn commit_edit_transaction<'a>(
        &'a self,
        transaction: &'a EditTransaction,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        self.delegate
            .commit_edit_transaction(transaction, cancellation)
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        self.delegate.metadata(path)
    }

    fn find_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        max_results: usize,
        max_output_bytes: usize,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, SearchResult> {
        self.delegate
            .find_files(root, pattern, max_results, max_output_bytes, cancellation)
    }

    fn execute_command<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout: Duration,
        environment: &'a CommandEnvironment,
        cancellation: CancellationToken,
        updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        let delegate = Arc::clone(&self.delegate);
        let wrapped = self.sandbox.wrapped_command(command);
        Box::pin(async move {
            delegate
                .execute_command(
                    &wrapped,
                    cwd,
                    timeout,
                    environment,
                    cancellation,
                    updates,
                )
                .await
        })
    }
}

/// The resolved, provider-visible coding surface used by one evaluation run.
///
/// This is derived from the checked-in Luau builtins rather than a Rust tool
/// factory or profile copy. It is retained only to record the exact surface in
/// evaluation evidence after the run has settled.
struct CodingSurface {
    system_prompt: String,
    tools: Vec<tea_core::tool::ToolDefinition>,
    hook_identity: Digest,
    static_bash_prompt_sha256: String,
    exit_status_observations: ExitStatusObservations,
}

#[cfg(test)]
fn coding_configuration(
    workspace: &Path,
    environment: CommandEnvironment,
    include_static_guidelines: bool,
    static_prompt_profile: StaticPromptProfile,
    tool_child_sandbox: Option<ToolChildSandbox>,
    edit_recovery_projection: EditRecoveryProjectionMode,
    pre_edit_tool_gate: PreEditToolGate,
) -> Result<(AgentConfiguration, CodingSurface), String> {
    coding_configuration_with_post_edit_validation(
        workspace,
        environment,
        include_static_guidelines,
        static_prompt_profile,
        tool_child_sandbox,
        edit_recovery_projection,
        pre_edit_tool_gate,
        PostEditValidationGate::disabled(),
    )
}

fn coding_configuration_with_post_edit_validation(
    workspace: &Path,
    environment: CommandEnvironment,
    include_static_guidelines: bool,
    static_prompt_profile: StaticPromptProfile,
    tool_child_sandbox: Option<ToolChildSandbox>,
    edit_recovery_projection: EditRecoveryProjectionMode,
    pre_edit_tool_gate: PreEditToolGate,
    post_edit_validation_gate: PostEditValidationGate,
) -> Result<(AgentConfiguration, CodingSurface), String> {
    if static_prompt_profile != StaticPromptProfile::BuiltinV1 && !include_static_guidelines {
        return Err("non-default static prompt profile requires static coding guidelines".into());
    }
    let limits = ExtensionLimits {
        max_source_bytes: 64 * 1024,
        max_memory_bytes: 1024 * 1024,
        max_interrupt_checks: 10_000,
    };
    let engine = LuauExtensionEngine;
    let operations: Arc<dyn CodingOperations> = match tool_child_sandbox {
        Some(sandbox) => Arc::new(SeatbeltToolChildOperations::new(
            Arc::new(LocalCodingOperations),
            sandbox,
        )),
        None => Arc::new(LocalCodingOperations),
    };
    let host = CodingHost::with_operations(workspace, operations)
        .map_err(|error| error.to_string())?
        .with_environment(environment);
    let mut prompt_sections = Vec::new();
    let mut static_bash_prompt_sha256 = None;
    let mut tools = ToolRegistry::default();
    let direct_exit_status_witness = DirectExitStatusWitness::default();
    let exit_status_observations = ExitStatusObservations::default();
    let hooks = edit_recovery_projection.context_hook(
        pre_edit_tool_gate,
        post_edit_validation_gate.clone(),
        direct_exit_status_witness.clone(),
        exit_status_observations.clone(),
    );
    for source in [
        builtins::read(limits),
        builtins::bash(limits),
        builtins::edit(limits),
        builtins::find(limits),
    ] {
        let descriptor = engine
            .describe(&source)
            .map_err(|error| error.to_string())?;
        let tool = descriptor
            .tools
            .first()
            .ok_or_else(|| format!("builtin {} declares no tool", source.extension_id))?;
        let implementation = match tool.capability.as_str() {
            WORKSPACE_READ_CAPABILITY_V1 => host.read_capability(),
            WORKSPACE_SEARCH_CAPABILITY_V1 => host.search_capability(),
            WORKSPACE_MUTATE_CAPABILITY_V1 => host.mutate_capability(),
            PROCESS_CAPABILITY_V1 => {
                let process = host.process_capability();
                if post_edit_validation_gate.enabled() {
                    Arc::new(ExitWitnessProcessCapability::new(
                        process,
                        direct_exit_status_witness.clone(),
                    ))
                } else {
                    process
                }
            }
            capability => return Err(format!("unsupported builtin capability {capability}")),
        };
        let mut bindings = ExtensionCapabilityBindings::new();
        bindings
            .insert(
                tool.capability.clone(),
                implementation,
                ExtensionToolLimits::default(),
            )
            .map_err(|error| error.to_string())?;
        bindings
            .fix_tool_capabilities(
                BTreeMap::from([(tool.name.clone(), tool.capability.clone())]),
                BTreeSet::new(),
            )
            .map_err(|error| error.to_string())?;
        let resolved = engine
            .resolve(
                &source,
                bindings,
                Arc::clone(&hooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .map_err(|error| error.to_string())?;
        let source_prompt_sections = static_prompt_profile
            .project_prompt_sections(&source.extension_id, descriptor.prompt_sections)?;
        if source.extension_id == "bash" {
            let bash_prompt = source_prompt_sections
                .iter()
                .find(|section| section.id == "bash")
                .ok_or_else(|| "bash builtin must declare its bash prompt section".to_owned())?;
            static_bash_prompt_sha256 = Some(sha256(bash_prompt.content.as_bytes()));
        }
        prompt_sections.extend(source_prompt_sections);
        for name in resolved
            .tools
            .names()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        {
            tools.insert(
                resolved
                    .tools
                    .get(&name)
                    .expect("resolved builtin tool remains registered")
                    .clone(),
            );
        }
    }
    let prompt_guidelines = if include_static_guidelines {
        static_prompt_profile
            .additional_static_guidance()
            .map(|guidance| format!("{STATIC_CODING_GUIDELINES}\n\n{guidance}"))
            .unwrap_or_else(|| STATIC_CODING_GUIDELINES.to_owned())
    } else {
        String::new()
    };
    let separator = if prompt_guidelines.is_empty() { "" } else { "\n\n" };
    let system_prompt = format!(
        "{}{}{}\n\nCurrent working directory: {}",
        prompt_sections
            .iter()
            .map(|section| section.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        separator,
        prompt_guidelines,
        host.workspace()
            .as_path()
            .to_string_lossy()
            .replace('\\', "/"),
    );
    let surface = CodingSurface {
        system_prompt: system_prompt.clone(),
        tools: tools.definitions(),
        hook_identity: hooks.identity(),
        static_bash_prompt_sha256: static_bash_prompt_sha256
            .ok_or_else(|| "static coding surface must include the bash prompt section".to_owned())?,
        exit_status_observations,
    };
    Ok((
        AgentConfiguration::new(system_prompt, tools, hooks),
        surface,
    ))
}

fn result_json(input: ResultJsonInput<'_>) -> JsonValue {
    let usage = input.provider.usage_snapshot();
    // OpenRouter's `prompt_tokens` includes provider cache reads/writes. Pi's
    // `usage.input` is the uncached portion, so normalize Tea at this adapter
    // boundary while retaining the cache components as separate fields.
    let input_tokens = uncached_input_tokens(
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
    let prompt_total = prompt_total_tokens(usage.input_tokens);
    let output = usage.output_tokens;
    let generation = input_tokens
        .zip(output)
        .map(|(input, output)| input.saturating_add(output));
    let all_tokens = prompt_total
        .zip(output)
        .map(|(prompt, output)| prompt.saturating_add(output));
    let prompt = &input.surface.system_prompt;
    let normalized_prompt = prompt.replace(
        &input.args.workspace.to_string_lossy().replace('\\', "/"),
        "{WORKSPACE}",
    );
    let controlled_sampling = input.args.harness_mode == HarnessMode::Static;
    let sampling_temperature = controlled_sampling
        .then_some(JsonValue::Number(JsonNumber::Float(SHOOTOUT_TEMPERATURE)))
        .unwrap_or(JsonValue::Null);
    let sampling_seed = controlled_sampling
        .then_some(JsonValue::from(SHOOTOUT_SEED))
        .unwrap_or(JsonValue::Null);
    let sampling_source = if controlled_sampling {
        JsonValue::from("adapter-set")
    } else {
        JsonValue::from("provider-default")
    };
    let tools = JsonValue::Array(
        input
            .surface
            .tools
            .iter()
            .map(|tool| {
                JsonValue::object([
                    ("name", JsonValue::String(tool.name.clone())),
                    ("description", JsonValue::String(tool.description.clone())),
                    ("parameters", tool.schema.clone()),
                ])
            })
            .collect(),
    );
    let surface_json = tools.to_json_string().expect("tool surface encodes");
    let environment_json = input
        .args
        .shell_environment_json
        .to_json_string()
        .expect("shell environment encodes");
    let tool_child_sandbox = input.args.tool_child_sandbox.as_ref();
    let static_prompt_profile = input.args.static_prompt_profile;
    let edit_recovery_projection = input.args.edit_recovery_projection;
    let pre_edit_tool_gate = input.pre_edit_tool_gate;
    let post_edit_validation_gate = input.post_edit_validation_gate;
    let cost = input.provider.cost_report();
    let mut counts = session_counts(input.session_snapshot);
    // Pi defines this field from completed compaction lifecycle events. The
    // Tea trace is collected concurrently, so this remains lossless while
    // preserving the shared event-level meaning.
    counts.compactions = input
        .trace
        .iter()
        .filter(|event| event.get("type").and_then(JsonValue::as_str) == Some("compaction_end"))
        .count() as u64;
    JsonValue::object([
        ("schema_version", JsonValue::from(RESULT_SCHEMA)),
        (
            "attempt_id",
            JsonValue::String(input.args.attempt_id.clone()),
        ),
        (
            "baseline_id",
            JsonValue::String(input.args.baseline_id.clone()),
        ),
        (
            "terminal",
            JsonValue::object([
                ("status", JsonValue::from(input.terminal.0)),
                (
                    "code",
                    input
                        .terminal
                        .1
                        .map(|value| JsonValue::from(value.to_owned()))
                        .unwrap_or(JsonValue::Null),
                ),
            ]),
        ),
        ("final_text", JsonValue::String(input.final_text)),
        (
            "runtime",
            JsonValue::object([
                ("implementation", JsonValue::from("tea")),
                ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
                ("revision", JsonValue::from("workspace-source")),
                ("dirty", JsonValue::Bool(false)),
                ("dirty_digest", JsonValue::Null),
            ]),
        ),
        (
            "model",
            JsonValue::object([
                ("provider", JsonValue::from("openrouter")),
                (
                    "requested_model",
                    JsonValue::String(input.args.model.clone()),
                ),
                ("returned_model", JsonValue::Null),
                ("returned_provider", JsonValue::Null),
                ("returned_model_provenance", JsonValue::Null),
                ("returned_provider_provenance", JsonValue::Null),
                (
                    "thinking_level",
                    JsonValue::from(thinking_name(input.args.thinking)),
                ),
                (
                    "max_output_tokens",
                    optional_u64(input.args.max_output_tokens),
                ),
                (
                    "sampling",
                    JsonValue::object([
                        ("temperature", sampling_temperature.clone()),
                        ("seed", sampling_seed.clone()),
                        ("source", sampling_source.clone()),
                    ]),
                ),
            ]),
        ),
        (
            "surface",
            JsonValue::object([
                ("system_prompt_bytes", JsonValue::from(prompt.len() as u64)),
                (
                    "system_prompt_sha256",
                    JsonValue::from(sha256(prompt.as_bytes())),
                ),
                (
                    "workspace_normalized_system_prompt_sha256",
                    JsonValue::from(sha256(normalized_prompt.as_bytes())),
                ),
                (
                    "tool_surface_sha256",
                    JsonValue::from(sha256(surface_json.as_bytes())),
                ),
                (
                    "prompt_tool_surface_sha256",
                    JsonValue::from(sha256(
                        JsonValue::Array(
                            input
                                .surface
                                .tools
                                .iter()
                                .map(|tool| {
                                    JsonValue::object([
                                        ("name", JsonValue::String(tool.name.clone())),
                                        (
                                            "description",
                                            JsonValue::String(tool.description.clone()),
                                        ),
                                    ])
                                })
                                .collect(),
                        )
                        .to_json_string()
                        .expect("prompt surface encodes")
                        .as_bytes(),
                    )),
                ),
                (
                    "wire_tool_surface_sha256",
                    input
                        .wire_evidence
                        .get("requests")
                        .and_then(JsonValue::as_array)
                        .and_then(|requests| requests.first())
                        .and_then(|request| request.get("tool_schema_sha256"))
                        .cloned()
                        .unwrap_or(JsonValue::Null),
                ),
                (
                    "execution_surface_sha256",
                    JsonValue::from(sha256(
                        JsonValue::Array(
                            input
                                .surface
                                .tools
                                .iter()
                                .map(|tool| {
                                    JsonValue::object([
                                        ("name", JsonValue::String(tool.name.clone())),
                                        (
                                            "execution_mode",
                                            JsonValue::String(
                                                match tool.execution_mode {
                                                    ToolExecutionMode::Sequential => "sequential",
                                                    ToolExecutionMode::Parallel => "parallel",
                                                }
                                                .into(),
                                            ),
                                        ),
                                    ])
                                })
                                .collect(),
                        )
                        .to_json_string()
                        .expect("execution surface encodes")
                        .as_bytes(),
                    )),
                ),
                (
                    "active_tools",
                    JsonValue::Array(
                        input
                            .surface
                            .tools
                            .iter()
                            .map(|tool| JsonValue::from(tool.name.clone()))
                            .collect(),
                    ),
                ),
                (
                    "authority",
                    JsonValue::object([
                        (
                            "tools",
                            JsonValue::Array(
                                input
                                    .surface
                                    .tools
                                    .iter()
                                    .map(|tool| JsonValue::String(tool.name.clone()))
                                    .collect(),
                            ),
                        ),
                        ("shell", JsonValue::Bool(true)),
                        (
                            "secret_boundary",
                            JsonValue::from("explicit shootout shell allowlist"),
                        ),
                    ]),
                ),
                ("research_tools", JsonValue::Array(Vec::new())),
                ("subagents", JsonValue::Bool(false)),
                ("shell_curl_available", JsonValue::Bool(true)),
                (
                    "shell_environment_sha256",
                    JsonValue::from(sha256(environment_json.as_bytes())),
                ),
                (
                    "static_prompt_profile",
                    JsonValue::object([
                        ("mode", JsonValue::from(static_prompt_profile.name())),
                        (
                            "bash_prompt_sha256",
                            JsonValue::from(input.surface.static_bash_prompt_sha256.clone()),
                        ),
                    ]),
                ),
                (
                    "tool_child_sandbox",
                    JsonValue::object([
                        (
                            "mode",
                            JsonValue::from(
                                tool_child_sandbox
                                    .map(|sandbox| sandbox.mode.name())
                                    .unwrap_or(ToolChildSandboxMode::None.name()),
                            ),
                        ),
                        (
                            "profile_sha256",
                            tool_child_sandbox
                                .map(|sandbox| JsonValue::from(sandbox.profile_sha256.clone()))
                                .unwrap_or(JsonValue::Null),
                        ),
                    ]),
                ),
                (
                    "edit_recovery_projection",
                    JsonValue::object([
                        (
                            "mode",
                            JsonValue::from(edit_recovery_projection.name()),
                        ),
                        (
                            "hook_identity_blake3",
                            JsonValue::from(input.surface.hook_identity.to_hex()),
                        ),
                        (
                            "correction_sha256",
                            edit_recovery_projection
                                .correction_sha256()
                                .map(JsonValue::from)
                                .unwrap_or(JsonValue::Null),
                        ),
                    ]),
                ),
                (
                    "pre_edit_tool_gate",
                    pre_edit_tool_gate.surface(),
                ),
                (
                    "post_edit_validation_gate",
                    post_edit_validation_gate.surface(),
                ),
            ]),
        ),
        (
            "timings",
            JsonValue::object([
                ("agent_ms", JsonValue::from(input.agent_ms)),
                ("candidate_validation_ms", JsonValue::from(0_u64)),
                ("rollover_ms", JsonValue::from(0_u64)),
            ]),
        ),
        ("wire", input.wire_evidence),
        (
            "effective_policy",
            JsonValue::object([
                (
                    "controlled",
                    JsonValue::object([
                        ("automatic_compaction", JsonValue::Bool(false)),
                        ("compaction_threshold", JsonValue::Null),
                        ("provider_retry", shootout_provider_retry_evidence()),
                        (
                            "request_timeout_seconds",
                            JsonValue::from(request_timeout_seconds(
                                input.args.outer_timeout_seconds,
                            )),
                        ),
                        (
                            "idle_timeout_seconds",
                            JsonValue::from(request_timeout_seconds(
                                input.args.outer_timeout_seconds,
                            )),
                        ),
                        (
                            "outer_attempt_timeout_seconds",
                            JsonValue::from(input.args.outer_timeout_seconds),
                        ),
                        (
                            "model_reasoning",
                            JsonValue::from(thinking_name(input.args.thinking)),
                        ),
                        (
                            "output_token_ceiling",
                            optional_u64(input.args.max_output_tokens),
                        ),
                        ("provider_routing", input.args.provider_routing.clone()),
                        ("pre_edit_tool_gate", pre_edit_tool_gate.surface()),
                        (
                            "post_edit_validation_gate",
                            post_edit_validation_gate.surface(),
                        ),
                        (
                            "sampling",
                            JsonValue::object([
                                ("temperature", sampling_temperature),
                                ("seed", sampling_seed),
                            ]),
                        ),
                    ]),
                ),
                (
                    "native",
                    JsonValue::object([(
                        "tool_execution",
                        JsonValue::Array(
                            input
                                .surface
                                .tools
                                .iter()
                                .map(|tool| {
                                    JsonValue::object([
                                        ("name", JsonValue::String(tool.name.clone())),
                                        (
                                            "execution_mode",
                                            JsonValue::from(match tool.execution_mode {
                                                ToolExecutionMode::Sequential => "sequential",
                                                ToolExecutionMode::Parallel => "parallel",
                                            }),
                                        ),
                                    ])
                                })
                                .collect(),
                        ),
                    )]),
                ),
                ("observability_unknown", JsonValue::Array(Vec::new())),
            ]),
        ),
        (
            "counts",
            JsonValue::object([
                // `turns` intentionally follows Pi's user-message semantics.
                ("turns", JsonValue::from(counts.user_messages)),
                ("model_turns", JsonValue::from(counts.model_turns)),
                (
                    "provider_requests",
                    JsonValue::from(counts.provider_requests),
                ),
                ("tool_calls", JsonValue::from(counts.tool_calls)),
                ("retries", JsonValue::from(counts.retries)),
                ("compactions", JsonValue::from(counts.compactions)),
            ]),
        ),
        (
            "usage",
            JsonValue::object([
                ("input", optional_u64(input_tokens)),
                ("prompt_total", optional_u64(prompt_total)),
                ("output", optional_u64(output)),
                ("generation", optional_u64(generation)),
                ("all_tokens", optional_u64(all_tokens)),
                ("reasoning", optional_u64(usage.reasoning_tokens)),
                ("cache_read", optional_u64(usage.cache_read_tokens)),
                ("cache_write", optional_u64(usage.cache_write_tokens)),
            ]),
        ),
        (
            "cost",
            JsonValue::object([
                ("kind", JsonValue::from("provider-reported")),
                ("currency", JsonValue::from("USD")),
                (
                    "total",
                    if cost.complete {
                        JsonValue::number(JsonNumber::Float(cost.reported_total_usd))
                            .expect("finite reported cost")
                    } else {
                        JsonValue::Null
                    },
                ),
            ]),
        ),
        (
            "harness",
            JsonValue::object([
                ("mode", JsonValue::from(input.args.harness_mode.name())),
                (
                    "base_snapshot_id",
                    JsonValue::String(input.snapshot.id.to_string()),
                ),
                (
                    "initial_snapshot_id",
                    JsonValue::String(input.snapshot.id.to_string()),
                ),
                (
                    "final_snapshot_id",
                    JsonValue::String(input.final_snapshot_id),
                ),
                (
                    "decision",
                    JsonValue::from(if input.args.harness_mode == HarnessMode::Static {
                        "not-applicable"
                    } else if input.activated {
                        "activated"
                    } else if input.candidate_count == 0 {
                        "no-change"
                    } else {
                        "rejected"
                    }),
                ),
                ("candidate_count", JsonValue::from(input.candidate_count)),
                (
                    "candidate_id",
                    input
                        .candidate_id
                        .map(JsonValue::String)
                        .unwrap_or(JsonValue::Null),
                ),
                (
                    "changed_surfaces",
                    JsonValue::Array(
                        input
                            .changed_surfaces
                            .into_iter()
                            .map(JsonValue::String)
                            .collect(),
                    ),
                ),
                ("candidate_source_bytes", JsonValue::from(0_u64)),
                (
                    "hypothesis",
                    input
                        .hypothesis
                        .map(JsonValue::String)
                        .unwrap_or(JsonValue::Null),
                ),
            ]),
        ),
        ("validation_evidence", input.validation_evidence.json()),
        (
            "trace",
            JsonValue::Array(
                input
                    .trace
                    .into_iter()
                    .chain(input.validation_evidence.trace())
                    .collect(),
            ),
        ),
    ])
}

fn main() -> Result<(), String> {
    let args = Args::parse()?;
    let task = read_json(&args.task_json, "task")?;
    let capabilities = read_json(&args.capabilities_json, "capabilities")?;
    if task.get("capabilities") != Some(&capabilities) {
        return Err("task and capability manifest disagree".into());
    }
    let prompt = task
        .get("prompt")
        .and_then(JsonValue::as_str)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| "evaluation task has no prompt".to_owned())?;
    let actual = capabilities
        .as_array()
        .ok_or_else(|| "capabilities must be an array".to_owned())?
        .iter()
        .map(|tool| tool.get("name").and_then(JsonValue::as_str))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "capability has no name".to_owned())?;
    if actual != ["read", "bash", "edit", "find"] {
        return Err("active tool list must be read/bash/edit/find".into());
    }
    let pre_edit_tool_gate = PreEditToolGate::from_task(
        args.pre_edit_tool_gate,
        &task,
        prompt,
        &args.workspace,
    )?;
    let post_edit_validation_gate = PostEditValidationGate::from_pre_edit(
        args.post_edit_validation_gate,
        &pre_edit_tool_gate,
    )?;
    let api_key = env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY must be supplied by vault".to_owned())?;
    let request_capture = OpenRouterRequestCapture::default();
    let provider_config = match args.max_output_tokens {
        Some(maximum) => {
            OpenRouterConfig::new(&api_key, args.model.clone()).with_max_tokens(maximum)
        }
        None => OpenRouterConfig::new(api_key, args.model.clone()),
    }
    .with_request_timeout(Duration::from_secs(request_timeout_seconds(
        args.outer_timeout_seconds,
    )))
    // Pi permits a disabled HTTP body idle timeout and Fx has no body deadline
    // after response headers. Keep Tea's finite outer request budget without
    // introducing a shorter stream-idle cutoff for this evaluation.
    .with_stall_timeout(Duration::from_secs(request_timeout_seconds(
        args.outer_timeout_seconds,
    )))
    .with_retry_policy(RetryPolicy::new(
        SHOOTOUT_PROVIDER_MAX_RETRIES,
        Duration::from_millis(250),
        Duration::from_secs(8),
    ))
    .with_provider_routing(args.provider_routing.clone())
    .with_request_capture(request_capture.clone());
    let provider_config = if args.harness_mode == HarnessMode::Static {
        provider_config
            .with_temperature(SHOOTOUT_TEMPERATURE)
            .with_seed(SHOOTOUT_SEED)
            .with_model_tool_allowlist(["read", "bash", "edit", "find"])
    } else {
        provider_config
    };
    let provider = Arc::new(OpenRouterProvider::new(provider_config));
    let (configuration, surface) = coding_configuration_with_post_edit_validation(
        &args.workspace,
        args.shell_environment.clone(),
        args.harness_mode == HarnessMode::Static,
        args.static_prompt_profile,
        args.tool_child_sandbox.clone(),
        args.edit_recovery_projection,
        pre_edit_tool_gate.clone(),
        post_edit_validation_gate.clone(),
    )?;
    let model = ModelDescriptor {
        provider: "openrouter".into(),
        model: args.model.clone(),
        revision: None,
    };
    let model_profile = model_profile(&model)?;
    // Build the live services before seeding the immutable snapshot so its
    // policy identities are copied from the same executable configuration.
    // `AgentConfiguration` is cloned only for this composition step; the
    // original is retained to describe the snapshot surface.
    let services =
        RuntimeServices::from_agent_configuration(provider.clone(), configuration.clone())
            .model(model.clone())
            .thinking_level(args.thinking);
    let runtime_identities = services.runtime_policy_identities();
    let session_root = args
        .evidence_dir
        .parent()
        .ok_or_else(|| "evidence directory needs a parent".to_owned())?
        .join("harness")
        .join("session.tea");
    fs::create_dir_all(session_root.parent().expect("session has parent"))
        .map_err(|error| error.to_string())?;
    let mut metadata = BTreeMap::new();
    metadata.insert(
        SELF_EXTENSION_MODE_METADATA_KEY.into(),
        args.harness_mode.extension_mode().metadata_value(),
    );
    metadata.insert(
        "tea.model.provider".into(),
        JsonValue::String("openrouter".into()),
    );
    metadata.insert(
        "tea.model.requested".into(),
        JsonValue::String(args.model.clone()),
    );
    metadata.insert(
        "tea.thinking".into(),
        JsonValue::String(thinking_name(args.thinking).into()),
    );
    let session_id = SessionId::new(format!("shootout-{}", args.attempt_id))
        .map_err(|error| error.to_string())?;
    let mut session = JsonlSession::create(
        &session_root,
        SessionHeader::new(session_id, args.workspace.to_string_lossy(), metadata),
        DurabilityMode::Strict,
    )
    .map_err(|error| error.to_string())?;
    let artifacts: Arc<dyn tea_session::ArtifactStore> = Arc::new(
        session
            .artifact_store()
            .map_err(|error| error.to_string())?,
    );
    let mut repository = HarnessRepository::with_extension_engine(
        Arc::clone(&artifacts),
        Arc::new(LuauExtensionEngine),
    );
    let snapshot = repository
        .stage_snapshot(snapshot_spec(
            &configuration,
            &model_profile,
            args.harness_mode,
            runtime_identities,
        ))
        .map_err(|error| error.to_string())?;
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 0)
        .map_err(|error| error.to_string())?;
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("initial-model").map_err(|error| error.to_string())?,
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: "openrouter".into(),
                    model: args.model.clone(),
                    revision: None,
                }),
            },
        )
        .map_err(|error| error.to_string())?;
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("initial-thinking").map_err(|error| error.to_string())?,
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                    level: thinking_name(args.thinking).into(),
                }),
            },
        )
        .map_err(|error| error.to_string())?;
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("initial-revision").map_err(|error| error.to_string())?,
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )
        .map_err(|error| error.to_string())?;
    let manager = Arc::new(
        HarnessResolver::new(repository, Default::default())
            .self_extension_mode(args.harness_mode.extension_mode()),
    );
    let harness = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: Arc::clone(&manager),
        root_identity: HarnessIdentity::new(
            revision.revision_id,
            snapshot.id.clone(),
            model_profile.profile_id,
        ),
        root_services: services,
        artifacts,
        rollover_budget: 1,
        subagents: None,
    })
    .map_err(|error| error.to_string())?;
    let subscription = harness
        .subscribe_events()
        .map_err(|error| error.to_string())?;
    // The supervisor fanout is deliberately bounded for interactive callers.
    // Consume it while the run is active so the evaluation trace does not
    // overflow that queue and silently lose lifecycle events.
    let initial_snapshot_id = snapshot.id.to_string();
    let collecting = Arc::new(AtomicBool::new(true));
    let collector_flag = Arc::clone(&collecting);
    let collector = thread::spawn(move || {
        let mut events = EventCollection::new(initial_snapshot_id);
        loop {
            match subscription.try_recv() {
                Ok(event) => events.observe(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if !collector_flag.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        // The producer may have published an event just before the stop flag;
        // drain the receiver once more before returning the authoritative trace.
        while let Ok(event) = subscription.try_recv() {
            events.observe(event);
        }
        events
    });
    let started = Instant::now();
    // `request_timeout` bounds one provider stream. The scored static run may
    // make many streams, so enforce its task budget once across the durable
    // root operation and leave the runner's static-only grace for evidence
    // finalization after cancellation. JIT keeps its existing outer handling.
    let outer_deadline = (args.harness_mode == HarnessMode::Static
        && args.outer_timeout_seconds != 0)
        .then(|| Duration::from_secs(args.outer_timeout_seconds));
    let outer_deadline_at = outer_deadline.map(|duration| started + duration);
    let deadline_harness = Arc::clone(&harness);
    let (mut outcome, mut outer_deadline_fired) = smol::block_on(settle_with_outer_deadline(
        outer_deadline,
        async {
            if args.harness_mode == HarnessMode::Jit {
                harness.run_authoring_prompt(prompt).await
            } else {
                harness.run_root_prompt(prompt).await
            }
        },
        move || {
            let _ = deadline_harness.abort_root();
        },
    ));
    let mut post_edit_reminder_issued = false;
    let mut post_edit_evidence_missing = false;
    if !outer_deadline_fired
        && args.harness_mode == HarnessMode::Static
        && matches!(&outcome, Ok(operation) if operation.is_completed())
    {
        let durable = harness.snapshot().map_err(|error| error.to_string())?;
        let evidence = validation_evidence_from_snapshot(
            &post_edit_validation_gate,
            &durable,
            &surface.exit_status_observations.snapshot(),
        );
        if evidence.pending() {
            // This is one normal root prompt, not an extension continuation:
            // its user/model turn is durable and counted exactly like Pi's.
            post_edit_reminder_issued = true;
            let remaining_deadline = outer_deadline_at
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let deadline_harness = Arc::clone(&harness);
            let (continuation, continuation_timed_out) = smol::block_on(
                settle_with_outer_deadline(
                    remaining_deadline,
                    harness.run_root_prompt(POST_EDIT_VALIDATION_REMINDER),
                    move || {
                        let _ = deadline_harness.abort_root();
                    },
                ),
            );
            outcome = continuation;
            outer_deadline_fired = continuation_timed_out;
            if !outer_deadline_fired
                && matches!(&outcome, Ok(operation) if operation.is_completed())
            {
                let durable = harness.snapshot().map_err(|error| error.to_string())?;
                post_edit_evidence_missing = validation_evidence_from_snapshot(
                    &post_edit_validation_gate,
                    &durable,
                    &surface.exit_status_observations.snapshot(),
                )
                .pending();
            }
        }
    }
    let agent_ms = started.elapsed().as_millis() as u64;
    collecting.store(false, Ordering::Release);
    let events = collector
        .join()
        .map_err(|_| "event collector thread panicked".to_owned())?;
    let durable = harness.snapshot().map_err(|error| error.to_string())?;
    let mut validation_evidence = validation_evidence_from_snapshot(
        &post_edit_validation_gate,
        &durable,
        &surface.exit_status_observations.snapshot(),
    );
    if post_edit_reminder_issued {
        let _ = validation_evidence.issue_reminder();
    }
    if post_edit_evidence_missing {
        validation_evidence.mark_missing();
    }
    let final_text = durable
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match &entry.body {
            SessionEntry::AssistantMessage(message) => Some(message.content.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let EventCollection {
        trace,
        candidate_count,
        candidate_id,
        changed_surfaces,
        activated,
        final_snapshot_id,
    } = events;
    let wire_evidence = write_wire_evidence(&args, &request_capture)?;
    let provider_error = provider
        .last_error_report()
        .map(|report| match report.status_code {
            Some(status) => format!("openrouter_{:?}_{status}", report.source).to_ascii_lowercase(),
            None => format!("openrouter_{:?}", report.source).to_ascii_lowercase(),
        });
    let terminal = if outer_deadline_fired {
        ("failed", Some("outer_timeout"))
    } else if post_edit_evidence_missing {
        ("failed", Some("post_edit_validation_evidence_missing"))
    } else {
        match &outcome {
        Ok(operation) if operation.is_completed() => ("completed", None),
        Ok(_) => (
            "failed",
            provider_error
                .as_deref()
                .or(Some("durable_operation_failed")),
        ),
        Err(_) => (
            "failed",
            provider_error.as_deref().or(Some("durable_runtime_error")),
        ),
        }
    };
    fs::create_dir_all(&args.evidence_dir).map_err(|error| error.to_string())?;
    let system_prompt = &surface.system_prompt;
    fs::write(args.evidence_dir.join("system-prompt.txt"), system_prompt)
        .map_err(|error| error.to_string())?;
    let tool_surface = JsonValue::Array(
        surface
            .tools
            .iter()
            .map(|tool| {
                JsonValue::object([
                    ("name", JsonValue::String(tool.name.clone())),
                    ("description", JsonValue::String(tool.description.clone())),
                    ("parameters", tool.schema.clone()),
                ])
            })
            .collect(),
    );
    fs::write(
        args.evidence_dir.join("tool-surface.json"),
        tool_surface
            .to_json_string()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let candidate = candidate_id
        .as_ref()
        .and_then(|id| tea_session::HarnessCandidateId::new(id.clone()).ok())
        .and_then(|id| manager.candidate(&id).ok());
    let hypothesis = candidate.as_ref().map(|candidate| {
        format!(
            "evidence: {}; expected effect: {}; regression risk: {}",
            candidate.draft.hypothesis.targeted_evidence,
            candidate.draft.hypothesis.expected_effect,
            candidate.draft.hypothesis.regression_risk
        )
    });
    write_jit_evidence(
        &args,
        &snapshot.id.to_string(),
        &final_snapshot_id,
        candidate.as_ref(),
        activated,
        &changed_surfaces,
    )?;
    let result = result_json(ResultJsonInput {
        args: &args,
        pre_edit_tool_gate: &pre_edit_tool_gate,
        post_edit_validation_gate: &post_edit_validation_gate,
        validation_evidence: &validation_evidence,
        provider: &provider,
        surface: &surface,
        snapshot: &snapshot,
        session_snapshot: &durable,
        final_snapshot_id,
        terminal,
        agent_ms,
        final_text,
        trace,
        wire_evidence,
        candidate_count,
        candidate_id,
        changed_surfaces,
        hypothesis,
        activated,
    });
    fs::write(
        &args.result_json,
        result.to_json_string().map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if outcome.is_err() {
        return Err("durable Tea operation failed after publishing a normalized result".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tea_core::coding::{CodingOperations, CommandTermination, LocalCodingOperations};
    use tea_core::effect::RunProvenance;
    use tea_core::event::{AgentEvent, AgentEventKind, EventSequence};
    use tea_core::harness::extension::{
        ExtensionCapability, ExtensionCapabilityFuture, ExtensionCapabilityRequest,
        ExtensionCapabilityResponse,
    };
    use tea_core::hooks::{ContextEnvelope, HookSet};
    use tea_core::runtime::TeaEvent;
    use tea_core::scheduler::{AdapterRequestObservation, CancellationToken};
    use tea_core::state::{
        AgentMessage, AgentToolCall, MessageId, RunId, SerializedJson, ToolCallId, TurnId,
    };
    use tea_core::tool::{AgentToolResult, ToolCall, ToolRegistry, ToolUpdateSink};
    use tea_protocol::JsonValue;
    use tea_session::LaneId;

    use super::{
        AgentConfiguration, CommandEnvironment, EditRecoveryProjectionMode, HarnessMode, ModelDescriptor, OpenAiContextHook,
        OpenRouterConfig, OpenRouterProvider, OpenRouterRequestCapture, REQUIRED_MODEL, RuntimeServices,
        DirectExitStatusWitness, ExitStatusObservations, ExitWitnessProcessCapability,
        POST_EDIT_VALIDATION_BLOCK_REASON, PostEditValidationGate, PostEditValidationGateMode,
        PRE_EDIT_TOOL_GATE_BLOCK_REASON, SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON,
        PreEditToolGate, PreEditToolGateHook, PreEditToolGateMode, ValidationEvidence,
        direct_foreground_shell_v1,
        pre_edit_tool_gate_violation,
        SeatbeltToolChildOperations, ToolChildSandbox, ToolChildSandboxMode,
        StaticPromptProfile, coding_configuration, model_profile, prompt_total_tokens, request_timeout_seconds, sha256,
        source_local_task_targets,
        shootout_provider_retry_evidence, snapshot_spec, uncached_input_tokens,
    };

    #[derive(Clone)]
    struct FixedProcessReceiptCapability {
        value: JsonValue,
    }

    impl ExtensionCapability for FixedProcessReceiptCapability {
        fn invoke(
            &self,
            _request: ExtensionCapabilityRequest,
            _cancellation: CancellationToken,
        ) -> ExtensionCapabilityFuture {
            let value = self.value.clone();
            Box::pin(async move { Ok(ExtensionCapabilityResponse { value }) })
        }
    }
    #[test]
    fn requested_deepseek_model_is_pinned() {
        assert_eq!(REQUIRED_MODEL, "deepseek/deepseek-v4-flash-0731");
        assert_ne!(REQUIRED_MODEL, "poolside/laguna-s-2.1:free");
        assert_eq!(HarnessMode::parse("jit").unwrap().name(), "jit");
        assert_eq!(
            StaticPromptProfile::parse("no-history-v1").unwrap().name(),
            "no-history-v1"
        );
        assert_eq!(
            StaticPromptProfile::parse("prefix-guard-v1").unwrap().name(),
            "prefix-guard-v1"
        );
        assert_eq!(
            StaticPromptProfile::parse("prefix-guard-focused-v1")
                .unwrap()
                .name(),
            "prefix-guard-focused-v1"
        );
        assert_eq!(
            PreEditToolGateMode::parse("direct-edit-v1").unwrap().name(),
            "direct-edit-v1"
        );
        assert!(StaticPromptProfile::parse("hidden-fallback").is_err());
    }

    #[test]
    fn emitted_controlled_provider_retry_disables_preoutput_replay_for_paired_shootouts() {
        assert_eq!(
            shootout_provider_retry_evidence(),
            JsonValue::object([
                ("enabled", JsonValue::Bool(true)),
                ("max_retries", JsonValue::from(0_u64)),
            ]),
        );
    }

    #[test]
    fn direct_edit_gate_blocks_pre_edit_exploration_and_reopens_after_successful_edit() {
        let direct_gate = PreEditToolGate::direct_edit_v1();
        let make_call = |id: &str, name: &str| ToolCall {
            id: ToolCallId::new(id).expect("fixture call ID"),
            name: name.into(),
            arguments: SerializedJson::new("{}"),
        };
        let before_edit = ContextEnvelope {
            version: 1,
            messages: Vec::new(),
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-bash", "bash"),
                &before_edit,
            ),
            Some(PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        );
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-find", "find"),
                &before_edit,
            ),
            Some(PRE_EDIT_TOOL_GATE_BLOCK_REASON)
        );
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-read", "read"),
                &before_edit,
            ),
            None
        );

        let after_failed_edit = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::ToolResult {
                id: MessageId(1),
                tool_call_id: ToolCallId::new("call-failed-edit").expect("fixture call ID"),
                tool_name: "edit".into(),
                content: "invalid arguments".into(),
                details: None,
                usage: Box::new(None),
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: true,
                failure: None,
            }],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-after-failed-edit", "find"),
                &after_failed_edit,
            ),
            Some(PRE_EDIT_TOOL_GATE_BLOCK_REASON),
            "a failed edit does not unlock exploration"
        );

        let same_batch_as_edit = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::Assistant {
                id: MessageId(2),
                content: String::new(),
                tool_calls: vec![AgentToolCall {
                    id: ToolCallId::new("call-batched-edit").expect("fixture call ID"),
                    name: "edit".into(),
                    arguments: SerializedJson::new("{\"files\":[]}"),
                }],
                stop_reason: None,
                error_message: None,
                opaque_context: Vec::new(),
            }],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-batched-bash", "bash"),
                &same_batch_as_edit,
            ),
            Some(PRE_EDIT_TOOL_GATE_BLOCK_REASON),
            "an edit call does not unlock another call in its own batch"
        );
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-edit", "edit"),
                &before_edit,
            ),
            None
        );

        let after_edit = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::ToolResult {
                id: MessageId(1),
                tool_call_id: ToolCallId::new("call-successful-edit").expect("fixture call ID"),
                tool_name: "edit".into(),
                content: "updated lib/router/index.js".into(),
                details: None,
                usage: Box::new(None),
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: false,
                failure: None,
            }],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(
                &direct_gate,
                &make_call("call-validate", "bash"),
                &after_edit,
            ),
            None,
            "post-edit validation remains available"
        );
    }

    #[test]
    fn direct_edit_gate_surface_is_stable_for_shared_policy_evidence() {
        assert_eq!(
            PRE_EDIT_TOOL_GATE_BLOCK_REASON,
            "Pre-edit direct workflow policy: before a successful edit result, bash and find are unavailable. Read the named source and make the smallest edit to the named target; after a successful edit, use bash or find only for focused validation.",
        );
        assert_eq!(
            PreEditToolGate::direct_edit_v1().surface(),
            JsonValue::object([
                ("mode", JsonValue::from("direct-edit-v1")),
                (
                    "blocked_tools",
                    JsonValue::Array(vec![JsonValue::from("bash"), JsonValue::from("find")]),
                ),
                ("target_restricted_tools", JsonValue::Array(Vec::new())),
                ("source_local_targets", JsonValue::Array(Vec::new())),
                (
                    "unlocks_after",
                    JsonValue::from("prior-successful-edit-result"),
                ),
                (
                    "same_batch_rule",
                    JsonValue::from("block-until-prior-successful-edit-result"),
                ),
                (
                    "block_reason_sha256",
                    JsonValue::from(
                        "952f707b9dc5b44deb555174c3cf546d00c9ab75c2b28664fe327508edcd42f4",
                    ),
                ),
            ]),
        );
        assert_eq!(
            PreEditToolGate::disabled().surface(),
            JsonValue::object([
                ("mode", JsonValue::from("none")),
                ("blocked_tools", JsonValue::Array(Vec::new())),
                ("target_restricted_tools", JsonValue::Array(Vec::new())),
                ("source_local_targets", JsonValue::Array(Vec::new())),
                ("unlocks_after", JsonValue::Null),
                ("same_batch_rule", JsonValue::Null),
                ("block_reason_sha256", JsonValue::Null),
            ]),
        );
    }

    #[test]
    fn post_edit_validation_requires_an_explicit_exit_zero_receipt_and_resets_for_later_edits() {
        let gate = PostEditValidationGate {
            mode: PostEditValidationGateMode::UnmaskedEvidenceV1,
            source_local_targets: std::collections::BTreeSet::from(["lib/response.js".into()]),
        };
        for command in ["npm test", "node scripts/check.js", "cargo test -p crate"] {
            assert!(direct_foreground_shell_v1(Some(command)), "{command}");
        }
        for command in [
            "",
            "npm test; true",
            "npm test | tail",
            "npm test && true",
            "npm test > out",
            "npm test $(pwd)",
            "npm test 'x'",
            "npm test \u{005c}",
            "bash check.sh",
            "env sh check.sh",
            "npm test\ntrue",
        ] {
            assert!(!direct_foreground_shell_v1(Some(command)), "{command}");
        }

        let target_edit = JsonValue::parse(r#"{"files":[{"path":"lib/response.js","edits":[]}]}"#).expect("target arguments");
        let other_edit = JsonValue::parse(r#"{"files":[{"path":"test/response.js","edits":[]}]}"#).expect("other arguments");
        let direct_bash = JsonValue::parse(r#"{"command":"npm test"}"#).expect("bash arguments");
        let masked_bash = JsonValue::parse(r#"{"command":"npm test | tail"}"#).expect("masked arguments");
        let mut evidence = ValidationEvidence::enabled();
        evidence.observe_assistant(&gate, vec![("target-edit", "edit", &target_edit)]);
        evidence.observe_result("target-edit", "edit", false, false);
        assert!(evidence.pending());
        evidence.observe_assistant(&gate, vec![("masked", "bash", &masked_bash)]);
        evidence.observe_assistant(&gate, vec![("unwitnessed", "bash", &direct_bash)]);
        evidence.observe_result("unwitnessed", "bash", false, false);
        assert!(evidence.pending(), "a non-error tool result alone is not evidence");

        evidence.observe_assistant(&gate, vec![
            ("same-batch-edit", "edit", &target_edit),
            ("same-batch-bash", "bash", &direct_bash),
        ]);
        evidence.observe_result("same-batch-bash", "bash", false, true);
        evidence.observe_result("same-batch-edit", "edit", false, false);
        assert_eq!(evidence.generation, 2);
        assert!(evidence.qualifying.is_none());

        let mut reversed_same_batch = ValidationEvidence::enabled();
        reversed_same_batch.observe_assistant(&gate, vec![("initial-target-edit", "edit", &target_edit)]);
        reversed_same_batch.observe_result("initial-target-edit", "edit", false, false);
        // A batch-wide scan must reject this bash even though its event appears
        // before a later failed edit in the assistant's call order.
        reversed_same_batch.observe_assistant(&gate, vec![
            ("reversed-bash", "bash", &direct_bash),
            ("reversed-failed-edit", "edit", &target_edit),
        ]);
        reversed_same_batch.observe_result("reversed-failed-edit", "edit", true, false);
        reversed_same_batch.observe_result("reversed-bash", "bash", false, true);
        assert!(reversed_same_batch.pending());
        assert!(reversed_same_batch.qualifying.is_none());

        evidence.observe_assistant(&gate, vec![("qualified", "bash", &direct_bash)]);
        evidence.observe_result("qualified", "bash", false, true);
        assert!(!evidence.pending());
        assert_eq!(
            evidence.json().get("qualifying_process_exit").and_then(JsonValue::as_str),
            Some("exited-zero"),
        );

        // After source-local's first declared target result, a successful
        // native non-target edit is allowed and invalidates older evidence.
        evidence.observe_assistant(&gate, vec![("later-native-edit", "edit", &other_edit)]);
        evidence.observe_result("later-native-edit", "edit", false, false);
        assert_eq!(evidence.generation, 3);
        assert!(evidence.qualifying.is_none());
        assert!(evidence.issue_reminder());
        assert!(!evidence.issue_reminder());
        evidence.mark_missing();
        let exported = evidence.json().to_json_string().expect("evidence encodes");
        assert!(exported.contains("\"state\":\"missing\""));
        assert!(!exported.contains("npm test"));
        assert_eq!(POST_EDIT_VALIDATION_BLOCK_REASON, "Validation evidence requires a direct foreground command whose exit status is visible. Avoid pipelines and status-suppression wrappers; choose an appropriate workspace-local check.");
    }

    #[test]
    fn process_receipt_decorator_binds_the_exact_call_id_and_requires_exited_zero() {
        let witness = DirectExitStatusWitness::default();
        let observations = ExitStatusObservations::default();
        let hook = PreEditToolGateHook {
            edit_recovery_projection: EditRecoveryProjectionMode::None,
            gate: PreEditToolGate::disabled(),
            post_edit_validation_gate: PostEditValidationGate::disabled(),
            direct_exit_status_witness: witness.clone(),
            exit_status_observations: observations.clone(),
        };
        let request = |call_id: &str| ExtensionCapabilityRequest {
            call_id: ToolCallId::new(call_id).expect("fixture capability call ID"),
            tool_name: "bash".into(),
            provenance: RunProvenance::default(),
            capability: "process".into(),
            method: "execute".into(),
            arguments: JsonValue::Null,
            updates: ToolUpdateSink::disabled(),
        };
        let tool_call = |call_id: &str| ToolCall {
            id: ToolCallId::new(call_id).expect("fixture tool call ID"),
            name: "bash".into(),
            arguments: SerializedJson::new("{}"),
        };
        let tool_result = |call_id: &str| AgentToolResult {
            tool_call_id: ToolCallId::new(call_id).expect("fixture result call ID"),
            content: String::new(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        };
        let invoke = |call_id: &str, value: JsonValue| {
            let decorator = ExitWitnessProcessCapability::new(
                Arc::new(FixedProcessReceiptCapability { value }),
                witness.clone(),
            );
            smol::block_on(decorator.invoke(request(call_id), CancellationToken::new()))
                .expect("fixture process capability succeeds");
        };

        invoke(
            "receipt-zero",
            JsonValue::object([
                ("termination", JsonValue::from("exited")),
                ("exitCode", JsonValue::from(0_u64)),
            ]),
        );
        // A different after-tool call cannot consume the receipt belonging to
        // `receipt-zero`; the key is the core-supplied call ID, not command or
        // completion order.
        hook.after_tool_call(&tool_call("other-call"), &tool_result("other-call"))
            .expect("after-tool hook observes missing receipt");
        hook.after_tool_call(&tool_call("receipt-zero"), &tool_result("receipt-zero"))
            .expect("after-tool hook observes exact zero receipt");
        invoke(
            "receipt-nonzero",
            JsonValue::object([
                ("termination", JsonValue::from("exited")),
                ("exitCode", JsonValue::from(1_u64)),
            ]),
        );
        hook.after_tool_call(&tool_call("receipt-nonzero"), &tool_result("receipt-nonzero"))
            .expect("after-tool hook observes nonzero receipt");
        invoke(
            "receipt-not-exited",
            JsonValue::object([
                ("termination", JsonValue::from("killed")),
                ("exitCode", JsonValue::from(0_u64)),
            ]),
        );
        hook.after_tool_call(&tool_call("receipt-not-exited"), &tool_result("receipt-not-exited"))
            .expect("after-tool hook observes non-exited receipt");

        let observed = observations.snapshot();
        assert_eq!(observed.get("other-call"), Some(&false));
        assert_eq!(observed.get("receipt-zero"), Some(&true));
        assert_eq!(observed.get("receipt-nonzero"), Some(&false));
        assert_eq!(observed.get("receipt-not-exited"), Some(&false));

        let gate = PostEditValidationGate {
            mode: PostEditValidationGateMode::UnmaskedEvidenceV1,
            source_local_targets: std::collections::BTreeSet::from(["lib/response.js".into()]),
        };
        let target_edit = JsonValue::parse(r#"{"files":[{"path":"lib/response.js","edits":[]}]}"#)
            .expect("target arguments");
        let direct_bash = JsonValue::parse(r#"{"command":"npm test"}"#).expect("bash arguments");
        for (call_id, expected_qualification) in [
            ("receipt-zero", true),
            ("receipt-nonzero", false),
            ("receipt-not-exited", false),
        ] {
            let mut evidence = ValidationEvidence::enabled();
            evidence.observe_assistant(&gate, vec![("target-edit", "edit", &target_edit)]);
            evidence.observe_result("target-edit", "edit", false, false);
            evidence.observe_assistant(&gate, vec![(call_id, "bash", &direct_bash)]);
            evidence.observe_result(
                call_id,
                "bash",
                false,
                observed.get(call_id).copied().unwrap_or(false),
            );
            assert_eq!(!evidence.pending(), expected_qualification, "{call_id}");
            assert_eq!(
                evidence
                    .json()
                    .get("qualifying_process_exit")
                    .and_then(JsonValue::as_str),
                expected_qualification.then_some("exited-zero"),
                "{call_id}",
            );
        }
    }

    #[test]
    fn source_local_gate_limits_pre_edit_paths_and_correlates_the_target_edit_result() {
        let gate = PreEditToolGate {
            mode: PreEditToolGateMode::SourceLocalV1,
            source_local_targets: vec!["lib/response.js".into()],
        };
        let call = |id: &str, name: &str, arguments: &str| ToolCall {
            id: ToolCallId::new(id).expect("fixture call ID"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        };
        let before_edit = ContextEnvelope {
            version: 1,
            messages: Vec::new(),
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(
                &gate,
                &call("target-read", "read", r#"{"path":"lib/response.js"}"#),
                &before_edit,
            ),
            None,
        );
        assert_eq!(
            pre_edit_tool_gate_violation(
                &gate,
                &call("other-read", "read", r#"{"path":"test/response.js"}"#),
                &before_edit,
            ),
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
        );
        assert_eq!(
            pre_edit_tool_gate_violation(
                &gate,
                &call("other-edit", "edit", r#"{"files":[{"path":"test/response.js","edits":[]}]}"#),
                &before_edit,
            ),
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
        );
        assert_eq!(
            pre_edit_tool_gate_violation(&gate, &call("pre-edit-bash", "bash", "{}"), &before_edit),
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
        );

        let target_edit_id = ToolCallId::new("target-edit").expect("fixture call ID");
        let same_batch = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::Assistant {
                id: MessageId(1),
                content: String::new(),
                tool_calls: vec![AgentToolCall {
                    id: target_edit_id.clone(),
                    name: "edit".into(),
                    arguments: SerializedJson::new(r#"{"files":[{"path":"lib/response.js","edits":[]}]}"#),
                }],
                stop_reason: None,
                error_message: None,
                opaque_context: Vec::new(),
            }],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(&gate, &call("same-batch-bash", "bash", "{}"), &same_batch),
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
            "an admitted edit call must not unlock a sibling in its own batch",
        );

        let mismatched_result = ContextEnvelope {
            version: 1,
            messages: vec![
                same_batch.messages[0].clone(),
                AgentMessage::ToolResult {
                    id: MessageId(2),
                    tool_call_id: ToolCallId::new("other-edit").expect("fixture call ID"),
                    tool_name: "edit".into(),
                    content: "updated another path".into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
            ],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(&gate, &call("mismatched-bash", "bash", "{}"), &mismatched_result),
            Some(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
        );

        let successful_target_edit = ContextEnvelope {
            version: 1,
            messages: vec![
                same_batch.messages[0].clone(),
                AgentMessage::ToolResult {
                    id: MessageId(2),
                    tool_call_id: target_edit_id,
                    tool_name: "edit".into(),
                    content: "updated declared target".into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
            ],
            host_messages: Vec::new(),
        };
        assert_eq!(
            pre_edit_tool_gate_violation(&gate, &call("post-edit-bash", "bash", "{}"), &successful_target_edit),
            None,
        );
    }

    #[test]
    fn source_local_task_metadata_requires_a_literal_regular_workspace_target() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-source-local-task-metadata-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(workspace.join("lib")).expect("temporary source directory");
        std::fs::write(workspace.join("lib/response.js"), "module.exports = {};")
            .expect("temporary source target");
        let task = JsonValue::parse(
            r#"{"source_local_v1":{"schema_version":"tea-coding-eval-source-local/v1","targets":["lib/response.js"]}}"#,
        )
        .expect("fixture task JSON");
        let prompt = "Repair lib/response.js without unrelated changes.";
        assert_eq!(
            source_local_task_targets(&task, prompt, &workspace).expect("valid source-local task"),
            vec!["lib/response.js"],
        );
        assert!(source_local_task_targets(&task, "Repair the response behavior.", &workspace).is_err());
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");
    }

    #[test]
    fn direct_edit_gate_composes_with_invalid_edit_recovery_without_changing_tools() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-direct-edit-gate-recovery-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (recovery, recovery_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::PrefixGuardFocusedV1,
            None,
            EditRecoveryProjectionMode::CanonicalV1,
            PreEditToolGate::disabled(),
        )
        .expect("recovery coding configuration");
        let (combined, combined_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::PrefixGuardFocusedV1,
            None,
            EditRecoveryProjectionMode::CanonicalV1,
            PreEditToolGate::direct_edit_v1(),
        )
        .expect("combined coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert_eq!(recovery_surface.system_prompt, combined_surface.system_prompt);
        assert_eq!(recovery_surface.tools, combined_surface.tools);
        assert_ne!(recovery.hooks.identity(), combined.hooks.identity());

        let malformed_error = "[tool error status: invalid_arguments]\nValidation failed for tool \"edit\":\n  - files: must have required properties files\n  - path: must not have additional properties\n  - edits: must not have additional properties";
        let context = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::ToolResult {
                id: MessageId(1),
                tool_call_id: ToolCallId::new("call-combined-malformed-edit")
                    .expect("fixture call ID"),
                tool_name: "edit".into(),
                content: malformed_error.into(),
                details: None,
                usage: Box::new(None),
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: true,
                failure: None,
            }],
            host_messages: Vec::new(),
        };
        let projected = HookSet::transform_context(combined.hooks.as_ref(), context)
            .expect("combined recovery context projection");
        let AgentMessage::ToolResult { content, .. } = &projected.messages[0] else {
            panic!("combined projection retains the malformed edit result");
        };
        assert!(content.starts_with(malformed_error));
        assert!(content.contains("one top-level `files` array"));
    }
    #[test]
    fn surface_fingerprints_use_real_sha256() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn openrouter_prompt_usage_matches_pi_cache_semantics() {
        assert_eq!(
            uncached_input_tokens(Some(282_674), Some(260_352), None),
            Some(22_322)
        );
        assert_eq!(prompt_total_tokens(Some(282_674)), Some(282_674));
        assert_eq!(uncached_input_tokens(Some(4), Some(8), Some(1)), Some(0));
        assert_eq!(prompt_total_tokens(Some(4)), Some(4));
    }

    #[test]
    fn shootout_request_timeout_matches_outer_budget_and_keeps_diagnostic_guard() {
        assert_eq!(request_timeout_seconds(1_800), 1_800);
        assert_eq!(request_timeout_seconds(0), 86_400);
    }

    #[test]
    fn wire_route_evidence_uses_only_the_observed_openrouter_route() {
        let capture = OpenRouterRequestCapture::default();
        capture.observe_response_headers(&[
            ("X-OpenRouter-Provider".into(), "DeepSeek".into()),
            ("x-openrouter-model".into(), "test-model".into()),
            ("authorization".into(), "must-not-be-retained".into()),
        ]);

        assert_eq!(
            super::returned_route_evidence(&capture),
            JsonValue::object([
                ("model", JsonValue::from("test-model")),
                ("provider", JsonValue::from("DeepSeek")),
                ("provenance", JsonValue::from("OpenRouter response header")),
            ]),
        );
    }

    #[test]
    fn outer_deadline_cancels_then_waits_for_the_drive_to_settle() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_drive = Arc::clone(&cancelled);
        let cancelled_for_deadline = Arc::clone(&cancelled);

        let (outcome, deadline_fired) = smol::block_on(super::settle_with_outer_deadline(
            Some(std::time::Duration::ZERO),
            async move {
                while !cancelled_for_drive.load(Ordering::Acquire) {
                    smol::Timer::after(std::time::Duration::from_millis(1)).await;
                }
                "durably settled"
            },
            move || cancelled_for_deadline.store(true, Ordering::Release),
        ));

        assert!(deadline_fired);
        assert_eq!(outcome, "durably settled");
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn completed_drive_disarms_outer_deadline() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_deadline = Arc::clone(&cancelled);

        let (outcome, deadline_fired) = smol::block_on(super::settle_with_outer_deadline(
            Some(std::time::Duration::from_secs(1)),
            async { "completed" },
            move || cancelled_for_deadline.store(true, Ordering::Release),
        ));

        assert!(!deadline_fired);
        assert_eq!(outcome, "completed");
        assert!(!cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn static_coding_prompt_sets_completion_oriented_agent_role() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-static-prompt-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (configuration, _) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::BuiltinV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("static coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert!(configuration.system_prompt.contains(
            "You are an expert coding assistant operating inside Tea, a coding agent harness."
        ));
        assert!(configuration.system_prompt.contains(
            "Do not finish after inspection when the requested code change is still unmade."
        ));
        assert!(configuration.system_prompt.contains(
            "Do not inspect Git history, branches, tags, reflogs, remotes, or upstream references."
        ));
        assert!(configuration.system_prompt.contains(
            "A code-change task is not complete with an empty diff."
        ));
        assert!(configuration.system_prompt.contains(
            "When a task names a source file, inspect it and then make the smallest safe edit before giving a long explanation."
        ));
    }

    #[test]
    fn no_history_static_prompt_profile_removes_the_bash_git_invitation_without_changing_tools() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-static-prompt-profile-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (builtin, builtin_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::BuiltinV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("builtin static coding configuration");
        let (no_history, no_history_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::NoHistoryV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("no-history static coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert_eq!(builtin_surface.tools, no_history_surface.tools);
        assert_eq!(builtin.hooks.identity(), no_history.hooks.identity());
        assert!(builtin.system_prompt.contains("Git, builds, and ordinary directory inspection."));
        assert!(!no_history.system_prompt.contains("Git, builds, and ordinary directory inspection."));
        assert!(no_history.system_prompt.contains(
            "workspace-local builds, and focused local validation. Use `find` for workspace discovery."
        ));
        assert_ne!(builtin_surface.system_prompt, no_history_surface.system_prompt);
        assert_ne!(
            super::host_profile_digest(&builtin),
            super::host_profile_digest(&no_history),
            "the model-visible static prompt profile must be durable"
        );
    }

    #[test]
    fn prefix_guard_static_prompt_profile_adds_only_the_diagnostic_guard_guidance() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-prefix-guard-static-prompt-profile-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (no_history, no_history_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::NoHistoryV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("no-history static coding configuration");
        let (prefix_guard, prefix_guard_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::PrefixGuardV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("prefix-guard static coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert_eq!(no_history_surface.tools, prefix_guard_surface.tools);
        assert_eq!(
            no_history_surface.static_bash_prompt_sha256,
            prefix_guard_surface.static_bash_prompt_sha256,
            "the candidate must retain the no-history Bash projection"
        );
        assert_eq!(no_history.hooks.identity(), prefix_guard.hooks.identity());
        assert!(!no_history.system_prompt.contains("Routing tasks: a RegExp substring match"));
        assert!(prefix_guard.system_prompt.contains(
            "Routing tasks: a RegExp substring match is not a mount prefix. Only trim a `layerPath` that equals the start of `path`; otherwise continue to the next layer unchanged. Put the guard at the existing trim boundary; do not expand `layerPath` or modify matching internals."
        ));
        assert_ne!(no_history_surface.system_prompt, prefix_guard_surface.system_prompt);
        assert_ne!(
            super::host_profile_digest(&no_history),
            super::host_profile_digest(&prefix_guard),
            "the model-visible prefix-guard diagnostic must be durable"
        );
    }

    #[test]
    fn focused_prefix_guard_profile_requires_the_target_only_guard_and_validator() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-focused-prefix-guard-static-prompt-profile-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (no_history, no_history_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::NoHistoryV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("no-history static coding configuration");
        let (focused, focused_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::PrefixGuardFocusedV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("focused prefix-guard static coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert_eq!(
            focused_surface.static_bash_prompt_sha256,
            no_history_surface.static_bash_prompt_sha256
        );
        assert_eq!(focused.hooks.identity(), no_history.hooks.identity());
        assert!(focused.system_prompt.contains(
            "Routing-task diagnostic: after reading `lib/router/index.js`, edit only that file. In `trim_prefix`, before the existing path-separator validation, reject a `layerPath` that is not a prefix of `path`; then run the focused validator. Do not create reproduction files or modify matching internals."
        ));
        assert_eq!(
            focused_surface.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
            vec!["read", "bash", "edit", "find"]
        );
    }

    #[test]
    fn edit_recovery_projection_adds_one_canonical_hint_only_to_the_trailing_malformed_edit_error() {
        let malformed_error = "[tool error status: invalid_arguments]\nValidation failed for tool \"edit\":\n  - files: must have required properties files\n  - path: must not have additional properties\n  - edits: must not have additional properties\n\nReceived arguments:\n{\"path\":\"lib/router/index.js\",\"edits\":[]}";
        let context = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::ToolResult {
                id: MessageId(1),
                tool_call_id: ToolCallId::new("call-malformed-edit").expect("fixture call ID"),
                tool_name: "edit".into(),
                content: malformed_error.into(),
                details: None,
                usage: Box::new(None),
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: true,
                // Rehydrated durable context has no typed failure; matching
                // must rely on the retained schema-error shape instead.
                failure: None,
            }],
            host_messages: Vec::new(),
        };
        let original = context.clone();

        let projected = super::project_invalid_edit_recovery(context);
        let AgentMessage::ToolResult { content, .. } = &projected.messages[0] else {
            panic!("projected message remains a tool result");
        };
        assert!(content.starts_with(malformed_error), "{content}");
        assert_eq!(content.matches("one top-level `files` array").count(), 1, "{content}");
        let AgentMessage::ToolResult { content, .. } = &original.messages[0] else {
            panic!("canonical message remains a tool result");
        };
        assert_eq!(content, malformed_error);
    }

    #[test]
    fn edit_recovery_projection_targets_only_the_latest_trailing_error() {
        let malformed_error = "[tool error status: invalid_arguments]\nValidation failed for tool \"edit\":\n  - files: must have required properties files\n  - path: must not have additional properties\n  - edits: must not have additional properties";
        let context = ContextEnvelope {
            version: 1,
            messages: vec![
                AgentMessage::ToolResult {
                    id: MessageId(1),
                    tool_call_id: ToolCallId::new("call-historical-malformed-edit")
                        .expect("fixture call ID"),
                    tool_name: "edit".into(),
                    content: malformed_error.into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: true,
                    failure: None,
                },
                AgentMessage::Assistant {
                    id: MessageId(2),
                    content: String::new(),
                    tool_calls: Vec::new(),
                    stop_reason: None,
                    error_message: None,
                    opaque_context: Vec::new(),
                },
                AgentMessage::ToolResult {
                    id: MessageId(3),
                    tool_call_id: ToolCallId::new("call-earlier-trailing-malformed-edit")
                        .expect("fixture call ID"),
                    tool_name: "edit".into(),
                    content: malformed_error.into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: true,
                    failure: None,
                },
                AgentMessage::ToolResult {
                    id: MessageId(4),
                    tool_call_id: ToolCallId::new("call-non-error-malformed-edit")
                        .expect("fixture call ID"),
                    tool_name: "edit".into(),
                    content: malformed_error.into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
                AgentMessage::ToolResult {
                    id: MessageId(5),
                    tool_call_id: ToolCallId::new("call-current-malformed-edit")
                        .expect("fixture call ID"),
                    tool_name: "edit".into(),
                    content: malformed_error.into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: true,
                    failure: None,
                },
            ],
            host_messages: Vec::new(),
        };

        let projected = super::project_invalid_edit_recovery(context);
        for index in [0, 2, 3] {
            let AgentMessage::ToolResult { content, .. } = &projected.messages[index] else {
                panic!("fixture message remains a tool result");
            };
            assert!(!content.contains(super::EDIT_RECOVERY_PROJECTION_HINT), "{content}");
        }
        let AgentMessage::ToolResult { content, .. } = &projected.messages[4] else {
            panic!("latest fixture message remains a tool result");
        };
        assert_eq!(
            content.matches("one top-level `files` array").count(),
            1,
            "{content}"
        );
    }

    #[test]
    fn edit_recovery_projection_preserves_static_surface_and_serializes_the_error_retry_hint() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-edit-recovery-surface-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).expect("temporary workspace");
        let (standard, standard_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::BuiltinV1,
            None,
            EditRecoveryProjectionMode::None,
            PreEditToolGate::disabled(),
        )
        .expect("standard static coding configuration");
        let (recovery, recovery_surface) = coding_configuration(
            &workspace,
            CommandEnvironment::empty(),
            true,
            StaticPromptProfile::BuiltinV1,
            None,
            EditRecoveryProjectionMode::CanonicalV1,
            PreEditToolGate::disabled(),
        )
        .expect("recovery static coding configuration");
        std::fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");

        assert_eq!(standard_surface.system_prompt, recovery_surface.system_prompt);
        assert_eq!(standard_surface.tools, recovery_surface.tools);
        assert_ne!(standard_surface.hook_identity, recovery_surface.hook_identity);
        assert_ne!(standard.hooks.identity(), recovery.hooks.identity());
        assert_ne!(
            super::host_profile_digest(&standard),
            super::host_profile_digest(&recovery),
            "a model-visible recovery policy needs a distinct durable profile"
        );

        let call_id = ToolCallId::new("call-current-malformed-edit").expect("fixture call ID");
        let malformed_error = "[tool error status: invalid_arguments]\nValidation failed for tool \"edit\":\n  - files: must have required properties files\n  - path: must not have additional properties\n  - edits: must not have additional properties";
        let context = ContextEnvelope {
            version: 1,
            messages: vec![
                AgentMessage::Assistant {
                    id: MessageId(1),
                    content: String::new(),
                    tool_calls: vec![AgentToolCall {
                        id: call_id.clone(),
                        name: "edit".into(),
                        arguments: SerializedJson::new("{\"path\":\"lib/router/index.js\",\"edits\":[]}"),
                    }],
                    stop_reason: None,
                    error_message: None,
                    opaque_context: Vec::new(),
                },
                AgentMessage::ToolResult {
                    id: MessageId(2),
                    tool_call_id: call_id,
                    tool_name: "edit".into(),
                    content: malformed_error.into(),
                    details: None,
                    usage: Box::new(None),
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: true,
                    failure: None,
                },
            ],
            host_messages: Vec::new(),
        };
        let wire = HookSet::convert_to_llm(
            &super::EditRecoveryProjectionHook,
            HookSet::transform_context(&super::EditRecoveryProjectionHook, context)
                .expect("recovery context projection"),
        )
        .expect("OpenAI-compatible context conversion");
        let wire = JsonValue::parse(&wire).expect("serialized provider context");
        let tool_message = wire
            .as_array()
            .and_then(|messages| messages.get(1))
            .expect("tool message follows the assistant")
            .as_object()
            .expect("tool message object");
        let content = tool_message
            .get("content")
            .and_then(JsonValue::as_str)
            .expect("tool message content");
        assert!(content.starts_with(malformed_error), "{content}");
        assert_eq!(content.matches("one top-level `files` array").count(), 1, "{content}");
        assert_eq!(tool_message.get("is_error"), Some(&JsonValue::Bool(true)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_tool_child_sandbox_allows_workspace_and_blocks_external_access() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-seatbelt-workspace-{}",
            std::process::id()
        ));
        let external_marker = std::env::temp_dir().join(format!(
            "tea-eval-seatbelt-external-{}",
            std::process::id()
        ));
        fs::create_dir_all(&workspace).expect("temporary workspace");
        let sandbox = ToolChildSandbox::macos_seatbelt(
            ToolChildSandboxMode::MacosSeatbeltV1,
            &workspace,
            &[],
        )
            .expect("macOS Seatbelt sandbox configuration");
        let operations = SeatbeltToolChildOperations::new(
            Arc::new(LocalCodingOperations),
            sandbox,
        );
        let command = format!(
            "printf workspace > marker; touch {external}; printf ' outside=%s' \"$?\"; test -r /etc/hosts; printf ' hosts=%s' \"$?\"; ln -s /etc/hosts outside-link; test -r outside-link; printf ' symlink=%s' \"$?\"; /bin/bash -c 'test -r /etc/hosts'; printf ' child=%s' \"$?\"",
            external = external_marker.display(),
        );
        let output = smol::block_on(operations.execute_command(
            &command,
            &workspace,
            Duration::from_secs(5),
            &CommandEnvironment::empty(),
            CancellationToken::new(),
            ToolUpdateSink::disabled(),
        ))
        .expect("sandboxed command settles");

        let external_exists = external_marker.exists();
        if external_exists {
            fs::remove_file(&external_marker).expect("remove escaped test marker");
        }
        assert_eq!(output.termination, CommandTermination::Exited { code: 0 });
        assert_eq!(fs::read(workspace.join("marker")).expect("workspace marker"), b"workspace");
        let text = String::from_utf8(output.stdout).expect("command output is UTF-8");
        assert!(text.contains("outside=1"), "{text}");
        assert!(text.contains("hosts=1"), "{text}");
        assert!(text.contains("symlink=1"), "{text}");
        assert!(text.contains("child=1"), "{text}");
        assert!(!external_exists, "sandboxed command wrote outside its workspace");
        fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_v2_blocks_git_data_without_blocking_workspace_listing_or_source_work() {
        let workspace = std::env::temp_dir().join(format!(
            "tea-eval-seatbelt-v2-workspace-{}",
            std::process::id()
        ));
        fs::create_dir_all(workspace.join(".git")).expect("temporary Git metadata directory");
        fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("temporary Git metadata");
        fs::write(workspace.join("source.js"), "module.exports = 1;\n")
            .expect("temporary source file");
        let sandbox = ToolChildSandbox::macos_seatbelt(
            ToolChildSandboxMode::MacosSeatbeltV2,
            &workspace,
            &[],
        )
        .expect("macOS Seatbelt v2 sandbox configuration");
        let operations = SeatbeltToolChildOperations::new(
            Arc::new(LocalCodingOperations),
            sandbox,
        );
        let output = smol::block_on(operations.execute_command(
            "ls -la > /dev/null 2>&1; printf ' list=%s' \"$?\"; test -r source.js; printf ' source=%s' \"$?\"; cat .git/HEAD > /dev/null 2>&1; printf ' git-read=%s' \"$?\"; printf blocked > .git/probe; printf ' git-write=%s' \"$?\"",
            &workspace,
            Duration::from_secs(5),
            &CommandEnvironment::empty(),
            CancellationToken::new(),
            ToolUpdateSink::disabled(),
        ))
        .expect("sandboxed command settles");

        assert_eq!(output.termination, CommandTermination::Exited { code: 0 });
        let text = String::from_utf8(output.stdout).expect("command output is UTF-8");
        assert!(text.contains("list=0"), "{text}");
        assert!(text.contains("source=0"), "{text}");
        assert!(text.contains("git-read=1"), "{text}");
        assert!(text.contains("git-write=1"), "{text}");
        assert!(!workspace.join(".git/probe").exists());
        fs::remove_dir_all(&workspace).expect("temporary workspace cleanup");
    }

    #[test]
    fn provider_request_observation_has_a_stable_trace_name() {
        let event = TeaEvent::Agent {
            lane_id: LaneId::main(),
            event: AgentEvent {
                run_id: RunId(1),
                sequence: EventSequence(1),
                kind: AgentEventKind::ProviderRequestObserved {
                    turn_id: TurnId(1),
                    observation: AdapterRequestObservation::default(),
                },
            },
        };
        assert_eq!(super::event_name(&event), "provider_request_observed");
    }

    #[test]
    fn snapshot_policy_identities_match_the_runtime_services() {
        let configuration = AgentConfiguration::new(
            "shootout test prompt",
            ToolRegistry::default(),
            Arc::new(OpenAiContextHook),
        );
        let model = ModelDescriptor {
            provider: "openrouter".into(),
            model: REQUIRED_MODEL.into(),
            revision: None,
        };
        let profile = model_profile(&model).expect("test model profile");
        let provider = Arc::new(OpenRouterProvider::new(OpenRouterConfig::new(
            "test-key",
            REQUIRED_MODEL,
        )));
        let services = RuntimeServices::from_agent_configuration(provider, configuration.clone())
            .model(model)
            .thinking_level(super::ThinkingLevel::High);
        let identities = services.runtime_policy_identities();
        let snapshot = snapshot_spec(&configuration, &profile, HarnessMode::Jit, identities);

        assert_eq!(snapshot.hook_bundle_digest, identities.hook_bundle_digest);
        assert_eq!(
            snapshot.compaction_policy_digest,
            identities.compaction_policy_digest
        );
        assert_eq!(
            snapshot.tool_projection_digest,
            identities.tool_projection_digest
        );
        assert_eq!(
            snapshot.failure_policy_digest,
            identities.failure_policy_digest
        );
    }
}
