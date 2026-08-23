//! Shared argument parsing and result formatting for standard coding tools.

use super::contract::OperationError;
use crate::error::ToolError;
use crate::tool::{AgentToolResult, ToolCall, ToolContext};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tea_protocol::{JsonNumber, JsonValue};

pub(crate) const MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub(crate) const MAX_OUTPUT_LINES: usize = 2_000;

pub(crate) fn invalid(name: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        tool: name.to_owned(),
        message: message.into(),
    }
}

pub(crate) fn operation_failure(name: &str, error: OperationError) -> ToolError {
    if error.message() == "cancelled" {
        ToolError::Cancelled {
            tool: name.to_owned(),
        }
    } else {
        ToolError::Execution {
            tool: name.to_owned(),
            message: error.to_string(),
        }
    }
}

pub(crate) fn result_ok(call: &ToolCall, content: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id.clone(),
        content: content.into(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

pub(crate) fn parse_object(
    name: &str,
    call: &ToolCall,
) -> Result<Arc<BTreeMap<String, JsonValue>>, ToolError> {
    match JsonValue::parse(call.arguments.as_str()) {
        Ok(JsonValue::Object(value)) => Ok(Arc::new(value)),
        Ok(_) => Err(invalid(name, "arguments must be a JSON object")),
        Err(_) => Err(invalid(name, "arguments must be valid JSON")),
    }
}

pub(crate) fn field<'a>(
    name: &str,
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<&'a JsonValue, ToolError> {
    object
        .get(key)
        .ok_or_else(|| invalid(name, format!("missing required argument {key:?}")))
}

pub(crate) fn string_field(
    name: &str,
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<String, ToolError> {
    match field(name, object, key)? {
        JsonValue::String(value) => Ok(value.clone()),
        _ => Err(invalid(name, format!("argument {key:?} must be a string"))),
    }
}

pub(crate) fn optional_string(
    name: &str,
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<String>, ToolError> {
    object
        .get(key)
        .map(|value| match value {
            JsonValue::String(value) => Ok(value.clone()),
            _ => Err(invalid(name, format!("argument {key:?} must be a string"))),
        })
        .transpose()
}

pub(crate) fn optional_bool(
    name: &str,
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<bool>, ToolError> {
    object
        .get(key)
        .map(|value| match value {
            JsonValue::Bool(value) => Ok(*value),
            _ => Err(invalid(name, format!("argument {key:?} must be a boolean"))),
        })
        .transpose()
}

pub(crate) fn optional_positive_usize(
    name: &str,
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<usize>, ToolError> {
    object
        .get(key)
        .map(|value| number_to_usize(name, key, value))
        .transpose()
}

pub(crate) fn number_to_usize(
    name: &str,
    key: &str,
    value: &JsonValue,
) -> Result<usize, ToolError> {
    let number = match value {
        JsonValue::Number(number) => *number,
        _ => return Err(invalid(name, format!("argument {key:?} must be a number"))),
    };
    let integer = match number {
        JsonNumber::Unsigned(value) if value > 0 => value,
        JsonNumber::Signed(value) if value > 0 => value as u64,
        JsonNumber::Float(value) if value.is_finite() && value > 0.0 && value.fract() == 0.0 => {
            value as u64
        }
        _ => {
            return Err(invalid(
                name,
                format!("argument {key:?} must be a positive integer"),
            ));
        }
    };
    usize::try_from(integer).map_err(|_| invalid(name, format!("argument {key:?} is too large")))
}

pub(crate) fn optional_timeout(
    name: &str,
    object: &BTreeMap<String, JsonValue>,
) -> Result<Option<f64>, ToolError> {
    let Some(value) = object.get("timeout") else {
        return Ok(None);
    };
    let number = match value {
        JsonValue::Number(JsonNumber::Float(value)) => *value,
        JsonValue::Number(JsonNumber::Unsigned(value)) => *value as f64,
        JsonValue::Number(JsonNumber::Signed(value)) => *value as f64,
        _ => return Err(invalid(name, "argument \"timeout\" must be a number")),
    };
    if !number.is_finite() || number <= 0.0 || number > 2_147_483.647 {
        return Err(invalid(
            name,
            "timeout must be a finite positive number no greater than 2147.483647 seconds",
        ));
    }
    Ok(Some(number))
}

pub(crate) fn path_error(
    name: &str,
    path: Result<PathBuf, OperationError>,
) -> Result<PathBuf, ToolError> {
    path.map_err(|error| operation_failure(name, error))
}

pub(crate) fn check_cancelled(name: &str, context: &ToolContext) -> Result<(), ToolError> {
    if context.cancellation.is_cancelled() {
        Err(ToolError::Cancelled {
            tool: name.to_owned(),
        })
    } else {
        Ok(())
    }
}

pub(crate) fn truncate_output(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut truncated = false;
    if lines.len() > MAX_OUTPUT_LINES {
        truncated = true;
        lines = lines.split_off(lines.len() - MAX_OUTPUT_LINES);
    }
    let mut output = lines.join("\n");
    if output.len() > MAX_OUTPUT_BYTES {
        truncated = true;
        let mut start = output.len() - MAX_OUTPUT_BYTES;
        while start < output.len() && !output.is_char_boundary(start) {
            start += 1;
        }
        output = output[start..].to_owned();
    }
    (output, truncated)
}

/// Truncate file content from the head, preserving complete UTF-8 lines.
///
/// Bash intentionally keeps its tail because the final diagnostics are normally most useful;
/// read follows Pi's file-oriented behavior and keeps the beginning instead.  Keeping this
/// separate from [`truncate_output`] also prevents a byte limit from slicing a UTF-8 character.
pub(crate) fn truncate_read_output(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if text.ends_with('\n') {
        lines.pop();
    }
    let total_bytes = text.len();
    if lines.len() <= MAX_OUTPUT_LINES && total_bytes <= MAX_OUTPUT_BYTES {
        return (text, false);
    }

    let mut output = String::new();
    for (output_lines, line) in lines.into_iter().take(MAX_OUTPUT_LINES).enumerate() {
        let separator = if output_lines == 0 { 0 } else { 1 };
        if output.len() + separator + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        if separator != 0 {
            output.push('\n');
        }
        output.push_str(line);
    }
    (output, true)
}
