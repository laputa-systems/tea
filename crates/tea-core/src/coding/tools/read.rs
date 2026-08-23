//! The standard read tool.

use super::arguments::{
    check_cancelled, operation_failure, optional_positive_usize, parse_object, path_error,
    result_ok, string_field, truncate_read_output,
};
use super::contract::CodingOperations;
use super::schemas::read_schema;
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct ReadTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl ReadTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_read()
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let args = match parse_object(self.name(), &call).and_then(|object| {
            Ok((
                string_field(self.name(), &object, "path")?,
                optional_positive_usize(self.name(), &object, "offset")?,
                optional_positive_usize(self.name(), &object, "limit")?,
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let path = match path_error(self.name(), self.root.resolve_existing(&args.0)) {
            Ok(path) => path,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        if let Err(error) = check_cancelled(self.name(), &context) {
            return Box::pin(std::future::ready(Err(error)));
        }
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            let bytes = operations
                .read_file(&path)
                .await
                .map_err(|error| operation_failure("read", error))?;
            let text = String::from_utf8_lossy(&bytes);
            let start = args.1.unwrap_or(1);
            let lines = text.split('\n').collect::<Vec<_>>();
            if start > lines.len() && !lines.is_empty() {
                return Err(ToolError::Execution {
                    tool: "read".into(),
                    message: format!("offset {start} is beyond end of file"),
                });
            }
            let begin = start.saturating_sub(1).min(lines.len());
            let selected = if let Some(limit) = args.2 {
                let end = begin.saturating_add(limit).min(lines.len());
                lines[begin..end].join("\n")
            } else {
                lines[begin..].join("\n")
            };
            let (output, truncated) = truncate_read_output(selected.as_bytes());
            let suffix = if truncated { "\n[truncated]" } else { "" };
            Ok(result_ok(&call, format!("{}{}", output, suffix)))
        })
    }
}

fn static_schema_read() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(read_schema)
}
