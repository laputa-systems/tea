//! Explicit JSON Lines and CBOR sinks for compact trajectory records.
//!
//! The trace contract deliberately does not choose a storage location. These
//! sinks only serialize each owned [`TraceEvent`](crate::TraceEvent) to a
//! caller-supplied writer; they do not open files, read clocks, buffer an
//! episode, or make any runtime decision. JSON Lines is convenient for human
//! inspection and streaming pipelines. CBOR is a compact, self-delimiting
//! sequence of the same records for machine-oriented archives.

use crate::event::{CompactionStage, EndReason, TraceEvent};
use crate::sink::TraceSink;
use std::io::{self, Write};

mod cbor;
mod json;

pub use json::{decode_json_line, decode_jsonl, JsonTraceDecodeError};

/// A [`TraceSink`] that writes one JSON object followed by a newline per event.
///
/// The JSON shape is the explicit v1 wire format. Every object carries
/// `schema_version` and a `type` discriminator. Its key order is stable, so
/// deterministic traces stay diff-friendly without an external serializer.
pub struct JsonLinesSink<W> {
    writer: W,
}

impl<W> JsonLinesSink<W> {
    /// Wrap a caller-owned writer.
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Return the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Borrow the underlying writer.
    pub const fn inner(&self) -> &W {
        &self.writer
    }

    /// Mutably borrow the underlying writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.writer
    }
}

impl<W: Write> TraceSink for JsonLinesSink<W> {
    type Error = io::Error;

    fn append(&mut self, event: TraceEvent) -> Result<(), Self::Error> {
        let mut record = String::new();
        json::write_json_event(&mut record, &event);
        self.writer.write_all(record.as_bytes())?;
        self.writer.write_all(b"\n")
    }
}

/// A [`TraceSink`] that appends one self-delimiting CBOR map per event.
///
/// The wire shape mirrors [`JsonLinesSink`]. Values use only definite-length
/// CBOR major types for maps, arrays, text, unsigned integers, booleans, and null;
/// no indefinite lengths, tags, floats, or host-specific extensions are used.
/// Concatenated values are intentionally valid CBOR sequence framing, which
/// allows a caller to stream records without a precomputed episode length.
pub struct CborSink<W> {
    writer: W,
}

impl<W> CborSink<W> {
    /// Wrap a caller-owned writer.
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Return the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Borrow the underlying writer.
    pub const fn inner(&self) -> &W {
        &self.writer
    }

    /// Mutably borrow the underlying writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.writer
    }
}

impl<W: Write> TraceSink for CborSink<W> {
    type Error = io::Error;

    fn append(&mut self, event: TraceEvent) -> Result<(), Self::Error> {
        let mut bytes = Vec::new();
        cbor::write_cbor_event(&mut bytes, &event);
        self.writer.write_all(&bytes)
    }
}

fn event_type(event: &TraceEvent) -> &'static str {
    match event {
        TraceEvent::EpisodeHeader(_) => "episode_header",
        TraceEvent::Turn(_) => "turn",
        TraceEvent::Tool(_) => "tool",
        TraceEvent::Compaction(_) => "compaction",
        TraceEvent::EpisodeEnd(_) => "episode_end",
    }
}

fn event_schema_version(_event: &TraceEvent) -> u16 {
    crate::event::TRACE_SCHEMA_VERSION
}

fn compaction_stage_name(stage: CompactionStage) -> &'static str {
    match stage {
        CompactionStage::Started => "started",
        CompactionStage::SourceSelected => "source_selected",
        CompactionStage::RequestPrepared => "request_prepared",
        CompactionStage::ProviderUsageObserved => "provider_usage_observed",
        CompactionStage::ReplacementProposed => "replacement_proposed",
        CompactionStage::Terminal => "terminal",
        CompactionStage::PostCompactionRequestObserved => "post_compaction_request_observed",
    }
}

fn end_reason_name(reason: &EndReason) -> &str {
    match reason {
        EndReason::Completed => "completed",
        EndReason::Cancelled => "cancelled",
        EndReason::Failed => "failed",
        EndReason::Aborted => "aborted",
        EndReason::Other(value) => value,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CacheEvidence, Compaction, CompactionStage, EpisodeHeader, TraceEvent, TraceProvenance,
        Turn,
    };
    use std::collections::BTreeMap;

    #[test]
    fn json_lines_is_one_stable_escaped_object_per_record() {
        let mut sink = JsonLinesSink::new(Vec::new());
        sink.append(TraceEvent::from(
            EpisodeHeader::new("episode\n1").with_metadata("z", "\u{0001}\""),
        ))
        .expect("in-memory write succeeds");
        sink.append(TraceEvent::from(
            Turn::new(3, "input").with_output("output"),
        ))
        .expect("in-memory write succeeds");
        let text = String::from_utf8(sink.into_inner()).expect("JSON is UTF-8");
        assert_eq!(text.lines().count(), 2);
        assert_eq!(
            text.lines().next(),
            Some(
                r#"{"schema_version":1,"type":"episode_header","episode_id":"episode\n1","metadata":{"z":"\u0001\""},"started_at_ms":null,"provenance":null}"#
            ),
        );
        assert!(text.contains(r#""type":"turn"#));
    }

    #[test]
    fn decoder_round_trips_canonical_episode_and_enforces_lifecycle() {
        let mut sink = JsonLinesSink::new(Vec::new());
        sink.append(EpisodeHeader::new("episode").into()).unwrap();
        sink.append(Turn::new(0, "input").into()).unwrap();
        sink.append(crate::EpisodeEnd::completed().into()).unwrap();
        let text = String::from_utf8(sink.into_inner()).unwrap();
        let events = decode_jsonl(&text).expect("canonical episode decodes");
        assert_eq!(events.len(), 3);
        assert!(matches!(events.first(), Some(TraceEvent::EpisodeHeader(_))));
        assert!(matches!(events.last(), Some(TraceEvent::EpisodeEnd(_))));
    }

    #[test]
    fn decoder_rejects_unknown_extra_and_noncanonical_records() {
        let valid = r#"{"schema_version":1,"type":"episode_end","reason":"completed","error":null,"finished_at_ms":null}"#;
        assert!(decode_json_line(valid).is_ok());
        assert!(decode_json_line(&valid[..valid.len() - 1]).is_err());
        assert!(decode_json_line(
            r#"{"schema_version":1,"type":"factory_internal","value":1}"#
        ).is_err());
        assert!(decode_json_line(
            r#" {"schema_version":1,"type":"episode_end","reason":"completed","error":null,"finished_at_ms":null}"#
        ).is_err());
    }

    #[test]
    fn decoder_rejects_terminal_before_header_or_records_after_end() {
        let end = r#"{"schema_version":1,"type":"episode_end","reason":"completed","error":null,"finished_at_ms":null}"#;
        let header = r#"{"schema_version":1,"type":"episode_header","episode_id":"e","metadata":{},"started_at_ms":null,"provenance":null}"#;
        assert!(decode_jsonl(&format!("{end}\n{header}\n")).is_err());
        assert!(decode_jsonl(&format!("{header}\n{end}\n{header}\n")).is_err());
    }

    #[test]
    fn decoder_rejects_a_second_header_inside_an_episode() {
        let header = r#"{"schema_version":1,"type":"episode_header","episode_id":"e","metadata":{},"started_at_ms":null,"provenance":null}"#;
        let end = r#"{"schema_version":1,"type":"episode_end","reason":"completed","error":null,"finished_at_ms":null}"#;
        assert!(decode_jsonl(&format!("{header}\n{header}\n{end}\n")).is_err());
    }

    #[test]
    fn cbor_uses_definite_maps_and_has_no_json_line_delimiter() {
        let mut sink = CborSink::new(Vec::new());
        sink.append(TraceEvent::from(EpisodeHeader::new("episode")))
            .expect("in-memory write succeeds");
        let bytes = sink.into_inner();
        // Major type 5 (map), length 6. The record is one CBOR value, not a
        // newline-delimited JSON encoding.
        assert_eq!(bytes.first(), Some(&0xa6));
        assert!(!bytes.contains(&b'\n'));
        assert!(
            bytes
                .windows("episode_header".len())
                .any(|window| window == b"episode_header")
        );
    }

    #[test]
    fn agent_attribution_round_trips_json_and_cbor_while_legacy_json_stays_readable() {
        let provenance = TraceProvenance {
            lane_id: Some("lane-child".into()),
            agent_id: Some("agent-child".into()),
            ..TraceProvenance::default()
        };
        let header = TraceEvent::from(
            EpisodeHeader::new("episode-agent").with_provenance(provenance.clone()),
        );

        let mut json = JsonLinesSink::new(Vec::new());
        json.append(header.clone())
            .expect("in-memory JSON write succeeds");
        let encoded = String::from_utf8(json.into_inner()).expect("JSON is UTF-8");
        assert!(encoded.contains(r#""lane_id":"lane-child""#));
        assert!(encoded.contains(r#""agent_id":"agent-child""#));
        let TraceEvent::EpisodeHeader(decoded) =
            decode_json_line(encoded.trim_end()).expect("canonical attribution decodes")
        else {
            panic!("header round trip must remain a header");
        };
        assert_eq!(decoded.provenance, Some(provenance));

        let legacy = r#"{"schema_version":1,"type":"episode_header","episode_id":"legacy","metadata":{},"started_at_ms":null,"provenance":{"session_id":null,"lane_id":"lane-child","operation_id":null,"epoch_id":null,"core_run_id":null,"harness_snapshot_id":null,"harness_revision_id":null,"model_harness_profile_id":null,"experiment_id":null}}"#;
        let TraceEvent::EpisodeHeader(legacy_header) =
            decode_json_line(legacy).expect("pre-agent v1 attribution remains readable")
        else {
            panic!("legacy header remains a header");
        };
        assert_eq!(
            legacy_header
                .provenance
                .as_ref()
                .and_then(|value| value.agent_id.as_deref()),
            None
        );

        let mut cbor = CborSink::new(Vec::new());
        cbor.append(header).expect("in-memory CBOR write succeeds");
        let bytes = cbor.into_inner();
        assert!(
            bytes
                .windows("agent_id".len())
                .any(|window| window == b"agent_id"),
            "CBOR provenance carries the same optional agent attribution"
        );
    }

    #[test]
    fn compaction_is_a_v1_content_free_record() {
        let mut record = Compaction::new("run-7:compact-1", CompactionStage::Terminal);
        record.strategy_id = Some("cache_replay_summary_v1".into());
        record.terminal_outcome = Some("committed".into());
        record.serialized_request_bytes = Some(321);
        let mut sink = JsonLinesSink::new(Vec::new());
        sink.append(TraceEvent::from(record))
            .expect("in-memory write succeeds");
        let text = String::from_utf8(sink.into_inner()).expect("JSON is UTF-8");
        assert!(text.contains(r#""schema_version":1,"type":"compaction""#));
        assert!(text.contains(r#""compaction_id":"run-7:compact-1""#));
        assert!(text.contains(r#""serialized_request_bytes":321"#));
        assert!(!text.contains("checkpoint"));
        assert!(!text.contains("prompt"));
    }

    #[test]
    fn turn_cache_evidence_serializes_logical_and_adapter_diagnostics() {
        let mut adapter_components = BTreeMap::new();
        adapter_components.insert("provider_tools".into(), 19);
        let evidence = CacheEvidence {
            continuity: Some("rebased".into()),
            changed_cache_domain_components: vec!["model".into()],
            common_context_prefix_ratio_millionths: Some(875_000),
            context_fingerprint: Some(71),
            serialized_request_bytes: Some(321),
            adapter_cache_domain_components: adapter_components,
            ..CacheEvidence::default()
        };
        let mut sink = JsonLinesSink::new(Vec::new());
        sink.append(TraceEvent::from(
            Turn::new(0, "redacted").with_cache_evidence(evidence),
        ))
        .expect("in-memory write succeeds");
        let text = String::from_utf8(sink.into_inner()).expect("JSON is UTF-8");
        assert!(text.contains(r#""continuity":"rebased""#));
        assert!(text.contains(r#""changed_cache_domain_components":["model"]"#));
        assert!(text.contains(r#""common_context_prefix_ratio_millionths":875000"#));
        assert!(text.contains(r#""context_fingerprint":71"#));
        assert!(text.contains(r#""serialized_request_bytes":321"#));
        assert!(text.contains(r#""adapter_cache_domain_components":{"provider_tools":19}"#));
    }
}
