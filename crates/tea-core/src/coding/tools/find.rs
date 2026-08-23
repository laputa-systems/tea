//! The standard find tool.

use super::arguments::{
    check_cancelled, invalid, operation_failure, optional_positive_usize, optional_string,
    parse_object, path_error, result_ok, string_field,
};
use super::contract::CodingOperations;
use super::schemas::find_schema;
use super::search::GlobMatcher;
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct FindTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl FindTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_find()
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let args = match parse_object(self.name(), &call).and_then(|object| {
            Ok((
                string_field(self.name(), &object, "pattern")?,
                optional_string(self.name(), &object, "path")?,
                optional_positive_usize(self.name(), &object, "limit")?.unwrap_or(1000),
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        if GlobMatcher::new(&args.0).is_err() {
            return Box::pin(std::future::ready(Err(invalid(
                self.name(),
                "pattern is not a supported glob",
            ))));
        }
        let path = match path_error(
            self.name(),
            self.root.resolve_existing(args.1.as_deref().unwrap_or(".")),
        ) {
            Ok(path) => path,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            check_cancelled("find", &context)?;
            let metadata = operations
                .metadata(&path)
                .await
                .map_err(|error| operation_failure("find", error))?;
            if !metadata.is_directory {
                return Err(ToolError::Execution {
                    tool: "find".into(),
                    message: "path is not a directory".into(),
                });
            }
            let results = operations
                .find_files(&path, &args.0, args.2)
                .await
                .map_err(|error| operation_failure("find", error))?;
            if results.is_empty() {
                return Ok(result_ok(&call, "No files found matching pattern"));
            }
            let mut output = results.join("\n");
            if results.len() >= args.2 {
                output.push_str(&format!("\n\n[{} results limit reached]", args.2));
            }
            Ok(result_ok(&call, output))
        })
    }
}

fn static_schema_find() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(find_schema)
}
