//! Provider-backed durable Tea adapter for the explicit Pi shootout.
//!
//! The executable accepts a credential only from its final `vault ... --`
//! process boundary. Coding-tool children receive the separate `--shell-env`
//! allowlist and never inherit that credential.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tea_core::agent::AgentConfiguration;
use tea_core::coding::{CommandEnvironment, DefaultCodingTools, PiDefaultCodingProfile};
use tea_core::event::AgentEventKind;
use tea_core::harness::{
    HarnessActor, HarnessRepository, HarnessResolver, HarnessResourceLimits, HarnessSnapshotSpec,
    ModelHarnessProfile, SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode,
    ToolPresentationDescriptor,
};
use tea_core::hooks::HookSet;
use tea_core::runtime::{HarnessEvent, HarnessIdentity, RuntimeServices, SessionRuntime, TeaEvent};
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_core::tool::ToolExecutionMode;
use tea_luau::LuauExtensionEngine;
use tea_protocol::{JsonNumber, JsonValue};
use tea_providers::openai::OpenAiContextHook;
use tea_providers::openrouter::{OpenRouterConfig, OpenRouterProvider};
use tea_session::{
    CanonicalHashWriter, Digest, DurabilityMode, EntryId, HarnessRevisionChangedEntry,
    JsonlSession, LaneId, ModelChangedEntry, ProvisionedEntry, SessionEntry, SessionHeader,
    SessionId, SessionWriter, ThinkingChangedEntry,
};

const RESULT_SCHEMA: &str = "tea-coding-eval-result/v2";
const REQUIRED_MODEL: &str = "poolside/laguna-s-2.1:free";
const REQUIRED_THINKING: &str = "high";
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
    shell_environment: CommandEnvironment,
    shell_environment_json: JsonValue,
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
            shell_environment,
            shell_environment_json: JsonValue::Object(normalized_shell),
        })
    }
}

fn read_json(path: &Path, label: &str) -> Result<JsonValue, String> {
    let source = fs::read_to_string(path).map_err(|_| format!("cannot read evaluation {label}"))?;
    JsonValue::parse(&source).map_err(|_| format!("evaluation {label} is not JSON"))
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
        hook_bundle_digest: Digest::from_bytes("tea-pi-shootout-openai-context-hook-v1"),
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits {
            source_bytes: 16 * 1024,
            ..HarnessResourceLimits::default()
        },
        compaction_policy_digest: Digest::from_bytes("tea-pi-shootout-no-compaction-v1"),
        tool_projection_digest: Digest::from_bytes("tea-core-recoverable-projection-v1"),
        failure_policy_digest: Digest::from_bytes("tea-core-tool-failure-policy-v1"),
    }
}

fn event_name(event: &TeaEvent) -> &'static str {
    match event {
        TeaEvent::Agent(event) => match &event.kind {
            AgentEventKind::AgentStart => "agent_start",
            AgentEventKind::TurnStart { .. } => "turn_start",
            AgentEventKind::MessageStart { .. } => "message_start",
            AgentEventKind::MessageUpdate { .. } => "message_update",
            AgentEventKind::MessageEnd { .. } => "message_end",
            AgentEventKind::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEventKind::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEventKind::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEventKind::CompactionStart { .. } => "compaction_start",
            AgentEventKind::CompactionEnd { .. } => "compaction_end",
            AgentEventKind::AgentEnd { .. } => "agent_end",
            _ => "runtime_event",
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
    profile: &'a PiDefaultCodingProfile,
    snapshot: &'a tea_core::harness::HarnessSnapshotV1,
    final_snapshot_id: String,
    terminal: (&'a str, Option<&'a str>),
    agent_ms: u64,
    final_text: String,
    trace: Vec<JsonValue>,
    candidate_count: u64,
    candidate_id: Option<String>,
    changed_surfaces: Vec<String>,
    hypothesis: Option<String>,
    activated: bool,
}

fn result_json(input: ResultJsonInput<'_>) -> JsonValue {
    let usage = input.provider.usage_snapshot();
    let input_tokens = usage.input_tokens;
    let output = usage.output_tokens;
    let generation = input_tokens
        .zip(output)
        .map(|(input, output)| input.saturating_add(output));
    let prompt = input
        .profile
        .system_prompt_for_workspace(&input.args.workspace);
    let normalized_prompt = prompt.replace(
        &input.args.workspace.to_string_lossy().replace('\\', "/"),
        "{WORKSPACE}",
    );
    let tools = JsonValue::Array(
        input
            .profile
            .tool_definitions()
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
    let turns = input
        .trace
        .iter()
        .filter(|event| event.get("type").and_then(JsonValue::as_str) == Some("turn_start"))
        .count() as u64;
    let tool_calls = input
        .trace
        .iter()
        .filter(|event| {
            event.get("type").and_then(JsonValue::as_str) == Some("tool_execution_start")
        })
        .count() as u64;
    let compactions = input
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
                        ("temperature", JsonValue::Null),
                        ("seed", JsonValue::Null),
                        ("source", JsonValue::from("provider-default")),
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
                    "active_tools",
                    JsonValue::Array(
                        input
                            .profile
                            .active_tool_names()
                            .map(|name| JsonValue::from(name.to_owned()))
                            .collect(),
                    ),
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
        (
            "counts",
            JsonValue::object([
                ("turns", JsonValue::from(turns)),
                ("provider_requests", JsonValue::Null),
                ("tool_calls", JsonValue::from(tool_calls)),
                ("retries", JsonValue::from(0_u64)),
                ("compactions", JsonValue::from(compactions)),
            ]),
        ),
        (
            "usage",
            JsonValue::object([
                ("input", optional_u64(input_tokens)),
                ("output", optional_u64(output)),
                ("generation", optional_u64(generation)),
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
    if actual != ["read", "bash", "edit", "write"] {
        return Err("active tool list must be read/bash/edit/write".into());
    }
    let api_key = env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY must be supplied by vault".to_owned())?;
    let provider_config = match args.max_output_tokens {
        Some(maximum) => {
            OpenRouterConfig::new(&api_key, args.model.clone()).with_max_tokens(maximum)
        }
        None => OpenRouterConfig::new(api_key, args.model.clone()),
    };
    let provider = Arc::new(OpenRouterProvider::new(provider_config));
    let tools = DefaultCodingTools::new(&args.workspace)
        .map_err(|error| error.to_string())?
        .with_environment(args.shell_environment.clone());
    let profile = PiDefaultCodingProfile::pinned_default().map_err(|error| error.to_string())?;
    let registry = tools.registry();
    profile
        .validate_registry(&registry)
        .map_err(|error| error.to_string())?;
    let configuration = AgentConfiguration::new(
        profile.system_prompt_for_workspace(tools.workspace().as_path()),
        registry,
        Arc::new(OpenAiContextHook) as Arc<dyn HookSet>,
    );
    let model = ModelDescriptor {
        provider: "openrouter".into(),
        model: args.model.clone(),
        revision: None,
    };
    let model_profile = model_profile(&model)?;
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
    let services = RuntimeServices::from_agent_configuration(provider.clone(), configuration)
        .model(model)
        .thinking_level(args.thinking);
    let manager = Arc::new(
        HarnessResolver::new(repository, services, Default::default())
            .self_extension_mode(args.harness_mode.extension_mode()),
    );
    let harness = SessionRuntime::new_with_artifact_store(
        session,
        Arc::clone(&manager),
        HarnessIdentity::new(
            revision.revision_id,
            snapshot.id.clone(),
            model_profile.profile_id,
        ),
        artifacts,
    )
    .map_err(|error| error.to_string())?
    .rollover_budget(1);
    let subscription = harness
        .subscribe_events()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let outcome = smol::block_on(async {
        if args.harness_mode == HarnessMode::Jit {
            harness.run_authoring_prompt(prompt).await
        } else {
            harness.run_prompt(prompt).await
        }
    });
    let agent_ms = started.elapsed().as_millis() as u64;
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
    let mut trace = Vec::new();
    let mut candidate_count = 0_u64;
    let mut candidate_id = None;
    let mut changed_surfaces = Vec::new();
    let mut activated = false;
    let mut final_snapshot_id = snapshot.id.to_string();
    while let Ok(event) = subscription.try_recv() {
        let name = event_name(&event);
        match &event {
            TeaEvent::Harness(HarnessEvent::CandidateStaged {
                candidate_id: staged,
                ..
            }) => {
                candidate_count = candidate_count.saturating_add(1);
                candidate_id = Some(staged.to_string());
            }
            TeaEvent::Harness(HarnessEvent::SnapshotActivated {
                snapshot_id: activated_snapshot,
                changed_surfaces: surfaces,
                ..
            }) => {
                activated = true;
                final_snapshot_id = activated_snapshot.to_string();
                changed_surfaces = surfaces
                    .iter()
                    .map(|surface| format!("{surface:?}").to_ascii_lowercase())
                    .collect();
            }
            _ => {}
        }
        trace.push(JsonValue::object([
            ("seq", JsonValue::from(trace.len() as u64 + 1)),
            ("type", JsonValue::from(name)),
        ]));
    }
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
    let system_prompt = profile.system_prompt_for_workspace(&args.workspace);
    fs::write(args.evidence_dir.join("system-prompt.txt"), &system_prompt)
        .map_err(|error| error.to_string())?;
    let tool_surface = JsonValue::Array(
        profile
            .tool_definitions()
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
        profile: &profile,
        snapshot: &snapshot,
        final_snapshot_id,
        terminal,
        agent_ms,
        final_text,
        trace,
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
    use super::{HarnessMode, REQUIRED_MODEL, sha256};
    #[test]
    fn requested_laguna_s_model_is_not_the_xs_model() {
        assert_eq!(REQUIRED_MODEL, "poolside/laguna-s-2.1:free");
        assert_ne!(REQUIRED_MODEL, "poolside/laguna-xs-2.1:free");
        assert_eq!(HarnessMode::parse("jit").unwrap().name(), "jit");
    }
    #[test]
    fn surface_fingerprints_use_real_sha256() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
