//! Stable read-only recovery tools backed by immutable artifacts and session history.

use crate::HarnessError;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tea_core::error::ToolError;
use tea_core::tool::{AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolRegistry, ToolUpdateSink};
use tea_session::{
    ArtifactId, ArtifactPolicy, ArtifactStore, LaneRecord, PayloadRef, SessionEntry,
    SessionSnapshot, SessionWriter, StoredMutation,
};
use tea_protocol::{JsonNumber, JsonValue};

pub(crate) const STABLE_ARTIFACT_TOOL_NAMES: [&str; 3] = [
    "tea_artifact_read",
    "tea_artifact_search",
    "tea_history_search",
];

trait HistoryAccess: Send + Sync {
    fn snapshot(&self) -> Result<SessionSnapshot, String>;
}

struct SessionHistory<S> {
    session: Arc<Mutex<S>>,
}

impl<S> HistoryAccess for SessionHistory<S>
where
    S: SessionWriter + Send + 'static,
{
    fn snapshot(&self) -> Result<SessionSnapshot, String> {
        self.session
            .lock()
            .map_err(|_| "durable session mutex is poisoned".to_owned())?
            .snapshot()
            .map_err(|error| error.to_string())
    }
}

/// Construct the fixed Rust-owned recovery tools for one immutable epoch.
///
/// The tool names are intentionally independent of any editable harness
/// source. They return direct bounded pages and never pass their output back
/// through the spill projector, avoiding a locator-on-locator recovery loop.
pub(crate) fn stable_artifact_tools<S>(
    session: Arc<Mutex<S>>,
    artifacts: Arc<dyn ArtifactStore>,
    policy: ArtifactPolicy,
) -> Result<ToolRegistry, HarnessError>
where
    S: SessionWriter + Send + 'static,
{
    policy
        .validate()
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    if !policy.model_readable {
        return Err(HarnessError::invalid_state(
            "stable artifact tools require a model-readable artifact policy",
        ));
    }
    let history: Arc<dyn HistoryAccess> = Arc::new(SessionHistory { session });
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(ArtifactReadTool {
        artifacts: Arc::clone(&artifacts),
        policy: policy.clone(),
    }));
    tools.insert(Arc::new(ArtifactSearchTool {
        artifacts: Arc::clone(&artifacts),
        policy: policy.clone(),
    }));
    tools.insert(Arc::new(HistorySearchTool {
        history,
        artifacts,
        policy,
    }));
    Ok(tools)
}

struct ArtifactReadTool {
    artifacts: Arc<dyn ArtifactStore>,
    policy: ArtifactPolicy,
}

impl AgentTool for ArtifactReadTool {
    fn name(&self) -> &str {
        "tea_artifact_read"
    }

    fn description(&self) -> &str {
        "Read one bounded page from a durable tea-artifact locator. This returns the requested page directly and never creates another artifact locator."
    }

    fn schema(&self) -> &JsonValue {
        read_schema()
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let result = (|| {
            let object = parse_arguments(self.name(), &call)?;
            let artifact_id = artifact_id(&object, self.name())?;
            let offset = optional_u64(&object, "offset", 0, self.name())?;
            let maximum_bytes = optional_usize(
                &object,
                "maximum_bytes",
                self.policy.maximum_page_bytes,
                self.policy.maximum_page_bytes,
                self.name(),
            )?;
            let page = self
                .artifacts
                .read_page(artifact_id, offset, maximum_bytes)
                .map_err(|error| execution_error(self.name(), error.to_string()))?;
            let content = json_text(JsonValue::object([
                ("artifact_id", JsonValue::String(artifact_id.to_hex())),
                ("offset", unsigned(page.offset)),
                ("eof", JsonValue::Bool(page.eof)),
                ("page", bytes_value(&page.bytes)),
            ]))?;
            Ok(success_result(call, content))
        })();
        Box::pin(std::future::ready(result))
    }
}

struct ArtifactSearchTool {
    artifacts: Arc<dyn ArtifactStore>,
    policy: ArtifactPolicy,
}

impl AgentTool for ArtifactSearchTool {
    fn name(&self) -> &str {
        "tea_artifact_search"
    }

    fn description(&self) -> &str {
        "Search a durable tea artifact with a literal query and return bounded contexts."
    }

    fn schema(&self) -> &JsonValue {
        search_schema()
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let result = (|| {
            let object = parse_arguments(self.name(), &call)?;
            let artifact_id = artifact_id(&object, self.name())?;
            let query = required_string(&object, "query", self.name())?;
            let maximum_results = optional_usize(&object, "maximum_results", 20, 100, self.name())?;
            let context_bytes = optional_usize(
                &object,
                "context_bytes",
                self.policy.maximum_page_bytes.min(512),
                self.policy.maximum_page_bytes,
                self.name(),
            )?;
            let matches = self
                .artifacts
                .search_literal(
                    artifact_id,
                    query.as_bytes(),
                    maximum_results,
                    context_bytes,
                )
                .map_err(|error| execution_error(self.name(), error.to_string()))?;
            let content = json_text(JsonValue::object([
                ("artifact_id", JsonValue::String(artifact_id.to_hex())),
                (
                    "matches",
                    JsonValue::Array(
                        matches
                            .into_iter()
                            .map(|found| {
                                JsonValue::object([
                                    ("offset", unsigned(found.offset)),
                                    ("context", bytes_value(&found.context)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]))?;
            Ok(success_result(call, content))
        })();
        Box::pin(std::future::ready(result))
    }
}

struct HistorySearchTool {
    history: Arc<dyn HistoryAccess>,
    artifacts: Arc<dyn ArtifactStore>,
    policy: ArtifactPolicy,
}

impl AgentTool for HistorySearchTool {
    fn name(&self) -> &str {
        "tea_history_search"
    }

    fn description(&self) -> &str {
        "Search durable session history, including compacted semantic entries and referenced artifacts, with bounded results."
    }

    fn schema(&self) -> &JsonValue {
        history_schema()
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let result = (|| {
            let object = parse_arguments(self.name(), &call)?;
            let query = required_string(&object, "text", self.name())?;
            let maximum_results = optional_usize(&object, "maximum_results", 20, 100, self.name())?;
            let tool_name = optional_string(&object, "tool_name", self.name())?;
            let operation_id = optional_string(&object, "operation_id", self.name())?;
            let entry_type = optional_string(&object, "entry_type", self.name())?;
            let sequence_start = optional_u64(&object, "sequence_start", 0, self.name())?;
            let sequence_end = optional_u64(&object, "sequence_end", u64::MAX, self.name())?;
            let error_only = optional_bool(&object, "error_only", false, self.name())?;
            let snapshot = self
                .history
                .snapshot()
                .map_err(|message| execution_error(self.name(), message))?;
            let operation_by_entry = operation_by_entry(&snapshot);
            let mut matches = Vec::new();
            for entry in snapshot.entries() {
                if entry.header.seq.0 < sequence_start || entry.header.seq.0 > sequence_end {
                    continue;
                }
                let kind = entry_kind(&entry.body);
                if entry_type.as_deref().is_some_and(|expected| expected != kind) {
                    continue;
                }
                if operation_id.as_deref().is_some_and(|expected| {
                    operation_by_entry
                        .get(&entry.header.id)
                        .map_or(true, |actual| actual != expected)
                }) {
                    continue;
                }
                if tool_name.as_deref().is_some_and(|expected| !entry_has_tool(&entry.body, expected)) {
                    continue;
                }
                if error_only && !entry_is_error(&entry.body) {
                    continue;
                }
                let text = entry_search_text(&entry.body)?;
                if text.contains(&query) {
                    matches.push(JsonValue::object([
                        ("entry_id", JsonValue::String(entry.header.id.to_string())),
                        ("entry_type", JsonValue::String(kind.into())),
                        ("sequence", unsigned(entry.header.seq.0)),
                        (
                            "operation_id",
                            operation_by_entry
                                .get(&entry.header.id)
                                .cloned()
                                .map(JsonValue::String)
                                .unwrap_or(JsonValue::Null),
                        ),
                        ("preview", JsonValue::String(bounded_preview(&text, &query))),
                    ]));
                }
                if matches.len() >= maximum_results {
                    break;
                }
                for artifact_id in entry.body.artifact_references() {
                    let found = self
                        .artifacts
                        .search_literal(
                            artifact_id,
                            query.as_bytes(),
                            maximum_results.saturating_sub(matches.len()).max(1),
                            self.policy.maximum_page_bytes.min(512),
                        )
                        .map_err(|error| execution_error(self.name(), error.to_string()))?;
                    for found in found {
                        matches.push(JsonValue::object([
                            ("entry_id", JsonValue::String(entry.header.id.to_string())),
                            ("entry_type", JsonValue::String(kind.into())),
                            ("sequence", unsigned(entry.header.seq.0)),
                            ("artifact_id", JsonValue::String(artifact_id.to_hex())),
                            ("artifact_offset", unsigned(found.offset)),
                            ("preview", bytes_value(&found.context)),
                        ]));
                        if matches.len() >= maximum_results {
                            break;
                        }
                    }
                }
                if matches.len() >= maximum_results {
                    break;
                }
            }
            let content = json_text(JsonValue::object([("matches", JsonValue::Array(matches))]))?;
            Ok(success_result(call, content))
        })();
        Box::pin(std::future::ready(result))
    }
}

fn success_result(call: ToolCall, content: String) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

fn parse_arguments(
    tool: &str,
    call: &ToolCall,
) -> Result<BTreeMap<String, JsonValue>, ToolError> {
    let value = JsonValue::parse(call.arguments.as_str()).map_err(|error| {
        ToolError::InvalidArguments {
            tool: tool.into(),
            message: error.to_string(),
        }
    })?;
    match value {
        JsonValue::Object(object) => Ok(object),
        other => Err(ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("expected JSON object arguments, got {:?}", other.kind()),
        }),
    }
}

fn artifact_id(
    object: &BTreeMap<String, JsonValue>,
    tool: &str,
) -> Result<ArtifactId, ToolError> {
    let value = required_string(object, "artifact_id", tool)?;
    ArtifactId::from_hex(&value).map_err(|error| ToolError::InvalidArguments {
        tool: tool.into(),
        message: format!("artifact_id must be a BLAKE3 hex digest: {error}"),
    })
}

fn required_string(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    tool: &str,
) -> Result<String, ToolError> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("{name} must be a non-empty string"),
        })
}

fn optional_string(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    tool: &str,
) -> Result<Option<String>, ToolError> {
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("{name} must be a non-empty string when supplied"),
        }),
    }
}

fn optional_u64(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    default: u64,
    tool: &str,
) -> Result<u64, ToolError> {
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(value) => value.as_u64().ok_or_else(|| ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("{name} must be a nonnegative integer"),
        }),
    }
}

fn optional_usize(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    default: usize,
    maximum: usize,
    tool: &str,
) -> Result<usize, ToolError> {
    let value = optional_u64(object, name, default as u64, tool)?;
    let value = usize::try_from(value).map_err(|_| ToolError::InvalidArguments {
        tool: tool.into(),
        message: format!("{name} exceeds platform bounds"),
    })?;
    if value == 0 || value > maximum {
        return Err(ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("{name} must be within 1..={maximum}"),
        });
    }
    Ok(value)
}

fn optional_bool(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    default: bool,
    tool: &str,
) -> Result<bool, ToolError> {
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(value) => value.as_bool().ok_or_else(|| ToolError::InvalidArguments {
            tool: tool.into(),
            message: format!("{name} must be a boolean"),
        }),
    }
}

fn execution_error(tool: &str, message: String) -> ToolError {
    ToolError::Execution {
        tool: tool.into(),
        message,
    }
}

fn json_text(value: JsonValue) -> Result<String, ToolError> {
    value.to_json_string().map_err(|error| ToolError::Execution {
        tool: "tea durable recovery tool".into(),
        message: error.to_string(),
    })
}

fn unsigned(value: u64) -> JsonValue {
    JsonValue::Number(JsonNumber::Unsigned(value))
}

fn bytes_value(bytes: &[u8]) -> JsonValue {
    match std::str::from_utf8(bytes) {
        Ok(value) => JsonValue::object([
            ("encoding", JsonValue::String("utf8".into())),
            ("data", JsonValue::String(value.into())),
        ]),
        Err(_) => JsonValue::object([
            ("encoding", JsonValue::String("hex".into())),
            ("data", JsonValue::String(hex(bytes))),
        ]),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn operation_by_entry(snapshot: &SessionSnapshot) -> BTreeMap<tea_session::EntryId, String> {
    let mut active = None::<String>;
    let mut result = BTreeMap::new();
    for mutation in snapshot.mutations() {
        match mutation {
            StoredMutation::Record(record) => match &record.record {
                LaneRecord::OperationStarted(record) => active = Some(record.id.to_string()),
                LaneRecord::OperationFinished(record)
                    if active.as_deref() == Some(record.operation_id.as_str()) =>
                {
                    active = None;
                }
                _ => {}
            },
            StoredMutation::Entry(entry) => {
                if let Some(operation_id) = &active {
                    result.insert(entry.header.id.clone(), operation_id.clone());
                }
            }
            StoredMutation::Lane(_) | StoredMutation::Fact(_) => {}
        }
    }
    result
}

fn entry_kind(entry: &SessionEntry) -> &'static str {
    match entry {
        SessionEntry::UserMessage(_) => "user",
        SessionEntry::AssistantMessage(_) => "assistant",
        SessionEntry::ToolResult(_) => "tool_result",
        SessionEntry::Compaction(_) => "compaction",
        SessionEntry::BranchSummary(_) => "branch_summary",
        SessionEntry::ModelChanged(_) => "model_changed",
        SessionEntry::ThinkingChanged(_) => "thinking_changed",
        SessionEntry::ToolActivationChanged(_) => "tool_activation_changed",
        SessionEntry::HarnessRevisionChanged(_) => "harness_revision_changed",
        SessionEntry::PluginMemory(_) => "plugin_memory",
        SessionEntry::Custom(_) => "custom",
    }
}

fn entry_has_tool(entry: &SessionEntry, expected: &str) -> bool {
    match entry {
        SessionEntry::AssistantMessage(assistant) => {
            assistant.tool_calls.iter().any(|call| call.name == expected)
        }
        SessionEntry::ToolResult(result) => result.tool_name == expected,
        _ => false,
    }
}

fn entry_is_error(entry: &SessionEntry) -> bool {
    matches!(entry, SessionEntry::ToolResult(result) if result.is_error)
}

fn entry_search_text(entry: &SessionEntry) -> Result<String, ToolError> {
    match entry {
        SessionEntry::UserMessage(user) => Ok(user.content.clone()),
        SessionEntry::AssistantMessage(assistant) => {
            let calls = assistant
                .tool_calls
                .iter()
                .map(|call| format!("{} {}", call.name, call.arguments.to_json_string().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(format!("{}\n{calls}", assistant.content))
        }
        SessionEntry::ToolResult(result) => {
            let projection = result
                .model_projection
                .to_json_string()
                .map_err(|error| execution_error("tea_history_search", error.to_string()))?;
            let inline = match &result.full_result {
                PayloadRef::Inline(value) => value
                    .to_json_string()
                    .map_err(|error| execution_error("tea_history_search", error.to_string()))?,
                PayloadRef::Artifact { artifact_id, .. } => format!("tea-artifact://blake3/{artifact_id}"),
            };
            Ok(format!("{}\n{}\n{inline}", result.tool_name, projection))
        }
        SessionEntry::Compaction(entry) => Ok(entry.summary.clone()),
        SessionEntry::BranchSummary(entry) => Ok(entry.summary.clone()),
        SessionEntry::ModelChanged(entry) => Ok(format!(
            "{} {} {}",
            entry.provider,
            entry.model,
            entry.revision.as_deref().unwrap_or_default()
        )),
        SessionEntry::ThinkingChanged(entry) => Ok(entry.level.clone()),
        SessionEntry::ToolActivationChanged(entry) => Ok(entry.active_tool_names.join("\n")),
        SessionEntry::HarnessRevisionChanged(entry) => {
            Ok(format!("{} {}", entry.revision_id, entry.snapshot_id))
        }
        SessionEntry::PluginMemory(entry) => match &entry.content {
            PayloadRef::Inline(value) => value
                .to_json_string()
                .map_err(|error| execution_error("tea_history_search", error.to_string())),
            PayloadRef::Artifact { artifact_id, .. } => Ok(format!("tea-artifact://blake3/{artifact_id}")),
        },
        SessionEntry::Custom(entry) => match &entry.payload {
            PayloadRef::Inline(value) => value
                .to_json_string()
                .map(|value| format!("{}\n{value}", entry.type_name))
                .map_err(|error| execution_error("tea_history_search", error.to_string())),
            PayloadRef::Artifact { artifact_id, .. } => {
                Ok(format!("{}\ntea-artifact://blake3/{artifact_id}", entry.type_name))
            }
        },
    }
}

fn bounded_preview(text: &str, query: &str) -> String {
    const MAXIMUM: usize = 1_024;
    let start = text.find(query).unwrap_or_default().saturating_sub(MAXIMUM / 3);
    let mut end = start.saturating_add(MAXIMUM).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut start = start;
    while start < end && !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    text[start..end].to_owned()
}

fn read_schema() -> &'static JsonValue {
    static SCHEMA: std::sync::OnceLock<JsonValue> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        JsonValue::parse(
            r#"{"type":"object","required":["artifact_id"],"properties":{"artifact_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"maximum_bytes":{"type":"integer","minimum":1}},"additionalProperties":false}"#,
        )
        .expect("stable artifact read schema is valid")
    })
}

fn search_schema() -> &'static JsonValue {
    static SCHEMA: std::sync::OnceLock<JsonValue> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        JsonValue::parse(
            r#"{"type":"object","required":["artifact_id","query"],"properties":{"artifact_id":{"type":"string"},"query":{"type":"string"},"maximum_results":{"type":"integer","minimum":1},"context_bytes":{"type":"integer","minimum":1}},"additionalProperties":false}"#,
        )
        .expect("stable artifact search schema is valid")
    })
}

fn history_schema() -> &'static JsonValue {
    static SCHEMA: std::sync::OnceLock<JsonValue> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        JsonValue::parse(
            r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"},"tool_name":{"type":"string"},"operation_id":{"type":"string"},"entry_type":{"type":"string"},"sequence_start":{"type":"integer","minimum":0},"sequence_end":{"type":"integer","minimum":0},"error_only":{"type":"boolean"},"maximum_results":{"type":"integer","minimum":1}},"additionalProperties":false}"#,
        )
        .expect("stable history search schema is valid")
    })
}
