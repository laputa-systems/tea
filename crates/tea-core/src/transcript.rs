//! Typed provider-facing transcript.
//!
//! A [`ModelRequest`](crate::scheduler::ModelRequest) carries the projected
//! conversation as typed messages rather than a serialized string. Adapters
//! derive their wire representation from this value. Configuration lives in
//! the transcript as [`AgentMessage::System`] messages; the helpers here are
//! the Rust port of upstream Pi's transcript replay (`resolveTranscript`,
//! `getCurrentTools`, `collapseSystemMessages`, `hasToolRedefinitions`) and
//! of the replay-compatibility rules in `transformMessages`.

use crate::state::{
    AgentMessage, AgentToolCall, AssistantContent, ConfigurationUpdate, EffectiveConfiguration,
    MessageId, ModelDescriptor, StopReason, ToolCallId,
};
use crate::tool::ToolDeclaration;
use std::collections::{BTreeMap, BTreeSet};
use tea_protocol::JsonValue;

/// Ordered typed transcript for one provider request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Transcript {
    /// Projected conversation, including system configuration messages.
    pub messages: Vec<AgentMessage>,
    /// Host-only notes (for example an extension continuation) that follow the
    /// conversation. They are not user-authored and adapters must not present
    /// them as user messages when the wire format distinguishes the roles.
    pub host_notes: Vec<String>,
}

/// How a transport carries configuration changes after the first message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationProjection {
    /// Later system messages are sent in place (native mid-conversation
    /// configuration updates). The leading configuration stays the initial one,
    /// so earlier request bytes are unchanged by a later update.
    InPlace,
    /// The current configuration replaces the leading one and later system
    /// messages are dropped. A change therefore rewrites the request prefix;
    /// the prompt-layout ledger reports it as a domain change.
    Collapsed,
}

/// A transcript resolved for one transport.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedTranscript {
    /// Configuration carried by the leading system position.
    pub leading: EffectiveConfiguration,
    /// Conversation in order. System messages remain only for
    /// [`ConfigurationProjection::InPlace`] and are always later updates.
    pub messages: Vec<AgentMessage>,
    /// Host-only notes following the conversation.
    pub host_notes: Vec<String>,
}

impl ResolvedTranscript {
    /// The configuration in force at the end of the transcript.
    pub fn current_configuration(&self) -> EffectiveConfiguration {
        let mut configuration = self.leading.clone();
        for message in &self.messages {
            if let AgentMessage::System { update, .. } = message {
                configuration.apply(update);
            }
        }
        configuration
    }
}

impl Transcript {
    /// Construct a transcript from projected messages.
    pub fn new(messages: Vec<AgentMessage>) -> Self {
        Self {
            messages,
            host_notes: Vec::new(),
        }
    }

    /// A standalone transcript: one leading configuration followed by
    /// conversation messages, with sequential identities.
    ///
    /// Hosts use this for requests outside an agent run, such as a standalone
    /// compaction summary.
    pub fn standalone(
        prompt: impl Into<crate::state::SystemPrompt>,
        tools: Vec<ToolDeclaration>,
        conversation: impl IntoIterator<Item = AgentMessage>,
    ) -> Self {
        let configuration = EffectiveConfiguration::new(&prompt.into(), tools);
        let mut messages = Vec::new();
        let update = configuration.as_initial_update();
        if !update.is_empty() {
            messages.push(AgentMessage::System {
                id: MessageId(1),
                update,
            });
        }
        for message in conversation {
            let id = MessageId(messages.len() as u64 + 1);
            messages.push(with_message_id(message, id));
        }
        Self {
            messages,
            host_notes: Vec::new(),
        }
    }

    /// The initial configuration: the first system message, wherever it is.
    ///
    /// Sessions recorded before configuration messages existed gain their
    /// first one at the first later request, after earlier history. That
    /// first recorded configuration is treated as the initial one; it is the
    /// only configuration evidence such a session has.
    pub fn initial_configuration(&self) -> EffectiveConfiguration {
        let mut configuration = EffectiveConfiguration::default();
        if let Some(update) = self.messages.iter().find_map(|message| match message {
            AgentMessage::System { update, .. } => Some(update),
            _ => None,
        }) {
            configuration.apply(update);
        }
        configuration
    }

    /// The configuration in force after every system message.
    pub fn current_configuration(&self) -> EffectiveConfiguration {
        EffectiveConfiguration::replay(&self.messages).unwrap_or_default()
    }

    /// The current rendered system prompt.
    pub fn system_prompt(&self) -> String {
        self.current_configuration().system_prompt()
    }

    /// The tools declared after every system message.
    pub fn tools(&self) -> Vec<ToolDeclaration> {
        self.current_configuration().tools
    }

    /// Every tool ever declared, in first-declaration order with its latest
    /// definition.
    pub fn declared_tools(&self) -> Vec<ToolDeclaration> {
        let mut order = Vec::<String>::new();
        let mut definitions = BTreeMap::<String, ToolDeclaration>::new();
        for update in self.updates() {
            for tool in &update.tools_added {
                if !definitions.contains_key(&tool.name) {
                    order.push(tool.name.clone());
                }
                definitions.insert(tool.name.clone(), tool.clone());
            }
        }
        order
            .into_iter()
            .filter_map(|name| definitions.remove(&name))
            .collect()
    }

    /// Whether a tool name was declared twice with different definitions.
    ///
    /// Transports that reference tools by name cannot express this.
    pub fn has_tool_redefinitions(&self) -> bool {
        let mut declared = BTreeMap::<&str, &ToolDeclaration>::new();
        for update in self.updates() {
            for tool in &update.tools_added {
                if let Some(previous) = declared.insert(tool.name.as_str(), tool)
                    && previous != tool
                {
                    return true;
                }
            }
        }
        false
    }

    /// Whether tool history contains a removal or same-name redeclaration
    /// that an addition-only transport cannot replay.
    pub fn has_non_additive_tool_changes(&self) -> bool {
        let mut declared = BTreeSet::<&str>::new();
        for update in self.updates() {
            if !update.tools_removed.is_empty() {
                return true;
            }
            for tool in &update.tools_added {
                if !declared.insert(tool.name.as_str()) {
                    return true;
                }
            }
        }
        false
    }

    /// Resolve the transcript for one transport.
    pub fn resolve(&self, projection: ConfigurationProjection) -> ResolvedTranscript {
        let first_system = self
            .messages
            .iter()
            .position(|message| matches!(message, AgentMessage::System { .. }));
        match projection {
            ConfigurationProjection::Collapsed => ResolvedTranscript {
                leading: self.current_configuration(),
                messages: self
                    .messages
                    .iter()
                    .filter(|message| !matches!(message, AgentMessage::System { .. }))
                    .cloned()
                    .collect(),
                host_notes: self.host_notes.clone(),
            },
            ConfigurationProjection::InPlace => ResolvedTranscript {
                leading: self.initial_configuration(),
                messages: self
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != first_system)
                    .map(|(_, message)| message.clone())
                    .collect(),
                host_notes: self.host_notes.clone(),
            },
        }
    }

    fn updates(&self) -> impl Iterator<Item = &ConfigurationUpdate> {
        self.messages.iter().filter_map(|message| match message {
            AgentMessage::System { update, .. } => Some(update),
            _ => None,
        })
    }

    /// Prepare the conversation for replay to `target`.
    ///
    /// This applies upstream Pi's replay rules explicitly:
    ///
    /// - errored and aborted assistant turns are incomplete and are dropped;
    /// - provider-exposed thinking and its private replay material are kept
    ///   only for a response produced by the same physical model; for any
    ///   other model the thinking is dropped rather than flattened into
    ///   answer text (Pi converts it to text; Tea deliberately does not);
    /// - redacted thinking is kept only for the same model;
    /// - a tool call without a result receives an explicit error result before
    ///   the next user or assistant message; and
    /// - a system update between a tool call and its results is moved after
    ///   those results, because providers require results to follow calls.
    pub fn prepared_for(&self, target: Option<&ModelDescriptor>) -> Transcript {
        let mut output = Vec::with_capacity(self.messages.len());
        let mut pending_calls: Vec<AgentToolCall> = Vec::new();
        let mut answered = BTreeSet::<ToolCallId>::new();
        let mut held_system: Vec<AgentMessage> = Vec::new();
        let mut synthetic_id = self
            .messages
            .iter()
            .map(|message| message.id().0)
            .max()
            .unwrap_or(0);

        let mut close_pending =
            |output: &mut Vec<AgentMessage>,
             pending: &mut Vec<AgentToolCall>,
             answered: &mut BTreeSet<ToolCallId>,
             held: &mut Vec<AgentMessage>| {
                for call in pending.drain(..) {
                    if !answered.contains(&call.id) {
                        synthetic_id = synthetic_id.saturating_add(1);
                        output.push(AgentMessage::ToolResult {
                            id: MessageId(synthetic_id),
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            content: "No result provided".into(),
                            details: None,
                            usage: Box::new(None),
                            added_tool_names: Vec::new(),
                            terminate: false,
                            is_error: true,
                            failure: None,
                        });
                    }
                }
                answered.clear();
                output.append(held);
            };

        for message in &self.messages {
            match message {
                AgentMessage::Assistant {
                    id,
                    content,
                    tool_calls,
                    stop_reason,
                    error_message,
                    opaque_context,
                    origin,
                } => {
                    close_pending(
                        &mut output,
                        &mut pending_calls,
                        &mut answered,
                        &mut held_system,
                    );
                    if matches!(stop_reason, Some(StopReason::Error | StopReason::Aborted)) {
                        continue;
                    }
                    let same_model = same_physical_model(origin.as_ref(), target);
                    let content = content
                        .iter()
                        .filter_map(|block| replay_block(block, same_model))
                        .collect::<Vec<_>>();
                    pending_calls = tool_calls.clone();
                    output.push(AgentMessage::Assistant {
                        id: *id,
                        content,
                        tool_calls: tool_calls.clone(),
                        stop_reason: *stop_reason,
                        error_message: error_message.clone(),
                        opaque_context: opaque_context.clone(),
                        origin: origin.clone(),
                    });
                }
                AgentMessage::ToolResult { tool_call_id, .. } => {
                    answered.insert(tool_call_id.clone());
                    output.push(message.clone());
                }
                AgentMessage::System { .. } => {
                    if pending_calls.is_empty() {
                        output.push(message.clone());
                    } else {
                        held_system.push(message.clone());
                    }
                }
                AgentMessage::User { .. } => {
                    close_pending(
                        &mut output,
                        &mut pending_calls,
                        &mut answered,
                        &mut held_system,
                    );
                    output.push(message.clone());
                }
            }
        }
        close_pending(
            &mut output,
            &mut pending_calls,
            &mut answered,
            &mut held_system,
        );
        Transcript {
            messages: output,
            host_notes: self.host_notes.clone(),
        }
    }

    /// Canonical, identity-free JSON for durable request material, digests,
    /// and prompt-layout evidence.
    pub fn canonical_json(&self) -> JsonValue {
        JsonValue::object([
            (
                "messages",
                JsonValue::Array(self.messages.iter().map(canonical_message).collect()),
            ),
            (
                "host_notes",
                JsonValue::Array(
                    self.host_notes
                        .iter()
                        .map(|note| JsonValue::String(note.clone()))
                        .collect(),
                ),
            ),
        ])
    }

    /// Canonical newline-delimited message bytes for one projection.
    ///
    /// The leading configuration is excluded: prompt-layout evidence measures
    /// it as the separate system-prompt and tool components.
    pub fn layout_context(&self, projection: ConfigurationProjection) -> String {
        let resolved = self.resolve(projection);
        let mut output = String::new();
        for message in &resolved.messages {
            output.push_str(
                &canonical_message(message)
                    .to_json_string()
                    .expect("canonical transcript JSON is always encodable"),
            );
            output.push('\n');
        }
        for note in &resolved.host_notes {
            output.push_str(
                &JsonValue::object([("host_note", JsonValue::String(note.clone()))])
                    .to_json_string()
                    .expect("canonical transcript JSON is always encodable"),
            );
            output.push('\n');
        }
        output
    }
}

fn with_message_id(message: AgentMessage, id: MessageId) -> AgentMessage {
    match message {
        AgentMessage::User { content, .. } => AgentMessage::User { id, content },
        AgentMessage::Assistant {
            content,
            tool_calls,
            stop_reason,
            error_message,
            opaque_context,
            origin,
            ..
        } => AgentMessage::Assistant {
            id,
            content,
            tool_calls,
            stop_reason,
            error_message,
            opaque_context,
            origin,
        },
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            usage,
            added_tool_names,
            terminate,
            is_error,
            failure,
            ..
        } => AgentMessage::ToolResult {
            id,
            tool_call_id,
            tool_name,
            content,
            details,
            usage,
            added_tool_names,
            terminate,
            is_error,
            failure,
        },
        AgentMessage::System { update, .. } => AgentMessage::System { id, update },
    }
}

/// A user message for a standalone transcript; its identity is reassigned.
pub fn user_message(content: impl Into<String>) -> AgentMessage {
    AgentMessage::User {
        id: MessageId(0),
        content: content.into(),
    }
}

/// Whether two physical model identities are the same for replay purposes.
pub fn same_physical_model(
    origin: Option<&ModelDescriptor>,
    target: Option<&ModelDescriptor>,
) -> bool {
    match (origin, target) {
        (Some(origin), Some(target)) => {
            origin.provider == target.provider && origin.model == target.model
        }
        _ => false,
    }
}

fn replay_block(block: &AssistantContent, same_model: bool) -> Option<AssistantContent> {
    match block {
        AssistantContent::Text { text } => {
            (!text.is_empty()).then(|| AssistantContent::Text { text: text.clone() })
        }
        AssistantContent::Thinking { text, signature } => {
            if !same_model {
                return None;
            }
            if text.trim().is_empty() && signature.is_none() {
                return None;
            }
            Some(AssistantContent::Thinking {
                text: text.clone(),
                signature: signature.clone(),
            })
        }
        AssistantContent::RedactedThinking { .. } => same_model.then(|| block.clone()),
    }
}

/// Canonical identity-free JSON for one message.
///
/// Provider-private payloads are represented by digest and length only, so
/// layout evidence never carries opaque material while still distinguishing a
/// changed signature.
pub fn canonical_message(message: &AgentMessage) -> JsonValue {
    match message {
        AgentMessage::User { content, .. } => JsonValue::object([
            ("role", JsonValue::from("user")),
            ("content", JsonValue::String(content.clone())),
        ]),
        AgentMessage::Assistant {
            content,
            tool_calls,
            stop_reason,
            opaque_context,
            ..
        } => JsonValue::object([
            ("role", JsonValue::from("assistant")),
            (
                "content",
                JsonValue::Array(content.iter().map(canonical_block).collect()),
            ),
            (
                "tool_calls",
                JsonValue::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            JsonValue::object([
                                ("id", JsonValue::String(call.id.as_str().to_owned())),
                                ("name", JsonValue::String(call.name.clone())),
                                (
                                    "arguments",
                                    JsonValue::String(call.arguments.as_str().to_owned()),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "stop_reason",
                stop_reason
                    .map(|reason| JsonValue::String(format!("{reason:?}")))
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "opaque_context",
                JsonValue::Array(
                    opaque_context
                        .iter()
                        .map(|item| {
                            JsonValue::object([
                                ("provider", JsonValue::String(item.provider().to_owned())),
                                ("kind", JsonValue::String(item.kind().to_owned())),
                                ("payload_digest", opaque_digest(item.payload())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            is_error,
            ..
        } => JsonValue::object([
            ("role", JsonValue::from("tool_result")),
            (
                "tool_call_id",
                JsonValue::String(tool_call_id.as_str().to_owned()),
            ),
            ("tool_name", JsonValue::String(tool_name.clone())),
            ("content", JsonValue::String(content.clone())),
            (
                "details",
                details
                    .as_ref()
                    .map(|details| JsonValue::String(details.as_str().to_owned()))
                    .unwrap_or(JsonValue::Null),
            ),
            ("is_error", JsonValue::Bool(*is_error)),
        ]),
        AgentMessage::System { update, .. } => JsonValue::object([
            ("role", JsonValue::from("system")),
            ("update", configuration_update_json(update)),
        ]),
    }
}

fn canonical_block(block: &AssistantContent) -> JsonValue {
    match block {
        AssistantContent::Text { text } => JsonValue::object([
            ("type", JsonValue::from("text")),
            ("text", JsonValue::String(text.clone())),
        ]),
        AssistantContent::Thinking { text, signature } => JsonValue::object([
            ("type", JsonValue::from("thinking")),
            ("text", JsonValue::String(text.clone())),
            (
                "signature_digest",
                signature
                    .as_ref()
                    .map(|signature| opaque_digest(signature.payload()))
                    .unwrap_or(JsonValue::Null),
            ),
        ]),
        AssistantContent::RedactedThinking { data } => JsonValue::object([
            ("type", JsonValue::from("redacted_thinking")),
            ("data_digest", opaque_digest(data.payload())),
        ]),
    }
}

fn opaque_digest(payload: &str) -> JsonValue {
    JsonValue::String(tea_session::Digest::from_bytes(payload.as_bytes()).to_hex())
}

/// Canonical JSON for one configuration update.
pub fn configuration_update_json(update: &ConfigurationUpdate) -> JsonValue {
    JsonValue::object([
        (
            "sections",
            JsonValue::Array(
                update
                    .sections
                    .iter()
                    .map(|change| {
                        JsonValue::object([
                            ("id", JsonValue::String(change.id.clone())),
                            (
                                "content",
                                change
                                    .content
                                    .clone()
                                    .map(JsonValue::String)
                                    .unwrap_or(JsonValue::Null),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "tools_added",
            JsonValue::Array(update.tools_added.iter().map(tool_declaration_json).collect()),
        ),
        (
            "tools_removed",
            JsonValue::Array(
                update
                    .tools_removed
                    .iter()
                    .map(|name| JsonValue::String(name.clone()))
                    .collect(),
            ),
        ),
    ])
}

/// Decode [`configuration_update_json`].
pub fn configuration_update_from_json(value: &JsonValue) -> Result<ConfigurationUpdate, String> {
    let object = value
        .as_object()
        .ok_or("configuration update must be an object")?;
    let array = |field: &str| {
        object
            .get(field)
            .and_then(JsonValue::as_array)
            .ok_or_else(|| format!("configuration update field {field:?} must be an array"))
    };
    let string = |value: &JsonValue, field: &str| {
        value
            .get(field)
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("configuration update field {field:?} must be a string"))
    };
    let sections = array("sections")?
        .iter()
        .map(|change| {
            Ok(crate::state::SectionChange {
                id: string(change, "id")?,
                content: match change.get("content") {
                    None | Some(JsonValue::Null) => None,
                    Some(_) => Some(string(change, "content")?),
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let tools_added = array("tools_added")?
        .iter()
        .map(|tool| {
            Ok(ToolDeclaration::new(
                string(tool, "name")?,
                string(tool, "description")?,
                tool.get("schema")
                    .cloned()
                    .ok_or("tool declaration has no schema")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let tools_removed = array("tools_removed")?
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "removed tool name must be a string".to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let update = ConfigurationUpdate {
        sections,
        tools_added,
        tools_removed,
    };
    update.validate().map_err(|error| error.to_string())?;
    Ok(update)
}

/// Canonical JSON for one model-visible tool declaration.
pub fn tool_declaration_json(tool: &ToolDeclaration) -> JsonValue {
    JsonValue::object([
        ("name", JsonValue::String(tool.name.clone())),
        ("description", JsonValue::String(tool.description.clone())),
        ("schema", tool.schema.clone()),
    ])
}

#[cfg(test)]
mod tests;
