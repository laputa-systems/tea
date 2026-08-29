//! Trusted workspace and process authority for the revisioned coding bundle.
//!
//! This module intentionally has no `AgentTool` implementations.  Luau owns
//! the model-facing tools; these host capabilities validate requests again and
//! retain the filesystem, process, and transaction invariants that source
//! changes must never be able to weaken.

use super::tools::{
    CodingOperations, CommandEnvironment, ConditionalFileCreate, ConditionalFileEdit,
    EditTransaction, EditTransactionOutcome, LocalCodingOperations, OperationError, WorkspaceRoot,
};
use crate::harness::extension::{
    ExtensionCapability, ExtensionCapabilityError, ExtensionCapabilityFuture,
    ExtensionCapabilityRequest, ExtensionCapabilityResponse,
};
use crate::scheduler::CancellationToken;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tea_protocol::{JsonNumber, JsonValue};
use tea_session::Digest;

/// Capability granted only to the coding builtin's `read` declaration.
pub const WORKSPACE_READ_CAPABILITY_V1: &str = "tea.workspace.read.v1";
/// Capability granted only to the coding builtin's optimized `find` declaration.
pub const WORKSPACE_SEARCH_CAPABILITY_V1: &str = "tea.workspace.search.v1";
/// Capability granted only to the coding builtin's transactional `edit` declaration.
pub const WORKSPACE_MUTATE_CAPABILITY_V1: &str = "tea.workspace.mutate.v1";
/// Capability granted only to the coding builtin's `bash` declaration.
pub const PROCESS_CAPABILITY_V1: &str = "tea.process.v1";

const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSACTION_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSACTION_BYTES: usize = 512 * 1024;
const MAX_FILES: usize = 32;
const MAX_EDITS_PER_FILE: usize = 64;
const MAX_TOTAL_EDITS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;

/// Explicit authority selected by a host for one coding bundle instance.
///
/// The host captures the canonical workspace and optional process environment
/// once. Individual methods expose only the smallest authority required by a
/// particular Luau tool declaration.
#[derive(Clone)]
pub struct CodingHost {
    workspace: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
    environment: CommandEnvironment,
}

impl std::fmt::Debug for CodingHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodingHost")
            .field("workspace", &self.workspace)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl CodingHost {
    /// Construct local workspace/process authority for one existing workspace.
    pub fn new(workspace: impl AsRef<std::path::Path>) -> Result<Self, OperationError> {
        Self::with_operations(workspace, Arc::new(LocalCodingOperations))
    }

    /// Construct authority over caller-owned host operations.
    pub fn with_operations(
        workspace: impl AsRef<std::path::Path>,
        operations: Arc<dyn CodingOperations>,
    ) -> Result<Self, OperationError> {
        Ok(Self {
            workspace: WorkspaceRoot::new(workspace)?,
            operations,
            environment: CommandEnvironment::empty(),
        })
    }

    /// Replace the explicitly selected process environment policy.
    pub fn with_environment(mut self, environment: CommandEnvironment) -> Self {
        self.environment = environment;
        self
    }

    /// Borrow the canonical workspace authority.
    pub fn workspace(&self) -> &WorkspaceRoot {
        &self.workspace
    }

    /// Return the narrow read-only file capability.
    pub fn read_capability(&self) -> Arc<dyn ExtensionCapability> {
        Arc::new(WorkspaceReadCapability(self.clone()))
    }

    /// Return the narrow optimized search capability.
    pub fn search_capability(&self) -> Arc<dyn ExtensionCapability> {
        Arc::new(WorkspaceSearchCapability(self.clone()))
    }

    /// Return the narrow transactional workspace-mutation capability.
    pub fn mutate_capability(&self) -> Arc<dyn ExtensionCapability> {
        Arc::new(WorkspaceMutationCapability(self.clone()))
    }

    /// Return the narrow process capability.
    pub fn process_capability(&self) -> Arc<dyn ExtensionCapability> {
        Arc::new(ProcessCapability(self.clone()))
    }
}

#[derive(Clone)]
struct WorkspaceReadCapability(CodingHost);

impl ExtensionCapability for WorkspaceReadCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let host = self.0.clone();
        Box::pin(async move {
            deny_unexpected_method(&request, "read")?;
            let arguments = object(&request.arguments, "workspace read arguments")?;
            reject_unknown_fields(arguments, &["path", "offset", "limit", "includeDigest"])?;
            let path_input = required_string(arguments, "path")?;
            validate_path(&path_input)?;
            let offset = optional_positive_usize(arguments, "offset")?.unwrap_or(1);
            let limit = optional_positive_usize(arguments, "limit")?;
            let include_digest = optional_bool(arguments, "includeDigest")?.unwrap_or(false);
            let path = host
                .workspace
                .resolve_existing(&path_input)
                .map_err(operation_error)?;
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            let metadata = host
                .operations
                .metadata(&path)
                .await
                .map_err(operation_error)?;
            if !metadata.is_regular_file {
                return Err(ExtensionCapabilityError::Execution {
                    message: format!("{path_input} is not an ordinary regular file"),
                });
            }
            let bytes = host
                .operations
                .read_file(&path)
                .await
                .map_err(operation_error)?;
            if bytes.len() > MAX_READ_BYTES {
                return Err(ExtensionCapabilityError::Execution {
                    message: format!("file exceeds the {MAX_READ_BYTES} byte read limit"),
                });
            }
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines = text.split('\n').collect::<Vec<_>>();
            if offset > lines.len() && !lines.is_empty() {
                return Err(ExtensionCapabilityError::Execution {
                    message: format!("offset {offset} is beyond end of file"),
                });
            }
            let begin = offset.saturating_sub(1).min(lines.len());
            let end = limit
                .map(|limit| begin.saturating_add(limit).min(lines.len()))
                .unwrap_or(lines.len());
            let selected = lines[begin..end].join("\n");
            let (content, truncated) = truncate_read_output(selected.as_bytes());
            let mut response = BTreeMap::from([
                ("content".into(), JsonValue::String(content)),
                ("truncated".into(), JsonValue::Bool(truncated)),
            ]);
            if include_digest {
                response.insert(
                    "digest".into(),
                    JsonValue::String(Digest::from_bytes(&bytes).to_hex()),
                );
            }
            Ok(ExtensionCapabilityResponse {
                value: JsonValue::Object(response),
            })
        })
    }
}

#[derive(Clone)]
struct WorkspaceSearchCapability(CodingHost);

impl ExtensionCapability for WorkspaceSearchCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let host = self.0.clone();
        Box::pin(async move {
            deny_unexpected_method(&request, "find")?;
            let arguments = object(&request.arguments, "workspace search arguments")?;
            reject_unknown_fields(arguments, &["pattern", "path", "limit"])?;
            let pattern = required_string(arguments, "pattern")?;
            if super::tools::search::GlobMatcher::new(&pattern).is_err() {
                return Err(ExtensionCapabilityError::InvalidArguments {
                    message: "pattern is not a supported glob".into(),
                });
            }
            let path_input = optional_string(arguments, "path")?.unwrap_or_else(|| ".".into());
            validate_path(&path_input)?;
            let limit = optional_positive_usize(arguments, "limit")?.unwrap_or(1000);
            let path = host
                .workspace
                .resolve_existing(&path_input)
                .map_err(operation_error)?;
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            let metadata = host
                .operations
                .metadata(&path)
                .await
                .map_err(operation_error)?;
            if !metadata.is_directory {
                return Err(ExtensionCapabilityError::Execution {
                    message: "path is not a directory".into(),
                });
            }
            let matches = host
                .operations
                .find_files(&path, &pattern, limit)
                .await
                .map_err(operation_error)?;
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            Ok(ExtensionCapabilityResponse {
                value: JsonValue::object([
                    (
                        "matches",
                        JsonValue::Array(matches.into_iter().map(JsonValue::String).collect()),
                    ),
                    (
                        "limit",
                        JsonValue::Number(JsonNumber::Unsigned(limit as u64)),
                    ),
                ]),
            })
        })
    }
}

#[derive(Clone)]
struct ProcessCapability(CodingHost);

impl ExtensionCapability for ProcessCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let host = self.0.clone();
        Box::pin(async move {
            deny_unexpected_method(&request, "run")?;
            let arguments = object(&request.arguments, "process arguments")?;
            reject_unknown_fields(arguments, &["command", "timeout"])?;
            let command = required_string(arguments, "command")?;
            if command.trim().is_empty() || command.len() > 64 * 1024 {
                return Err(ExtensionCapabilityError::InvalidArguments {
                    message: "command must contain 1 through 65536 bytes".into(),
                });
            }
            let timeout = optional_timeout(arguments)?;
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            let output = host
                .operations
                .execute_command(
                    &command,
                    host.workspace.as_path(),
                    timeout,
                    &host.environment,
                    cancellation.clone(),
                    request.updates,
                )
                .await
                .map_err(operation_error)?;
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }
            let mut combined = output.stdout;
            if !output.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push(b'\n');
                }
                combined.extend_from_slice(&output.stderr);
            }
            let (content, truncated) = truncate_output(&combined);
            Ok(ExtensionCapabilityResponse {
                value: JsonValue::object([
                    ("content", JsonValue::String(content)),
                    ("truncated", JsonValue::Bool(truncated)),
                    (
                        "exitCode",
                        output
                            .exit_code
                            .map(|code| JsonValue::Number(JsonNumber::Signed(i64::from(code))))
                            .unwrap_or(JsonValue::Null),
                    ),
                ]),
            })
        })
    }
}

#[derive(Clone)]
struct WorkspaceMutationCapability(CodingHost);

impl ExtensionCapability for WorkspaceMutationCapability {
    fn invoke(
        &self,
        request: ExtensionCapabilityRequest,
        cancellation: CancellationToken,
    ) -> ExtensionCapabilityFuture {
        let host = self.0.clone();
        Box::pin(async move {
            deny_unexpected_method(&request, "commit")?;
            let files = parse_mutation_files(&request.arguments)?;
            let mut resolved = Vec::with_capacity(files.len());
            let mut unique = BTreeSet::new();
            for file in files {
                let path = match file.kind {
                    MutationKind::Edits(_) => host.workspace.resolve_existing(&file.path),
                    MutationKind::Content(_) => host.workspace.resolve_for_write(&file.path),
                }
                .map_err(operation_error)?;
                if !unique.insert(path.clone()) {
                    return Err(ExtensionCapabilityError::InvalidArguments {
                        message: "files[] contains the same canonical file more than once".into(),
                    });
                }
                resolved.push((path, file));
            }
            if cancellation.is_cancelled() {
                return Err(ExtensionCapabilityError::Cancelled);
            }

            let existing_paths = resolved
                .iter()
                .filter_map(|(path, _)| path.exists().then_some(path.clone()))
                .collect::<Vec<_>>();
            let snapshots = host
                .operations
                .read_file_snapshots(&existing_paths, MAX_TRANSACTION_SNAPSHOT_BYTES)
                .await
                .map_err(operation_error)?;
            if snapshots.len() != existing_paths.len()
                || snapshots
                    .iter()
                    .zip(&existing_paths)
                    .any(|(snapshot, path)| snapshot.path != *path)
            {
                return Err(ExtensionCapabilityError::Execution {
                    message: "host returned snapshots that do not exactly match the requested mutation plan".into(),
                });
            }
            let snapshot_by_path = snapshots
                .into_iter()
                .map(|snapshot| (snapshot.path.clone(), snapshot))
                .collect::<BTreeMap<_, _>>();
            let mut edits = Vec::new();
            let mut creates = Vec::new();
            let mut replacement_count = 0_usize;
            let mut replacement_files = 0_usize;
            let mut created_files = 0_usize;
            for (path, file) in resolved {
                if cancellation.is_cancelled() {
                    return Err(ExtensionCapabilityError::Cancelled);
                }
                match file.kind {
                    MutationKind::Edits(replacements) => {
                        let snapshot = snapshot_by_path.get(&path).ok_or_else(|| {
                            ExtensionCapabilityError::Execution {
                                message: format!(
                                    "{} disappeared before its snapshot was read",
                                    file.path
                                ),
                            }
                        })?;
                        if !snapshot.is_regular_file {
                            return Err(ExtensionCapabilityError::Execution {
                                message: format!("{} is not an ordinary regular file", file.path),
                            });
                        }
                        verify_digest(&file.path, file.expected_digest, &snapshot.content)?;
                        let replacement = apply_replacements(
                            &file.path,
                            snapshot.content.clone(),
                            &replacements,
                        )?;
                        replacement_count = replacement_count.saturating_add(replacements.len());
                        replacement_files = replacement_files.saturating_add(1);
                        edits.push(ConditionalFileEdit {
                            path,
                            expected_content: snapshot.content.clone(),
                            replacement_content: replacement,
                        });
                    }
                    MutationKind::Content(content) => {
                        if let Some(snapshot) = snapshot_by_path.get(&path) {
                            if !snapshot.is_regular_file {
                                return Err(ExtensionCapabilityError::Execution {
                                    message: format!(
                                        "{} is not an ordinary regular file",
                                        file.path
                                    ),
                                });
                            }
                            verify_digest(&file.path, file.expected_digest, &snapshot.content)?;
                            replacement_files = replacement_files.saturating_add(1);
                            edits.push(ConditionalFileEdit {
                                path,
                                expected_content: snapshot.content.clone(),
                                replacement_content: content.into_bytes(),
                            });
                        } else {
                            if file.expected_digest.is_some() {
                                return Err(ExtensionCapabilityError::InvalidArguments {
                                    message: format!(
                                        "files[].expectedDigest is only valid when {} already exists",
                                        file.path
                                    ),
                                });
                            }
                            created_files = created_files.saturating_add(1);
                            creates.push(ConditionalFileCreate {
                                path,
                                content: content.into_bytes(),
                            });
                        }
                    }
                }
            }
            let transaction = EditTransaction {
                files: edits,
                creates,
            };
            match host
                .operations
                .commit_edit_transaction(&transaction, cancellation.clone())
                .await
                .map_err(operation_error)?
            {
                EditTransactionOutcome::Committed => Ok(ExtensionCapabilityResponse {
                    value: JsonValue::object([
                        (
                            "files",
                            JsonValue::Number(JsonNumber::Unsigned(
                                (replacement_files + created_files) as u64,
                            )),
                        ),
                        (
                            "replacements",
                            JsonValue::Number(JsonNumber::Unsigned(replacement_count as u64)),
                        ),
                        (
                            "created",
                            JsonValue::Number(JsonNumber::Unsigned(created_files as u64)),
                        ),
                        (
                            "replaced",
                            JsonValue::Number(JsonNumber::Unsigned(replacement_files as u64)),
                        ),
                    ]),
                }),
                EditTransactionOutcome::RolledBack { reason } => {
                    Err(ExtensionCapabilityError::Execution {
                        message: format!("edit transaction rolled back: {reason}"),
                    })
                }
                EditTransactionOutcome::Indeterminate { reason } => {
                    Err(ExtensionCapabilityError::Execution {
                        message: format!(
                            "edit transaction state is indeterminate: {reason}. Read every requested file before retrying."
                        ),
                    })
                }
            }
        })
    }
}

#[derive(Clone)]
struct MutationFile {
    path: String,
    expected_digest: Option<Digest>,
    kind: MutationKind,
}

#[derive(Clone)]
enum MutationKind {
    Edits(Vec<Replacement>),
    Content(String),
}

#[derive(Clone)]
struct Replacement {
    old: String,
    new: String,
}

fn parse_mutation_files(
    arguments: &JsonValue,
) -> Result<Vec<MutationFile>, ExtensionCapabilityError> {
    let root = object(arguments, "workspace mutation arguments")?;
    reject_unknown_fields(root, &["files"])?;
    let files = root
        .get("files")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
            message: "files must be an array".into(),
        })?;
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(ExtensionCapabilityError::InvalidArguments {
            message: format!("files must contain 1 through {MAX_FILES} entries"),
        });
    }
    let mut total_edits = 0_usize;
    let mut total_bytes = 0_usize;
    let mut parsed = Vec::with_capacity(files.len());
    for value in files {
        let file = object(value, "files[] entry")?;
        reject_unknown_fields(file, &["path", "expectedDigest", "edits", "content"])?;
        let path = required_string(file, "path")?;
        validate_path(&path)?;
        let expected_digest = optional_string(file, "expectedDigest")?
            .map(|value| {
                Digest::from_hex(&value).map_err(|error| {
                    ExtensionCapabilityError::InvalidArguments {
                        message: format!("files[].expectedDigest must be a BLAKE3 digest: {error}"),
                    }
                })
            })
            .transpose()?;
        let has_edits = file.contains_key("edits");
        let has_content = file.contains_key("content");
        if has_edits == has_content {
            return Err(ExtensionCapabilityError::InvalidArguments {
                message: "each files[] entry must contain exactly one of edits or content".into(),
            });
        }
        let kind = if let Some(content) = file.get("content") {
            let content =
                content
                    .as_str()
                    .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
                        message: "files[].content must be a string".into(),
                    })?;
            total_bytes = total_bytes.saturating_add(content.len());
            MutationKind::Content(content.to_owned())
        } else {
            let edits = file
                .get("edits")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
                    message: "files[].edits must be an array".into(),
                })?;
            if edits.is_empty() || edits.len() > MAX_EDITS_PER_FILE {
                return Err(ExtensionCapabilityError::InvalidArguments {
                    message: format!(
                        "files[].edits must contain 1 through {MAX_EDITS_PER_FILE} entries"
                    ),
                });
            }
            total_edits = total_edits.saturating_add(edits.len());
            if total_edits > MAX_TOTAL_EDITS {
                return Err(ExtensionCapabilityError::InvalidArguments {
                    message: format!(
                        "files[].edits may contain at most {MAX_TOTAL_EDITS} entries in total"
                    ),
                });
            }
            let mut replacements = Vec::with_capacity(edits.len());
            for edit in edits {
                let edit = object(edit, "files[].edits[] entry")?;
                reject_unknown_fields(edit, &["oldText", "newText"])?;
                let old = required_string(edit, "oldText")?;
                let new = required_string(edit, "newText")?;
                if old.is_empty() {
                    return Err(ExtensionCapabilityError::InvalidArguments {
                        message: "files[].edits[].oldText cannot be empty".into(),
                    });
                }
                total_bytes = total_bytes
                    .saturating_add(old.len())
                    .saturating_add(new.len());
                replacements.push(Replacement { old, new });
            }
            MutationKind::Edits(replacements)
        };
        if total_bytes > MAX_TRANSACTION_BYTES {
            return Err(ExtensionCapabilityError::InvalidArguments {
                message: format!(
                    "total edit/content bytes exceed the {MAX_TRANSACTION_BYTES} byte limit"
                ),
            });
        }
        parsed.push(MutationFile {
            path,
            expected_digest,
            kind,
        });
    }
    Ok(parsed)
}

fn apply_replacements(
    requested_path: &str,
    original: Vec<u8>,
    edits: &[Replacement],
) -> Result<Vec<u8>, ExtensionCapabilityError> {
    let mut text =
        String::from_utf8(original).map_err(|_| ExtensionCapabilityError::Execution {
            message: format!("{requested_path} is not valid UTF-8"),
        })?;
    let mut locations = Vec::with_capacity(edits.len());
    for edit in edits {
        let matches = text
            .match_indices(&edit.old)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(ExtensionCapabilityError::Execution {
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
        return Err(ExtensionCapabilityError::Execution {
            message: format!("edits overlap in the original snapshot for {requested_path}"),
        });
    }
    for (start, end, edit) in locations.into_iter().rev() {
        text.replace_range(start..end, &edit.new);
    }
    Ok(text.into_bytes())
}

fn verify_digest(
    path: &str,
    expected: Option<Digest>,
    content: &[u8],
) -> Result<(), ExtensionCapabilityError> {
    if let Some(expected) = expected
        && Digest::from_bytes(content) != expected
    {
        return Err(ExtensionCapabilityError::Execution {
            message: format!("expectedDigest does not match the original snapshot for {path}"),
        });
    }
    Ok(())
}

fn deny_unexpected_method(
    request: &ExtensionCapabilityRequest,
    expected: &str,
) -> Result<(), ExtensionCapabilityError> {
    if request.method == expected {
        Ok(())
    } else {
        Err(ExtensionCapabilityError::MethodDenied {
            capability: request.capability.clone(),
            method: request.method.clone(),
        })
    }
}

fn object<'a>(
    value: &'a JsonValue,
    label: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ExtensionCapabilityError> {
    value
        .as_object()
        .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
            message: format!("{label} must be an object"),
        })
}

fn reject_unknown_fields(
    value: &BTreeMap<String, JsonValue>,
    expected: &[&str],
) -> Result<(), ExtensionCapabilityError> {
    if let Some(field) = value
        .keys()
        .find(|field| !expected.contains(&field.as_str()))
    {
        return Err(ExtensionCapabilityError::InvalidArguments {
            message: format!("unexpected argument {field:?}"),
        });
    }
    Ok(())
}

fn required_string(
    value: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<String, ExtensionCapabilityError> {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
            message: format!("argument {field:?} must be a string"),
        })
}

fn optional_string(
    value: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<String>, ExtensionCapabilityError> {
    value
        .get(field)
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ExtensionCapabilityError::InvalidArguments {
                    message: format!("argument {field:?} must be a string"),
                }
            })
        })
        .transpose()
}

fn optional_bool(
    value: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<bool>, ExtensionCapabilityError> {
    value
        .get(field)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| ExtensionCapabilityError::InvalidArguments {
                    message: format!("argument {field:?} must be a boolean"),
                })
        })
        .transpose()
}

fn optional_positive_usize(
    value: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<usize>, ExtensionCapabilityError> {
    value
        .get(field)
        .map(|value| {
            let value = match value {
                JsonValue::Number(JsonNumber::Unsigned(value)) if *value > 0 => *value,
                JsonValue::Number(JsonNumber::Signed(value)) if *value > 0 => *value as u64,
                JsonValue::Number(JsonNumber::Float(value))
                    if value.is_finite() && *value > 0.0 && value.fract() == 0.0 =>
                {
                    *value as u64
                }
                _ => {
                    return Err(ExtensionCapabilityError::InvalidArguments {
                        message: format!("argument {field:?} must be a positive integer"),
                    });
                }
            };
            usize::try_from(value).map_err(|_| ExtensionCapabilityError::InvalidArguments {
                message: format!("argument {field:?} is too large"),
            })
        })
        .transpose()
}

fn optional_timeout(
    value: &BTreeMap<String, JsonValue>,
) -> Result<Option<f64>, ExtensionCapabilityError> {
    let Some(value) = value.get("timeout") else {
        return Ok(None);
    };
    let timeout = match value {
        JsonValue::Number(JsonNumber::Float(value)) => *value,
        JsonValue::Number(JsonNumber::Unsigned(value)) => *value as f64,
        JsonValue::Number(JsonNumber::Signed(value)) => *value as f64,
        _ => {
            return Err(ExtensionCapabilityError::InvalidArguments {
                message: "argument \"timeout\" must be a number".into(),
            });
        }
    };
    if !timeout.is_finite() || timeout <= 0.0 || timeout > 2_147_483.647 {
        return Err(ExtensionCapabilityError::InvalidArguments {
            message: "timeout must be a finite positive number no greater than 2147.483647 seconds"
                .into(),
        });
    }
    Ok(Some(timeout))
}

fn validate_path(path: &str) -> Result<(), ExtensionCapabilityError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(ExtensionCapabilityError::InvalidArguments {
            message: format!("path must contain 1 through {MAX_PATH_BYTES} bytes"),
        });
    }
    Ok(())
}

fn operation_error(error: OperationError) -> ExtensionCapabilityError {
    if error.message() == "cancelled" {
        ExtensionCapabilityError::Cancelled
    } else {
        ExtensionCapabilityError::Execution {
            message: error.to_string(),
        }
    }
}

fn truncate_output(bytes: &[u8]) -> (String, bool) {
    const MAX_OUTPUT_BYTES: usize = 50 * 1024;
    const MAX_OUTPUT_LINES: usize = 2_000;
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut truncated = false;
    if lines.len() > MAX_OUTPUT_LINES {
        truncated = true;
        lines = lines.split_off(lines.len() - MAX_OUTPUT_LINES);
    }
    let mut output = lines.join("\n");
    if output.len() > MAX_OUTPUT_BYTES {
        truncated = true;
        let mut start = output.len() - MAX_OUTPUT_BYTES;
        while start < output.len() && !output.is_char_boundary(start) {
            start += 1;
        }
        output = output[start..].to_owned();
    }
    (output, truncated)
}

fn truncate_read_output(bytes: &[u8]) -> (String, bool) {
    const MAX_OUTPUT_BYTES: usize = 50 * 1024;
    const MAX_OUTPUT_LINES: usize = 2_000;
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if text.ends_with('\n') {
        lines.pop();
    }
    if lines.len() <= MAX_OUTPUT_LINES && text.len() <= MAX_OUTPUT_BYTES {
        return (text, false);
    }
    let mut output = String::new();
    for (index, line) in lines.into_iter().take(MAX_OUTPUT_LINES).enumerate() {
        let separator = usize::from(index != 0);
        if output.len() + separator + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        if separator != 0 {
            output.push('\n');
        }
        output.push_str(line);
    }
    (output, true)
}
