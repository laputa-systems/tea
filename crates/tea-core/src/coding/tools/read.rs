//! The standard read tool.

use super::arguments::{
    check_cancelled, operation_failure, optional_bool, optional_positive_usize, parse_object,
    path_error, result_ok, string_field, truncate_read_output,
};
use super::contract::CodingOperations;
use super::schemas::{read_schema, read_v2_schema};
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{AgentTool, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use tea_protocol::JsonValue;

pub(crate) struct ReadTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
    include_digest: bool,
}

impl ReadTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self {
            root,
            operations,
            include_digest: false,
        }
    }

    /// Construct Tea v2's read surface, which can return a complete-file
    /// BLAKE3 digest for an immediately following conditional edit.
    pub(crate) fn tea_v2(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self {
            root,
            operations,
            include_digest: true,
        }
    }
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        if self.include_digest {
            read_v2_description()
        } else {
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
        }
    }
    fn schema(&self) -> &JsonValue {
        if self.include_digest {
            static_schema_read_v2()
        } else {
            static_schema_read()
        }
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let include_digest = self.include_digest;
        let args = match parse_object(self.name(), &call).and_then(|object| {
            Ok((
                string_field(self.name(), &object, "path")?,
                optional_positive_usize(self.name(), &object, "offset")?,
                optional_positive_usize(self.name(), &object, "limit")?,
                if include_digest {
                    optional_bool(self.name(), &object, "includeDigest")?.unwrap_or(false)
                } else {
                    false
                },
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
            let digest = args.3.then(|| tea_session::Digest::from_bytes(&bytes).to_hex());
            let digest_suffix = digest
                .as_deref()
                .map(|digest| format!("\n[complete-file blake3: {digest}]"))
                .unwrap_or_default();
            Ok(result_ok(&call, format!("{}{}{}", output, suffix, digest_suffix)))
        })
    }
}

pub(crate) const fn read_v2_description() -> &'static str {
    "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. Set includeDigest=true to receive the complete-file BLAKE3 digest for a conditional edit; the digest covers bytes even when displayed text is truncated."
}

fn static_schema_read_v2() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(read_v2_schema)
}

fn static_schema_read() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(read_schema)
}
