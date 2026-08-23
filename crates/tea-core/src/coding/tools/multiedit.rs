//! Tea v2's transactional multi-file exact-edit capability.

use super::arguments::{
    check_cancelled, field, invalid, operation_failure, optional_string, parse_object, path_error,
    result_ok, string_field,
};
use super::contract::{
    CodingOperations, ConditionalFileEdit, EditTransaction, EditTransactionOutcome,
};
use super::schemas::edit_v2_schema;
use super::workspace::WorkspaceRoot;
use crate::error::ToolError;
use crate::tool::{
    AgentTool, CancellationSettlementMode, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use tea_protocol::JsonValue;
use tea_session::Digest;

const MAX_FILES: usize = 32;
const MAX_EDITS_PER_FILE: usize = 64;
const MAX_TOTAL_EDITS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TOTAL_EDIT_BYTES: usize = 512 * 1024;
const MAX_TOTAL_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// The v2 implementation deliberately retains the durable `edit` tool name.
/// Its incompatible `files[]` request shape is selected by the separate Tea
/// v2 profile rather than a run-time one-of schema.
pub(crate) struct MultiEditTool {
    root: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
}

impl MultiEditTool {
    pub(crate) fn new(root: WorkspaceRoot, operations: Arc<dyn CodingOperations>) -> Self {
        Self { root, operations }
    }
}

#[derive(Clone)]
struct Replacement {
    old: String,
    new: String,
}

struct FileRequest {
    requested_path: String,
    expected_digest: Option<Digest>,
    edits: Vec<Replacement>,
}

impl AgentTool for MultiEditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        multiedit_v2_description()
    }

    fn schema(&self) -> &JsonValue {
        static_schema_edit_v2()
    }

    fn requires_exclusive_batch(&self) -> bool {
        true
    }

    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        // Once the host transaction has been requested, its receipt must settle
        // instead of the scheduler dropping a possibly committed future.
        CancellationSettlementMode::AwaitFuture
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let requests = match parse_multiedit_args(self.name(), &call) {
            Ok(value) => value,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let mut resolved = Vec::with_capacity(requests.len());
        let mut unique = BTreeSet::new();
        for request in requests {
            let path = match path_error(self.name(), self.root.resolve_existing(&request.requested_path)) {
                Ok(path) => path,
                Err(error) => return Box::pin(std::future::ready(Err(error))),
            };
            if !unique.insert(path.clone()) {
                return Box::pin(std::future::ready(Err(invalid(
                    self.name(),
                    "files[] contains the same canonical file more than once",
                ))));
            }
            resolved.push((path, request));
        }
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            check_cancelled("edit", &context)?;
            let paths = resolved
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            let snapshots = operations
                .read_file_snapshots(&paths, MAX_TOTAL_SNAPSHOT_BYTES)
                .await
                .map_err(|error| operation_failure("edit", error))?;
            if snapshots.len() != resolved.len()
                || snapshots
                    .iter()
                    .zip(&resolved)
                    .any(|(snapshot, (path, _))| &snapshot.path != path)
            {
                return Err(ToolError::Execution {
                    tool: "edit".into(),
                    message: "host returned snapshots that do not exactly match the requested edit plan".into(),
                });
            }
            let mut snapshot_bytes = 0_usize;
            let mut planned = Vec::with_capacity(resolved.len());
            let mut replacement_count = 0_usize;
            for ((path, request), snapshot) in resolved.into_iter().zip(snapshots) {
                check_cancelled("edit", &context)?;
                if !snapshot.is_regular_file {
                    return Err(ToolError::Execution {
                        tool: "edit".into(),
                        message: format!("{} is not an ordinary regular file", request.requested_path),
                    });
                }
                let original = snapshot.content;
                snapshot_bytes = snapshot_bytes.saturating_add(original.len());
                if snapshot_bytes > MAX_TOTAL_SNAPSHOT_BYTES {
                    return Err(ToolError::Execution {
                        tool: "edit".into(),
                        message: format!(
                            "the complete original snapshots exceed the {} byte transaction limit",
                            MAX_TOTAL_SNAPSHOT_BYTES
                        ),
                    });
                }
                if let Some(expected) = request.expected_digest {
                    if Digest::from_bytes(&original) != expected {
                        return Err(ToolError::Execution {
                            tool: "edit".into(),
                            message: format!(
                                "expectedDigest does not match the original snapshot for {}",
                                request.requested_path
                            ),
                        });
                    }
                }
                let replacement = apply_replacements(&request.requested_path, original, &request.edits)?;
                replacement_count = replacement_count.saturating_add(request.edits.len());
                planned.push(ConditionalFileEdit {
                    path,
                    expected_content: replacement.0,
                    replacement_content: replacement.1,
                });
            }
            check_cancelled("edit", &context)?;
            let transaction = EditTransaction {
                files: planned,
            };
            match operations
                .commit_edit_transaction(&transaction, context.cancellation.clone())
                .await
                .map_err(|error| operation_failure("edit", error))?
            {
                EditTransactionOutcome::Committed => Ok(result_ok(
                    &call,
                    format!(
                        "Applied {} replacements in {} files.",
                        replacement_count, transaction.files.len(),
                    ),
                )),
                EditTransactionOutcome::RolledBack { reason } => Err(ToolError::Execution {
                    tool: "edit".into(),
                    message: format!("edit transaction rolled back: {reason}"),
                }),
                EditTransactionOutcome::Indeterminate { reason } => Err(ToolError::Execution {
                    tool: "edit".into(),
                    message: format!(
                        "edit transaction state is indeterminate: {reason}. Read every requested file before retrying."
                    ),
                }),
            }
        })
    }
}

pub(crate) const fn multiedit_v2_description() -> &'static str {
    "Atomically validate an exact-edit plan for multiple existing UTF-8 files, then request one conditional host transaction. Supply files[] with one unique path per file. Each edits[].oldText must match exactly once and must not overlap any other edit in that file's original snapshot. All paths, optional expectedDigest values, UTF-8 decoding, and exact-match preconditions are checked before the transaction commit request. This tool must be the only call in its assistant tool batch. The local host guarantees all-precondition validation before publication and best-effort rollback on ordinary failure; it does not claim crash-atomic multi-file visibility."
}

fn parse_multiedit_args(name: &str, call: &ToolCall) -> Result<Vec<FileRequest>, ToolError> {
    let object = parse_object(name, call)?;
    let files = match field(name, &object, "files")? {
        JsonValue::Array(files) => files,
        _ => return Err(invalid(name, "argument \"files\" must be an array")),
    };
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(invalid(
            name,
            format!("argument \"files\" must contain 1 through {MAX_FILES} entries"),
        ));
    }
    let mut total_edits = 0_usize;
    let mut total_edit_bytes = 0_usize;
    let mut result = Vec::with_capacity(files.len());
    for file in files {
        let file = match file {
            JsonValue::Object(file) => file,
            _ => return Err(invalid(name, "each files[] entry must be an object")),
        };
        let requested_path = string_field(name, file, "path")?;
        if requested_path.is_empty() || requested_path.len() > MAX_PATH_BYTES {
            return Err(invalid(
                name,
                format!("files[].path must contain 1 through {MAX_PATH_BYTES} bytes"),
            ));
        }
        let expected_digest = optional_string(name, file, "expectedDigest")?
            .map(|value| {
                Digest::from_hex(&value).map_err(|error| {
                    invalid(name, format!("files[].expectedDigest must be a BLAKE3 digest: {error}"))
                })
            })
            .transpose()?;
        let edits = match field(name, file, "edits")? {
            JsonValue::Array(edits) => edits,
            _ => return Err(invalid(name, "files[].edits must be an array")),
        };
        if edits.is_empty() || edits.len() > MAX_EDITS_PER_FILE {
            return Err(invalid(
                name,
                format!("files[].edits must contain 1 through {MAX_EDITS_PER_FILE} entries"),
            ));
        }
        total_edits = total_edits.saturating_add(edits.len());
        if total_edits > MAX_TOTAL_EDITS {
            return Err(invalid(
                name,
                format!("files[].edits may contain at most {MAX_TOTAL_EDITS} entries in total"),
            ));
        }
        let mut replacements = Vec::with_capacity(edits.len());
        for edit in edits {
            let edit = match edit {
                JsonValue::Object(edit) => edit,
                _ => return Err(invalid(name, "each files[].edits[] entry must be an object")),
            };
            let old = string_field(name, edit, "oldText")?;
            let new = string_field(name, edit, "newText")?;
            if old.is_empty() {
                return Err(invalid(name, "files[].edits[].oldText cannot be empty"));
            }
            total_edit_bytes = total_edit_bytes.saturating_add(old.len()).saturating_add(new.len());
            if total_edit_bytes > MAX_TOTAL_EDIT_BYTES {
                return Err(invalid(
                    name,
                    format!("total oldText and newText bytes exceed the {MAX_TOTAL_EDIT_BYTES} byte limit"),
                ));
            }
            replacements.push(Replacement { old, new });
        }
        result.push(FileRequest {
            requested_path,
            expected_digest,
            edits: replacements,
        });
    }
    Ok(result)
}

fn apply_replacements(
    requested_path: &str,
    original: Vec<u8>,
    edits: &[Replacement],
) -> Result<(Vec<u8>, Vec<u8>), ToolError> {
    let mut text = String::from_utf8(original.clone()).map_err(|_| ToolError::Execution {
        tool: "edit".into(),
        message: format!("{requested_path} is not valid UTF-8"),
    })?;
    let mut locations = Vec::with_capacity(edits.len());
    for edit in edits {
        let matches = text
            .match_indices(&edit.old)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(ToolError::Execution {
                tool: "edit".into(),
                message: format!(
                    "oldText for {requested_path} must match exactly once; found {} matches",
                    matches.len()
                ),
            });
        }
        let start = matches[0];
        locations.push((start, start + edit.old.len(), edit));
    }
    locations.sort_by_key(|(start, _, _)| *start);
    if locations.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ToolError::Execution {
            tool: "edit".into(),
            message: format!("edits overlap in the original snapshot for {requested_path}"),
        });
    }
    for (start, end, edit) in locations.into_iter().rev() {
        text.replace_range(start..end, &edit.new);
    }
    Ok((original, text.into_bytes()))
}

fn static_schema_edit_v2() -> &'static JsonValue {
    use std::sync::OnceLock;
    static VALUE: OnceLock<JsonValue> = OnceLock::new();
    VALUE.get_or_init(edit_v2_schema)
}
