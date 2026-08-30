//! Machine-readable operators' commands for durable sessions.
//!
//! Mutating and artifact-management commands retain explicit directory
//! arguments. Read-only `inspect` and `dump` commands resolve a session ID
//! below the caller's Tea home, then validate the immutable session header
//! before reading any records.

use super::super::build_info;
use super::{AppError, SessionCommand};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use tea_luau::LuauExtensionEngine;
use tea_protocol::JsonValue;
use tea_session::{
    verify_session, ArtifactId, ArtifactQuota, ArtifactStore, DurabilityMode, FileArtifactStore,
    JsonlSession, SessionFact, SessionVerification,
};

/// Execute one explicit persistence command and return exactly one JSON object.
pub fn run_session_command(command: SessionCommand) -> Result<String, AppError> {
    let result = match command {
        SessionCommand::InspectPath { directory } => {
            let inspection = JsonlSession::inspect(&directory)?;
            inspection_json(
                "inspect",
                &inspection.snapshot,
                inspection.torn_tail_offset,
                None,
                None,
                None,
            )
        }
        SessionCommand::Inspect {
            session_id,
            tea_home,
        } => {
            let directory = resolve_session_id(&session_id, tea_home.as_deref())?;
            let inspection = JsonlSession::inspect(&directory)?;
            inspection_json(
                "inspect",
                &inspection.snapshot,
                inspection.torn_tail_offset,
                None,
                None,
                None,
            )
        }
        SessionCommand::Dump {
            session_id,
            tea_home,
        } => {
            let directory = resolve_session_id(&session_id, tea_home.as_deref())?;
            dump_session(&directory)?
        }
        SessionCommand::Repair { directory } => {
            let repair = JsonlSession::repair_torn_tail(&directory, DurabilityMode::Strict)?;
            JsonValue::object([
                ("cache_warning", optional_string(repair.cache_warning)),
                ("operation", JsonValue::String("repair".into())),
                (
                    "truncated_tail_offset",
                    repair
                        .truncated_tail_offset
                        .map(JsonValue::from)
                        .unwrap_or(JsonValue::Null),
                ),
            ])
        }
        SessionCommand::RebuildMeta { directory } => {
            let (snapshot, head_cache_warning) =
                super::durable::rebuild_host_session_metadata(&directory)?;
            let mut fields = snapshot_identity_fields(&snapshot);
            fields.insert("cache_warning".into(), optional_string(head_cache_warning));
            fields.insert("operation".into(), JsonValue::String("rebuild-meta".into()));
            JsonValue::Object(fields)
        }
        SessionCommand::Verify {
            directory,
            additional_roots,
        } => {
            let inspection = JsonlSession::inspect(&directory)?;
            let artifacts = FileArtifactStore::open(directory.join("objects"))?;
            let roots = complete_artifact_roots(
                &inspection.snapshot,
                &artifacts,
                parse_roots(additional_roots)?,
            )?;
            let verification =
                verify_session(&inspection.snapshot, &artifacts, roots.iter().copied())
                    .map_err(|error| AppError::Setup(error.to_string()))?;
            inspection_json(
                "verify",
                &inspection.snapshot,
                inspection.torn_tail_offset,
                Some(&verification),
                Some(JsonlSession::head_cache_is_current(
                    &directory,
                    &inspection.snapshot,
                )),
                Some(super::durable::host_session_metadata_is_current(
                    &directory,
                    &inspection.snapshot,
                )),
            )
        }
        SessionCommand::Gc {
            directory,
            additional_roots,
            apply,
        } => {
            let mut session = JsonlSession::open(&directory, DurabilityMode::Strict)?;
            let snapshot = session.snapshot()?;
            let artifacts = session.artifact_store()?;
            let roots =
                complete_artifact_roots(&snapshot, &artifacts, parse_roots(additional_roots)?)?;
            let plan = tea_session::plan_artifact_gc(
                &artifacts,
                &snapshot,
                roots.iter().copied(),
                ArtifactQuota::default(),
            )?;
            let candidates = JsonValue::Array(
                plan.unreferenced
                    .iter()
                    .map(|item| JsonValue::String(item.artifact_id.to_hex()))
                    .collect(),
            );
            let removed = if apply {
                session
                    .collect_unreferenced_artifacts(
                        roots.iter().copied(),
                        ArtifactQuota::default(),
                    )?
                    .removed
                    .len() as u64
            } else {
                0
            };
            JsonValue::object([
                ("applied", JsonValue::Bool(apply)),
                ("candidates", candidates),
                ("operation", JsonValue::String("gc".into())),
                ("removed", JsonValue::from(removed)),
            ])
        }
        SessionCommand::Export {
            source,
            destination,
            additional_roots,
        } => {
            let mut session = JsonlSession::open(&source, DurabilityMode::Strict)?;
            let snapshot = session.snapshot()?;
            let artifacts = session.artifact_store()?;
            let roots =
                complete_artifact_roots(&snapshot, &artifacts, parse_roots(additional_roots)?)?;
            let export = session
                .export_to(&destination, roots.iter().copied())
                .map_err(|error| AppError::Setup(error.to_string()))?;
            export_json("export", &export.directory, &export.verification)
        }
        SessionCommand::Restore {
            source,
            destination,
        } => {
            let mut session = JsonlSession::open(&source, DurabilityMode::Strict)?;
            let snapshot = session.snapshot()?;
            let artifacts = session.artifact_store()?;
            let declared_roots = read_export_roots(&source, &snapshot, &artifacts)?;
            let roots = complete_artifact_roots(&snapshot, &artifacts, declared_roots.clone())?;
            if declared_roots.iter().collect::<BTreeSet<_>>() != roots.iter().collect() {
                return Err(AppError::Setup(
                    "export manifest omits immutable harness source roots required by its session prefix"
                        .into(),
                ));
            }
            let export = session
                .export_to(&destination, roots)
                .map_err(|error| AppError::Setup(error.to_string()))?;
            export_json("restore", &export.directory, &export.verification)
        }
    };
    result.to_json_string().map_err(|error| {
        AppError::Setup(format!("could not encode session command output: {error}"))
    })
}

fn resolve_session_id(
    session_id: &std::ffi::OsStr,
    tea_home: Option<&Path>,
) -> Result<std::path::PathBuf, AppError> {
    let id = session_id
        .to_str()
        .ok_or_else(|| AppError::Setup("session ID must be UTF-8".into()))?;
    tea_session::SessionId::new(id.to_owned())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let home = match tea_home {
        Some(path) if path.as_os_str().is_empty() => {
            return Err(AppError::Setup("--tea-home must not be empty".into()));
        }
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .map(|path| path.join(".tea"))
            .ok_or_else(|| AppError::Setup("could not resolve the user home directory".into()))?,
    };
    let root = home.join("sessions");
    let root_metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::Setup(format!(
                "session {id:?} was not found in {}",
                root.display()
            )));
        }
        Err(error) => {
            return Err(AppError::Setup(format!(
                "could not inspect {}: {error}",
                root.display()
            )));
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(AppError::Setup(format!(
            "session root {} must be a real directory",
            root.display()
        )));
    }
    let entries = fs::read_dir(&root)
        .map_err(|error| AppError::Setup(format!("could not list {}: {error}", root.display())))?;
    let wanted = format!("{id}.tea");
    let mut matches = Vec::new();
    for entry in entries {
        let workspace_root = entry
            .map_err(|error| AppError::Setup(error.to_string()))?
            .path();
        let metadata = fs::symlink_metadata(&workspace_root)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::Setup(format!(
                "workspace session root {} must be a real directory",
                workspace_root.display()
            )));
        }
        if !metadata.is_dir() {
            continue;
        }
        let candidate = workspace_root.join(&wanted);
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(AppError::Setup(error.to_string())),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::Setup(format!(
                "session path {} must be a real directory",
                candidate.display()
            )));
        }
        let inspection = JsonlSession::inspect(&candidate).map_err(|error| {
            AppError::Setup(format!(
                "could not inspect session {}: {error}",
                candidate.display()
            ))
        })?;
        if inspection.snapshot.header().session_id.as_str() != id {
            return Err(AppError::Setup(format!(
                "session path {} disagrees with its immutable header",
                candidate.display()
            )));
        }
        matches.push(candidate);
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(AppError::Setup(format!(
            "session {id:?} was not found in {}",
            root.display()
        ))),
        _ => Err(AppError::Setup(format!(
            "session ID {id:?} is ambiguous across workspace roots"
        ))),
    }
}

fn dump_session(directory: &Path) -> Result<JsonValue, AppError> {
    let inspection = JsonlSession::inspect(directory)?;
    let path = directory.join("session.jsonl");
    let bytes = fs::read(&path)
        .map_err(|error| AppError::Setup(format!("could not read {}: {error}", path.display())))?;
    let end = inspection
        .torn_tail_offset
        .map(|offset| {
            usize::try_from(offset).map_err(|_| {
                AppError::Setup(format!(
                    "session tail offset exceeds addressable memory: {offset}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(bytes.len());
    let prefix = bytes.get(..end).ok_or_else(|| {
        AppError::Setup(format!(
            "session tail offset {end} exceeds {} bytes",
            bytes.len()
        ))
    })?;
    let mut records = Vec::new();
    for line in prefix.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(line)
            .map_err(|error| AppError::Setup(format!("session record is not UTF-8: {error}")))?;
        let mut record = JsonValue::parse(line)
            .map_err(|error| AppError::Setup(format!("could not parse session record: {error}")))?;
        redact_opaque_continuation_payloads(&mut record);
        records.push(record);
    }
    Ok(JsonValue::object([
        ("operation", JsonValue::String("dump".into())),
        (
            "session_id",
            JsonValue::String(inspection.snapshot.header().session_id.to_string()),
        ),
        (
            "tea_version",
            session_metadata_string(
                &inspection.snapshot,
                build_info::SESSION_VERSION_METADATA_KEY,
            ),
        ),
        (
            "tea_git_sha",
            session_metadata_string(
                &inspection.snapshot,
                build_info::SESSION_GIT_SHA_METADATA_KEY,
            ),
        ),
        (
            "through_digest",
            JsonValue::String(inspection.snapshot.last_digest().to_hex()),
        ),
        (
            "through_seq",
            JsonValue::from(inspection.snapshot.last_sequence().0),
        ),
        (
            "torn_tail_offset",
            inspection
                .torn_tail_offset
                .map(JsonValue::from)
                .unwrap_or(JsonValue::Null),
        ),
        ("records", JsonValue::Array(records)),
    ]))
}

/// Remove provider-private continuation bytes from the human-facing dump while
/// preserving every other authoritative record field. The JSONL file itself
/// remains complete so the matching adapter can resume its protocol state.
fn redact_opaque_continuation_payloads(value: &mut JsonValue) {
    match value {
        JsonValue::Array(values) => {
            for value in values {
                redact_opaque_continuation_payloads(value);
            }
        }
        JsonValue::Object(fields) => {
            if let Some(JsonValue::Array(items)) = fields.get_mut("opaque_context") {
                for item in items {
                    if let Some(item) = item.as_object_mut() {
                        if item.contains_key("payload") {
                            item.insert("payload".into(), JsonValue::String("[redacted]".into()));
                        }
                    }
                }
            }
            for value in fields.values_mut() {
                redact_opaque_continuation_payloads(value);
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
}

fn inspection_json(
    operation: &str,
    snapshot: &tea_session::SessionSnapshot,
    torn_tail_offset: Option<u64>,
    verification: Option<&SessionVerification>,
    head_cache_current: Option<bool>,
    metadata_cache_current: Option<bool>,
) -> JsonValue {
    let mut fields = snapshot_identity_fields(snapshot);
    fields.insert("operation".into(), JsonValue::String(operation.into()));
    fields.insert(
        "torn_tail_offset".into(),
        torn_tail_offset
            .map(JsonValue::from)
            .unwrap_or(JsonValue::Null),
    );
    if let Some(verification) = verification {
        fields.insert(
            "artifact_bytes".into(),
            JsonValue::from(verification.artifact_bytes),
        );
        fields.insert(
            "artifact_count".into(),
            JsonValue::from(verification.artifact_count as u64),
        );
        fields.insert(
            "orphan_artifacts".into(),
            JsonValue::Array(
                verification
                    .orphaned_artifacts
                    .iter()
                    .map(|item| {
                        JsonValue::object([
                            ("artifact_id", JsonValue::String(item.artifact_id.to_hex())),
                            ("byte_len", JsonValue::from(item.byte_len)),
                        ])
                    })
                    .collect(),
            ),
        );
    }
    if let Some(head_cache_current) = head_cache_current {
        fields.insert(
            "head_cache_current".into(),
            JsonValue::Bool(head_cache_current),
        );
    }
    if let Some(metadata_cache_current) = metadata_cache_current {
        fields.insert(
            "metadata_cache_current".into(),
            JsonValue::Bool(metadata_cache_current),
        );
    }
    JsonValue::Object(fields)
}

fn export_json(operation: &str, directory: &Path, verification: &SessionVerification) -> JsonValue {
    JsonValue::object([
        (
            "directory",
            JsonValue::String(directory.display().to_string()),
        ),
        ("operation", JsonValue::String(operation.into())),
        (
            "session_id",
            JsonValue::String(verification.session_id.to_string()),
        ),
        (
            "through_digest",
            JsonValue::String(verification.last_digest.to_hex()),
        ),
        ("through_seq", JsonValue::from(verification.last_sequence.0)),
    ])
}

fn snapshot_identity_fields(
    snapshot: &tea_session::SessionSnapshot,
) -> std::collections::BTreeMap<String, JsonValue> {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "session_id".into(),
        JsonValue::String(snapshot.header().session_id.to_string()),
    );
    fields.insert(
        "through_digest".into(),
        JsonValue::String(snapshot.last_digest().to_hex()),
    );
    fields.insert(
        "through_seq".into(),
        JsonValue::from(snapshot.last_sequence().0),
    );
    fields.insert(
        "tea_version".into(),
        session_metadata_string(snapshot, build_info::SESSION_VERSION_METADATA_KEY),
    );
    fields.insert(
        "tea_git_sha".into(),
        session_metadata_string(snapshot, build_info::SESSION_GIT_SHA_METADATA_KEY),
    );
    fields
}

fn session_metadata_string(snapshot: &tea_session::SessionSnapshot, key: &str) -> JsonValue {
    snapshot
        .header()
        .metadata
        .get(key)
        .and_then(JsonValue::as_str)
        .map(|value| JsonValue::String(value.to_owned()))
        .unwrap_or(JsonValue::Null)
}

fn optional_string(value: Option<String>) -> JsonValue {
    value.map(JsonValue::String).unwrap_or(JsonValue::Null)
}

fn parse_roots(values: Vec<OsString>) -> Result<Vec<ArtifactId>, AppError> {
    values
        .into_iter()
        .map(|value| {
            let value = value.to_str().ok_or_else(|| {
                AppError::Setup("artifact root IDs must be valid UTF-8 hexadecimal digests".into())
            })?;
            ArtifactId::from_hex(value).map_err(|error| {
                AppError::Setup(format!("invalid artifact root {value:?}: {error}"))
            })
        })
        .collect()
}

/// Combine explicit operator roots with the source objects retained by every
/// immutable harness catalog committed in the session. A catalog descriptor is
/// a direct session artifact root, but its source tree objects are only
/// reachable after validating the manifest itself.
fn complete_artifact_roots(
    snapshot: &tea_session::SessionSnapshot,
    artifacts: &FileArtifactStore,
    additional_roots: Vec<ArtifactId>,
) -> Result<Vec<ArtifactId>, AppError> {
    let mut roots = additional_roots.into_iter().collect::<BTreeSet<_>>();
    let artifacts: Arc<dyn tea_session::ArtifactStore> = Arc::new(artifacts.clone());
    for stored in snapshot.facts() {
        let SessionFact::HarnessCatalog(catalog) = &stored.fact else {
            continue;
        };
        roots.extend(
            tea_core::harness::verify_harness_catalog_with_extension_engine(
                catalog,
                Arc::clone(&artifacts),
                Arc::new(LuauExtensionEngine),
            )
            .map_err(|error| {
                AppError::Setup(format!(
                    "immutable harness catalog verification failed: {error}"
                ))
            })?,
        );
    }
    Ok(roots.into_iter().collect())
}

fn read_export_roots(
    directory: &Path,
    snapshot: &tea_session::SessionSnapshot,
    artifacts: &FileArtifactStore,
) -> Result<Vec<ArtifactId>, AppError> {
    let path = directory.join("export.json");
    let source = fs::read_to_string(&path)
        .map_err(|error| AppError::Setup(format!("could not read {}: {error}", path.display())))?;
    let value = JsonValue::parse(&source).map_err(|error| {
        AppError::Setup(format!(
            "invalid export manifest {}: {error}",
            path.display()
        ))
    })?;
    let canonical = value.to_json_string().map_err(|error| {
        AppError::Setup(format!("could not canonicalize export manifest: {error}"))
    })?;
    if source.strip_suffix('\n') != Some(canonical.as_str()) {
        return Err(AppError::Setup(
            "export manifest is not canonical JSON with one final newline".into(),
        ));
    }
    let object = value
        .as_object()
        .filter(|object| object.len() == 5)
        .ok_or_else(|| AppError::Setup("export manifest has an unexpected schema".into()))?;
    if object.get("format").and_then(JsonValue::as_str) != Some("tea-session-export-v1") {
        return Err(AppError::Setup(
            "export manifest has an unsupported format".into(),
        ));
    }
    if object.get("session_id").and_then(JsonValue::as_str)
        != Some(snapshot.header().session_id.as_str())
        || object.get("through_seq").and_then(JsonValue::as_u64) != Some(snapshot.last_sequence().0)
        || object.get("through_digest").and_then(JsonValue::as_str)
            != Some(snapshot.last_digest().to_hex().as_str())
    {
        return Err(AppError::Setup(
            "export manifest does not name the validated source session prefix".into(),
        ));
    }
    let descriptors = object
        .get("artifacts")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            AppError::Setup("export manifest has no artifact descriptor array".into())
        })?;
    let roots = descriptors
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .filter(|object| {
                    object.len() == 2
                        && object.contains_key("artifact_id")
                        && object.contains_key("byte_len")
                })
                .ok_or_else(|| AppError::Setup("export artifact has an unexpected schema".into()))?;
            let value = object
                .get("artifact_id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| AppError::Setup("export artifact ID is not a string".into()))?;
            let artifact_id = ArtifactId::from_hex(value)
                .map_err(|error| AppError::Setup(format!("invalid export manifest artifact ID: {error}")))?;
            let byte_len = object
                .get("byte_len")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| AppError::Setup("export artifact byte length is not an unsigned integer".into()))?;
            let actual = artifacts.verify_object(artifact_id).map_err(|error| {
                AppError::Setup(format!(
                    "export manifest artifact {artifact_id} cannot be verified: {error}"
                ))
            })?;
            if actual != byte_len {
                return Err(AppError::Setup(format!(
                    "export manifest artifact {artifact_id} has byte length {byte_len}, but verified bytes have length {actual}"
                )));
            }
            Ok(artifact_id)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut sorted = roots.clone();
    sorted.sort();
    sorted.dedup();
    if roots != sorted {
        return Err(AppError::Setup(
            "export manifest artifacts must be strictly sorted without duplicates".into(),
        ));
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_redacts_opaque_continuation_payloads_without_removing_their_identity() {
        let mut record = JsonValue::object([(
            "body",
            JsonValue::object([(
                "opaque_context",
                JsonValue::Array(vec![JsonValue::object([
                    ("provider", JsonValue::String("codex".into())),
                    ("kind", JsonValue::String("reasoning".into())),
                    ("item_id", JsonValue::String("rs_1".into())),
                    ("payload", JsonValue::String("encrypted-secret".into())),
                ])]),
            )]),
        )]);

        redact_opaque_continuation_payloads(&mut record);
        let rendered = record
            .to_json_string()
            .expect("redacted dump record should serialize");
        assert!(!rendered.contains("encrypted-secret"));
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("\"provider\":\"codex\""));
        assert!(rendered.contains("\"item_id\":\"rs_1\""));
    }
}
