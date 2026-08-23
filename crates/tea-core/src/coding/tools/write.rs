//! The standard write tool.

use super::arguments::{
    check_cancelled, operation_failure, parse_object, path_error, result_ok, string_field,
};
use super::contract::CodingOperations;
use super::schemas::write_schema;
use super::workspace::WorkspaceRoot;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::path::Path;
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct WriteTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl WriteTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_write()
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
                string_field(self.name(), &object, "content")?,
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let path = match path_error(self.name(), self.root.resolve_for_write(&args.0)) {
            Ok(path) => path,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.as_path().to_path_buf());
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            check_cancelled("write", &context)?;
            operations
                .create_dir_all(&parent)
                .await
                .map_err(|error| operation_failure("write", error))?;
            operations
                .write_file(&path, args.1.as_bytes())
                .await
                .map_err(|error| operation_failure("write", error))?;
            Ok(result_ok(
                &call,
                format!("Successfully wrote {} bytes to {}", args.1.len(), args.0),
            ))
        })
    }
}

fn static_schema_write() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(write_schema)
}
