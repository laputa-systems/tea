//! JSON Lines encoding for trace events.

use super::{compaction_stage_name, end_reason_name, event_schema_version, event_type};
use crate::event::{
    CacheEvidence, Compaction, CompactionStage, EndReason, EpisodeEnd, EpisodeHeader, Tool,
    TraceEvent, TraceProvenance, Turn,
};
use std::collections::BTreeMap;
use std::fmt;

/// Error returned when a JSONL trace record is not exactly the v1 wire form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonTraceDecodeError {
    message: String,
}

impl JsonTraceDecodeError {
    fn new(message: impl Into<String>) -> Self { Self { message: message.into() } }
}

impl fmt::Display for JsonTraceDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for JsonTraceDecodeError {}

/// Decode one canonical JSONL record. Whitespace, unknown fields, alternate
/// numeric forms, and extra Factory records are rejected by canonical replay.
pub fn decode_json_line(line: &str) -> Result<TraceEvent, JsonTraceDecodeError> {
    let value = miniserde::json::from_str::<miniserde::json::Value>(line)
        .map_err(|_| JsonTraceDecodeError::new("invalid JSON trace record"))?;
    let object = match value { miniserde::json::Value::Object(value) => value, _ => return Err(JsonTraceDecodeError::new("trace record is not an object")) };
    let schema = take_u64(&object, "schema_version")?;
    if schema != u64::from(crate::TRACE_SCHEMA_VERSION) { return Err(JsonTraceDecodeError::new("unsupported trace schema version")); }
    let kind = take_str(&object, "type")?;
    let event = match kind.as_str() {
        "episode_header" => parse_header(&object)?,
        "turn" => parse_turn(&object)?,
        "tool" => parse_tool(&object)?,
        "compaction" => parse_compaction(&object)?,
        "episode_end" => parse_end(&object)?,
        _ => return Err(JsonTraceDecodeError::new("unknown trace record type")),
    };
    let mut canonical = String::new();
    write_json_event(&mut canonical, &event);
    if canonical != line && legacy_header_without_agent_id(&event).as_deref() != Some(line) {
        return Err(JsonTraceDecodeError::new("non-canonical or extra trace fields"));
    }
    Ok(event)
}

/// Decode a complete JSONL episode, enforcing header/terminal lifecycle order.
pub fn decode_jsonl(input: &str) -> Result<Vec<TraceEvent>, JsonTraceDecodeError> {
    if input.is_empty() { return Err(JsonTraceDecodeError::new("empty trace")); }
    let mut events = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() { continue; }
        events.push(decode_json_line(line)?);
    }
    if !matches!(events.first(), Some(TraceEvent::EpisodeHeader(_))) ||
       !matches!(events.last(), Some(TraceEvent::EpisodeEnd(_))) ||
       events.iter().skip(1).any(|event| matches!(event, TraceEvent::EpisodeHeader(_))) ||
       events.iter().take(events.len().saturating_sub(1)).any(TraceEvent::is_terminal) {
        return Err(JsonTraceDecodeError::new("invalid episode lifecycle"));
    }
    Ok(events)
}

type Obj = std::collections::BTreeMap<String, miniserde::json::Value>;
fn take<'a>(o: &'a Obj, n: &str) -> Result<&'a miniserde::json::Value, JsonTraceDecodeError> { o.get(n).ok_or_else(|| JsonTraceDecodeError::new(format!("missing field {n}"))) }
fn take_str(o: &Obj, n: &str) -> Result<String, JsonTraceDecodeError> { match take(o,n)? { miniserde::json::Value::String(v) => Ok(v.clone()), _ => Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_opt_str(o: &Obj, n: &str) -> Result<Option<String>, JsonTraceDecodeError> { match o.get(n) { None|Some(miniserde::json::Value::Null) => Ok(None), Some(miniserde::json::Value::String(v)) => Ok(Some(v.clone())), Some(_) => Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_u64(o: &Obj, n: &str) -> Result<u64, JsonTraceDecodeError> { match take(o,n)? { miniserde::json::Value::Number(miniserde::json::Number::U64(v)) => Ok(*v), _ => Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_opt_u64(o: &Obj, n: &str) -> Result<Option<u64>, JsonTraceDecodeError> { match o.get(n) { None|Some(miniserde::json::Value::Null) => Ok(None), Some(miniserde::json::Value::Number(miniserde::json::Number::U64(v))) => Ok(Some(*v)), Some(_) => Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_opt_bool(o: &Obj, n: &str) -> Result<Option<bool>, JsonTraceDecodeError> { match o.get(n) { None|Some(miniserde::json::Value::Null) => Ok(None), Some(miniserde::json::Value::Bool(v)) => Ok(Some(*v)), Some(_) => Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_map_str(o: &Obj, n: &str) -> Result<BTreeMap<String,String>, JsonTraceDecodeError> { match take(o,n)? { miniserde::json::Value::Object(m) => m.iter().map(|(k,v)| match v { miniserde::json::Value::String(s)=>Ok((k.clone(),s.clone())), _=>Err(JsonTraceDecodeError::new("map value has wrong type")) }).collect(), _=>Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }
fn take_strings(o: &Obj, n: &str) -> Result<Vec<String>, JsonTraceDecodeError> { match take(o,n)? { miniserde::json::Value::Array(a) => a.iter().map(|v| match v { miniserde::json::Value::String(s)=>Ok(s.clone()), _=>Err(JsonTraceDecodeError::new("array value has wrong type")) }).collect(), _=>Err(JsonTraceDecodeError::new(format!("field {n} has wrong type"))) } }

fn parse_header(o: &Obj) -> Result<TraceEvent, JsonTraceDecodeError> {
    let mut h=EpisodeHeader::new(take_str(o,"episode_id")?); h.metadata=take_map_str(o,"metadata")?; h.started_at_ms=take_opt_u64(o,"started_at_ms")?; h.provenance=parse_provenance(o)?; Ok(h.into())
}
fn parse_provenance(o: &Obj) -> Result<Option<TraceProvenance>, JsonTraceDecodeError> { match take(o,"provenance")? { miniserde::json::Value::Null=>Ok(None), miniserde::json::Value::Object(p)=>Ok(Some(TraceProvenance { session_id:take_opt_str(p,"session_id")?, lane_id:take_opt_str(p,"lane_id")?, agent_id:take_opt_str(p,"agent_id")?, operation_id:take_opt_str(p,"operation_id")?, epoch_id:take_opt_str(p,"epoch_id")?, core_run_id:take_opt_str(p,"core_run_id")?, harness_snapshot_id:take_opt_str(p,"harness_snapshot_id")?, harness_revision_id:take_opt_str(p,"harness_revision_id")?, model_harness_profile_id:take_opt_str(p,"model_harness_profile_id")?, experiment_id:take_opt_str(p,"experiment_id")? })), _=>Err(JsonTraceDecodeError::new("field provenance has wrong type")) } }
fn parse_turn(o: &Obj) -> Result<TraceEvent, JsonTraceDecodeError> { let mut t=Turn::new(u32::try_from(take_u64(o,"index")?).map_err(|_|JsonTraceDecodeError::new("index out of range"))?,take_str(o,"input")?); t.output=take_opt_str(o,"output")?; t.stop_reason=take_opt_str(o,"stop_reason")?; t.cache_evidence=parse_cache(o)?; Ok(t.into()) }
fn parse_tool(o: &Obj) -> Result<TraceEvent, JsonTraceDecodeError> { let mut t=Tool::new(u32::try_from(take_u64(o,"turn_index")?).map_err(|_|JsonTraceDecodeError::new("turn_index out of range"))?,take_str(o,"call_id")?,take_str(o,"name")?,take_str(o,"input")?); t.output=take_opt_str(o,"output")?; t.error=take_opt_str(o,"error")?; Ok(t.into()) }
fn parse_end(o: &Obj) -> Result<TraceEvent, JsonTraceDecodeError> { let reason=match take_str(o,"reason")?.as_str() {"completed"=>EndReason::Completed,"cancelled"=>EndReason::Cancelled,"failed"=>EndReason::Failed,"aborted"=>EndReason::Aborted,v=>EndReason::Other(v.to_owned())}; Ok(EpisodeEnd { reason,error:take_opt_str(o,"error")?,finished_at_ms:take_opt_u64(o,"finished_at_ms")? }.into()) }
fn parse_cache(o: &Obj) -> Result<Option<CacheEvidence>, JsonTraceDecodeError> {
    let v=take(o,"cache_evidence")?; let miniserde::json::Value::Object(p)=v else { if matches!(v, miniserde::json::Value::Null){return Ok(None)} return Err(JsonTraceDecodeError::new("field cache_evidence has wrong type")) };
    let mut c=CacheEvidence::default(); c.continuity=take_opt_str(p,"continuity")?; c.cache_domain_fingerprint=take_opt_u64(p,"cache_domain_fingerprint")?; c.changed_cache_domain_components=take_strings(p,"changed_cache_domain_components")?; c.context_bytes=take_opt_u64(p,"context_bytes")?; c.common_context_prefix_bytes=take_opt_u64(p,"common_context_prefix_bytes")?; c.common_context_prefix_ratio_millionths=take_opt_u64(p,"common_context_prefix_ratio_millionths")?.map(|v|u32::try_from(v)).transpose().map_err(|_|JsonTraceDecodeError::new("ratio out of range"))?; c.context_projection_changed=take_opt_bool(p,"context_projection_changed")?; c.context_fingerprint=take_opt_u64(p,"context_fingerprint")?; c.system_prompt_fingerprint=take_opt_u64(p,"system_prompt_fingerprint")?; c.tool_definition_fingerprint=take_opt_u64(p,"tool_definition_fingerprint")?; c.tool_order_fingerprint=take_opt_u64(p,"tool_order_fingerprint")?; c.model_fingerprint=take_opt_u64(p,"model_fingerprint")?; c.thinking_fingerprint=take_opt_u64(p,"thinking_fingerprint")?; c.deterministic_common_prefix_bytes=take_opt_u64(p,"deterministic_common_prefix_bytes")?; c.deterministic_common_prefix_tokens_estimate=take_opt_u64(p,"deterministic_common_prefix_tokens_estimate")?; c.provider_cache_read_tokens=take_opt_u64(p,"provider_cache_read_tokens")?; c.provider_cache_write_tokens=take_opt_u64(p,"provider_cache_write_tokens")?; c.serialized_request_bytes=take_opt_u64(p,"serialized_request_bytes")?; c.adapter_cache_domain_fingerprint=take_opt_u64(p,"adapter_cache_domain_fingerprint")?; c.provider_surface_digest=take_opt_str(p,"provider_surface_digest")?;
    match take(p,"adapter_cache_domain_components")? { miniserde::json::Value::Object(m)=>{ c.adapter_cache_domain_components=m.iter().map(|(k,v)|match v {miniserde::json::Value::Number(miniserde::json::Number::U64(n))=>Ok((k.clone(),*n)), _=>Err(JsonTraceDecodeError::new("map value has wrong type"))}).collect::<Result<_,_>>()? }, _=>return Err(JsonTraceDecodeError::new("field adapter_cache_domain_components has wrong type")) }
    Ok(Some(c))
}
fn parse_compaction(o: &Obj) -> Result<TraceEvent, JsonTraceDecodeError> {
    let stage=match take_str(o,"stage")?.as_str() {"started"=>CompactionStage::Started,"source_selected"=>CompactionStage::SourceSelected,"request_prepared"=>CompactionStage::RequestPrepared,"provider_usage_observed"=>CompactionStage::ProviderUsageObserved,"replacement_proposed"=>CompactionStage::ReplacementProposed,"terminal"=>CompactionStage::Terminal,"post_compaction_request_observed"=>CompactionStage::PostCompactionRequestObserved,_=>return Err(JsonTraceDecodeError::new("unknown compaction stage"))};
    let mut c=Compaction::new(take_str(o,"compaction_id")?,stage);
    macro_rules! s {($f:ident)=>{c.$f=take_opt_str(o,stringify!($f))?;};} macro_rules! n {($f:ident)=>{c.$f=take_opt_u64(o,stringify!($f))?.map(|v|usize::try_from(v)).transpose().map_err(|_|JsonTraceDecodeError::new("number out of range"))?;};} macro_rules! u {($f:ident)=>{c.$f=take_opt_u64(o,stringify!($f))?.map(|v|u32::try_from(v)).transpose().map_err(|_|JsonTraceDecodeError::new("number out of range"))?;};} macro_rules! q {($f:ident)=>{c.$f=take_opt_u64(o,stringify!($f))?;};} macro_rules! b {($f:ident)=>{c.$f=take_opt_bool(o,stringify!($f))?;};}
    s!(trigger);s!(reason);s!(phase);s!(strategy_id);u!(strategy_schema_version);s!(request_layout);q!(prompt_fingerprint);q!(source_history_revision);u!(attempt);u!(automatic_ordinal);u!(overflow_retry_ordinal);b!(retry_provider_request);n!(source_message_count);n!(source_message_bytes);n!(retained_message_count);n!(retained_suffix_bytes);n!(tool_result_bytes);n!(compactor_context_bytes);n!(compactor_tool_count);b!(tools_execution_prohibited);b!(source_is_active_context_prefix);n!(replacement_message_count);n!(replacement_bytes);q!(estimated_context_tokens_after);q!(headroom_tokens);b!(structural_validation_passed);b!(retained_suffix_exact);b!(source_generation_matches);q!(provider_input_tokens);q!(provider_output_tokens);q!(provider_cache_read_tokens);q!(provider_cache_write_tokens);n!(serialized_request_bytes);q!(cache_domain_fingerprint);s!(terminal_outcome);u!(post_compaction_turn_index); Ok(c.into())
}

pub(super) fn write_json_event(output: &mut String, event: &TraceEvent) {
    write_json_event_with_agent_id(output, event, true);
}

fn write_json_event_with_agent_id(
    output: &mut String,
    event: &TraceEvent,
    include_agent_id: bool,
) {
    output.push('{');
    json_field_name(output, "schema_version");
    output.push_str(&event_schema_version(event).to_string());
    output.push(',');
    json_field_name(output, "type");
    json_string(output, event_type(event));
    match event {
        TraceEvent::EpisodeHeader(header) => {
            write_json_header(output, header, include_agent_id)
        }
        TraceEvent::Turn(turn) => write_json_turn(output, turn),
        TraceEvent::Tool(tool) => write_json_tool(output, tool),
        TraceEvent::Compaction(compaction) => write_json_compaction(output, compaction),
        TraceEvent::EpisodeEnd(end) => write_json_end(output, end),
    }
    output.push('}');
}

fn write_json_header(output: &mut String, header: &EpisodeHeader, include_agent_id: bool) {
    output.push(',');
    json_field_string(output, "episode_id", &header.episode_id);
    output.push(',');
    json_field_name(output, "metadata");
    json_map(output, &header.metadata);
    output.push(',');
    json_field_optional_number(output, "started_at_ms", header.started_at_ms);
    output.push(',');
    json_field_name(output, "provenance");
    json_optional_provenance(output, header.provenance.as_ref(), include_agent_id);
}

fn write_json_turn(output: &mut String, turn: &Turn) {
    output.push(',');
    json_field_name(output, "index");
    output.push_str(&turn.index.to_string());
    output.push(',');
    json_field_string(output, "input", &turn.input);
    output.push(',');
    json_field_optional_string(output, "output", turn.output.as_deref());
    output.push(',');
    json_field_optional_string(output, "stop_reason", turn.stop_reason.as_deref());
    output.push(',');
    json_field_name(output, "cache_evidence");
    json_optional_cache_evidence(output, turn.cache_evidence.as_ref());
}

fn json_optional_provenance(
    output: &mut String,
    provenance: Option<&TraceProvenance>,
    include_agent_id: bool,
) {
    let Some(provenance) = provenance else {
        output.push_str("null");
        return;
    };
    output.push('{');
    let mut fields = vec![
        ("session_id", provenance.session_id.as_deref()),
        ("lane_id", provenance.lane_id.as_deref()),
    ];
    if include_agent_id {
        fields.push(("agent_id", provenance.agent_id.as_deref()));
    }
    fields.extend([
        ("operation_id", provenance.operation_id.as_deref()),
        ("epoch_id", provenance.epoch_id.as_deref()),
        ("core_run_id", provenance.core_run_id.as_deref()),
        (
            "harness_snapshot_id",
            provenance.harness_snapshot_id.as_deref(),
        ),
        (
            "harness_revision_id",
            provenance.harness_revision_id.as_deref(),
        ),
        (
            "model_harness_profile_id",
            provenance.model_harness_profile_id.as_deref(),
        ),
        ("experiment_id", provenance.experiment_id.as_deref()),
    ]);
    for (index, (name, value)) in fields.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        json_field_optional_string(output, name, value);
    }
    output.push('}');
}

fn legacy_header_without_agent_id(event: &TraceEvent) -> Option<String> {
    let TraceEvent::EpisodeHeader(header) = event else {
        return None;
    };
    if header
        .provenance
        .as_ref()
        .is_none_or(|provenance| provenance.agent_id.is_some())
    {
        return None;
    }
    let mut legacy = String::new();
    write_json_event_with_agent_id(&mut legacy, event, false);
    Some(legacy)
}

fn json_optional_cache_evidence(output: &mut String, evidence: Option<&CacheEvidence>) {
    let Some(evidence) = evidence else {
        output.push_str("null");
        return;
    };
    output.push('{');
    json_field_optional_string(output, "continuity", evidence.continuity.as_deref());
    output.push(',');
    json_field_optional_number(
        output,
        "cache_domain_fingerprint",
        evidence.cache_domain_fingerprint,
    );
    output.push(',');
    json_field_name(output, "changed_cache_domain_components");
    json_string_array(output, &evidence.changed_cache_domain_components);
    output.push(',');
    json_field_optional_number(output, "context_bytes", evidence.context_bytes);
    output.push(',');
    json_field_optional_number(
        output,
        "common_context_prefix_bytes",
        evidence.common_context_prefix_bytes,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "common_context_prefix_ratio_millionths",
        evidence
            .common_context_prefix_ratio_millionths
            .map(u64::from),
    );
    output.push(',');
    json_field_optional_bool(
        output,
        "context_projection_changed",
        evidence.context_projection_changed,
    );
    output.push(',');
    json_field_optional_number(output, "context_fingerprint", evidence.context_fingerprint);
    output.push(',');
    json_field_optional_number(
        output,
        "system_prompt_fingerprint",
        evidence.system_prompt_fingerprint,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "tool_definition_fingerprint",
        evidence.tool_definition_fingerprint,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "tool_order_fingerprint",
        evidence.tool_order_fingerprint,
    );
    output.push(',');
    json_field_optional_number(output, "model_fingerprint", evidence.model_fingerprint);
    output.push(',');
    json_field_optional_number(
        output,
        "thinking_fingerprint",
        evidence.thinking_fingerprint,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "deterministic_common_prefix_bytes",
        evidence.deterministic_common_prefix_bytes,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "deterministic_common_prefix_tokens_estimate",
        evidence.deterministic_common_prefix_tokens_estimate,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "provider_cache_read_tokens",
        evidence.provider_cache_read_tokens,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "provider_cache_write_tokens",
        evidence.provider_cache_write_tokens,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "serialized_request_bytes",
        evidence.serialized_request_bytes,
    );
    output.push(',');
    json_field_optional_number(
        output,
        "adapter_cache_domain_fingerprint",
        evidence.adapter_cache_domain_fingerprint,
    );
    output.push(',');
    json_field_name(output, "adapter_cache_domain_components");
    json_u64_map(output, &evidence.adapter_cache_domain_components);
    output.push(',');
    json_field_optional_string(
        output,
        "provider_surface_digest",
        evidence.provider_surface_digest.as_deref(),
    );
    output.push('}');
}

fn write_json_tool(output: &mut String, tool: &Tool) {
    output.push(',');
    json_field_name(output, "turn_index");
    output.push_str(&tool.turn_index.to_string());
    output.push(',');
    json_field_string(output, "call_id", &tool.call_id);
    output.push(',');
    json_field_string(output, "name", &tool.name);
    output.push(',');
    json_field_string(output, "input", &tool.input);
    output.push(',');
    json_field_optional_string(output, "output", tool.output.as_deref());
    output.push(',');
    json_field_optional_string(output, "error", tool.error.as_deref());
}

fn write_json_end(output: &mut String, end: &EpisodeEnd) {
    output.push(',');
    json_field_string(output, "reason", end_reason_name(&end.reason));
    output.push(',');
    json_field_optional_string(output, "error", end.error.as_deref());
    output.push(',');
    json_field_optional_number(output, "finished_at_ms", end.finished_at_ms);
}

fn write_json_compaction(output: &mut String, compaction: &Compaction) {
    json_field_string_with_comma(output, "compaction_id", &compaction.compaction_id);
    json_field_string_with_comma(output, "stage", compaction_stage_name(compaction.stage));
    macro_rules! optional_string {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value.as_deref() {
                json_field_string_with_comma(output, $name, value);
            }
        };
    }
    macro_rules! optional_number {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value {
                json_field_number_with_comma(output, $name, value as u64);
            }
        };
    }
    macro_rules! optional_bool {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value {
                json_field_bool_with_comma(output, $name, value);
            }
        };
    }
    optional_string!("trigger", compaction.trigger);
    optional_string!("reason", compaction.reason);
    optional_string!("phase", compaction.phase);
    optional_string!("strategy_id", compaction.strategy_id);
    optional_number!(
        "strategy_schema_version",
        compaction.strategy_schema_version
    );
    optional_string!("request_layout", compaction.request_layout);
    optional_number!("prompt_fingerprint", compaction.prompt_fingerprint);
    optional_number!(
        "source_history_revision",
        compaction.source_history_revision
    );
    optional_number!("attempt", compaction.attempt);
    optional_number!("automatic_ordinal", compaction.automatic_ordinal);
    optional_number!("overflow_retry_ordinal", compaction.overflow_retry_ordinal);
    optional_bool!("retry_provider_request", compaction.retry_provider_request);
    optional_number!("source_message_count", compaction.source_message_count);
    optional_number!("source_message_bytes", compaction.source_message_bytes);
    optional_number!("retained_message_count", compaction.retained_message_count);
    optional_number!("retained_suffix_bytes", compaction.retained_suffix_bytes);
    optional_number!("tool_result_bytes", compaction.tool_result_bytes);
    optional_number!(
        "compactor_context_bytes",
        compaction.compactor_context_bytes
    );
    optional_number!("compactor_tool_count", compaction.compactor_tool_count);
    optional_bool!(
        "tools_execution_prohibited",
        compaction.tools_execution_prohibited
    );
    optional_bool!(
        "source_is_active_context_prefix",
        compaction.source_is_active_context_prefix
    );
    optional_number!(
        "replacement_message_count",
        compaction.replacement_message_count
    );
    optional_number!("replacement_bytes", compaction.replacement_bytes);
    optional_number!(
        "estimated_context_tokens_after",
        compaction.estimated_context_tokens_after
    );
    optional_number!("headroom_tokens", compaction.headroom_tokens);
    optional_bool!(
        "structural_validation_passed",
        compaction.structural_validation_passed
    );
    optional_bool!("retained_suffix_exact", compaction.retained_suffix_exact);
    optional_bool!(
        "source_generation_matches",
        compaction.source_generation_matches
    );
    optional_number!("provider_input_tokens", compaction.provider_input_tokens);
    optional_number!("provider_output_tokens", compaction.provider_output_tokens);
    optional_number!(
        "provider_cache_read_tokens",
        compaction.provider_cache_read_tokens
    );
    optional_number!(
        "provider_cache_write_tokens",
        compaction.provider_cache_write_tokens
    );
    optional_number!(
        "serialized_request_bytes",
        compaction.serialized_request_bytes
    );
    optional_number!(
        "cache_domain_fingerprint",
        compaction.cache_domain_fingerprint
    );
    optional_string!("terminal_outcome", compaction.terminal_outcome);
    optional_number!(
        "post_compaction_turn_index",
        compaction.post_compaction_turn_index
    );
}

fn json_field_string_with_comma(output: &mut String, name: &str, value: &str) {
    output.push(',');
    json_field_string(output, name, value);
}

fn json_field_number_with_comma(output: &mut String, name: &str, value: u64) {
    output.push(',');
    json_field_name(output, name);
    output.push_str(&value.to_string());
}

fn json_field_bool_with_comma(output: &mut String, name: &str, value: bool) {
    output.push(',');
    json_field_name(output, name);
    output.push_str(if value { "true" } else { "false" });
}

fn json_field_name(output: &mut String, name: &str) {
    json_string(output, name);
    output.push(':');
}

fn json_field_string(output: &mut String, name: &str, value: &str) {
    json_field_name(output, name);
    json_string(output, value);
}

fn json_field_optional_string(output: &mut String, name: &str, value: Option<&str>) {
    json_field_name(output, name);
    match value {
        Some(value) => json_string(output, value),
        None => output.push_str("null"),
    }
}

fn json_field_optional_number(output: &mut String, name: &str, value: Option<u64>) {
    json_field_name(output, name);
    match value {
        Some(value) => output.push_str(&value.to_string()),
        None => output.push_str("null"),
    }
}

fn json_field_optional_bool(output: &mut String, name: &str, value: Option<bool>) {
    json_field_name(output, name);
    match value {
        Some(value) => output.push_str(if value { "true" } else { "false" }),
        None => output.push_str("null"),
    }
}

fn json_map(output: &mut String, values: &BTreeMap<String, String>) {
    output.push('{');
    for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        json_field_string(output, key, value);
    }
    output.push('}');
}

fn json_u64_map(output: &mut String, values: &BTreeMap<String, u64>) {
    output.push('{');
    for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        json_field_name(output, key);
        output.push_str(&value.to_string());
    }
    output.push('}');
}

fn json_string_array(output: &mut String, values: &[String]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        json_string(output, value);
    }
    output.push(']');
}

fn json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1F}' => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}
