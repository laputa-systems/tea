//! The standard bash tool.

use super::arguments::{
    check_cancelled, operation_failure, optional_timeout, parse_object, result_ok, string_field,
    truncate_output,
};
use super::contract::{CodingOperations, CommandEnvironment};
use super::schemas::bash_schema;
use super::workspace::WorkspaceRoot;
use crate::tool::{AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct BashTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
    environment: CommandEnvironment,
}

impl BashTool {
    pub(crate) fn new(
        root: WorkspaceRoot,
        operations: Arc<dyn CodingOperations>,
        environment: CommandEnvironment,
    ) -> Self {
        Self {
            root,
            operations,
            environment,
        }
    }
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_bash()
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let args = match parse_object(self.name(), &call).and_then(|object| {
            Ok((
                string_field(self.name(), &object, "command")?,
                optional_timeout(self.name(), &object)?,
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let operations = Arc::clone(&self.operations);
        let root = self.root.clone();
        let environment = self.environment.clone();
        Box::pin(async move {
            check_cancelled("bash", &context)?;
            let output = operations
                .execute_command(
                    &args.0,
                    root.as_path(),
                    args.1,
                    &environment,
                    context.cancellation.clone(),
                    updates,
                )
                .await
                .map_err(|error| operation_failure("bash", error))?;
            let mut combined = output.stdout;
            if !output.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.extend_from_slice(b"\n");
                }
                combined.extend_from_slice(&output.stderr);
            }
            let (text, truncated) = truncate_output(&combined);
            if output.exit_code.unwrap_or(1) != 0 {
                return Ok(AgentToolResult {
                    tool_call_id: call.id.clone(),
                    content: if text.is_empty() {
                        format!(
                            "command exited with status {}",
                            output.exit_code.unwrap_or(-1)
                        )
                    } else {
                        text
                    },
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: true,
                    failure: Some(crate::tool::ToolFailure::recoverable()),
                });
            }
            let mut content = text;
            if truncated {
                content.push_str("\n[truncated]");
            }
            if content.is_empty() {
                content.push_str("(no output)");
            }
            Ok(result_ok(&call, content))
        })
    }
}

fn static_schema_bash() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(bash_schema)
}
