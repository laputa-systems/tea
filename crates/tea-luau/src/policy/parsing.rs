//! Policy declaration and pre-tool decision parsing.

use super::types::{
    PolicyAfterToolOutput, PolicyContextAnnotation, PolicyContextProjectionPatch,
    PolicyHostCommand, PolicyHostCommandHandler, PolicyMemoryProposal, PolicyMemoryRetention,
    PolicyMemoryVisibility, PolicyResumeHook,
};
use super::{PolicyError, PolicyPromptSection, PolicyTool};
use crate::bundle::{BUNDLE_ABI_V2_VERSION, BUNDLE_ABI_VERSION};
use mlua::{Function, Table, Value};
use std::collections::BTreeSet;
use tea_core::harness::extension::{
    ExtensionCommandResult, ExtensionIdleResult, ExtensionStateUpdate,
};
use tea_core::hooks::{AfterToolCall, BeforeToolCall, Replacement};
use tea_core::state::SerializedJson;
use tea_core::tool::{AgentToolResult, CancellationSettlementMode, ToolExecutionMode};
use tea_protocol::JsonValue;

pub(super) struct ParsedDeclaration {
    pub(super) prompt_sections: Vec<PolicyPromptSection>,
    pub(super) tools: Vec<PolicyTool>,
    pub(super) host_commands: Vec<PolicyHostCommand>,
    pub(super) host_command_handlers: Vec<PolicyHostCommandHandler>,
    pub(super) before_tool_call: Option<Function>,
    pub(super) after_tool_call: Option<Function>,
    pub(super) context_projection: Option<Function>,
    pub(super) resume_hooks: Vec<PolicyResumeHook>,
    pub(super) on_idle: Option<Function>,
}

pub(super) fn parse_declaration(
    declaration: &Table,
    abi_version: u32,
) -> Result<ParsedDeclaration, PolicyError> {
    match abi_version {
        BUNDLE_ABI_VERSION => parse_canonical_declaration(declaration, false, "v1"),
        BUNDLE_ABI_V2_VERSION => parse_canonical_declaration(declaration, true, "v2"),
        _ => Err(PolicyError::Contract {
            message: format!("unsupported policy declaration ABI {abi_version}"),
        }),
    }
}

fn parse_canonical_declaration(
    declaration: &Table,
    allow_host_extensions: bool,
    abi_name: &str,
) -> Result<ParsedDeclaration, PolicyError> {
    let mut fields = vec![
        "prompt_sections",
        "tools",
        "before_tool",
        "after_tool",
        "context_projection",
        "resume_hooks",
    ];
    if allow_host_extensions {
        fields.extend(["commands", "on_idle"]);
    }
    require_only_fields(
        declaration,
        &fields,
        &format!("{abi_name} policy declaration"),
    )?;
    let declared_sections = declaration
        .get::<Option<Table>>("prompt_sections")
        .map_err(contract_error)?
        .ok_or_else(|| PolicyError::Contract {
            message: "v1 policy must declare prompt_sections as an array".into(),
        })?;
    let prompt_sections = parse_prompt_sections(&declared_sections)?;
    let before_tool = declaration
        .get::<Option<Function>>("before_tool")
        .map_err(contract_error)?;
    let after_tool_call = declaration
        .get::<Option<Function>>("after_tool")
        .map_err(contract_error)?;
    let context_projection = declaration
        .get::<Option<Function>>("context_projection")
        .map_err(contract_error)?;
    let resume_hooks = declaration
        .get::<Option<Table>>("resume_hooks")
        .map_err(contract_error)?
        .map(|hooks| parse_resume_hooks(&hooks))
        .transpose()?
        .unwrap_or_default();
    let (host_commands, host_command_handlers, on_idle) = if allow_host_extensions {
        let (commands, handlers) = parse_host_commands(declaration)?;
        let on_idle = declaration
            .get::<Option<Function>>("on_idle")
            .map_err(contract_error)?;
        (commands, handlers, on_idle)
    } else {
        (Vec::new(), Vec::new(), None)
    };
    let tools = parse_tools(declaration)?;
    Ok(ParsedDeclaration {
        prompt_sections,
        tools,
        host_commands,
        host_command_handlers,
        before_tool_call: before_tool,
        after_tool_call,
        context_projection,
        resume_hooks,
        on_idle,
    })
}

fn parse_host_commands(
    declaration: &Table,
) -> Result<(Vec<PolicyHostCommand>, Vec<PolicyHostCommandHandler>), PolicyError> {
    let Some(commands) = declaration
        .get::<Option<Table>>("commands")
        .map_err(contract_error)?
    else {
        return Ok((Vec::new(), Vec::new()));
    };
    require_dense_array(&commands, "v2 extension commands")?;
    let mut names = BTreeSet::new();
    let mut metadata = Vec::new();
    let mut handlers = Vec::new();
    for command in commands.sequence_values::<Table>() {
        let command = command.map_err(contract_error)?;
        require_only_fields(
            &command,
            &["name", "help", "allowed_while_active", "handler"],
            "v2 extension command",
        )?;
        let name: String = command.get("name").map_err(contract_error)?;
        let help: String = command.get("help").map_err(contract_error)?;
        let handler: Function = command.get("handler").map_err(contract_error)?;
        let allowed_while_active = command
            .get::<Option<bool>>("allowed_while_active")
            .map_err(contract_error)?
            .unwrap_or(false);
        if !is_command_name(&name) {
            return Err(PolicyError::Contract {
                message: format!(
                    "v2 extension command name {name:?} must begin with /, use ASCII letters, digits, _ or -, and be at most 80 bytes"
                ),
            });
        }
        if help.trim().is_empty()
            || help.len() > 240
            || help.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(PolicyError::Contract {
                message: format!("v2 extension command {name:?} help must be non-empty printable text of at most 240 bytes"),
            });
        }
        if !names.insert(name.clone()) {
            return Err(PolicyError::Contract {
                message: format!("v2 policy contains duplicate extension command {name:?}"),
            });
        }
        metadata.push(PolicyHostCommand {
            name: name.clone(),
            help,
            allowed_while_active,
        });
        handlers.push(PolicyHostCommandHandler {
            name,
            function: handler,
        });
    }
    Ok((metadata, handlers))
}

/// Parse the ABI-v1 lifecycle table. A registration uses one stable local
/// ID for all three phases so recovery can hand it only the values it wrote
/// before the operation and epoch commits.  The host later namespaces this
/// local ID with the immutable plugin identity before persisting it.
fn parse_resume_hooks(declaration: &Table) -> Result<Vec<PolicyResumeHook>, PolicyError> {
    require_dense_array(declaration, "v1 resume_hooks")?;
    let mut ids = BTreeSet::new();
    let mut hooks = Vec::new();
    for hook in declaration.sequence_values::<Table>() {
        let hook = hook.map_err(contract_error)?;
        require_only_fields(
            &hook,
            &["id", "before_operation", "before_epoch", "before_resume"],
            "v1 resume hook",
        )?;
        let id: String = hook.get("id").map_err(contract_error)?;
        if !is_portable_label(&id) {
            return Err(PolicyError::Contract {
                message: format!(
                    "v1 resume hook ID {id:?} must use [A-Za-z0-9._-] and be at most 120 bytes"
                ),
            });
        }
        if !ids.insert(id.clone()) {
            return Err(PolicyError::Contract {
                message: format!("v1 policy contains duplicate resume hook {id:?}"),
            });
        }
        let before_operation = hook
            .get::<Option<Function>>("before_operation")
            .map_err(contract_error)?;
        let before_epoch = hook
            .get::<Option<Function>>("before_epoch")
            .map_err(contract_error)?;
        let before_resume = hook
            .get::<Option<Function>>("before_resume")
            .map_err(contract_error)?;
        if before_operation.is_none() && before_epoch.is_none() && before_resume.is_none() {
            return Err(PolicyError::Contract {
                message: format!(
                    "v1 resume hook {id:?} must declare before_operation, before_epoch, or before_resume"
                ),
            });
        }
        hooks.push(PolicyResumeHook {
            id,
            before_operation,
            before_epoch,
            before_resume,
        });
    }
    Ok(hooks)
}

fn parse_prompt_sections(declaration: &Table) -> Result<Vec<PolicyPromptSection>, PolicyError> {
    require_dense_array(declaration, "v1 prompt_sections")?;
    let mut ids = BTreeSet::new();
    let mut sections = Vec::new();
    for section in declaration.sequence_values::<Table>() {
        let section = section.map_err(contract_error)?;
        require_only_fields(&section, &["id", "content"], "v1 prompt section")?;
        let id: String = section.get("id").map_err(contract_error)?;
        let content: String = section.get("content").map_err(contract_error)?;
        if !is_portable_label(&id) {
            return Err(PolicyError::Contract {
                message: format!("v1 prompt section ID {id:?} must use [A-Za-z0-9._-]"),
            });
        }
        if content.trim().is_empty() {
            return Err(PolicyError::Contract {
                message: format!("v1 prompt section {id:?} must not be empty"),
            });
        }
        if !ids.insert(id.clone()) {
            return Err(PolicyError::Contract {
                message: format!("v1 policy contains duplicate prompt section {id:?}"),
            });
        }
        sections.push(PolicyPromptSection { id, content });
    }
    Ok(sections)
}

fn parse_tools(declaration: &Table) -> Result<Vec<PolicyTool>, PolicyError> {
    let Some(declared_tools) = declaration
        .get::<Option<Table>>("tools")
        .map_err(contract_error)?
    else {
        return Ok(Vec::new());
    };
    require_dense_array(&declared_tools, "policy tools")?;
    let mut names = BTreeSet::new();
    let mut tools = Vec::new();
    for declared_tool in declared_tools.sequence_values::<Table>() {
        let declared_tool = declared_tool.map_err(contract_error)?;
        let tool = parse_tool(&declared_tool)?;
        if !names.insert(tool.name.clone()) {
            return Err(PolicyError::Contract {
                message: format!("tools contains duplicate name {:?}", tool.name),
            });
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn parse_tool(declaration: &Table) -> Result<PolicyTool, PolicyError> {
    require_only_fields(
        declaration,
        &[
            "name",
            "description",
            "capability",
            "execution_mode",
            "requires_exclusive_batch",
            "cancellation_settlement_mode",
            "schema_json",
            "handler_source",
            "handler_module",
        ],
        "policy tool declaration",
    )?;
    let name = required_field(declaration, "name")?;
    let description = required_field(declaration, "description")?;
    let capability = required_field(declaration, "capability")?;
    let schema_json = required_field(declaration, "schema_json")?;
    let handler_source = declaration
        .get::<Option<String>>("handler_source")
        .map_err(contract_error)?;
    let handler_module = declaration
        .get::<Option<String>>("handler_module")
        .map_err(contract_error)?;
    for (field, value) in [
        ("name", name.as_str()),
        ("description", description.as_str()),
        ("capability", capability.as_str()),
        ("schema_json", schema_json.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(PolicyError::Contract {
                message: format!("tool field {field:?} must not be empty"),
            });
        }
    }
    let execution_mode = match required_field(declaration, "execution_mode")?.as_str() {
        "sequential" => ToolExecutionMode::Sequential,
        "parallel" => ToolExecutionMode::Parallel,
        value => {
            return Err(PolicyError::Contract {
                message: format!(
                    "tool {name:?} has invalid execution_mode {value:?}; expected sequential or parallel"
                ),
            });
        }
    };
    let requires_exclusive_batch = declaration
        .get::<Option<bool>>("requires_exclusive_batch")
        .map_err(contract_error)?
        .unwrap_or(false);
    let cancellation_settlement_mode = match declaration
        .get::<Option<String>>("cancellation_settlement_mode")
        .map_err(contract_error)?
        .as_deref()
        .unwrap_or("drop_future")
    {
        "drop_future" => CancellationSettlementMode::DropFuture,
        "await_future" => CancellationSettlementMode::AwaitFuture,
        value => {
            return Err(PolicyError::Contract {
                message: format!(
                    "tool {name:?} has invalid cancellation_settlement_mode {value:?}; expected drop_future or await_future"
                ),
            });
        }
    };
    let schema = JsonValue::parse(&schema_json).map_err(|error| PolicyError::Contract {
        message: format!("tool {name:?} schema_json is invalid: {error}"),
    })?;
    if handler_source
        .as_deref()
        .is_some_and(|source| source.trim().is_empty())
    {
        return Err(PolicyError::Contract {
            message: format!("tool {name:?} handler_source must not be empty when declared"),
        });
    }
    if handler_module
        .as_deref()
        .is_some_and(|module| module.trim().is_empty())
    {
        return Err(PolicyError::Contract {
            message: format!("tool {name:?} handler_module must not be empty when declared"),
        });
    }
    if handler_source.is_some() && handler_module.is_some() {
        return Err(PolicyError::Contract {
            message: format!("tool {name:?} cannot declare both handler_source and handler_module"),
        });
    }
    Ok(PolicyTool {
        name,
        description,
        schema,
        capability,
        execution_mode,
        requires_exclusive_batch,
        cancellation_settlement_mode,
        handler_source,
        handler_module,
    })
}

fn required_field(declaration: &Table, name: &str) -> Result<String, PolicyError> {
    declaration
        .get::<String>(name)
        .map_err(|error| PolicyError::Contract {
            message: format!("tool field {name:?} is required and must be a string: {error}"),
        })
}

fn is_portable_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_command_name(value: &str) -> bool {
    value.len() >= 2
        && value.len() <= 80
        && value.starts_with('/')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Parse the shared, deliberately narrow command/idle state mutation result.
pub(super) fn parse_extension_result(value: Value) -> Result<ExtensionCommandResult, PolicyError> {
    if matches!(value, Value::Nil) {
        return Ok(ExtensionCommandResult::default());
    }
    let Value::Table(table) = value else {
        return Err(PolicyError::Contract {
            message: "extension command and on_idle handlers must return nil or a result table"
                .into(),
        });
    };
    require_only_fields(
        &table,
        &["notice", "state", "internal_input"],
        "extension command result",
    )?;
    let notice = optional_string(&table, "notice")?;
    if notice.as_deref().is_some_and(|notice| {
        notice.trim().is_empty()
            || notice.len() > 4096
            || notice.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err(PolicyError::Contract {
            message:
                "extension command notice must be printable non-empty text of at most 4096 bytes"
                    .into(),
        });
    }
    let internal_input = optional_string(&table, "internal_input")?;
    if internal_input
        .as_deref()
        .is_some_and(|input| input.trim().is_empty() || input.len() > 16 * 1024)
    {
        return Err(PolicyError::Contract {
            message: "extension internal_input must be non-empty text of at most 16384 bytes"
                .into(),
        });
    }
    let state = table
        .get::<Option<Table>>("state")
        .map_err(contract_error)?
        .map(|state| {
            require_only_fields(&state, &["kind", "content_json"], "extension state update")?;
            let kind: String = state.get("kind").map_err(contract_error)?;
            let content_json: String = state.get("content_json").map_err(contract_error)?;
            if !is_portable_label(&kind) || content_json.len() > 16 * 1024 {
                return Err(PolicyError::Contract {
                    message: "extension state update kind must be portable and content_json must be at most 16384 bytes".into(),
                });
            }
            let content = JsonValue::parse(&content_json).map_err(|error| PolicyError::Contract {
                message: format!("extension state content_json must be valid JSON: {error}"),
            })?;
            Ok(ExtensionStateUpdate { kind, content })
        })
        .transpose()?;
    Ok(ExtensionCommandResult {
        notice,
        state,
        internal_input,
    })
}

pub(super) fn parse_idle_result(value: Value) -> Result<ExtensionIdleResult, PolicyError> {
    let result = parse_extension_result(value)?;
    Ok(ExtensionIdleResult {
        state: result.state,
        internal_input: result.internal_input,
    })
}

pub(super) fn parse_decision(value: Value) -> Result<BeforeToolCall, PolicyError> {
    match value {
        Value::String(value) if value.to_str().map_err(runtime_error)?.as_ref() == "allow" => {
            Ok(BeforeToolCall::Allow)
        }
        Value::Table(value) => {
            require_only_fields(
                &value,
                &["action", "reason", "arguments_json"],
                "before_tool result",
            )?;
            let action: String = value.get("action").map_err(contract_error)?;
            match action.as_str() {
                "normalize" => {
                    let arguments: String = value.get("arguments_json").map_err(contract_error)?;
                    if arguments.trim().is_empty() {
                        return Err(PolicyError::Contract {
                            message: "before_tool normalize arguments_json must not be empty"
                                .to_owned(),
                        });
                    }
                    tea_protocol::JsonValue::parse(&arguments).map_err(|error| {
                        PolicyError::Contract {
                            message: format!(
                                "before_tool normalize arguments_json must be valid JSON: {error}"
                            ),
                        }
                    })?;
                    Ok(BeforeToolCall::Normalize {
                        arguments: SerializedJson::new(arguments),
                    })
                }
                "block" | "terminate" => {
                    let reason: String = value.get("reason").map_err(contract_error)?;
                    if reason.trim().is_empty() {
                        return Err(PolicyError::Contract {
                            message: "before_tool denial reason must not be empty".to_owned(),
                        });
                    }
                    if action == "block" {
                        Ok(BeforeToolCall::Block { reason })
                    } else {
                        Ok(BeforeToolCall::Terminate { reason })
                    }
                }
                _ => Err(PolicyError::Contract {
                    message: format!(
                        "before_tool action {action:?} must be block, terminate, or normalize",
                    ),
                }),
            }
        }
        _ => Err(PolicyError::Contract {
            message: "before_tool must return \"allow\", { action, reason }, or { action = \"normalize\", arguments_json }".to_owned(),
        }),
    }
}

/// Decode the deliberately narrow ABI-v1 post-tool projection. The policy
/// sees a completed result, but it cannot name usage, failure classification,
/// dynamic tools, or any raw-artifact field. Those remain Rust-owned facts.
pub(super) fn parse_after_tool_output(value: Value) -> Result<PolicyAfterToolOutput, PolicyError> {
    match value {
        Value::Nil => Ok(PolicyAfterToolOutput {
            projection: AfterToolCall::default(),
            memory: None,
        }),
        Value::String(value) if value.to_str().map_err(runtime_error)?.as_ref() == "keep" => {
            Ok(PolicyAfterToolOutput {
                projection: AfterToolCall::default(),
                memory: None,
            })
        }
        Value::Table(table) => {
            require_only_fields(
                &table,
                &[
                    "content",
                    "details_json",
                    "is_error",
                    "terminate",
                    "recovery_hint",
                    "annotations_json",
                    "memory",
                ],
                "after_tool result",
            )?;
            let content = optional_string(&table, "content")?;
            let details_json = optional_string(&table, "details_json")?;
            let is_error = optional_bool(&table, "is_error")?;
            let terminate = optional_bool(&table, "terminate")?;
            let recovery_hint = optional_string(&table, "recovery_hint")?;
            let annotations_json = optional_string(&table, "annotations_json")?;
            let memory = table
                .get::<Option<Table>>("memory")
                .map_err(contract_error)?
                .map(|proposal| parse_memory_proposal(&proposal))
                .transpose()?;

            if recovery_hint
                .as_deref()
                .is_some_and(|hint| hint.trim().is_empty())
            {
                return Err(PolicyError::Contract {
                    message: "after_tool recovery_hint must not be empty when declared".into(),
                });
            }
            if details_json.is_some() && (recovery_hint.is_some() || annotations_json.is_some()) {
                return Err(PolicyError::Contract {
                    message: "after_tool details_json cannot be combined with recovery_hint or annotations_json; use one replacement payload".into(),
                });
            }

            let details = if let Some(details_json) = details_json {
                parse_serialized_json("after_tool details_json", details_json)?
            } else if recovery_hint.is_some() || annotations_json.is_some() {
                let mut fields = Vec::new();
                if let Some(recovery_hint) = recovery_hint {
                    fields.push(("recovery_hint", JsonValue::String(recovery_hint)));
                }
                if let Some(annotations_json) = annotations_json {
                    let annotations = JsonValue::parse(&annotations_json).map_err(|error| {
                        PolicyError::Contract {
                            message: format!(
                                "after_tool annotations_json must be valid JSON: {error}"
                            ),
                        }
                    })?;
                    if !matches!(annotations, JsonValue::Array(_) | JsonValue::Object(_)) {
                        return Err(PolicyError::Contract {
                            message: "after_tool annotations_json must be a JSON array or object"
                                .into(),
                        });
                    }
                    fields.push(("annotations", annotations));
                }
                let details_json = JsonValue::object(fields)
                    .to_json_string()
                    .map_err(|error| PolicyError::Contract {
                        message: format!("after_tool annotation payload cannot encode: {error}"),
                    })?;
                Some(SerializedJson::new(details_json))
            } else {
                None
            };

            Ok(PolicyAfterToolOutput {
                projection: AfterToolCall {
                    content: content
                        .map(Replacement::Replace)
                        .unwrap_or(Replacement::Keep),
                    details: match details {
                        Some(details) => Replacement::Replace(Some(details)),
                        None => Replacement::Keep,
                    },
                    is_error: is_error
                        .map(Replacement::Replace)
                        .unwrap_or(Replacement::Keep),
                    // ABI v1 policies are deliberately unable to falsify raw
                    // usage, failures, or capability registrations.
                    failure: Replacement::Keep,
                    usage: Replacement::Keep,
                    added_tool_names: Replacement::Keep,
                    terminate,
                },
                memory,
            })
        }
        _ => Err(PolicyError::Contract {
            message: "after_tool must return nil, \"keep\", or a bounded projection table".into(),
        }),
    }
}

const MAX_CONTEXT_PATCH_ENTRY_IDS: usize = 512;
const MAX_CONTEXT_PATCH_ANNOTATIONS: usize = 32;
const MAX_CONTEXT_ANNOTATION_BYTES: usize = 4 * 1024;

/// Decode the metadata-only ABI-v1 context policy result. Entry IDs are
/// intentionally opaque strings here: `tea_core::runtime` maps them to one branch
/// and rejects references outside that branch or any protected invariant.
pub(super) fn parse_context_projection(
    value: Value,
) -> Result<PolicyContextProjectionPatch, PolicyError> {
    let Value::Table(table) = value else {
        if matches!(value, Value::Nil) {
            return Ok(PolicyContextProjectionPatch::default());
        }
        return Err(PolicyError::Contract {
            message: "context_projection must return nil or a bounded patch table".into(),
        });
    };
    require_only_fields(
        &table,
        &[
            "retain_entries",
            "omit_eligible_entries",
            "annotations",
            "selected_memory",
            "requested_compaction_strategy",
        ],
        "context_projection result",
    )?;
    let retain_entries = parse_context_entry_ids(&table, "retain_entries")?;
    let omit_eligible_entries = parse_context_entry_ids(&table, "omit_eligible_entries")?;
    let selected_memory = parse_context_entry_ids(&table, "selected_memory")?;
    let annotations = parse_context_annotations(&table)?;
    let requested_compaction_strategy = table
        .get::<Option<String>>("requested_compaction_strategy")
        .map_err(contract_error)?;
    if requested_compaction_strategy
        .as_deref()
        .is_some_and(|value| !is_portable_label(value))
    {
        return Err(PolicyError::Contract {
            message: "context_projection requested_compaction_strategy must use [A-Za-z0-9._-] and be at most 120 bytes".into(),
        });
    }
    Ok(PolicyContextProjectionPatch {
        retain_entries,
        omit_eligible_entries,
        annotations,
        selected_memory,
        requested_compaction_strategy,
    })
}

fn parse_context_entry_ids(table: &Table, field: &str) -> Result<Vec<String>, PolicyError> {
    let Some(values) = table.get::<Option<Table>>(field).map_err(contract_error)? else {
        return Ok(Vec::new());
    };
    require_dense_array(&values, &format!("context_projection {field}"))?;
    if values.raw_len() > MAX_CONTEXT_PATCH_ENTRY_IDS {
        return Err(PolicyError::Contract {
            message: format!(
                "context_projection {field} exceeds {MAX_CONTEXT_PATCH_ENTRY_IDS} entries"
            ),
        });
    }
    let mut ids = BTreeSet::new();
    values
        .sequence_values::<String>()
        .map(|value| {
            let value = value.map_err(contract_error)?;
            if !is_safe_context_entry_id(&value) {
                return Err(PolicyError::Contract {
                    message: format!(
                        "context_projection {field} contains an invalid bounded entry ID"
                    ),
                });
            }
            if !ids.insert(value.clone()) {
                return Err(PolicyError::Contract {
                    message: format!("context_projection {field} repeats entry {value:?}"),
                });
            }
            Ok(value)
        })
        .collect()
}

fn parse_context_annotations(table: &Table) -> Result<Vec<PolicyContextAnnotation>, PolicyError> {
    let Some(values) = table
        .get::<Option<Table>>("annotations")
        .map_err(contract_error)?
    else {
        return Ok(Vec::new());
    };
    require_dense_array(&values, "context_projection annotations")?;
    if values.raw_len() > MAX_CONTEXT_PATCH_ANNOTATIONS {
        return Err(PolicyError::Contract {
            message: format!(
                "context_projection annotations exceeds {MAX_CONTEXT_PATCH_ANNOTATIONS} entries"
            ),
        });
    }
    let mut ids = BTreeSet::new();
    values
        .sequence_values::<Table>()
        .map(|value| {
            let value = value.map_err(contract_error)?;
            require_only_fields(&value, &["id", "content"], "context annotation")?;
            let id: String = value.get("id").map_err(contract_error)?;
            let content: String = value.get("content").map_err(contract_error)?;
            if !is_portable_label(&id)
                || content.is_empty()
                || content.len() > MAX_CONTEXT_ANNOTATION_BYTES
            {
                return Err(PolicyError::Contract {
                    message: "context annotation requires a portable ID and non-empty <=4096 byte content".into(),
                });
            }
            if !ids.insert(id.clone()) {
                return Err(PolicyError::Contract {
                    message: format!("context_projection repeats annotation {id:?}"),
                });
            }
            Ok(PolicyContextAnnotation { id, content })
        })
        .collect()
}

fn is_safe_context_entry_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
}

/// Maximum exact JSON bytes a lifecycle hook may ask Rust to retain for one
/// registration and one durable boundary. This bound is independent of the
/// VM heap limit so a small Lua value cannot expand into an unexpectedly
/// large session record through serialization.
pub(super) const MAX_RESUME_STATE_BYTES: usize = 16 * 1024;

/// Decode one lifecycle state's deliberately narrow JSON-string output.
///
/// Operation and epoch callbacks may return `nil` to contribute no state, or
/// a JSON string that Rust persists under the hook's stable ID. Tables are
/// not accepted because accepting Lua object conversion would make durable
/// JSON spelling depend on host conversion rules.
pub(super) fn parse_resume_state(
    value: Value,
    phase: &str,
) -> Result<Option<JsonValue>, PolicyError> {
    match value {
        Value::Nil => Ok(None),
        Value::String(value) => {
            let state = value.to_str().map_err(runtime_error)?;
            if state.len() > MAX_RESUME_STATE_BYTES {
                return Err(PolicyError::Contract {
                    message: format!(
                        "{phase} resume state is {} bytes, exceeding the {} byte limit",
                        state.len(),
                        MAX_RESUME_STATE_BYTES,
                    ),
                });
            }
            JsonValue::parse(state.as_ref()).map(Some).map_err(|error| PolicyError::Contract {
                message: format!("{phase} resume state must be valid JSON: {error}"),
            })
        }
        _ => Err(PolicyError::Contract {
            message: format!(
                "{phase} must return nil or a JSON string no larger than {MAX_RESUME_STATE_BYTES} bytes"
            ),
        }),
    }
}

const MAX_PLUGIN_MEMORY_CONTENT_BYTES: usize = 16 * 1024;
const MAX_PLUGIN_MEMORY_PROVENANCE: usize = 32;

/// Parse a single typed memory proposal. Rust later attaches the immutable
/// plugin identity, creates the semantic entry ID, and decides whether a
/// payload needs external artifact retention; Lua can supply neither.
fn parse_memory_proposal(table: &Table) -> Result<PolicyMemoryProposal, PolicyError> {
    require_only_fields(
        table,
        &[
            "kind",
            "content_json",
            "provenance",
            "visibility",
            "retention",
        ],
        "after_tool memory proposal",
    )?;
    let kind: String = table.get("kind").map_err(contract_error)?;
    if !is_portable_label(&kind) {
        return Err(PolicyError::Contract {
            message: "after_tool memory kind must use [A-Za-z0-9._-] and be at most 120 bytes"
                .into(),
        });
    }
    let content_json: String = table.get("content_json").map_err(contract_error)?;
    if content_json.len() > MAX_PLUGIN_MEMORY_CONTENT_BYTES {
        return Err(PolicyError::Contract {
            message: format!(
                "after_tool memory content_json is {} bytes, exceeding the {} byte limit",
                content_json.len(),
                MAX_PLUGIN_MEMORY_CONTENT_BYTES,
            ),
        });
    }
    let content = JsonValue::parse(&content_json).map_err(|error| PolicyError::Contract {
        message: format!("after_tool memory content_json must be valid JSON: {error}"),
    })?;
    let visibility = match table
        .get::<String>("visibility")
        .map_err(contract_error)?
        .as_str()
    {
        "model_visible" => PolicyMemoryVisibility::ModelVisible,
        "external_only" => PolicyMemoryVisibility::ExternalOnly,
        value => {
            return Err(PolicyError::Contract {
                message: format!(
                    "after_tool memory visibility {value:?} must be model_visible or external_only"
                ),
            });
        }
    };
    let retention = match table
        .get::<String>("retention")
        .map_err(contract_error)?
        .as_str()
    {
        "session" => PolicyMemoryRetention::Session,
        "checkpoint" => PolicyMemoryRetention::Checkpoint,
        value => {
            return Err(PolicyError::Contract {
                message: format!(
                    "after_tool memory retention {value:?} must be session or checkpoint"
                ),
            });
        }
    };
    let provenance = table
        .get::<Option<Table>>("provenance")
        .map_err(contract_error)?
        .map(|values| {
            require_dense_array(&values, "after_tool memory provenance")?;
            if values.raw_len() > MAX_PLUGIN_MEMORY_PROVENANCE {
                return Err(PolicyError::Contract {
                    message: format!(
                        "after_tool memory provenance exceeds {MAX_PLUGIN_MEMORY_PROVENANCE} entries"
                    ),
                });
            }
            values
                .sequence_values::<String>()
                .map(|value| {
                    let value = value.map_err(contract_error)?;
                    if value.is_empty()
                        || value.len() > 200
                        || value.chars().any(char::is_control)
                    {
                        return Err(PolicyError::Contract {
                            message: "after_tool memory provenance values must be non-empty bounded text without controls".into(),
                        });
                    }
                    Ok(value)
                })
                .collect::<Result<Vec<_>, PolicyError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(PolicyMemoryProposal {
        kind,
        content,
        provenance,
        visibility,
        retention,
    })
}

/// Construct the only result shape that an ABI-v1 policy may inspect. This
/// stays here beside parsing so new fields cannot accidentally become script
/// visible merely because Rust's `AgentToolResult` grows.
pub(super) fn policy_result_fields(
    result: &AgentToolResult,
) -> Result<Vec<(&'static str, JsonValue)>, PolicyError> {
    let mut fields = vec![
        ("content", JsonValue::String(result.content.clone())),
        ("is_error", JsonValue::Bool(result.is_error)),
        ("terminate", JsonValue::Bool(result.terminate)),
    ];
    if let Some(details) = &result.details {
        let value = JsonValue::parse(details.as_str()).map_err(|error| PolicyError::Runtime {
            message: format!("completed tool details are not valid JSON: {error}"),
        })?;
        fields.push(("details", value));
        fields.push((
            "details_json",
            JsonValue::String(details.as_str().to_owned()),
        ));
    }
    Ok(fields)
}

fn parse_serialized_json(
    field: &str,
    value: String,
) -> Result<Option<SerializedJson>, PolicyError> {
    JsonValue::parse(&value).map_err(|error| PolicyError::Contract {
        message: format!("{field} must be valid JSON: {error}"),
    })?;
    Ok(Some(SerializedJson::new(value)))
}

fn optional_string(table: &Table, name: &str) -> Result<Option<String>, PolicyError> {
    let value = table
        .get::<Option<String>>(name)
        .map_err(|error| PolicyError::Contract {
            message: format!("{name} must be a string when declared: {error}"),
        })?;
    if value.as_ref().is_some_and(|value| value.len() > 16 * 1024) {
        return Err(PolicyError::Contract {
            message: format!("{name} exceeds the 16384 byte ABI-v1 projection limit"),
        });
    }
    Ok(value)
}

fn optional_bool(table: &Table, name: &str) -> Result<Option<bool>, PolicyError> {
    table
        .get::<Option<bool>>(name)
        .map_err(|error| PolicyError::Contract {
            message: format!("{name} must be a boolean when declared: {error}"),
        })
}

fn require_only_fields(table: &Table, allowed: &[&str], surface: &str) -> Result<(), PolicyError> {
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(contract_error)?;
        let Value::String(key) = key else {
            return Err(PolicyError::Contract {
                message: format!("{surface} has a non-string field name"),
            });
        };
        let key = key.to_str().map_err(runtime_error)?;
        if !allowed.contains(&key.as_ref()) {
            return Err(PolicyError::Contract {
                message: format!("{surface} has unknown field {key:?}"),
            });
        }
    }
    Ok(())
}

/// Reject sparse or dictionary-shaped values where the ABI requires a stable
/// ordered array.  `sequence_values` alone would quietly ignore a named
/// field, which would make a candidate's visible source differ from the
/// executable declaration Rust validated.
fn require_dense_array(table: &Table, surface: &str) -> Result<(), PolicyError> {
    let mut count = 0_i64;
    let mut greatest = 0_i64;
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(contract_error)?;
        let Value::Integer(index) = key else {
            return Err(PolicyError::Contract {
                message: format!("{surface} must be an array with positive integer indexes"),
            });
        };
        if index <= 0 {
            return Err(PolicyError::Contract {
                message: format!("{surface} must be an array with positive integer indexes"),
            });
        }
        count += 1;
        greatest = greatest.max(index);
    }
    if count != greatest {
        return Err(PolicyError::Contract {
            message: format!("{surface} must not contain sparse indexes"),
        });
    }
    Ok(())
}

pub(super) fn runtime_error(error: mlua::Error) -> PolicyError {
    PolicyError::Runtime {
        message: error.to_string(),
    }
}

fn contract_error(error: mlua::Error) -> PolicyError {
    PolicyError::Contract {
        message: error.to_string(),
    }
}
