//! The standard edit tool.

use super::arguments::{
    check_cancelled, field, invalid, operation_failure, parse_object, path_error, result_ok,
    string_field,
};
use super::contract::CodingOperations;
use super::schemas::edit_schema;
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct EditTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl EditTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

#[derive(Clone)]
struct EditSpec {
    old: String,
    new: String,
}

impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }
    fn schema(&self) -> &JsonValue {
        static_schema_edit()
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let args = match parse_edit_args(self.name(), &call) {
            Ok(args) => args,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let path = match path_error(self.name(), self.root.resolve_existing(&args.0)) {
            Ok(path) => path,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            check_cancelled("edit", &context)?;
            let original = operations
                .read_file(&path)
                .await
                .map_err(|error| operation_failure("edit", error))?;
            let mut text = String::from_utf8(original).map_err(|_| ToolError::Execution {
                tool: "edit".into(),
                message: "file is not valid UTF-8".into(),
            })?;
            let mut locations = Vec::new();
            for edit in &args.1 {
                if edit.old.is_empty() {
                    return Err(ToolError::InvalidArguments {
                        tool: "edit".into(),
                        message: "oldText cannot be empty".into(),
                    });
                }
                let matches = text
                    .match_indices(&edit.old)
                    .map(|(start, _)| start)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(ToolError::Execution {
                        tool: "edit".into(),
                        message: format!(
                            "oldText must match exactly once; found {} matches",
                            matches.len()
                        ),
                    });
                }
                let start = matches[0];
                let end = start + edit.old.len();
                locations.push((start, end, edit.clone()));
            }
            locations.sort_by_key(|(start, _, _)| *start);
            for pair in locations.windows(2) {
                if pair[0].1 > pair[1].0 {
                    return Err(ToolError::Execution {
                        tool: "edit".into(),
                        message: "edits overlap in the original file".into(),
                    });
                }
            }
            for (start, end, edit) in locations.into_iter().rev() {
                text.replace_range(start..end, &edit.new);
            }
            operations
                .write_file(&path, text.as_bytes())
                .await
                .map_err(|error| operation_failure("edit", error))?;
            Ok(result_ok(
                &call,
                format!(
                    "Successfully replaced {} block(s) in {}.",
                    args.1.len(),
                    args.0
                ),
            ))
        })
    }
}

fn parse_edit_args(name: &str, call: &ToolCall) -> Result<(String, Vec<EditSpec>), ToolError> {
    let object = parse_object(name, call)?;
    let path = string_field(name, &object, "path")?;
    let edits = match field(name, &object, "edits")? {
        JsonValue::Array(edits) => edits,
        _ => return Err(invalid(name, "argument \"edits\" must be an array")),
    };
    if edits.is_empty() {
        return Err(invalid(name, "argument \"edits\" cannot be empty"));
    }
    let mut result = Vec::with_capacity(edits.len());
    for edit in edits {
        let object = match edit {
            JsonValue::Object(object) => object,
            _ => return Err(invalid(name, "each edit must be an object")),
        };
        result.push(EditSpec {
            old: string_field(name, object, "oldText")?,
            new: string_field(name, object, "newText")?,
        });
    }
    Ok((path, result))
}

fn static_schema_edit() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(edit_schema)
}
