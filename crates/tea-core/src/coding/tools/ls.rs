//! The standard ls tool.

use super::arguments::{
    check_cancelled, operation_failure, optional_positive_usize, optional_string, parse_object,
    path_error, result_ok,
};
use super::contract::CodingOperations;
use super::schemas::ls_schema;
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct LsTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl LsTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_ls()
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let args = match parse_object(self.name(), &call).and_then(|object| {
            Ok((
                optional_string(self.name(), &object, "path")?,
                optional_positive_usize(self.name(), &object, "limit")?.unwrap_or(500),
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let path = match path_error(
            self.name(),
            self.root.resolve_existing(args.0.as_deref().unwrap_or(".")),
        ) {
            Ok(path) => path,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            check_cancelled("ls", &context)?;
            let metadata = operations
                .metadata(&path)
                .await
                .map_err(|error| operation_failure("ls", error))?;
            if !metadata.is_directory {
                return Err(ToolError::Execution {
                    tool: "ls".into(),
                    message: "path is not a directory".into(),
                });
            }
            let mut entries = operations
                .read_dir(&path)
                .await
                .map_err(|error| operation_failure("ls", error))?;
            entries.sort_by(|left, right| {
                left.name
                    .to_lowercase()
                    .cmp(&right.name.to_lowercase())
                    .then_with(|| left.name.cmp(&right.name))
            });
            if entries.is_empty() {
                return Ok(result_ok(&call, "(empty directory)"));
            }
            let limited = entries.len() > args.1;
            entries.truncate(args.1);
            let output = entries
                .into_iter()
                .map(|entry| {
                    if entry.is_directory {
                        format!("{}/", entry.name)
                    } else {
                        entry.name
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let output = if limited {
                format!("{}\n\n[{} entries limit reached]", output, args.1)
            } else {
                output
            };
            Ok(result_ok(&call, output))
        })
    }
}

fn static_schema_ls() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(ls_schema)
}
