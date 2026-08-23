//! JSON Lines encoding for trace events.

use super::{compaction_stage_name, end_reason_name, event_schema_version, event_type};
use crate::event::{
    CacheEvidence, Compaction, EpisodeEnd, EpisodeHeader, Tool, TraceEvent, TraceProvenance,
    Turn,
};
use std::collections::BTreeMap;

pub(super) fn write_json_event(output: &mut String, event: &TraceEvent) {
    output.push('{');
    json_field_name(output, "schema_version");
    output.push_str(&event_schema_version(event).to_string());
    output.push(',');
    json_field_name(output, "type");
    json_string(output, event_type(event));
    match event {
        TraceEvent::EpisodeHeader(header) => write_json_header(output, header),
        TraceEvent::Turn(turn) => write_json_turn(output, turn),
        TraceEvent::Tool(tool) => write_json_tool(output, tool),
        TraceEvent::Compaction(compaction) => write_json_compaction(output, compaction),
        TraceEvent::EpisodeEnd(end) => write_json_end(output, end),
    }
    output.push('}');
}

fn write_json_header(output: &mut String, header: &EpisodeHeader) {
    output.push(',');
    json_field_string(output, "episode_id", &header.episode_id);
    output.push(',');
    json_field_name(output, "metadata");
    json_map(output, &header.metadata);
    output.push(',');
    json_field_optional_number(output, "started_at_ms", header.started_at_ms);
    output.push(',');
    json_field_name(output, "provenance");
    json_optional_provenance(output, header.provenance.as_ref());
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

fn json_optional_provenance(output: &mut String, provenance: Option<&TraceProvenance>) {
    let Some(provenance) = provenance else {
        output.push_str("null");
        return;
    };
    output.push('{');
    let fields = [
        ("session_id", provenance.session_id.as_deref()),
        ("lane_id", provenance.lane_id.as_deref()),
        ("operation_id", provenance.operation_id.as_deref()),
        ("epoch_id", provenance.epoch_id.as_deref()),
        ("core_run_id", provenance.core_run_id.as_deref()),
        ("harness_snapshot_id", provenance.harness_snapshot_id.as_deref()),
        ("harness_revision_id", provenance.harness_revision_id.as_deref()),
        (
            "model_harness_profile_id",
            provenance.model_harness_profile_id.as_deref(),
        ),
        ("experiment_id", provenance.experiment_id.as_deref()),
    ];
    for (index, (name, value)) in fields.into_iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        json_field_optional_string(output, name, value);
    }
    output.push('}');
}

fn json_optional_cache_evidence(output: &mut String, evidence: Option<&CacheEvidence>) {
    let Some(evidence) = evidence else {
        output.push_str("null");
        return;
    };
    output.push('{');
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
