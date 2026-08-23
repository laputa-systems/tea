//! Runtime-owned trace capture and durable redaction.

use crate::harness::HarnessError;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tea_trace::{Redactor, TraceEvent, TraceSink};

/// In-memory staging sink for one core run's trace.
///
/// `SessionRuntime` persists the redacted JSON Lines artifact only after core
/// has emitted its terminal event and before the epoch closes.
#[derive(Clone, Default)]
pub(super) struct TraceCaptureSink {
    events: Arc<Mutex<Vec<TraceEvent>>>,
}

impl TraceCaptureSink {
    pub(super) fn events(&self) -> Result<Vec<TraceEvent>, HarnessError> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| HarnessError::invalid_state("trace capture mutex is poisoned"))
    }
}

impl TraceSink for TraceCaptureSink {
    type Error = Infallible;

    fn append(&mut self, event: TraceEvent) -> Result<(), Self::Error> {
        self.events
            .lock()
            .expect("trace capture mutex is not reentered by a trace observer")
            .push(event);
        Ok(())
    }
}

/// Redacts content before it reaches durable trace storage.
///
/// Provenance, identities, cache evidence, and lifecycle labels remain, while
/// prompts, model output, tool arguments/results, and terminal diagnostics do
/// not enter the immutable artifact.
pub(super) struct DurableTraceRedactor;

impl Redactor for DurableTraceRedactor {
    fn redact(&self, event: TraceEvent) -> TraceEvent {
        match event {
            TraceEvent::Turn(mut turn) => {
                if !turn.input.is_empty() {
                    turn.input = "[redacted]".into();
                }
                if turn.output.is_some() {
                    turn.output = Some("[redacted]".into());
                }
                TraceEvent::Turn(turn)
            }
            TraceEvent::Tool(mut tool) => {
                if !tool.input.is_empty() {
                    tool.input = "[redacted]".into();
                }
                if tool.output.is_some() {
                    tool.output = Some("[redacted]".into());
                }
                if tool.error.is_some() {
                    tool.error = Some("[redacted]".into());
                }
                TraceEvent::Tool(tool)
            }
            TraceEvent::EpisodeEnd(mut end) => {
                if end.error.is_some() {
                    end.error = Some("[redacted]".into());
                }
                TraceEvent::EpisodeEnd(end)
            }
            event @ (TraceEvent::EpisodeHeader(_) | TraceEvent::Compaction(_)) => event,
        }
    }
}
