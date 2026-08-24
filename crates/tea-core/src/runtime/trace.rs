//! Runtime-owned trace capture and durable redaction.

use crate::harness::HarnessError;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tea_trace::{Redactor, TraceEvent, TraceSink};

/// In-memory staging sink for one core run's trace.
///
/// `SessionSupervisor` persists the redacted JSON Lines artifact only after core
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

#[cfg(test)]
mod tests {
    use super::*;
    use tea_trace::{EpisodeEnd, EpisodeHeader, Tool, TraceProvenance, Turn};

    #[test]
    fn durable_redaction_keeps_lane_agent_attribution_without_task_report_or_patch_content() {
        const TASK: &str = "task-only-secret";
        const REPORT: &str = "report-only-secret";
        const PATCH: &str = "patch-only-secret";

        let redactor = DurableTraceRedactor;
        let header = redactor.redact(TraceEvent::from(
            EpisodeHeader::new("episode")
                .with_provenance(TraceProvenance {
                    lane_id: Some("lane-child".into()),
                    agent_id: Some("agent-child".into()),
                    ..TraceProvenance::default()
                }),
        ));
        let turn = redactor.redact(TraceEvent::from(Turn::new(0, TASK).with_output(REPORT)));
        let tool = redactor.redact(TraceEvent::from(
            Tool::new(0, "call", "edit", PATCH).with_output(PATCH),
        ));
        let end = redactor.redact(TraceEvent::from(EpisodeEnd {
            reason: tea_trace::EndReason::Failed,
            error: Some(REPORT.into()),
            finished_at_ms: None,
        }));

        let TraceEvent::EpisodeHeader(header) = header else {
            panic!("redaction preserves the header kind");
        };
        let provenance = header
            .provenance
            .as_ref()
            .expect("content-free attribution remains");
        assert_eq!(provenance.lane_id.as_deref(), Some("lane-child"));
        assert_eq!(provenance.agent_id.as_deref(), Some("agent-child"));

        let TraceEvent::Turn(turn) = turn else {
            panic!("redaction preserves the turn kind");
        };
        assert_eq!(turn.input, "[redacted]");
        assert_eq!(turn.output.as_deref(), Some("[redacted]"));
        let TraceEvent::Tool(tool) = tool else {
            panic!("redaction preserves the tool kind");
        };
        assert_eq!(tool.input, "[redacted]");
        assert_eq!(tool.output.as_deref(), Some("[redacted]"));
        let TraceEvent::EpisodeEnd(end) = end else {
            panic!("redaction preserves the terminal kind");
        };
        assert_eq!(end.error.as_deref(), Some("[redacted]"));

        let retained = format!("{header:?}{turn:?}{tool:?}{end:?}");
        for secret in [TASK, REPORT, PATCH] {
            assert!(
                !retained.contains(secret),
                "durable trace must not retain task, report, or patch content"
            );
        }
    }
}
