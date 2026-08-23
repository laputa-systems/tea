use std::collections::{BTreeMap, BTreeSet};
use tea_core::queue::QueueMode;
use tea_core::scheduler::{ModelStream, ModelStreamEvent};
use tea_core::state::{
    AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ThinkingLevel, ToolCallId,
};
use tea_core::tool::ToolExecutionMode;
use tea_protocol::{JsonNumber, JsonValue};

use super::fixture::{
    Fixture, FixtureAction, FixtureActiveQueueArrival, FixtureAfterToolReplace,
    FixtureBeforeToolPolicy, FixtureContextHooks, FixtureModelStream, FixtureToolResponse,
    FixtureToolSpec, FixtureUsage,
};

impl Fixture {
    pub(super) fn parse(input: &str) -> Result<Self, String> {
        let fixture = JsonValue::parse(input).map_err(|error| error.to_string())?;
        let root = object(&fixture, "fixture")?;
        if number_field(root, "format_version")? != 1
            || string_field(root, "kind")? != "declarative_parity_fixture"
        {
            return Err("expected format_version 1 declarative_parity_fixture".into());
        }
        let setup = object(field(root, "setup")?, "setup")?;
        let model = object(field(setup, "model")?, "setup.model")?;
        let host = object(field(root, "host")?, "host")?;
        let tools = parse_tools(setup, host)?;

        let actions = parse_actions(field(root, "actions")?)?;

        let (streams, last_usage, last_stop_reason) =
            parse_model_script(field(root, "model_script")?)?;
        Ok(Self {
            id: string_field(root, "id")?.to_owned(),
            system_prompt: string_field(setup, "system_prompt")?.to_owned(),
            provider: string_field(model, "provider")?.to_owned(),
            model: string_field(model, "id")?.to_owned(),
            thinking_level: parse_thinking_level(string_field(setup, "thinking_level")?)?,
            steering_mode: parse_queue_mode(setup.get("steering_mode"))?,
            follow_up_mode: parse_queue_mode(setup.get("follow_up_mode"))?,
            actions,
            before_tool_policy: parse_before_tool_policy(host)?,
            after_tool_replace: parse_after_tool_replace(host)?,
            context_hooks: parse_context_hooks(setup)?,
            should_stop_after_turn: match host.get("should_stop_after_turn") {
                None => false,
                Some(JsonValue::Bool(value)) => *value,
                Some(_) => return Err("host.should_stop_after_turn must be a boolean".into()),
            },
            hold_agent_end_observer: parse_hold_agent_end_observer(host)?,
            tools,
            streams,
            last_usage,
            last_stop_reason,
        })
    }
}

fn parse_hold_agent_end_observer(host: &BTreeMap<String, JsonValue>) -> Result<bool, String> {
    let Some(observer) = host.get("observer") else {
        return Ok(false);
    };
    let observer = object(observer, "host.observer")?;
    match observer.get("hold_agent_end") {
        Some(JsonValue::Bool(true)) => Ok(true),
        Some(JsonValue::Bool(false)) => {
            Err("host.observer.hold_agent_end must be true in the v1 fixture adapter".into())
        }
        Some(_) => Err("host.observer.hold_agent_end must be a boolean".into()),
        None => Err("host.observer.hold_agent_end is required".into()),
    }
}

fn parse_before_tool_policy(
    host: &BTreeMap<String, JsonValue>,
) -> Result<Option<FixtureBeforeToolPolicy>, String> {
    let Some(rule) = host.get("before_tool_call") else {
        return Ok(None);
    };
    let rule = object(rule, "host.before_tool_call")?;
    let yield_once = match rule.get("yield_once") {
        None => false,
        Some(JsonValue::Bool(value)) => *value,
        Some(_) => return Err("host.before_tool_call.yield_once must be a boolean".into()),
    };
    let cancel_after_yield = match rule.get("cancel_after_yield") {
        None => false,
        Some(JsonValue::Bool(value)) => *value,
        Some(_) => return Err("host.before_tool_call.cancel_after_yield must be a boolean".into()),
    };
    if cancel_after_yield && !yield_once {
        return Err("host.before_tool_call.cancel_after_yield requires yield_once".into());
    }
    Ok(Some(FixtureBeforeToolPolicy {
        tool_name: string_field(rule, "tool_name")?.to_owned(),
        reason: string_field(rule, "reason")?.to_owned(),
        terminate: match rule.get("terminate") {
            None => false,
            Some(JsonValue::Bool(value)) => *value,
            Some(_) => return Err("host.before_tool_call.terminate must be a boolean".into()),
        },
        yield_once,
        cancel_after_yield,
    }))
}

fn parse_after_tool_replace(
    host: &BTreeMap<String, JsonValue>,
) -> Result<Option<FixtureAfterToolReplace>, String> {
    let Some(rule) = host.get("after_tool_call") else {
        return Ok(None);
    };
    let rule = object(rule, "host.after_tool_call")?;
    Ok(Some(FixtureAfterToolReplace {
        tool_name: string_field(rule, "tool_name")?.to_owned(),
        content: string_field(rule, "content")?.to_owned(),
        is_error: bool_field(rule, "is_error")?,
        terminate: match rule.get("terminate") {
            None => None,
            Some(JsonValue::Bool(value)) => Some(*value),
            Some(_) => return Err("host.after_tool_call.terminate must be a boolean".into()),
        },
    }))
}

fn parse_context_hooks(
    setup: &BTreeMap<String, JsonValue>,
) -> Result<Option<FixtureContextHooks>, String> {
    let Some(value) = setup.get("context_hooks") else {
        return Ok(None);
    };
    let value = object(value, "setup.context_hooks")?;
    let host_messages = string_array(
        field(value, "host_messages")?,
        "setup.context_hooks.host_messages",
    )?;
    let transform_append_host_message =
        string_field(value, "transform_append_host_message")?.to_owned();
    let convert_prefix = string_field(value, "convert_prefix")?.to_owned();
    let next = object(
        field(value, "prepare_next_turn")?,
        "setup.context_hooks.prepare_next_turn",
    )?;
    let next_host_messages = string_array(
        field(next, "host_messages")?,
        "setup.context_hooks.prepare_next_turn.host_messages",
    )?;
    let next_model = object(
        field(next, "model")?,
        "setup.context_hooks.prepare_next_turn.model",
    )?;
    Ok(Some(FixtureContextHooks {
        host_messages,
        transform_append_host_message,
        convert_prefix,
        next_host_messages,
        next_model: ModelDescriptor {
            provider: string_field(next_model, "provider")?.to_owned(),
            model: string_field(next_model, "id")?.to_owned(),
            revision: None,
        },
        next_thinking_level: parse_thinking_level(string_field(next, "thinking_level")?)?,
    }))
}

fn string_array(value: &JsonValue, path: &str) -> Result<Vec<String>, String> {
    array(value, path)?
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            JsonValue::String(value) => Ok(value.clone()),
            _ => Err(format!("{path}[{index}] must be a string")),
        })
        .collect()
}

fn parse_actions(value: &JsonValue) -> Result<Vec<FixtureAction>, String> {
    let actions = array(value, "actions")?;
    if actions.is_empty() {
        return Err("the v1 runner requires at least one action".into());
    }
    let parsed = actions
        .iter()
        .map(|action| {
            let action = object(action, "fixture action")?;
            match string_field(action, "kind")? {
                "steer" => Ok(FixtureAction::Steer(
                    string_field(action, "text")?.to_owned(),
                )),
                "follow_up" => Ok(FixtureAction::FollowUp(
                    string_field(action, "text")?.to_owned(),
                )),
                "prompt" => Ok(FixtureAction::Prompt(
                    string_field(action, "text")?.to_owned(),
                )),
                "continue" => Ok(FixtureAction::Continue),
                kind => Err(format!("the v1 runner does not support action {kind:?}")),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !parsed
        .iter()
        .any(|action| matches!(action, FixtureAction::Prompt(_) | FixtureAction::Continue))
    {
        return Err("the v1 runner requires an action that starts a run".into());
    }
    Ok(parsed)
}

fn parse_queue_mode(value: Option<&JsonValue>) -> Result<QueueMode, String> {
    match value {
        None => Ok(QueueMode::OneAtATime),
        Some(JsonValue::String(value)) if value == "all" => Ok(QueueMode::All),
        Some(JsonValue::String(value)) if value == "one-at-a-time" => Ok(QueueMode::OneAtATime),
        Some(_) => Err("setup queue mode must be all or one-at-a-time".into()),
    }
}

fn parse_tools(
    setup: &BTreeMap<String, JsonValue>,
    host: &BTreeMap<String, JsonValue>,
) -> Result<Vec<FixtureToolSpec>, String> {
    let mut host_responses = BTreeMap::<String, Vec<FixtureToolResponse>>::new();
    for (index, entry) in array(field(host, "tools")?, "host.tools")?
        .iter()
        .enumerate()
    {
        let entry = object(entry, "host.tools entry")?;
        let name = string_field(entry, "name")?.to_owned();
        if host_responses.contains_key(&name) {
            return Err(format!("host.tools[{index}] repeats {name:?}"));
        }
        let responses = array(field(entry, "calls")?, "host.tools calls")?
            .iter()
            .map(parse_tool_response)
            .collect::<Result<Vec<_>, _>>()?;
        host_responses.insert(name, responses);
    }

    let mut names = BTreeSet::new();
    array(field(setup, "tools")?, "setup.tools")?
        .iter()
        .map(|entry| {
            let entry = object(entry, "setup.tools entry")?;
            let name = string_field(entry, "name")?.to_owned();
            if !names.insert(name.clone()) {
                return Err(format!("setup.tools repeats {name:?}"));
            }
            Ok(FixtureToolSpec {
                description: string_field(entry, "description")?.to_owned(),
                execution_mode: parse_tool_execution_mode(entry.get("execution_mode"))?,
                parameters: SerializedJson::new(
                    field(entry, "parameters")?
                        .to_json_string()
                        .map_err(|error| error.to_string())?,
                ),
                responses: host_responses.remove(&name).unwrap_or_default(),
                name,
            })
        })
        .collect()
}

fn parse_tool_response(value: &JsonValue) -> Result<FixtureToolResponse, String> {
    let value = object(value, "host tool call")?;
    let result = object(field(value, "result")?, "host tool result")?;
    let content = array(field(result, "content")?, "host tool result.content")?;
    if content.len() != 1 {
        return Err(
            "the v1 fixture adapter supports exactly one text tool-result content part".into(),
        );
    }
    let text = object(&content[0], "host tool result.content[0]")?;
    if string_field(text, "type")? != "text" {
        return Err("the v1 fixture adapter supports text tool-result content only".into());
    }
    let yield_once = match value.get("yield_once") {
        None => false,
        Some(JsonValue::Bool(value)) => *value,
        Some(_) => return Err("host tool call field \"yield_once\" must be a boolean".into()),
    };
    let enqueue_during_execution = match value.get("enqueue_during_execution") {
        None => None,
        Some(value) => {
            let arrival = object(value, "host tool call enqueue_during_execution")?;
            let text = string_field(arrival, "text")?.to_owned();
            match string_field(arrival, "kind")? {
                "steer" => Some(FixtureActiveQueueArrival::Steer(text)),
                "follow_up" => Some(FixtureActiveQueueArrival::FollowUp(text)),
                kind => {
                    return Err(format!(
                        "host tool call enqueue_during_execution.kind must be steer or follow_up, got {kind:?}"
                    ));
                }
            }
        }
    };
    if enqueue_during_execution.is_some() && !yield_once {
        return Err("host tool call enqueue_during_execution requires yield_once".into());
    }
    Ok(FixtureToolResponse {
        arguments: SerializedJson::new(
            field(value, "arguments")?
                .to_json_string()
                .map_err(|error| error.to_string())?,
        ),
        content: string_field(text, "text")?.to_owned(),
        is_error: bool_field(result, "is_error")?,
        yield_once,
        updates: match value.get("updates") {
            None => Vec::new(),
            Some(JsonValue::Array(updates)) => updates
                .iter()
                .enumerate()
                .map(|(index, update)| match update {
                    JsonValue::String(update) => Ok(update.clone()),
                    _ => Err(format!("host tool call updates[{index}] must be a string")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("host tool call field \"updates\" must be an array".into()),
        },
        cancel_after_update: match value.get("cancel_after_update") {
            None => false,
            Some(JsonValue::Bool(value)) => *value,
            Some(_) => {
                return Err("host tool call field \"cancel_after_update\" must be a boolean".into());
            }
        },
        enqueue_during_execution,
        terminate: match result.get("terminate") {
            None => false,
            Some(JsonValue::Bool(value)) => *value,
            Some(_) => return Err("host tool result field \"terminate\" must be a boolean".into()),
        },
    })
}

fn parse_model_script(
    value: &JsonValue,
) -> Result<(Vec<FixtureModelStream>, FixtureUsage, StopReason), String> {
    let turns = array(value, "model_script")?;
    if turns.is_empty() {
        return Err("model_script must contain at least one turn".into());
    }
    let mut last_usage = None;
    let mut last_stop_reason = None;
    let streams = turns
        .iter()
        .enumerate()
        .map(|(turn_index, turn)| {
            let turn = object(turn, "model_script turn")?;
            let cancel_after_text_delta = parse_cancel_after(turn.get("cancel_after"), turn_index)?;
            let chunks = array(field(turn, "chunks")?, "model_script chunks")?;
            if chunks.is_empty() {
                return Err(format!("model_script[{turn_index}] has no chunks"));
            }
            let mut events = Vec::new();
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let chunk = object(chunk, "model_script chunk")?;
                let kind = string_field(chunk, "kind")?;
                match kind {
                    "text_delta" if chunk_index + 1 < chunks.len() => events.push(
                        ModelStreamEvent::TextDelta(string_field(chunk, "text")?.to_owned()),
                    ),
                    "tool_call" if chunk_index + 1 < chunks.len() => {
                        let id = ToolCallId::new(string_field(chunk, "id")?)
                            .map_err(|error| error.to_string())?;
                        events.push(ModelStreamEvent::ToolCall(AgentToolCall {
                            id,
                            name: string_field(chunk, "name")?.to_owned(),
                            arguments: SerializedJson::new(
                                field(chunk, "arguments")?
                                    .to_json_string()
                                    .map_err(|error| error.to_string())?,
                            ),
                        }));
                    }
                    "done" if chunk_index + 1 == chunks.len() => {
                        let stop_reason = parse_stop_reason(string_field(chunk, "stop_reason")?)?;
                        let usage = FixtureUsage::parse(field(chunk, "usage")?)?;
                        last_usage = Some(usage);
                        last_stop_reason = Some(stop_reason);
                        events.push(ModelStreamEvent::End(stop_reason));
                    }
                    "error" if chunk_index + 1 == chunks.len() => {
                        let stop_reason = parse_stop_reason(string_field(chunk, "reason")?)?;
                        if !matches!(stop_reason, StopReason::Error | StopReason::Aborted) {
                            return Err(format!(
                                "model-script error at turn {turn_index}, index {chunk_index} must use error or aborted"
                            ));
                        }
                        let usage = FixtureUsage::parse(field(chunk, "usage")?)?;
                        last_usage = Some(usage);
                        last_stop_reason = Some(stop_reason);
                        match stop_reason {
                            StopReason::Error => events.push(ModelStreamEvent::Error {
                                message: string_field(chunk, "message")?.to_owned(),
                            }),
                            StopReason::Aborted => events.push(ModelStreamEvent::Aborted {
                                message: string_field(chunk, "message")?.to_owned(),
                            }),
                            _ => {
                                return Err(
                                    "model-script error stop reason escaped validation".into(),
                                );
                            }
                        }
                    }
                    _ => {
                        return Err(format!(
                            "unsupported model-script chunk {kind:?} at turn {turn_index}, index {chunk_index}"
                        ));
                    }
                }
            }
            if cancel_after_text_delta {
                let Some(text_delta_index) = events
                    .iter()
                    .position(|event| matches!(event, ModelStreamEvent::TextDelta(_)))
                else {
                    return Err(format!(
                        "model_script[{turn_index}].cancel_after text_delta requires a text_delta chunk"
                    ));
                };
                events.truncate(text_delta_index + 1);
                events.push(ModelStreamEvent::Aborted {
                    message: "Operation aborted".into(),
                });
                last_stop_reason = Some(StopReason::Aborted);
            }
            Ok(FixtureModelStream {
                stream: ModelStream { events },
                cancel_after_text_delta,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        streams,
        last_usage.ok_or_else(|| "model script must end with done or error".to_owned())?,
        last_stop_reason.ok_or_else(|| "model script must end with done or error".to_owned())?,
    ))
}

fn parse_cancel_after(value: Option<&JsonValue>, turn_index: usize) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(JsonValue::String(value)) if value == "text_delta" => Ok(true),
        Some(JsonValue::String(value)) => Err(format!(
            "model_script[{turn_index}].cancel_after does not support {value:?}; use text_delta"
        )),
        Some(_) => Err(format!(
            "model_script[{turn_index}].cancel_after must be text_delta"
        )),
    }
}

impl FixtureUsage {
    fn parse(value: &JsonValue) -> Result<Self, String> {
        let usage = object(value, "usage")?;
        Ok(Self {
            input: number_field(usage, "input")?,
            output: number_field(usage, "output")?,
            cache_read: number_field(usage, "cache_read")?,
            cache_write: number_field(usage, "cache_write")?,
            total_tokens: number_field(usage, "total_tokens")?,
        })
    }
}

fn parse_thinking_level(value: &str) -> Result<ThinkingLevel, String> {
    match value {
        // Retain the pre-alignment spelling for old fixtures; upstream's
        // default agent setting is the canonical `off` level.
        "default" => Ok(ThinkingLevel::Off),
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(format!("unsupported thinking level {value:?}")),
    }
}

fn parse_tool_execution_mode(value: Option<&JsonValue>) -> Result<ToolExecutionMode, String> {
    match value {
        None => Ok(ToolExecutionMode::Parallel),
        Some(JsonValue::String(value)) if value == "parallel" => Ok(ToolExecutionMode::Parallel),
        Some(JsonValue::String(value)) if value == "sequential" => {
            Ok(ToolExecutionMode::Sequential)
        }
        Some(_) => Err("setup tool field \"execution_mode\" must be parallel or sequential".into()),
    }
}

fn parse_stop_reason(value: &str) -> Result<StopReason, String> {
    match value {
        "stop" => Ok(StopReason::Stop),
        "tool_call" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::Length),
        "aborted" => Ok(StopReason::Aborted),
        "cancelled" => Ok(StopReason::Cancelled),
        "error" => Ok(StopReason::Error),
        _ => Err(format!("unsupported model stop reason {value:?}")),
    }
}

fn field<'a>(object: &'a BTreeMap<String, JsonValue>, name: &str) -> Result<&'a JsonValue, String> {
    object
        .get(name)
        .ok_or_else(|| format!("fixture is missing required field {name:?}"))
}

fn object<'a>(value: &'a JsonValue, path: &str) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    match value {
        JsonValue::Object(value) => Ok(value),
        _ => Err(format!("{path} must be an object")),
    }
}

fn array<'a>(value: &'a JsonValue, path: &str) -> Result<&'a [JsonValue], String> {
    match value {
        JsonValue::Array(value) => Ok(value),
        _ => Err(format!("{path} must be an array")),
    }
}

fn string_field<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a str, String> {
    match field(object, name)? {
        JsonValue::String(value) => Ok(value),
        _ => Err(format!("fixture field {name:?} must be a string")),
    }
}

fn bool_field(object: &BTreeMap<String, JsonValue>, name: &str) -> Result<bool, String> {
    match field(object, name)? {
        JsonValue::Bool(value) => Ok(*value),
        _ => Err(format!("fixture field {name:?} must be a boolean")),
    }
}

fn number_field(object: &BTreeMap<String, JsonValue>, name: &str) -> Result<u64, String> {
    match field(object, name)? {
        JsonValue::Number(JsonNumber::Unsigned(value)) => Ok(*value),
        JsonValue::Number(JsonNumber::Signed(value)) if *value >= 0 => Ok(*value as u64),
        _ => Err(format!(
            "fixture field {name:?} must be a non-negative integer"
        )),
    }
}
