//! Concrete model-provider adapters and checked-in model catalogs.
//!
//! The provider-independent ports remain in [`tea_core::scheduler`]. This crate owns concrete
//! transports, wire formats, retry behavior, and catalog data behind explicit Cargo features.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::result_large_err)]

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod error {
    pub use tea_core::error::*;
}
mod scheduler {
    pub use tea_core::scheduler::*;
}
mod state {
    pub use tea_core::state::*;
}
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex",
    feature = "provider-anthropic"
))]
mod transcript {
    pub use tea_core::transcript::*;
}
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod tool {
    pub use tea_core::tool::*;
}

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod json;

mod registry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod retry;
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
mod transport_runtime;

#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-local",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
pub mod openai;

pub use registry::{
    ConfiguredProvider, MODEL_CATALOG_VERSION, ModelDescriptor, ModelSelection,
    ProviderCapabilities, ProviderConfiguration, ProviderConfigurationKind, ProviderEntry,
    ProviderRegistry, RegistryError,
};
#[cfg(any(
    feature = "provider-openrouter",
    feature = "provider-opencode-zen",
    feature = "provider-codex"
))]
pub use retry::RetryPolicy;

#[cfg(feature = "provider-codex")]
pub mod codex;
#[cfg(feature = "provider-local")]
pub mod local;
#[cfg(feature = "provider-opencode-zen")]
pub mod opencode_zen;
#[cfg(feature = "provider-openrouter")]
pub mod openrouter;

#[cfg(all(
    test,
    any(
        feature = "provider-openrouter",
        feature = "provider-local",
        feature = "provider-opencode-zen",
        feature = "provider-codex",
        feature = "provider-anthropic"
    )
))]
pub(crate) mod test_support {
    //! Typed request fixtures for adapter tests.

    use crate::json::JsonValue;
    use crate::state::{AgentMessage, AgentToolCall, MessageId, SerializedJson, ToolCallId};
    use crate::tool::ToolDefinition;
    use crate::transcript::Transcript;

    /// Build a typed transcript from a system prompt, tools, and a compact
    /// Chat-style message list (`user`, `assistant`, `tool` roles).
    pub(crate) fn transcript(system: &str, tools: &[ToolDefinition], chat: &str) -> Transcript {
        let chat = JsonValue::parse(chat).expect("fixture chat JSON");
        let messages = chat
            .as_array()
            .expect("fixture chat array")
            .iter()
            .map(|message| {
                let text = |field: &str| {
                    message
                        .get(field)
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                match message.get("role").and_then(JsonValue::as_str) {
                    Some("user") => AgentMessage::User {
                        id: MessageId(0),
                        content: text("content"),
                    },
                    Some("assistant") => AgentMessage::Assistant {
                        id: MessageId(0),
                        content: crate::state::text_content(text("content")),
                        tool_calls: message
                            .get("tool_calls")
                            .and_then(JsonValue::as_array)
                            .unwrap_or_default()
                            .iter()
                            .map(|call| AgentToolCall {
                                id: ToolCallId::new(
                                    call.get("id").and_then(JsonValue::as_str).unwrap_or("call"),
                                )
                                .expect("fixture call ID"),
                                name: call
                                    .get("function")
                                    .and_then(|function| function.get("name"))
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                arguments: SerializedJson::new(
                                    call.get("function")
                                        .and_then(|function| function.get("arguments"))
                                        .and_then(JsonValue::as_str)
                                        .unwrap_or("{}"),
                                ),
                            })
                            .collect(),
                        stop_reason: None,
                        error_message: None,
                        opaque_context: Vec::new(),
                        origin: None,
                    },
                    Some("tool") => AgentMessage::ToolResult {
                        id: MessageId(0),
                        tool_call_id: ToolCallId::new(text("tool_call_id"))
                            .expect("fixture result call ID"),
                        tool_name: "fixture".into(),
                        content: text("content"),
                        details: None,
                        usage: Box::new(None),
                        added_tool_names: Vec::new(),
                        terminate: false,
                        is_error: false,
                        failure: None,
                    },
                    other => panic!("unsupported fixture role {other:?}"),
                }
            })
            .collect::<Vec<_>>();
        Transcript::standalone(
            system,
            tools.iter().map(ToolDefinition::declaration).collect(),
            messages,
        )
    }
}
