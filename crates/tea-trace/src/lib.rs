#![forbid(unsafe_code)]
#![doc = "A compact, dependency-free, append-only trajectory contract."]
#![doc = ""]
#![doc = "The V0 boundary is intentionally small:"]
#![doc = ""]
#![doc = "* [`event`] owns the episode header, turn, tool, and end records;"]
#![doc = "* [`sink`] defines the append-only observer boundary; and"]
#![doc = "* [`redaction`] makes the sensitive-data boundary explicit."]
#![doc = ""]
#![doc = "A valid episode is linear and append-only: one `EpisodeHeader`, zero or more"]
#![doc = "`Turn`/`Tool` records, then one `EpisodeEnd`.  The runtime is responsible"]
#![doc = "for enforcing that lifecycle.  This crate does not own a clock, executor,"]
#![doc = "session store, UI tree, model provider, or serialization format."]
#![doc = ""]
#![doc = "Trace output is optional telemetry.  Integrations should put a"]
#![doc = "[`redaction::RedactingSink`] in front of their sink and use"]
#![doc = "[`sink::IsolatedSink`] when sink failures must never affect agent state."]

pub mod encoding;
pub mod event;
pub mod redaction;
pub mod sink;

pub use encoding::{CborSink, JsonLinesSink};
pub use event::{
    COMPACTION_TRACE_SCHEMA_VERSION, Compaction, CompactionStage, EndReason, EpisodeEnd,
    EpisodeHeader, ModelTurn, TRACE_SCHEMA_VERSION, Tool, ToolExecution, TraceEvent,
    TraceEventKind, Turn, TurnIndex,
};
pub use redaction::{NoRedaction, RedactingSink, Redactor};
pub use sink::{IsolatedSink, NoopSink, TraceSink};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RedactingBuffer {
        events: Vec<TraceEvent>,
    }

    impl TraceSink for RedactingBuffer {
        type Error = ();

        fn append(&mut self, event: TraceEvent) -> Result<(), Self::Error> {
            self.events.push(event);
            Ok(())
        }
    }

    struct ReplaceSecrets;

    impl Redactor for ReplaceSecrets {
        fn redact(&self, event: TraceEvent) -> TraceEvent {
            match event {
                TraceEvent::Turn(mut turn) => {
                    turn.input = "[redacted]".to_owned();
                    TraceEvent::Turn(turn)
                }
                event => event,
            }
        }
    }

    #[test]
    fn events_have_a_linear_kind_and_terminal_marker() {
        let header = TraceEvent::from(EpisodeHeader::new("episode-1"));
        let turn = TraceEvent::from(Turn::new(0, "prompt"));
        let tool = TraceEvent::from(Tool::new(0, "call-1", "shell", "{}"));
        let end = TraceEvent::from(EpisodeEnd::completed());

        assert_eq!(header.kind(), TraceEventKind::EpisodeHeader);
        assert_eq!(turn.kind(), TraceEventKind::Turn);
        assert_eq!(tool.kind(), TraceEventKind::Tool);
        assert_eq!(end.kind(), TraceEventKind::EpisodeEnd);
        assert!(!header.is_terminal());
        assert!(end.is_terminal());
    }

    #[test]
    fn redaction_precedes_sink_and_isolation_counts_failures() {
        let sink = RedactingBuffer::default();
        let redacting = RedactingSink::new(sink, ReplaceSecrets);
        let mut isolated = IsolatedSink::new(redacting);

        isolated
            .append(TraceEvent::from(Turn::new(0, "secret")))
            .expect("isolation is infallible");

        let sink = isolated.into_inner().into_inner();
        assert_eq!(sink.events.len(), 1);
        match &sink.events[0] {
            TraceEvent::Turn(turn) => assert_eq!(turn.input, "[redacted]"),
            event => panic!("unexpected event: {event:?}"),
        }
    }
}
