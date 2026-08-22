//! Deterministic operational facts extracted from canonical tool messages.
//!
//! The ledger is host-owned data, not a model-generated recollection. It keeps
//! exact workspace paths where tool arguments exposed them, fingerprints
//! potentially sensitive command/output text, and deliberately never retains
//! tool output or file contents. The provider compactor can attach this bounded
//! surface to an experimental checkpoint without expanding `tea-core`'s
//! provider-neutral message contract.

use std::collections::BTreeMap;
use tea_core::state::{AgentMessage, ToolCallId};
use tea_protocol::JsonValue;

const MAX_LEDGER_ENTRIES: usize = 64;

/// A privacy-safe, deterministically ordered operation fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceLedgerEntry {
    kind: &'static str,
    target: String,
    status: &'static str,
    generation: usize,
    diagnostic_fingerprint: u64,
}

/// Bounded, host-derived state suitable for checkpoint injection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct WorkspaceLedger {
    entries: Vec<WorkspaceLedgerEntry>,
}

impl WorkspaceLedger {
    /// Extract settled tool operations from canonical history.
    pub(super) fn from_messages(messages: &[AgentMessage]) -> Self {
        let mut calls = BTreeMap::<String, PendingOperation>::new();
        let mut entries = BTreeMap::<(String, String, String), WorkspaceLedgerEntry>::new();
        for (generation, message) in messages.iter().enumerate() {
            match message {
                AgentMessage::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        calls.insert(
                            call.id.to_string(),
                            PendingOperation::from_call(
                                call.id.clone(),
                                &call.name,
                                call.arguments.as_str(),
                            ),
                        );
                    }
                }
                AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    content,
                    is_error,
                    ..
                } => {
                    let operation = calls.remove(&tool_call_id.to_string()).unwrap_or_else(|| {
                        PendingOperation::unknown(tool_call_id.clone(), tool_name)
                    });
                    let entry = WorkspaceLedgerEntry {
                        kind: operation.kind,
                        target: operation.target,
                        status: if *is_error { "failed" } else { "succeeded" },
                        generation: generation.saturating_add(1),
                        diagnostic_fingerprint: stable_fingerprint(content.as_bytes()),
                    };
                    entries.insert(
                        (entry.kind.into(), entry.target.clone(), entry.status.into()),
                        entry,
                    );
                }
                AgentMessage::User { .. } => {}
            }
        }
        let mut entries = entries.into_values().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            (
                left.generation,
                left.kind,
                left.target.as_str(),
                left.status,
            )
                .cmp(&(
                    right.generation,
                    right.kind,
                    right.target.as_str(),
                    right.status,
                ))
        });
        if entries.len() > MAX_LEDGER_ENTRIES {
            entries.drain(..entries.len() - MAX_LEDGER_ENTRIES);
        }
        Self { entries }
    }

    /// Render an ordinary-text checkpoint section without raw tool payloads.
    pub(super) fn render(&self) -> String {
        if self.entries.is_empty() {
            return "## Workspace Ledger\n- No settled tool operations were observed.".into();
        }
        let mut rendered = String::from("## Workspace Ledger\n");
        for entry in &self.entries {
            rendered.push_str(&format!(
                "- {} | {} | {} | diagnostic:{:016x}\n",
                entry.kind, entry.target, entry.status, entry.diagnostic_fingerprint
            ));
        }
        rendered.pop();
        rendered
    }
}

#[derive(Clone, Debug)]
struct PendingOperation {
    kind: &'static str,
    target: String,
}

impl PendingOperation {
    fn from_call(_id: ToolCallId, tool_name: &str, arguments: &str) -> Self {
        let object = JsonValue::parse(arguments).ok();
        let path = object
            .as_ref()
            .and_then(|value| value.get("path"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let command = object
            .as_ref()
            .and_then(|value| value.get("command"))
            .and_then(JsonValue::as_str);
        let pattern = object
            .as_ref()
            .and_then(|value| value.get("pattern"))
            .and_then(JsonValue::as_str);
        match tool_name {
            "read" => Self::with_target("file_read", path, arguments),
            "write" => Self::with_target("file_written", path, arguments),
            "edit" => Self::with_target("file_modified", path, arguments),
            "bash" => Self {
                kind: "command",
                target: command
                    .map(command_signature)
                    .unwrap_or_else(|| argument_signature(arguments)),
            },
            "grep" => Self {
                kind: "search",
                target: pattern
                    .map(command_signature)
                    .unwrap_or_else(|| argument_signature(arguments)),
            },
            "find" | "ls" => Self::with_target("workspace_inspection", path, arguments),
            _ => Self {
                kind: "tool",
                target: format!("{tool_name}:{}", argument_signature(arguments)),
            },
        }
    }

    fn unknown(_id: ToolCallId, tool_name: &str) -> Self {
        Self {
            kind: "tool",
            target: format!("{tool_name}:unmatched-result"),
        }
    }

    fn with_target(kind: &'static str, target: Option<String>, arguments: &str) -> Self {
        Self {
            kind,
            target: target.unwrap_or_else(|| argument_signature(arguments)),
        }
    }
}

fn command_signature(value: &str) -> String {
    format!("sha:{:016x}", stable_fingerprint(value.as_bytes()))
}

fn argument_signature(value: &str) -> String {
    format!("args:{}", command_signature(value))
}

fn stable_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_core::state::{AgentToolCall, MessageId, SerializedJson, StopReason};

    fn call(id: &str, name: &str, arguments: &str) -> AgentToolCall {
        AgentToolCall {
            id: ToolCallId::new(id).expect("test call id"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        }
    }

    fn result(id: u64, call_id: &str, name: &str, error: bool, content: &str) -> AgentMessage {
        AgentMessage::ToolResult {
            id: MessageId(id),
            tool_call_id: ToolCallId::new(call_id).expect("test result id"),
            tool_name: name.into(),
            content: content.into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: error,
            failure: None,
        }
    }

    #[test]
    fn ledger_preserves_paths_and_fingerprints_sensitive_command_and_output() {
        let ledger = WorkspaceLedger::from_messages(&[
            AgentMessage::Assistant {
                id: MessageId(1),
                content: String::new(),
                tool_calls: vec![
                    call("read-1", "read", r#"{"path":"src/main.rs"}"#),
                    call(
                        "bash-1",
                        "bash",
                        r#"{"command":"cargo test --secret never-store"}"#,
                    ),
                    call("edit-1", "edit", r#"{"path":"src/main.rs","edits":[]}"#),
                ],
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
            },
            result(2, "read-1", "read", false, "private file body"),
            result(3, "bash-1", "bash", true, "private command output"),
            result(4, "edit-1", "edit", false, "private changed text"),
        ]);
        let rendered = ledger.render();
        assert_eq!(ledger.entries.len(), 3);
        assert!(rendered.contains("file_read | src/main.rs | succeeded"));
        assert!(rendered.contains("file_modified | src/main.rs | succeeded"));
        assert!(rendered.contains("command | sha:"));
        assert!(rendered.contains("failed"));
        assert!(!rendered.contains("never-store"));
        assert!(!rendered.contains("private file body"));
        assert!(!rendered.contains("private command output"));
    }

    #[test]
    fn ledger_deduplicates_same_operation_and_keeps_latest_status_generation() {
        let messages = vec![
            AgentMessage::Assistant {
                id: MessageId(1),
                content: String::new(),
                tool_calls: vec![call("read-1", "read", r#"{"path":"README.md"}"#)],
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
            },
            result(2, "read-1", "read", false, "first"),
            AgentMessage::Assistant {
                id: MessageId(3),
                content: String::new(),
                tool_calls: vec![call("read-2", "read", r#"{"path":"README.md"}"#)],
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
            },
            result(4, "read-2", "read", false, "second"),
        ];
        let ledger = WorkspaceLedger::from_messages(&messages);
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].generation, 4);
    }
}
