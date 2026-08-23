//! Recoverable tool-result retention and deterministic model projections.

use std::fmt;
use tea_core::tool::AgentToolResult;
use tea_protocol::JsonValue;
use tea_session::{
    ArtifactError, ArtifactId, ArtifactPolicy, ArtifactPolicyId, ArtifactStore, PayloadRef,
};

const TOOL_RESULT_MEDIA_TYPE: &str = "application/vnd.tea.tool-result+json";
const PROJECTION_STRATEGY_ID: &str = "tea-recoverable-tool-result-v1";
const DIRECT_READER_PROJECTION_STRATEGY_ID: &str = "tea-direct-recovery-page-v1";
const MEDIUM_HEAD_BYTES: usize = 4_096;
const MEDIUM_TAIL_BYTES: usize = 1_024;

/// Complete retention and bounded model-facing projection prepared together.
///
/// The caller persists `full_result` before exposing `model_projection` to a
/// core transcript. This prevents a projected locator from pointing at bytes
/// that were never durably retained.
#[derive(Clone, Debug, PartialEq)]
pub struct RetainedToolResult {
    /// Complete redacted canonical tool outcome.
    pub full_result: PayloadRef,
    /// Exact policy selected by the immutable harness snapshot.
    pub artifact_policy_id: ArtifactPolicyId,
    /// Bounded model-visible object whose `content` field is safe to restore.
    pub model_projection: JsonValue,
    /// Stable projection algorithm identity for the semantic entry.
    pub projection_strategy_id: String,
}

/// Errors while retaining complete data or constructing its projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolResultRetentionError {
    /// The selected policy cannot safely expose this result.
    Policy { message: String },
    /// The immutable artifact store rejected the complete payload.
    Artifact(ArtifactError),
    /// A core-owned structured detail payload was not valid JSON.
    InvalidDetails { message: String },
    /// The protocol value could not produce canonical JSON bytes.
    Encode { message: String },
}

impl fmt::Display for ToolResultRetentionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy { message } => write!(formatter, "invalid artifact policy: {message}"),
            Self::Artifact(error) => error.fmt(formatter),
            Self::InvalidDetails { message } => {
                write!(
                    formatter,
                    "tool result details are not valid JSON: {message}"
                )
            }
            Self::Encode { message } => {
                write!(formatter, "cannot encode canonical tool result: {message}")
            }
        }
    }
}

impl std::error::Error for ToolResultRetentionError {}

impl From<ArtifactError> for ToolResultRetentionError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

/// Retain a complete result and build a deterministic bounded view from the
/// same value.
///
/// This compatibility helper is appropriate when no policy changes the
/// model-visible projection. Durable execution uses
/// [`retain_tool_result_with_projection`] so raw capability evidence remains
/// recoverable even when a post-tool policy redacts its model-facing view.
/// Retain exact raw capability evidence before exposing a potentially changed
/// model projection.
///
/// `raw_result` is never rewritten by this function. `model_result` may have
/// been produced by a bounded policy hook, but it cannot alter the artifact or
/// inline full result that recovery uses to explain the completed effect.
pub fn retain_tool_result_with_projection(
    store: &dyn ArtifactStore,
    policy: &ArtifactPolicy,
    raw_result: &AgentToolResult,
    model_result: &AgentToolResult,
) -> Result<RetainedToolResult, ToolResultRetentionError> {
    policy.validate()?;
    if !policy.model_readable {
        return Err(ToolResultRetentionError::Policy {
            message: "a model-visible tool result requires a model-readable artifact policy".into(),
        });
    }
    let raw_details = result_details(raw_result)?;
    let model_details = result_details(model_result)?;
    let full_value = full_value(raw_result, raw_details);
    let canonical =
        full_value
            .to_json_string()
            .map_err(|error| ToolResultRetentionError::Encode {
                message: error.to_string(),
            })?;
    if canonical.len() <= policy.maximum_inline_bytes {
        return Ok(RetainedToolResult {
            full_result: PayloadRef::Inline(full_value),
            artifact_policy_id: policy.policy_id.clone(),
            model_projection: inline_projection(&model_result.content, model_details),
            projection_strategy_id: PROJECTION_STRATEGY_ID.into(),
        });
    }

    let descriptor = store.put(canonical.as_bytes(), TOOL_RESULT_MEDIA_TYPE)?;
    Ok(RetainedToolResult {
        full_result: PayloadRef::Artifact {
            artifact_id: descriptor.artifact_id,
            byte_len: descriptor.byte_len,
            media_type: descriptor.media_type,
        },
        artifact_policy_id: policy.policy_id.clone(),
        model_projection: artifact_projection(&model_result.content, descriptor.artifact_id),
        projection_strategy_id: PROJECTION_STRATEGY_ID.into(),
    })
}

/// Retain a bounded direct recovery-tool response without recursively creating
/// another locator while preserving a raw/model result split.
pub(crate) fn retain_direct_recovery_result_with_projection(
    policy: &ArtifactPolicy,
    raw_result: &AgentToolResult,
    model_result: &AgentToolResult,
) -> Result<RetainedToolResult, ToolResultRetentionError> {
    policy.validate()?;
    Ok(RetainedToolResult {
        full_result: PayloadRef::Inline(full_value(raw_result, result_details(raw_result)?)),
        artifact_policy_id: policy.policy_id.clone(),
        model_projection: inline_projection(&model_result.content, result_details(model_result)?),
        projection_strategy_id: DIRECT_READER_PROJECTION_STRATEGY_ID.into(),
    })
}

fn result_details(result: &AgentToolResult) -> Result<JsonValue, ToolResultRetentionError> {
    result
        .details
        .as_ref()
        .map(|details| {
            JsonValue::parse(details.as_str()).map_err(|error| {
                ToolResultRetentionError::InvalidDetails {
                    message: error.to_string(),
                }
            })
        })
        .transpose()
        .map(|value| value.unwrap_or(JsonValue::Null))
}

fn full_value(result: &AgentToolResult, details: JsonValue) -> JsonValue {
    JsonValue::object([
        ("content", JsonValue::String(result.content.clone())),
        ("details", details.clone()),
        (
            "failure",
            result
                .failure
                .as_ref()
                .map(|failure| JsonValue::String(format!("{:?}", failure.disposition())))
                .unwrap_or(JsonValue::Null),
        ),
        ("is_error", JsonValue::Bool(result.is_error)),
        ("terminate", JsonValue::Bool(result.terminate)),
    ])
}

/// Return the stable model-facing content encoded by a retained projection.
pub(crate) fn projection_content(
    projection: &JsonValue,
) -> Result<(String, Option<String>), ToolResultRetentionError> {
    let content = projection
        .get("content")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ToolResultRetentionError::Policy {
            message: "tool-result model projection has no string content".into(),
        })?
        .to_owned();
    let details = projection
        .get("details")
        .filter(|details| !details.is_null())
        .map(|details| {
            details
                .to_json_string()
                .map_err(|error| ToolResultRetentionError::Encode {
                    message: error.to_string(),
                })
        })
        .transpose()?;
    Ok((content, details))
}

fn inline_projection(content: &str, details: JsonValue) -> JsonValue {
    JsonValue::object([
        ("content", JsonValue::String(content.into())),
        ("details", details),
    ])
}

fn artifact_projection(content: &str, artifact_id: ArtifactId) -> JsonValue {
    let content_bytes = content.len();
    let head_limit = MEDIUM_HEAD_BYTES.min(content_bytes);
    let head_end = utf8_prefix_boundary(content, head_limit);
    let tail_limit = MEDIUM_TAIL_BYTES.min(content_bytes.saturating_sub(head_end));
    let tail_start = utf8_suffix_boundary(content, tail_limit);
    let locator = if head_end < tail_start {
        format!(
            "[full tool result: tea-artifact://blake3/{artifact_id}; preview omits bytes {head_end}..{tail_start}; use tea_artifact_search or tea_artifact_read]"
        )
    } else {
        format!(
            "[full tool result: tea-artifact://blake3/{artifact_id}; complete structured result is available with tea_artifact_read]"
        )
    };
    let mut preview = String::with_capacity(
        locator
            .len()
            .saturating_add(head_end)
            .saturating_add(content.len().saturating_sub(tail_start))
            .saturating_add(2),
    );
    preview.push_str(&locator);
    preview.push('\n');
    preview.push_str(&content[..head_end]);
    if head_end < tail_start {
        preview.push('\n');
        preview.push_str("[... omitted; see locator above ...]\n");
        preview.push_str(&content[tail_start..]);
    }
    JsonValue::object([
        ("content", JsonValue::String(preview)),
        (
            "recovery_locator",
            JsonValue::String(format!("tea-artifact://blake3/{artifact_id}")),
        ),
    ])
}

fn utf8_prefix_boundary(value: &str, maximum_bytes: usize) -> usize {
    let mut boundary = maximum_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    boundary
}

fn utf8_suffix_boundary(value: &str, maximum_bytes: usize) -> usize {
    let mut boundary = value.len().saturating_sub(maximum_bytes.min(value.len()));
    while boundary < value.len() && !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_add(1);
    }
    boundary
}
