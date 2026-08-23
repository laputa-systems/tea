//! Machine-readable operators' commands for explicit session directories.
//!
//! These commands never discover a session home or select a provider. They
//! operate only on the paths and transitive immutable roots the caller names.

use super::{AppError, SessionCommand};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tea_protocol::JsonValue;
use tea_session::{
    verify_session, ArtifactId, ArtifactQuota, ArtifactStore, DurabilityMode, FileArtifactStore,
    JsonlSession, SessionFact, SessionVerification,
};

/// Execute one explicit persistence command and return exactly one JSON object.
pub fn run_session_command(command: SessionCommand) -> Result<String, AppError> {
    let result = match command {
        SessionCommand::Inspect { directory } => {
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
    fields
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
            tea_harness::verify_harness_catalog(catalog, Arc::clone(&artifacts)).map_err(
                |error| {
                    AppError::Setup(format!(
                        "immutable harness catalog verification failed: {error}"
                    ))
                },
            )?,
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
