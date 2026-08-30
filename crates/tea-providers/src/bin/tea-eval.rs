//! Provider-backed durable Tea adapter for the explicit Pi shootout.
//!
//! The executable accepts a credential only from its final `vault ... --`
//! process boundary. Coding-tool children receive the separate `--shell-env`
//! allowlist and never inherit that credential.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tea_core::agent::AgentConfiguration;
use tea_core::coding::{
    CodingHost, CommandEnvironment, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
    WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
};
use tea_core::event::AgentEventKind;
use tea_core::harness::extension::{
    ExtensionCapabilityBindings, ExtensionEngine, ExtensionLimits, ExtensionMemoryCollector,
    ExtensionToolLimits,
};
use tea_core::harness::{
    HarnessActor, HarnessRepository, HarnessResolver, HarnessResourceLimits, HarnessSnapshotSpec,
    ModelHarnessProfile, SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode,
    ToolPresentationDescriptor,
};
use tea_core::hooks::HookSet;
use tea_core::runtime::{
    HarnessEvent, HarnessIdentity, RuntimePolicyIdentities, RuntimeServices, SessionSupervisor,
    SessionSupervisorInput, TeaEvent,
};
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_core::tool::{ToolExecutionMode, ToolRegistry};
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

const RESULT_SCHEMA: &str = "tea-coding-eval-result/v3";
const REQUIRED_MODEL: &str = "deepseek/deepseek-v4-flash-0731";
const REQUIRED_THINKING: &str = "high";
const SHOOTOUT_TEMPERATURE: f64 = 0.0;
const SHOOTOUT_SEED: u64 = 20260829;
const SHOOTOUT_PROVIDER_MAX_RETRIES: u32 = 3;
/// Keep the provider request from expiring before the outer shootout budget.
/// A zero outer budget is an uncapped diagnostic, but the HTTP transport still
/// needs a finite deadline to avoid an OS-level socket hanging forever.
const DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS: u64 = 86_400;
/// Evaluation-only guidance that mirrors the concise parts of Pi's native
/// coding prompt. It reduces avoidable exploratory turns without changing the
/// closed capability set or any core runtime policy.
const STATIC_CODING_GUIDELINES: &str = "Guidelines:\n- Be concise in responses and show file paths clearly.\n- Use `read` to inspect known files instead of using `bash` with cat or sed.\n- Use `find` for workspace discovery; never search from `/`, inspect home/cache directories, or inspect outside the workspace.\n- Keep bash commands bounded and workspace-local; do not run network, package-install, or upstream/repository-history probes.\n- Honor the requested null/undefined semantics; when an optional nullish value should be ignored, guard it before validation or calculation.\n- Batch independent inspections in one tool turn when practical.\n- Use `edit` for precise changes, then run the relevant validator and stop once the fix is verified.\n- For targeted edits, use `files[].edits[]` with exact `oldText` and `newText`; batch independent edits in one call.\n- Keep edit matches small and unique; do not repeat unchanged probes.\n- Once the relevant check passes, finish without further exploratory inspection.";
const JIT_ADDENDUM: &str = "Task-local harness adaptation is available but optional.\n\nFirst inspect the task and repository using the normal coding tools. Use NoChange unless you have concrete evidence that one bounded harness change is likely to improve this task.\n\nYou may stage at most one task-local harness candidate. It may alter only currently supported prompt, tool-presentation, hook, context, memory-selection, failure-policy, or compaction-policy surfaces. It cannot grant new authority, change the provider or model, access hidden validators, use subagents, or add a web-research tool.\n\nA candidate must include observed task or failure evidence, a root-cause hypothesis, expected effect, regression risk, and the harness surfaces changed. If it activates, continue solving the same task under the new immutable harness revision. All adaptation time and model usage count toward the task result.";

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
    thinking: ThinkingLevel,
    max_output_tokens: Option<u64>,
    outer_timeout_seconds: u64,
    provider_routing: JsonValue,
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
            "--thinking-level",
            "--max-output-tokens",
            "--outer-timeout-seconds",
            "--provider-routing-json",
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
        let attempt_path_replacements =
            std::iter::once((take("--workspace")?, "{WORKSPACE}".to_owned()))
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
            workspace: PathBuf::from(take("--workspace")?),
            capabilities_json: PathBuf::from(take("--capabilities-json")?),
            result_json: PathBuf::from(take("--result-json")?),
            evidence_dir: PathBuf::from(take("--evidence-dir")?),
            attempt_id: take("--attempt-id")?,
            baseline_id,
            harness_mode,
            thinking: ThinkingLevel::High,
            max_output_tokens,
            outer_timeout_seconds,
            provider_routing,
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

fn request_timeout_seconds(outer_timeout_seconds: u64) -> u64 {
    if outer_timeout_seconds == 0 {
        DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS
    } else {
        outer_timeout_seconds
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
    let private = JsonValue::object([
        (
            "schema_version",
            JsonValue::from("tea-pi-wire-request-evidence/v1"),
        ),
        ("requests", JsonValue::Array(requests.clone())),
        (
            "returned_route",
            JsonValue::object([
                ("model", JsonValue::Null),
                ("provider", JsonValue::Null),
                ("provenance", JsonValue::Null),
            ]),
        ),
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
        (
            "returned_route",
            JsonValue::object([
                ("model", JsonValue::Null),
                ("provider", JsonValue::Null),
                ("provenance", JsonValue::Null),
            ]),
        ),
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

/// The resolved, provider-visible coding surface used by one evaluation run.
///
/// This is derived from the checked-in Luau builtins rather than a Rust tool
/// factory or profile copy. It is retained only to record the exact surface in
/// evaluation evidence after the run has settled.
struct CodingSurface {
    system_prompt: String,
    tools: Vec<tea_core::tool::ToolDefinition>,
}

fn coding_configuration(
    workspace: &Path,
    environment: CommandEnvironment,
    include_static_guidelines: bool,
) -> Result<(AgentConfiguration, CodingSurface), String> {
    let limits = ExtensionLimits {
        max_source_bytes: 64 * 1024,
        max_memory_bytes: 1024 * 1024,
        max_interrupt_checks: 10_000,
    };
    let engine = LuauExtensionEngine;
    let host = CodingHost::new(workspace)
        .map_err(|error| error.to_string())?
        .with_environment(environment);
    let mut prompt_sections = Vec::new();
    let mut tools = ToolRegistry::default();
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
            PROCESS_CAPABILITY_V1 => host.process_capability(),
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
                Arc::new(OpenAiContextHook) as Arc<dyn HookSet>,
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .map_err(|error| error.to_string())?;
        prompt_sections.extend(descriptor.prompt_sections);
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
    let prompt_guidelines = include_static_guidelines
        .then_some(STATIC_CODING_GUIDELINES)
        .unwrap_or("");
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
    };
    Ok((
        AgentConfiguration::new(system_prompt, tools, Arc::new(OpenAiContextHook)),
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
                        (
                            "provider_retry",
                            JsonValue::object([
                                ("enabled", JsonValue::Bool(true)),
                                (
                                    "max_retries",
                                    JsonValue::from(u64::from(SHOOTOUT_PROVIDER_MAX_RETRIES)),
                                ),
                            ]),
                        ),
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
        ("trace", JsonValue::Array(input.trace)),
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
    let (configuration, surface) = coding_configuration(
        &args.workspace,
        args.shell_environment.clone(),
        args.harness_mode == HarnessMode::Static,
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
    let outcome = smol::block_on(async {
        if args.harness_mode == HarnessMode::Jit {
            harness.run_authoring_prompt(prompt).await
        } else {
            harness.run_root_prompt(prompt).await
        }
    });
    let agent_ms = started.elapsed().as_millis() as u64;
    collecting.store(false, Ordering::Release);
    let events = collector
        .join()
        .map_err(|_| "event collector thread panicked".to_owned())?;
    let durable = harness.snapshot().map_err(|error| error.to_string())?;
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
    let terminal = match &outcome {
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
    use std::sync::Arc;

    use tea_core::event::{AgentEvent, AgentEventKind, EventSequence};
    use tea_core::runtime::TeaEvent;
    use tea_core::scheduler::AdapterRequestObservation;
    use tea_core::state::{RunId, TurnId};
    use tea_core::tool::ToolRegistry;
    use tea_session::LaneId;

    use super::{
        AgentConfiguration, HarnessMode, ModelDescriptor, OpenAiContextHook, OpenRouterConfig,
        OpenRouterProvider, REQUIRED_MODEL, RuntimeServices, model_profile, prompt_total_tokens,
        request_timeout_seconds, sha256, snapshot_spec, uncached_input_tokens,
    };
    #[test]
    fn requested_deepseek_model_is_pinned() {
        assert_eq!(REQUIRED_MODEL, "deepseek/deepseek-v4-flash-0731");
        assert_ne!(REQUIRED_MODEL, "poolside/laguna-s-2.1:free");
        assert_eq!(HarnessMode::parse("jit").unwrap().name(), "jit");
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
