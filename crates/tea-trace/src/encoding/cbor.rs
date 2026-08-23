//! Definite-length CBOR sequence encoding for trace events.

use super::{compaction_stage_name, end_reason_name, event_schema_version, event_type};
use crate::event::{
    CacheEvidence, Compaction, EpisodeEnd, EpisodeHeader, Tool, TraceEvent, TraceProvenance, Turn,
};

pub(super) fn write_cbor_event(output: &mut Vec<u8>, event: &TraceEvent) {
    let field_count = match event {
        TraceEvent::EpisodeHeader(_) => 6,
        TraceEvent::Turn(_) => 7,
        TraceEvent::EpisodeEnd(_) => 5,
        TraceEvent::Tool(_) => 8,
        TraceEvent::Compaction(compaction) => compaction_field_count(compaction),
    };
    cbor_map(output, field_count);
    cbor_text(output, "schema_version");
    cbor_unsigned(output, event_schema_version(event).into());
    cbor_text(output, "type");
    cbor_text(output, event_type(event));
    match event {
        TraceEvent::EpisodeHeader(header) => write_cbor_header(output, header),
        TraceEvent::Turn(turn) => write_cbor_turn(output, turn),
        TraceEvent::Tool(tool) => write_cbor_tool(output, tool),
        TraceEvent::Compaction(compaction) => write_cbor_compaction(output, compaction),
        TraceEvent::EpisodeEnd(end) => write_cbor_end(output, end),
    }
}

fn write_cbor_header(output: &mut Vec<u8>, header: &EpisodeHeader) {
    cbor_text(output, "episode_id");
    cbor_text(output, &header.episode_id);
    cbor_text(output, "metadata");
    cbor_map(output, header.metadata.len());
    for (key, value) in &header.metadata {
        cbor_text(output, key);
        cbor_text(output, value);
    }
    cbor_text(output, "started_at_ms");
    cbor_optional_unsigned(output, header.started_at_ms);
    cbor_text(output, "provenance");
    cbor_optional_provenance(output, header.provenance.as_ref());
}

fn write_cbor_turn(output: &mut Vec<u8>, turn: &Turn) {
    cbor_text(output, "index");
    cbor_unsigned(output, turn.index.into());
    cbor_text(output, "input");
    cbor_text(output, &turn.input);
    cbor_text(output, "output");
    cbor_optional_text(output, turn.output.as_deref());
    cbor_text(output, "stop_reason");
    cbor_optional_text(output, turn.stop_reason.as_deref());
    cbor_text(output, "cache_evidence");
    cbor_optional_cache_evidence(output, turn.cache_evidence.as_ref());
}

fn cbor_optional_provenance(output: &mut Vec<u8>, provenance: Option<&TraceProvenance>) {
    let Some(provenance) = provenance else {
        output.push(0xf6);
        return;
    };
    cbor_map(output, 9);
    for (name, value) in [
        ("session_id", provenance.session_id.as_deref()),
        ("lane_id", provenance.lane_id.as_deref()),
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
    ] {
        cbor_text(output, name);
        cbor_optional_text(output, value);
    }
}

fn cbor_optional_cache_evidence(output: &mut Vec<u8>, evidence: Option<&CacheEvidence>) {
    let Some(evidence) = evidence else {
        output.push(0xf6);
        return;
    };
    cbor_map(output, 21);
    cbor_text(output, "continuity");
    cbor_optional_text(output, evidence.continuity.as_deref());
    cbor_text(output, "cache_domain_fingerprint");
    cbor_optional_unsigned(output, evidence.cache_domain_fingerprint);
    cbor_text(output, "changed_cache_domain_components");
    cbor_text_array(output, &evidence.changed_cache_domain_components);
    cbor_text(output, "context_bytes");
    cbor_optional_unsigned(output, evidence.context_bytes);
    cbor_text(output, "common_context_prefix_bytes");
    cbor_optional_unsigned(output, evidence.common_context_prefix_bytes);
    cbor_text(output, "common_context_prefix_ratio_millionths");
    cbor_optional_unsigned(
        output,
        evidence
            .common_context_prefix_ratio_millionths
            .map(u64::from),
    );
    cbor_text(output, "context_projection_changed");
    cbor_optional_bool(output, evidence.context_projection_changed);
    cbor_text(output, "context_fingerprint");
    cbor_optional_unsigned(output, evidence.context_fingerprint);
    cbor_text(output, "system_prompt_fingerprint");
    cbor_optional_unsigned(output, evidence.system_prompt_fingerprint);
    cbor_text(output, "tool_definition_fingerprint");
    cbor_optional_unsigned(output, evidence.tool_definition_fingerprint);
    cbor_text(output, "tool_order_fingerprint");
    cbor_optional_unsigned(output, evidence.tool_order_fingerprint);
    cbor_text(output, "model_fingerprint");
    cbor_optional_unsigned(output, evidence.model_fingerprint);
    cbor_text(output, "thinking_fingerprint");
    cbor_optional_unsigned(output, evidence.thinking_fingerprint);
    cbor_text(output, "deterministic_common_prefix_bytes");
    cbor_optional_unsigned(output, evidence.deterministic_common_prefix_bytes);
    cbor_text(output, "deterministic_common_prefix_tokens_estimate");
    cbor_optional_unsigned(output, evidence.deterministic_common_prefix_tokens_estimate);
    cbor_text(output, "provider_cache_read_tokens");
    cbor_optional_unsigned(output, evidence.provider_cache_read_tokens);
    cbor_text(output, "provider_cache_write_tokens");
    cbor_optional_unsigned(output, evidence.provider_cache_write_tokens);
    cbor_text(output, "serialized_request_bytes");
    cbor_optional_unsigned(output, evidence.serialized_request_bytes);
    cbor_text(output, "adapter_cache_domain_fingerprint");
    cbor_optional_unsigned(output, evidence.adapter_cache_domain_fingerprint);
    cbor_text(output, "adapter_cache_domain_components");
    cbor_u64_map(output, &evidence.adapter_cache_domain_components);
    cbor_text(output, "provider_surface_digest");
    cbor_optional_text(output, evidence.provider_surface_digest.as_deref());
}

fn write_cbor_tool(output: &mut Vec<u8>, tool: &Tool) {
    cbor_text(output, "turn_index");
    cbor_unsigned(output, tool.turn_index.into());
    cbor_text(output, "call_id");
    cbor_text(output, &tool.call_id);
    cbor_text(output, "name");
    cbor_text(output, &tool.name);
    cbor_text(output, "input");
    cbor_text(output, &tool.input);
    cbor_text(output, "output");
    cbor_optional_text(output, tool.output.as_deref());
    cbor_text(output, "error");
    cbor_optional_text(output, tool.error.as_deref());
}

fn write_cbor_end(output: &mut Vec<u8>, end: &EpisodeEnd) {
    cbor_text(output, "reason");
    cbor_text(output, end_reason_name(&end.reason));
    cbor_text(output, "error");
    cbor_optional_text(output, end.error.as_deref());
    cbor_text(output, "finished_at_ms");
    cbor_optional_unsigned(output, end.finished_at_ms);
}

fn compaction_field_count(compaction: &Compaction) -> usize {
    4 + [
        compaction.trigger.is_some(),
        compaction.reason.is_some(),
        compaction.phase.is_some(),
        compaction.strategy_id.is_some(),
        compaction.strategy_schema_version.is_some(),
        compaction.request_layout.is_some(),
        compaction.prompt_fingerprint.is_some(),
        compaction.source_history_revision.is_some(),
        compaction.attempt.is_some(),
        compaction.automatic_ordinal.is_some(),
        compaction.overflow_retry_ordinal.is_some(),
        compaction.retry_provider_request.is_some(),
        compaction.source_message_count.is_some(),
        compaction.source_message_bytes.is_some(),
        compaction.retained_message_count.is_some(),
        compaction.retained_suffix_bytes.is_some(),
        compaction.tool_result_bytes.is_some(),
        compaction.compactor_context_bytes.is_some(),
        compaction.compactor_tool_count.is_some(),
        compaction.tools_execution_prohibited.is_some(),
        compaction.source_is_active_context_prefix.is_some(),
        compaction.replacement_message_count.is_some(),
        compaction.replacement_bytes.is_some(),
        compaction.estimated_context_tokens_after.is_some(),
        compaction.headroom_tokens.is_some(),
        compaction.structural_validation_passed.is_some(),
        compaction.retained_suffix_exact.is_some(),
        compaction.source_generation_matches.is_some(),
        compaction.provider_input_tokens.is_some(),
        compaction.provider_output_tokens.is_some(),
        compaction.provider_cache_read_tokens.is_some(),
        compaction.provider_cache_write_tokens.is_some(),
        compaction.serialized_request_bytes.is_some(),
        compaction.cache_domain_fingerprint.is_some(),
        compaction.terminal_outcome.is_some(),
        compaction.post_compaction_turn_index.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count()
}

fn write_cbor_compaction(output: &mut Vec<u8>, compaction: &Compaction) {
    cbor_text(output, "compaction_id");
    cbor_text(output, &compaction.compaction_id);
    cbor_text(output, "stage");
    cbor_text(output, compaction_stage_name(compaction.stage));
    macro_rules! optional_text {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value.as_deref() {
                cbor_text(output, $name);
                cbor_text(output, value);
            }
        };
    }
    macro_rules! optional_unsigned {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value {
                cbor_text(output, $name);
                cbor_unsigned(output, value as u64);
            }
        };
    }
    macro_rules! optional_bool {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value {
                cbor_text(output, $name);
                cbor_bool(output, value);
            }
        };
    }
    optional_text!("trigger", compaction.trigger);
    optional_text!("reason", compaction.reason);
    optional_text!("phase", compaction.phase);
    optional_text!("strategy_id", compaction.strategy_id);
    optional_unsigned!(
        "strategy_schema_version",
        compaction.strategy_schema_version
    );
    optional_text!("request_layout", compaction.request_layout);
    optional_unsigned!("prompt_fingerprint", compaction.prompt_fingerprint);
    optional_unsigned!(
        "source_history_revision",
        compaction.source_history_revision
    );
    optional_unsigned!("attempt", compaction.attempt);
    optional_unsigned!("automatic_ordinal", compaction.automatic_ordinal);
    optional_unsigned!("overflow_retry_ordinal", compaction.overflow_retry_ordinal);
    optional_bool!("retry_provider_request", compaction.retry_provider_request);
    optional_unsigned!("source_message_count", compaction.source_message_count);
    optional_unsigned!("source_message_bytes", compaction.source_message_bytes);
    optional_unsigned!("retained_message_count", compaction.retained_message_count);
    optional_unsigned!("retained_suffix_bytes", compaction.retained_suffix_bytes);
    optional_unsigned!("tool_result_bytes", compaction.tool_result_bytes);
    optional_unsigned!(
        "compactor_context_bytes",
        compaction.compactor_context_bytes
    );
    optional_unsigned!("compactor_tool_count", compaction.compactor_tool_count);
    optional_bool!(
        "tools_execution_prohibited",
        compaction.tools_execution_prohibited
    );
    optional_bool!(
        "source_is_active_context_prefix",
        compaction.source_is_active_context_prefix
    );
    optional_unsigned!(
        "replacement_message_count",
        compaction.replacement_message_count
    );
    optional_unsigned!("replacement_bytes", compaction.replacement_bytes);
    optional_unsigned!(
        "estimated_context_tokens_after",
        compaction.estimated_context_tokens_after
    );
    optional_unsigned!("headroom_tokens", compaction.headroom_tokens);
    optional_bool!(
        "structural_validation_passed",
        compaction.structural_validation_passed
    );
    optional_bool!("retained_suffix_exact", compaction.retained_suffix_exact);
    optional_bool!(
        "source_generation_matches",
        compaction.source_generation_matches
    );
    optional_unsigned!("provider_input_tokens", compaction.provider_input_tokens);
    optional_unsigned!("provider_output_tokens", compaction.provider_output_tokens);
    optional_unsigned!(
        "provider_cache_read_tokens",
        compaction.provider_cache_read_tokens
    );
    optional_unsigned!(
        "provider_cache_write_tokens",
        compaction.provider_cache_write_tokens
    );
    optional_unsigned!(
        "serialized_request_bytes",
        compaction.serialized_request_bytes
    );
    optional_unsigned!(
        "cache_domain_fingerprint",
        compaction.cache_domain_fingerprint
    );
    optional_text!("terminal_outcome", compaction.terminal_outcome);
    optional_unsigned!(
        "post_compaction_turn_index",
        compaction.post_compaction_turn_index
    );
}

fn cbor_optional_text(output: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => cbor_text(output, value),
        None => output.push(0xf6),
    }
}

fn cbor_optional_unsigned(output: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => cbor_unsigned(output, value),
        None => output.push(0xf6),
    }
}

fn cbor_optional_bool(output: &mut Vec<u8>, value: Option<bool>) {
    match value {
        Some(value) => cbor_bool(output, value),
        None => output.push(0xf6),
    }
}

fn cbor_bool(output: &mut Vec<u8>, value: bool) {
    output.push(if value { 0xf5 } else { 0xf4 });
}

fn cbor_text(output: &mut Vec<u8>, value: &str) {
    cbor_major_length(output, 3, value.len() as u64);
    output.extend_from_slice(value.as_bytes());
}

fn cbor_map(output: &mut Vec<u8>, length: usize) {
    cbor_major_length(output, 5, length as u64);
}

fn cbor_text_array(output: &mut Vec<u8>, values: &[String]) {
    cbor_major_length(output, 4, values.len() as u64);
    for value in values {
        cbor_text(output, value);
    }
}

fn cbor_u64_map(output: &mut Vec<u8>, values: &std::collections::BTreeMap<String, u64>) {
    cbor_map(output, values.len());
    for (key, value) in values {
        cbor_text(output, key);
        cbor_unsigned(output, *value);
    }
}

fn cbor_unsigned(output: &mut Vec<u8>, value: u64) {
    cbor_major_length(output, 0, value);
}

fn cbor_major_length(output: &mut Vec<u8>, major: u8, value: u64) {
    debug_assert!(major <= 7);
    match value {
        0..=23 => output.push((major << 5) | value as u8),
        24..=255 => output.extend_from_slice(&[(major << 5) | 24, value as u8]),
        256..=65_535 => {
            output.push((major << 5) | 25);
            output.extend_from_slice(&(value as u16).to_be_bytes());
        }
        65_536..=4_294_967_295 => {
            output.push((major << 5) | 26);
            output.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            output.push((major << 5) | 27);
            output.extend_from_slice(&value.to_be_bytes());
        }
    }
}
