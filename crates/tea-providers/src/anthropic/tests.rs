//! Anthropic adapter tests ported from upstream Pi.
//!
//! Each test names its upstream source (`packages/ai/test/*.test.ts`). Payload
//! tests call the real request builder; stream tests drive the real adapter
//! through `tea-http` against a loopback HTTP/1.1 server that delivers bytes in
//! arbitrary fragments. Expected values are written out independently of the
//! implementation.

mod fixture;
mod payload_tests;
mod stream_tests;

use super::*;
use crate::state::{
    AgentMessage, AgentToolCall, AssistantContent, ConfigurationUpdate, MessageId,
    OpaqueProviderContextItem, SectionChange, SerializedJson, ThinkingLevel, ToolCallId,
};
use crate::tool::ToolDeclaration;
use crate::transcript::Transcript;
use tea_protocol::JsonValue;

fn descriptor(model: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: PROVIDER_ID.into(),
        model: model.into(),
        revision: None,
    }
}

fn tool(name: &str) -> ToolDeclaration {
    ToolDeclaration::new(
        name,
        format!("{name} tool"),
        JsonValue::object([
            ("type", JsonValue::from("object")),
            (
                "properties",
                JsonValue::object([(
                    "path",
                    JsonValue::object([("type", JsonValue::from("string"))]),
                )]),
            ),
            ("required", JsonValue::Array(vec![JsonValue::from("path")])),
        ]),
    )
}

fn system(id: u64, sections: &[(&str, Option<&str>)], added: &[ToolDeclaration], removed: &[&str]) -> AgentMessage {
    AgentMessage::System {
        id: MessageId(id),
        update: ConfigurationUpdate {
            sections: sections
                .iter()
                .map(|(id, content)| SectionChange {
                    id: (*id).into(),
                    content: content.map(str::to_owned),
                })
                .collect(),
            tools_added: added.to_vec(),
            tools_removed: removed.iter().map(|name| (*name).to_owned()).collect(),
        },
    }
}

fn user(id: u64, text: &str) -> AgentMessage {
    AgentMessage::User {
        id: MessageId(id),
        content: text.into(),
    }
}

fn assistant(
    id: u64,
    origin: Option<&str>,
    content: Vec<AssistantContent>,
    tool_calls: Vec<AgentToolCall>,
    opaque_context: Vec<OpaqueProviderContextItem>,
) -> AgentMessage {
    AgentMessage::Assistant {
        id: MessageId(id),
        content,
        tool_calls,
        stop_reason: Some(crate::state::StopReason::Stop),
        error_message: None,
        opaque_context,
        origin: origin.map(descriptor),
    }
}

fn signature(payload: &str) -> OpaqueProviderContextItem {
    OpaqueProviderContextItem::new(PROVIDER_ID, payload::SIGNATURE_CONTEXT_KIND, None, payload)
        .expect("bounded signature")
}

fn request(model: &str, messages: Vec<AgentMessage>, thinking: ThinkingLevel) -> ModelRequest {
    ModelRequest {
        transcript: Transcript::new(messages),
        model: Some(descriptor(model)),
        thinking_level: thinking,
        ..ModelRequest::default()
    }
}

fn call(id: &str, name: &str) -> AgentToolCall {
    AgentToolCall {
        id: ToolCallId::new(id).expect("call id"),
        name: name.into(),
        arguments: SerializedJson::new(r#"{"path":"README.md"}"#),
    }
}
