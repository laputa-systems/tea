//! The standard grep tool.

use super::arguments::{
    check_cancelled, invalid, operation_failure, optional_bool, optional_positive_usize,
    optional_string, parse_object, path_error, result_ok, string_field,
};
use super::contract::{CodingOperations, GrepOptions};
use super::schemas::grep_schema;
use super::search::TinyPattern;
use super::workspace::WorkspaceRoot;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct GrepTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl GrepTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_grep()
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
                optional_string(self.name(), &object, "glob")?,
                optional_bool(self.name(), &object, "ignoreCase")?.unwrap_or(false),
                optional_bool(self.name(), &object, "literal")?.unwrap_or(false),
                optional_positive_usize(self.name(), &object, "context")?.unwrap_or(0),
                optional_positive_usize(self.name(), &object, "limit")?.unwrap_or(100),
            ))
        }) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        if !args.4 && TinyPattern::new(&args.0, args.3).is_err() {
            return Box::pin(std::future::ready(Err(invalid(
                self.name(),
                "pattern is not a supported regular expression",
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
            check_cancelled("grep", &context)?;
            let search_root = path;
            let matches = operations
                .grep_files(
                    &search_root,
                    &args.0,
                    GrepOptions {
                        ignore_case: args.3,
                        literal: args.4,
                        context: args.5,
                        limit: args.6,
                        glob: args.2,
                    },
                )
                .await
                .map_err(|error| operation_failure("grep", error))?;
            if matches.is_empty() {
                return Ok(result_ok(&call, "No matches found"));
            }
            let mut output = String::new();
            for item in matches {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&format!("{}:{}: {}", item.path, item.line, item.text));
            }
            Ok(result_ok(&call, output))
        })
    }
}

fn static_schema_grep() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(grep_schema)
}
