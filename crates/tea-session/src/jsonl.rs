//! Strict append-only JSONL v1 session storage.

use crate::ids::*;
use crate::model::*;
use crate::store::{SessionError, SessionReader, SessionWriter, commit_time_ms, validate_snapshot};
use crate::{
    ArtifactError, ArtifactStore, JsonValue, LaneId, SessionVerification, SessionVerificationError,
    verify_session,
};
use rustix::fs::{FlockOperation, flock};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EXPORT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

macro_rules! parse_id {
    ($type:ty, $value:expr) => {
        <$type as ParseOpaqueId>::parse_opaque($value).map_err(|error| error.to_string())?
    };
}

/// Filesystem durability selected explicitly by the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurabilityMode {
    /// Flush and synchronize every committed JSONL mutation. This is the
    /// production default for accepted operations and effect boundaries.
    Strict,
    /// Flush each line but deliberately skip `fsync` for local development.
    /// The mode is explicit in the constructor and never silently selected.
    Development,
}

/// One complete portable durable-session export.
///
/// The destination contains the authoritative JSONL prefix and only immutable
/// objects reachable from that prefix plus caller-supplied transitive roots.
/// Reconstructible worktrees, orphan artifacts, and process-local state are
/// intentionally excluded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionExport {
    /// Newly created session-directory export root.
    pub directory: PathBuf,
    /// Verification evidence computed from the exported durable prefix.
    pub verification: SessionVerification,
}

/// Failure while creating an atomic portable session export.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionExportError {
    /// The source or destination durable-session operation failed.
    Session(SessionError),
    /// A required immutable object could not be read or copied safely.
    Artifact(ArtifactError),
    /// The source or completed export failed read-only verification.
    Verification(SessionVerificationError),
}

impl std::fmt::Display for SessionExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(error) => write!(formatter, "session export failed: {error}"),
            Self::Artifact(error) => write!(formatter, "session export artifact failed: {error}"),
            Self::Verification(error) => {
                write!(formatter, "session export verification failed: {error}")
            }
        }
    }
}

impl std::error::Error for SessionExportError {}

impl From<SessionError> for SessionExportError {
    fn from(value: SessionError) -> Self {
        Self::Session(value)
    }
}

impl From<ArtifactError> for SessionExportError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

impl From<SessionVerificationError> for SessionExportError {
    fn from(value: SessionVerificationError) -> Self {
        Self::Verification(value)
    }
}

/// A single-writer JSONL v1 session directory.
///
/// The held advisory file lock is released by the operating system if the
/// process dies, so recovery does not depend on deleting a stale lock file.
#[derive(Debug)]
pub struct JsonlSession {
    directory: PathBuf,
    session_path: PathBuf,
    file: File,
    snapshot: SessionSnapshot,
    durability: DurabilityMode,
    fault: Option<String>,
}

impl JsonlSession {
    /// Create a new session directory and its initial v1 header atomically.
    pub fn create(
        directory: impl AsRef<Path>,
        header: SessionHeader,
        durability: DurabilityMode,
    ) -> Result<Self, SessionError> {
        if header.kind != "session" || header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::InvalidInput {
                message: "JSONL v1 creation requires a v1 session header".into(),
            });
        }
        let directory = directory.as_ref().to_path_buf();
        if directory.exists() {
            return Err(SessionError::Io {
                path: directory.display().to_string(),
                message: "refusing to create a session over an existing directory".into(),
            });
        }
        create_private_directory(&directory)?;
        create_layout(&directory)?;
        let session_path = directory.join("session.jsonl");
        let encoded = encode_header(&header).to_json_string().map_err(|error| {
            SessionError::InvalidInput {
                message: format!("session header cannot encode as JSON: {error}"),
            }
        })?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&session_path)
            .map_err(|error| io(&session_path, error))?;
        acquire_writer_lock(&file, &session_path)?;
        if let Err(error) = write_complete_line(&mut file, &encoded, durability, &session_path) {
            let _ = fs::remove_file(&session_path);
            return Err(error);
        }
        if durability == DurabilityMode::Strict {
            sync_directory(&directory)?;
        }
        write_head_cache(&directory, Sequence(0), durability)?;
        Ok(Self {
            directory,
            session_path,
            file,
            snapshot: SessionSnapshot::empty(header),
            durability,
            fault: None,
        })
    }

    /// Open a v1 session, truncate only an uncommitted final tail, validate
    /// every complete line, and acquire the sole writer lock.
    pub fn open(
        directory: impl AsRef<Path>,
        durability: DurabilityMode,
    ) -> Result<Self, SessionError> {
        let directory = directory.as_ref().to_path_buf();
        ensure_real_directory(&directory)?;
        ensure_layout(&directory)?;
        let session_path = directory.join("session.jsonl");
        ensure_regular_file(&session_path)?;
        let mut file = OpenOptions::new()
            .read(true)
            
            .append(true)
            .open(&session_path)
            .map_err(|error| io(&session_path, error))?;
        acquire_writer_lock(&file, &session_path)?;
        let source = recover_complete_source(&mut file, &session_path, durability)?;
        let source =
            truncate_malformed_final_json_line(&mut file, &session_path, source, durability)?;
        let snapshot = decode_snapshot(&session_path, &source)?;
        validate_snapshot(&snapshot)?;
        Ok(Self {
            directory,
            session_path,
            file,
            snapshot,
            durability,
            fault: None,
        })
    }

    /// Return the explicit session-directory root.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Open the immutable object store colocated with this v1 session.
    ///
    /// Callers that construct a durable harness over this writer should pass
    /// this store to `DurableHarness::new_with_artifact_store`, so every
    /// `tea-artifact://` locator remains valid after the JSONL writer reopens.
    pub fn artifact_store(&self) -> Result<crate::FileArtifactStore, crate::ArtifactError> {
        crate::FileArtifactStore::open(self.directory.join("objects"))
    }

    /// Create a complete immutable export at a new sibling directory.
    ///
    /// The source writer synchronizes its current prefix before copying it.
    /// A temporary private directory is renamed only after every referenced
    /// object has been copied and independently verified, so callers never
    /// receive a plausible-looking partial export.
    pub fn export_to(
        &mut self,
        destination: impl AsRef<Path>,
        additional_roots: impl IntoIterator<Item = crate::ArtifactId>,
    ) -> Result<SessionExport, SessionExportError> {
        self.file
            .sync_data()
            .map_err(|error| io(&self.session_path, error))?;
        let snapshot = self.snapshot.clone();
        let additional_roots = additional_roots
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let source_store = self.artifact_store()?;
        let verification =
            verify_session(&snapshot, &source_store, additional_roots.iter().copied())?;

        let destination = destination.as_ref().to_path_buf();
        if destination.exists() {
            return Err(SessionError::InvalidInput {
                message: format!(
                    "refusing to export a session over existing destination {}",
                    destination.display()
                ),
            }
            .into());
        }
        let parent = destination
            .parent()
            .ok_or_else(|| SessionError::InvalidInput {
                message: "session export destination must have a parent directory".into(),
            })?;
        ensure_export_parent(parent)?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| SessionError::InvalidInput {
                message: "session export destination must have a UTF-8 file name".into(),
            })?;
        let nonce = NEXT_EXPORT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.export-{}-{nonce:016x}",
            std::process::id()
        ));

        let result = (|| {
            create_private_directory(&temporary)?;
            create_layout(&temporary)?;
            copy_session_prefix(
                &self.session_path,
                &temporary.join("session.jsonl"),
                self.durability,
            )?;
            write_head_cache(&temporary, snapshot.last_sequence(), self.durability)?;

            let destination_store = crate::FileArtifactStore::open(temporary.join("objects"))?;
            for artifact_id in &verification.artifact_roots {
                let bytes = source_store.get(*artifact_id)?;
                let copied = destination_store.put(&bytes, "application/octet-stream")?;
                if copied.artifact_id != *artifact_id || copied.byte_len != bytes.len() as u64 {
                    return Err(
                        SessionVerificationError::Artifact(ArtifactError::Corruption {
                            artifact_id: *artifact_id,
                            message:
                                "export destination returned a different immutable object identity"
                                    .into(),
                        })
                        .into(),
                    );
                }
            }
            verify_session(
                &snapshot,
                &destination_store,
                additional_roots.iter().copied(),
            )?;
            fs::rename(&temporary, &destination).map_err(|error| io(&destination, error))?;
            if self.durability == DurabilityMode::Strict {
                sync_directory(parent)?;
            }

            let exported = JsonlSession::open(&destination, self.durability)?;
            let exported_store = exported.artifact_store()?;
            let exported_verification = verify_session(
                &exported.snapshot()?,
                &exported_store,
                additional_roots.iter().copied(),
            )?;
            Ok(SessionExport {
                directory: destination.clone(),
                verification: exported_verification,
            })
        })();
        if result.is_err() && temporary.exists() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    /// Return an atomic snapshot of the current durable prefix.
    pub fn snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        Ok(self.snapshot.clone())
    }

    fn writable(&self) -> Result<(), SessionError> {
        match &self.fault {
            Some(message) => Err(SessionError::Faulted {
                message: message.clone(),
            }),
            None => Ok(()),
        }
    }

    fn write_mutation(&mut self, mutation: StoredMutation) -> Result<(), SessionError> {
        self.writable()?;
        let mut candidate = self.snapshot.clone();
        match &mutation {
            StoredMutation::Entry(value) => candidate.push_entry(value.clone()),
            StoredMutation::Record(value) => candidate.push_record(value.clone()),
            StoredMutation::Lane(value) => candidate.push_lane_mutation(value.clone()),
            StoredMutation::Fact(value) => candidate.push_fact(value.clone()),
        }
        // Reject invalid caller input before it can become a durable line.
        // This keeps an in-process validation failure from poisoning the
        // append-only log or requiring recovery to interpret an invalid fact.
        validate_snapshot(&candidate)?;
        let encoded = encode_mutation(&mutation)
            .to_json_string()
            .map_err(|error| SessionError::InvalidInput {
                message: format!("session mutation cannot encode as JSON: {error}"),
            })?;
        if let Err(error) = write_complete_line(
            &mut self.file,
            &encoded,
            self.durability,
            &self.session_path,
        ) {
            self.fault = Some(error.to_string());
            return Err(error);
        }
        // `session.jsonl` is authoritative. A cache failure cannot undo its
        // committed prefix, so omit cache update rather than claiming a write
        // failed after its durable model already succeeded.
        let _ = write_head_cache(&self.directory, candidate.last_sequence(), self.durability);
        self.snapshot = candidate;
        Ok(())
    }
}

impl SessionReader for JsonlSession {
    fn snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        Self::snapshot(self)
    }
}

impl SessionWriter for JsonlSession {
    fn append_entry(
        &mut self,
        lane_id: &LaneId,
        entry: ProvisionedEntry,
    ) -> Result<StoredEntry, SessionError> {
        self.writable()?;
        if self
            .snapshot
            .entries()
            .iter()
            .any(|stored| stored.header.id == entry.id)
        {
            return Err(SessionError::InvalidInput {
                message: format!("entry ID {} already materialized", entry.id),
            });
        }
        let reduction = crate::reduce_lane(self.snapshot.clone(), lane_id.clone())?;
        let stored = StoredEntry {
            lane_id: lane_id.clone(),
            header: EntryHeader {
                id: entry.id,
                parent_id: reduction.lane_state.leaf_id,
                seq: self.snapshot.next_sequence(),
                timestamp_ms: commit_time_ms(),
            },
            body: entry.body,
        };
        self.write_mutation(StoredMutation::Entry(stored.clone()))?;
        Ok(stored)
    }

    fn append_record(&mut self, record: LaneRecord) -> Result<StoredRecord, SessionError> {
        self.writable()?;
        let stored = StoredRecord {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: commit_time_ms(),
            record,
        };
        self.write_mutation(StoredMutation::Record(stored.clone()))?;
        Ok(stored)
    }

    fn append_lane_mutation(
        &mut self,
        mutation: LaneMutation,
    ) -> Result<StoredLaneMutation, SessionError> {
        self.writable()?;
        let stored = StoredLaneMutation {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: commit_time_ms(),
            mutation,
        };
        self.write_mutation(StoredMutation::Lane(stored.clone()))?;
        Ok(stored)
    }

    fn append_fact(&mut self, fact: SessionFact) -> Result<StoredFact, SessionError> {
        self.writable()?;
        let stored = StoredFact {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: commit_time_ms(),
            fact,
        };
        self.write_mutation(StoredMutation::Fact(stored.clone()))?;
        Ok(stored)
    }
}

fn create_layout(directory: &Path) -> Result<(), SessionError> {
    for relative in [
        "objects",
        "objects/blake3",
        "harness",
        "harness/trees",
        "harness/snapshots",
        "harness/revisions",
        "harness/candidates",
        "worktrees",
        "worktrees/main",
        "worktrees/main/plugins",
        "traces",
        "evals",
    ] {
        create_private_directory(&directory.join(relative))?;
    }
    Ok(())
}

fn ensure_export_parent(path: &Path) -> Result<(), SessionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::InvalidInput {
            message: format!(
                "session export parent {} must be a real non-symlink directory",
                path.display()
            ),
        });
    }
    Ok(())
}

fn copy_session_prefix(
    source: &Path,
    destination: &Path,
    durability: DurabilityMode,
) -> Result<(), SessionError> {
    let source_metadata = fs::symlink_metadata(source).map_err(|error| io(source, error))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(SessionError::InvalidInput {
            message: format!(
                "session export source {} must be a regular non-symlink file",
                source.display()
            ),
        });
    }
    let bytes = fs::read(source).map_err(|error| io(source, error))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut output = options
        .open(destination)
        .map_err(|error| io(destination, error))?;
    output
        .write_all(&bytes)
        .map_err(|error| io(destination, error))?;
    output.flush().map_err(|error| io(destination, error))?;
    if durability == DurabilityMode::Strict {
        output.sync_data().map_err(|error| io(destination, error))?;
    }
    Ok(())
}

fn ensure_layout(directory: &Path) -> Result<(), SessionError> {
    for relative in [
        "objects",
        "objects/blake3",
        "harness",
        "harness/trees",
        "harness/snapshots",
        "harness/revisions",
        "harness/candidates",
        "worktrees",
        "worktrees/main",
        "worktrees/main/plugins",
        "traces",
        "evals",
    ] {
        ensure_real_directory(&directory.join(relative))?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), SessionError> {
    fs::create_dir(path).map_err(|error| io(path, error))?;
    set_private_directory(path)
}

fn ensure_real_directory(path: &Path) -> Result<(), SessionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::InvalidInput {
            message: format!("{} must be a real non-symlink directory", path.display()),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(SessionError::InvalidInput {
                message: format!("{} has non-private session permissions", path.display()),
            });
        }
    }
    Ok(())
}

fn ensure_regular_file(path: &Path) -> Result<(), SessionError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SessionError::InvalidInput {
            message: format!("{} must be a regular non-symlink file", path.display()),
        });
    }
    Ok(())
}

fn set_private_directory(path: &Path) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io(path, error))?;
    }
    Ok(())
}

fn acquire_writer_lock(file: &File, path: &Path) -> Result<(), SessionError> {
    flock(file, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            SessionError::WriterBusy {
                path: path.display().to_string(),
            }
        } else {
            io(path, error.into())
        }
    })
}

fn write_complete_line(
    file: &mut File,
    json: &str,
    durability: DurabilityMode,
    path: &Path,
) -> Result<(), SessionError> {
    file.write_all(json.as_bytes())
        .map_err(|error| io(path, error))?;
    file.write_all(b"\n").map_err(|error| io(path, error))?;
    file.flush().map_err(|error| io(path, error))?;
    if durability == DurabilityMode::Strict {
        file.sync_data().map_err(|error| io(path, error))?;
    }
    Ok(())
}

fn write_head_cache(
    directory: &Path,
    sequence: Sequence,
    durability: DurabilityMode,
) -> Result<(), SessionError> {
    let destination = directory.join("HEAD");
    let temporary = directory.join(".HEAD.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| io(&temporary, error))?;
    file.write_all(sequence.0.to_string().as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .map_err(|error| io(&temporary, error))?;
    if durability == DurabilityMode::Strict {
        file.sync_data().map_err(|error| io(&temporary, error))?;
    }
    drop(file);
    fs::rename(&temporary, &destination).map_err(|error| io(&destination, error))?;
    if durability == DurabilityMode::Strict {
        sync_directory(directory)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SessionError> {
    let directory = File::open(path).map_err(|error| io(path, error))?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io(path, error)),
    }
}

fn recover_complete_source(
    file: &mut File,
    path: &Path,
    durability: DurabilityMode,
) -> Result<String, SessionError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io(path, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| io(path, error))?;
    let original_len = bytes.len();
    let committed_len = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(position) => position + 1,
        None => 0,
    };
    if committed_len != original_len {
        bytes.truncate(committed_len);
        file.set_len(committed_len as u64)
            .map_err(|error| io(path, error))?;
        if durability == DurabilityMode::Strict {
            file.sync_data().map_err(|error| io(path, error))?;
        }
    }
    String::from_utf8(bytes).map_err(|error| SessionError::Format {
        path: path.display().to_string(),
        line: 0,
        message: format!("session JSONL is not UTF-8: {error}"),
    })
}

fn truncate_malformed_final_json_line(
    file: &mut File,
    path: &Path,
    source: String,
    durability: DurabilityMode,
) -> Result<String, SessionError> {
    if !source.ends_with('\n') {
        return Ok(source);
    }
    let without_final_newline = &source[..source.len() - 1];
    let Some(previous_newline) = without_final_newline.rfind('\n') else {
        // The lone line is the required header. It is not a recoverable tail.
        return Ok(source);
    };
    let final_line = &without_final_newline[previous_newline + 1..];
    let final_line_is_valid_mutation = JsonValue::parse(final_line)
        .ok()
        .and_then(|value| decode_mutation(&value).ok())
        .is_some();
    if final_line_is_valid_mutation {
        return Ok(source);
    }
    let retained_len = previous_newline + 1;
    file.set_len(retained_len as u64)
        .map_err(|error| io(path, error))?;
    if durability == DurabilityMode::Strict {
        file.sync_data().map_err(|error| io(path, error))?;
    }
    Ok(source[..retained_len].to_owned())
}

fn decode_snapshot(path: &Path, source: &str) -> Result<SessionSnapshot, SessionError> {
    let mut lines = source.lines();
    let Some(header_line) = lines.next() else {
        return Err(SessionError::Format {
            path: path.display().to_string(),
            line: 1,
            message: "session file has no header".into(),
        });
    };
    let header_json = parse_line(path, 1, header_line)?;
    let header = decode_header(&header_json).map_err(|message| format_error(path, 1, message))?;
    let mut snapshot = SessionSnapshot::empty(header);
    for (offset, line) in lines.enumerate() {
        let line_number = offset + 2;
        let value = parse_line(path, line_number, line)?;
        let mutation =
            decode_mutation(&value).map_err(|message| format_error(path, line_number, message))?;
        match mutation {
            StoredMutation::Entry(value) => snapshot.push_entry(value),
            StoredMutation::Record(value) => snapshot.push_record(value),
            StoredMutation::Lane(value) => snapshot.push_lane_mutation(value),
            StoredMutation::Fact(value) => snapshot.push_fact(value),
        }
    }
    Ok(snapshot)
}

fn parse_line(path: &Path, line: usize, source: &str) -> Result<JsonValue, SessionError> {
    JsonValue::parse(source).map_err(|error| SessionError::Format {
        path: path.display().to_string(),
        line,
        message: error.to_string(),
    })
}

fn format_error(path: &Path, line: usize, message: String) -> SessionError {
    SessionError::Format {
        path: path.display().to_string(),
        line,
        message,
    }
}

fn io(path: &Path, error: std::io::Error) -> SessionError {
    SessionError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn encode_header(header: &SessionHeader) -> JsonValue {
    JsonValue::object([
        ("kind", JsonValue::String("session".into())),
        ("version", JsonValue::from(u64::from(header.version))),
        ("session_id", string_value(&header.session_id)),
        ("created_at_ms", JsonValue::from(header.created_at_ms)),
        ("workspace", JsonValue::String(header.workspace.clone())),
        ("metadata", JsonValue::Object(header.metadata.clone())),
        ("initial_lane", string_value(&header.initial_lane)),
    ])
}

fn decode_header(value: &JsonValue) -> Result<SessionHeader, String> {
    let object = object(value)?;
    if required_string(object, "kind")? != "session" {
        return Err("header kind must be `session`".into());
    }
    let version = required_u64(object, "version")?;
    if version != u64::from(SESSION_FORMAT_VERSION) {
        return Err(format!("unsupported session version {version}"));
    }
    Ok(SessionHeader {
        kind: "session".into(),
        version: SESSION_FORMAT_VERSION,
        session_id: parse_id!(SessionId, required_string(object, "session_id")?),
        created_at_ms: required_u64(object, "created_at_ms")?,
        workspace: required_string(object, "workspace")?,
        metadata: required_metadata(object, "metadata")?,
        initial_lane: parse_id!(LaneId, required_string(object, "initial_lane")?),
    })
}

fn encode_mutation(mutation: &StoredMutation) -> JsonValue {
    match mutation {
        StoredMutation::Entry(entry) => JsonValue::object([
            ("kind", JsonValue::String("entry".into())),
            ("seq", JsonValue::from(entry.header.seq.0)),
            ("timestamp_ms", JsonValue::from(entry.header.timestamp_ms)),
            (
                "payload",
                JsonValue::object([
                    ("lane_id", string_value(&entry.lane_id)),
                    ("id", string_value(&entry.header.id)),
                    ("parent_id", optional_id(entry.header.parent_id.as_ref())),
                    ("entry", encode_entry(&entry.body)),
                ]),
            ),
        ]),
        StoredMutation::Record(record) => JsonValue::object([
            ("kind", JsonValue::String("record".into())),
            ("seq", JsonValue::from(record.seq.0)),
            ("timestamp_ms", JsonValue::from(record.timestamp_ms)),
            ("payload", encode_record(&record.record)),
        ]),
        StoredMutation::Lane(lane) => JsonValue::object([
            ("kind", JsonValue::String("lane".into())),
            ("seq", JsonValue::from(lane.seq.0)),
            ("timestamp_ms", JsonValue::from(lane.timestamp_ms)),
            ("payload", encode_lane_mutation(&lane.mutation)),
        ]),
        StoredMutation::Fact(fact) => JsonValue::object([
            ("kind", JsonValue::String("fact".into())),
            ("seq", JsonValue::from(fact.seq.0)),
            ("timestamp_ms", JsonValue::from(fact.timestamp_ms)),
            ("payload", encode_fact(&fact.fact)),
        ]),
    }
}

fn decode_mutation(value: &JsonValue) -> Result<StoredMutation, String> {
    let fields = object(value)?;
    let kind = required_string(fields, "kind")?;
    let seq = Sequence(required_u64(fields, "seq")?);
    let timestamp_ms = required_u64(fields, "timestamp_ms")?;
    let payload = required_value(fields, "payload")?;
    match kind.as_str() {
        "entry" => {
            let payload = object(payload)?;
            let lane_id = parse_id!(LaneId, required_string(payload, "lane_id")?);
            let id = parse_id!(EntryId, required_string(payload, "id")?);
            let parent_id = optional_id_of::<EntryId>(payload, "parent_id")?;
            let body = decode_entry(required_value(payload, "entry")?)?;
            Ok(StoredMutation::Entry(StoredEntry {
                lane_id,
                header: EntryHeader {
                    id,
                    parent_id,
                    seq,
                    timestamp_ms,
                },
                body,
            }))
        }
        "record" => Ok(StoredMutation::Record(StoredRecord {
            seq,
            timestamp_ms,
            record: decode_record(payload)?,
        })),
        "lane" => Ok(StoredMutation::Lane(StoredLaneMutation {
            seq,
            timestamp_ms,
            mutation: decode_lane_mutation(payload)?,
        })),
        "fact" => Ok(StoredMutation::Fact(StoredFact {
            seq,
            timestamp_ms,
            fact: decode_fact(payload)?,
        })),
        _ => Err(format!("unknown session line kind {kind:?}")),
    }
}

fn encode_entry(entry: &SessionEntry) -> JsonValue {
    match entry {
        SessionEntry::UserMessage(entry) => JsonValue::object([
            ("type", JsonValue::String("user_message".into())),
            ("content", JsonValue::String(entry.content.clone())),
            ("metadata", JsonValue::Object(entry.metadata.clone())),
        ]),
        SessionEntry::AssistantMessage(entry) => JsonValue::object([
            ("type", JsonValue::String("assistant_message".into())),
            ("content", JsonValue::String(entry.content.clone())),
            (
                "tool_calls",
                JsonValue::Array(
                    entry
                        .tool_calls
                        .iter()
                        .map(encode_assistant_tool_call)
                        .collect(),
                ),
            ),
            ("stop_reason", optional_string(entry.stop_reason.as_deref())),
            (
                "error_message",
                optional_string(entry.error_message.as_deref()),
            ),
            ("metadata", JsonValue::Object(entry.metadata.clone())),
        ]),
        SessionEntry::ToolResult(entry) => JsonValue::object([
            ("type", JsonValue::String("tool_result".into())),
            (
                "tool_call_id",
                JsonValue::String(entry.tool_call_id.clone()),
            ),
            ("tool_name", JsonValue::String(entry.tool_name.clone())),
            ("full_result", encode_payload_ref(&entry.full_result)),
            ("model_projection", entry.model_projection.clone()),
            ("is_error", JsonValue::Bool(entry.is_error)),
            ("terminate", JsonValue::Bool(entry.terminate)),
            ("usage", encode_usage(&entry.usage)),
            (
                "projection_strategy_id",
                JsonValue::String(entry.projection_strategy_id.clone()),
            ),
            (
                "artifact_policy_id",
                string_value(&entry.artifact_policy_id),
            ),
        ]),
        SessionEntry::Compaction(entry) => JsonValue::object([
            ("type", JsonValue::String("compaction".into())),
            ("covered_from", optional_id(entry.covered_from.as_ref())),
            ("covered_to", optional_id(entry.covered_to.as_ref())),
            (
                "retained_tail_boundary",
                optional_id(entry.retained_tail_boundary.as_ref()),
            ),
            ("summary", JsonValue::String(entry.summary.clone())),
            ("strategy_id", JsonValue::String(entry.strategy_id.clone())),
            (
                "recovery_index_artifact",
                optional_artifact(entry.recovery_index_artifact),
            ),
            (
                "harness_revision_id",
                optional_id(entry.harness_revision_id.as_ref()),
            ),
        ]),
        SessionEntry::BranchSummary(entry) => JsonValue::object([
            ("type", JsonValue::String("branch_summary".into())),
            ("summary", JsonValue::String(entry.summary.clone())),
            ("covered_to", optional_id(entry.covered_to.as_ref())),
        ]),
        SessionEntry::ModelChanged(entry) => JsonValue::object([
            ("type", JsonValue::String("model_changed".into())),
            ("provider", JsonValue::String(entry.provider.clone())),
            ("model", JsonValue::String(entry.model.clone())),
            ("revision", optional_string(entry.revision.as_deref())),
        ]),
        SessionEntry::ThinkingChanged(entry) => JsonValue::object([
            ("type", JsonValue::String("thinking_changed".into())),
            ("level", JsonValue::String(entry.level.clone())),
        ]),
        SessionEntry::ToolActivationChanged(entry) => JsonValue::object([
            ("type", JsonValue::String("tool_activation_changed".into())),
            (
                "active_tool_names",
                JsonValue::Array(
                    entry
                        .active_tool_names
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
        ]),
        SessionEntry::HarnessRevisionChanged(entry) => JsonValue::object([
            ("type", JsonValue::String("harness_revision_changed".into())),
            ("revision_id", string_value(&entry.revision_id)),
            ("snapshot_id", string_value(&entry.snapshot_id)),
            ("rollback_from", optional_id(entry.rollback_from.as_ref())),
        ]),
        SessionEntry::PluginMemory(entry) => JsonValue::object([
            ("type", JsonValue::String("plugin_memory".into())),
            ("plugin_id", JsonValue::String(entry.plugin_id.clone())),
            ("kind", JsonValue::String(entry.kind.clone())),
            ("content", encode_payload_ref(&entry.content)),
            (
                "provenance",
                JsonValue::Array(
                    entry
                        .provenance
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            (
                "visibility",
                JsonValue::String(memory_visibility_name(entry.visibility).into()),
            ),
            (
                "retention",
                JsonValue::String(memory_retention_name(entry.retention).into()),
            ),
        ]),
        SessionEntry::Custom(entry) => JsonValue::object([
            ("type", JsonValue::String("custom".into())),
            ("type_name", JsonValue::String(entry.type_name.clone())),
            ("payload", encode_payload_ref(&entry.payload)),
            ("model_visible", JsonValue::Bool(entry.model_visible)),
        ]),
    }
}

fn decode_entry(value: &JsonValue) -> Result<SessionEntry, String> {
    let object = object(value)?;
    match required_string(object, "type")?.as_str() {
        "user_message" => Ok(SessionEntry::UserMessage(UserMessageEntry {
            content: required_string(object, "content")?,
            metadata: required_metadata(object, "metadata")?,
        })),
        "assistant_message" => Ok(SessionEntry::AssistantMessage(AssistantMessageEntry {
            content: required_string(object, "content")?,
            tool_calls: required_array(object, "tool_calls")?
                .iter()
                .map(decode_assistant_tool_call)
                .collect::<Result<Vec<_>, _>>()?,
            stop_reason: optional_string_of(object, "stop_reason")?,
            error_message: optional_string_of(object, "error_message")?,
            metadata: required_metadata(object, "metadata")?,
        })),
        "tool_result" => Ok(SessionEntry::ToolResult(ToolResultEntry {
            tool_call_id: required_string(object, "tool_call_id")?,
            tool_name: required_string(object, "tool_name")?,
            full_result: decode_payload_ref(required_value(object, "full_result")?)?,
            model_projection: required_value(object, "model_projection")?.clone(),
            is_error: required_bool(object, "is_error")?,
            terminate: required_bool(object, "terminate")?,
            usage: decode_usage(required_value(object, "usage")?)?,
            projection_strategy_id: required_string(object, "projection_strategy_id")?,
            artifact_policy_id: parse_id!(
                ArtifactPolicyId,
                required_string(object, "artifact_policy_id")?
            ),
        })),
        "compaction" => Ok(SessionEntry::Compaction(CompactionEntry {
            covered_from: optional_id_of::<EntryId>(object, "covered_from")?,
            covered_to: optional_id_of::<EntryId>(object, "covered_to")?,
            retained_tail_boundary: optional_id_of::<EntryId>(object, "retained_tail_boundary")?,
            summary: required_string(object, "summary")?,
            strategy_id: required_string(object, "strategy_id")?,
            recovery_index_artifact: optional_artifact_of(object, "recovery_index_artifact")?,
            harness_revision_id: optional_id_of::<HarnessRevisionId>(
                object,
                "harness_revision_id",
            )?,
        })),
        "branch_summary" => Ok(SessionEntry::BranchSummary(BranchSummaryEntry {
            summary: required_string(object, "summary")?,
            covered_to: optional_id_of::<EntryId>(object, "covered_to")?,
        })),
        "model_changed" => Ok(SessionEntry::ModelChanged(ModelChangedEntry {
            provider: required_string(object, "provider")?,
            model: required_string(object, "model")?,
            revision: optional_string_of(object, "revision")?,
        })),
        "thinking_changed" => Ok(SessionEntry::ThinkingChanged(ThinkingChangedEntry {
            level: required_string(object, "level")?,
        })),
        "tool_activation_changed" => Ok(SessionEntry::ToolActivationChanged(
            ToolActivationChangedEntry {
                active_tool_names: string_array(required_array(object, "active_tool_names")?)?,
            },
        )),
        "harness_revision_changed" => Ok(SessionEntry::HarnessRevisionChanged(
            HarnessRevisionChangedEntry {
                revision_id: parse_id!(HarnessRevisionId, required_string(object, "revision_id")?),
                snapshot_id: parse_id!(HarnessSnapshotId, required_string(object, "snapshot_id")?),
                rollback_from: optional_id_of::<HarnessRevisionId>(object, "rollback_from")?,
            },
        )),
        "plugin_memory" => Ok(SessionEntry::PluginMemory(PluginMemoryEntry {
            plugin_id: required_string(object, "plugin_id")?,
            kind: required_string(object, "kind")?,
            content: decode_payload_ref(required_value(object, "content")?)?,
            provenance: string_array(required_array(object, "provenance")?)?,
            visibility: parse_memory_visibility(&required_string(object, "visibility")?)?,
            retention: parse_memory_retention(&required_string(object, "retention")?)?,
        })),
        "custom" => Ok(SessionEntry::Custom(CustomEntry {
            type_name: required_string(object, "type_name")?,
            payload: decode_payload_ref(required_value(object, "payload")?)?,
            model_visible: required_bool(object, "model_visible")?,
        })),
        other => Err(format!("unknown semantic entry type {other:?}")),
    }
}

fn encode_record(record: &LaneRecord) -> JsonValue {
    match record {
        LaneRecord::OperationStarted(record) => JsonValue::object([
            ("type", JsonValue::String("operation_started".into())),
            ("id", string_value(&record.id)),
            ("lane_id", string_value(&record.lane_id)),
            (
                "source_leaf_id",
                optional_id(record.source_leaf_id.as_ref()),
            ),
            ("operation_kind", encode_operation_kind(&record.kind)),
            (
                "original_input",
                JsonValue::Array(
                    record
                        .original_input
                        .iter()
                        .map(encode_provisioned_entry)
                        .collect(),
                ),
            ),
            (
                "initial_harness_revision",
                string_value(&record.initial_harness_revision),
            ),
            (
                "model_harness_profile",
                string_value(&record.model_harness_profile),
            ),
            (
                "operation_resume_data",
                encode_hook_data(&record.operation_resume_data),
            ),
        ]),
        LaneRecord::OperationFinished(record) => JsonValue::object([
            ("type", JsonValue::String("operation_finished".into())),
            ("operation_id", string_value(&record.operation_id)),
            ("outcome", encode_operation_outcome(&record.outcome)),
        ]),
        LaneRecord::AbortRequested(record) => JsonValue::object([
            ("type", JsonValue::String("abort_requested".into())),
            ("operation_id", string_value(&record.operation_id)),
            ("reason", optional_string(record.reason.as_deref())),
        ]),
        LaneRecord::EpochStarted(record) => JsonValue::object([
            ("type", JsonValue::String("epoch_started".into())),
            ("id", string_value(&record.id)),
            ("operation_id", string_value(&record.operation_id)),
            (
                "epoch_index",
                JsonValue::from(u64::from(record.epoch_index)),
            ),
            (
                "source_leaf_id",
                optional_id(record.source_leaf_id.as_ref()),
            ),
            (
                "harness_revision_id",
                string_value(&record.harness_revision_id),
            ),
            (
                "harness_snapshot_id",
                string_value(&record.harness_snapshot_id),
            ),
            (
                "model_harness_profile",
                string_value(&record.model_harness_profile),
            ),
            ("core_run_id", string_value(&record.core_run_id)),
            (
                "epoch_resume_data",
                encode_hook_data(&record.epoch_resume_data),
            ),
        ]),
        LaneRecord::EpochFinished(record) => JsonValue::object([
            ("type", JsonValue::String("epoch_finished".into())),
            ("epoch_id", string_value(&record.epoch_id)),
            ("operation_id", string_value(&record.operation_id)),
            (
                "reason",
                JsonValue::String(epoch_finish_reason_name(&record.reason).into()),
            ),
        ]),
        LaneRecord::StepAttempted(record) => JsonValue::object([
            ("type", JsonValue::String("step_attempted".into())),
            ("id", string_value(&record.id)),
            ("operation_id", string_value(&record.operation_id)),
            ("epoch_id", string_value(&record.epoch_id)),
            (
                "step_kind",
                JsonValue::String(step_kind_name(record.kind).into()),
            ),
            ("attempt", JsonValue::from(u64::from(record.attempt))),
            ("result_entry_id", string_value(&record.result_entry_id)),
            ("reason", optional_string(record.reason.as_deref())),
        ]),
        LaneRecord::ProviderRequestStarted(record) => JsonValue::object([
            ("type", JsonValue::String("provider_request_started".into())),
            ("request_id", string_value(&record.request_id)),
            ("operation_id", string_value(&record.operation_id)),
            ("epoch_id", string_value(&record.epoch_id)),
            ("step_id", string_value(&record.step_id)),
            (
                "physical_attempt",
                JsonValue::from(u64::from(record.physical_attempt)),
            ),
            (
                "model_harness_profile",
                string_value(&record.model_harness_profile),
            ),
            (
                "request_surface_digest",
                digest_value(record.request_surface_digest),
            ),
            (
                "idempotency_key",
                optional_string(record.idempotency_key.as_deref()),
            ),
        ]),
        LaneRecord::ProviderRequestSettled(record) => JsonValue::object([
            ("type", JsonValue::String("provider_request_settled".into())),
            ("request_id", string_value(&record.request_id)),
            ("operation_id", string_value(&record.operation_id)),
            ("outcome", record.outcome.clone()),
            (
                "usage",
                optional_value(record.usage.as_ref().map(encode_usage)),
            ),
            (
                "response_artifact",
                optional_artifact(record.response_artifact),
            ),
            (
                "classification",
                JsonValue::String(provider_classification_name(&record.classification).into()),
            ),
        ]),
        LaneRecord::ToolStarted(record) => JsonValue::object([
            ("type", JsonValue::String("tool_started".into())),
            ("record_id", string_value(&record.record_id)),
            ("operation_id", string_value(&record.operation_id)),
            ("epoch_id", string_value(&record.epoch_id)),
            (
                "assistant_entry_id",
                string_value(&record.assistant_entry_id),
            ),
            ("tool_index", JsonValue::from(u64::from(record.tool_index))),
            (
                "tool_call_id",
                JsonValue::String(record.tool_call_id.clone()),
            ),
            ("tool_name", JsonValue::String(record.tool_name.clone())),
            ("effective_args", record.effective_args.clone()),
            ("result_entry_id", string_value(&record.result_entry_id)),
            (
                "replay_policy_at_start",
                JsonValue::String(tool_replay_policy_name(record.replay_policy_at_start).into()),
            ),
            (
                "tool_definition_digest",
                digest_value(record.tool_definition_digest),
            ),
            (
                "harness_revision_id",
                string_value(&record.harness_revision_id),
            ),
            (
                "idempotency_key",
                JsonValue::String(record.idempotency_key.clone()),
            ),
        ]),
        LaneRecord::QueueEnqueued(record) => JsonValue::object([
            ("type", JsonValue::String("queue_enqueued".into())),
            ("operation_id", string_value(&record.operation_id)),
            (
                "queue_item_id",
                JsonValue::String(record.queue_item_id.clone()),
            ),
        ]),
        LaneRecord::QueueCancelled(record) => JsonValue::object([
            ("type", JsonValue::String("queue_cancelled".into())),
            ("operation_id", string_value(&record.operation_id)),
            (
                "queue_item_id",
                JsonValue::String(record.queue_item_id.clone()),
            ),
        ]),
        LaneRecord::WriteDeferred(record) => JsonValue::object([
            ("type", JsonValue::String("write_deferred".into())),
            ("operation_id", string_value(&record.operation_id)),
            ("entry", encode_provisioned_entry(&record.entry)),
        ]),
        LaneRecord::HarnessActivationRequested(record) => JsonValue::object([
            (
                "type",
                JsonValue::String("harness_activation_requested".into()),
            ),
            ("operation_id", string_value(&record.operation_id)),
            ("candidate_id", string_value(&record.candidate_id)),
            (
                "parent_revision_id",
                string_value(&record.parent_revision_id),
            ),
            (
                "proposed_snapshot_id",
                string_value(&record.proposed_snapshot_id),
            ),
            ("revision_entry_id", string_value(&record.revision_entry_id)),
        ]),
        LaneRecord::Usage(record) => JsonValue::object([
            ("type", JsonValue::String("usage".into())),
            ("operation_id", string_value(&record.operation_id)),
            ("request_id", optional_id(record.request_id.as_ref())),
            ("usage", encode_usage(&record.usage)),
        ]),
    }
}

fn decode_record(value: &JsonValue) -> Result<LaneRecord, String> {
    let object = object(value)?;
    match required_string(object, "type")?.as_str() {
        "operation_started" => Ok(LaneRecord::OperationStarted(OperationStartedRecord {
            id: parse_id!(OperationId, required_string(object, "id")?),
            lane_id: parse_id!(LaneId, required_string(object, "lane_id")?),
            source_leaf_id: optional_id_of::<EntryId>(object, "source_leaf_id")?,
            kind: decode_operation_kind(required_value(object, "operation_kind")?)?,
            original_input: required_array(object, "original_input")?
                .iter()
                .map(decode_provisioned_entry)
                .collect::<Result<Vec<_>, _>>()?,
            initial_harness_revision: parse_id!(
                HarnessRevisionId,
                required_string(object, "initial_harness_revision")?
            ),
            model_harness_profile: parse_id!(
                ModelHarnessProfileId,
                required_string(object, "model_harness_profile")?
            ),
            operation_resume_data: decode_hook_data(required_value(
                object,
                "operation_resume_data",
            )?)?,
        })),
        "operation_finished" => Ok(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            outcome: decode_operation_outcome(required_value(object, "outcome")?)?,
        })),
        "abort_requested" => Ok(LaneRecord::AbortRequested(AbortRequestedRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            reason: optional_string_of(object, "reason")?,
        })),
        "epoch_started" => Ok(LaneRecord::EpochStarted(EpochStartedRecord {
            id: parse_id!(EpochId, required_string(object, "id")?),
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            epoch_index: u32::try_from(required_u64(object, "epoch_index")?)
                .map_err(|_| "epoch index exceeds u32".to_string())?,
            source_leaf_id: optional_id_of::<EntryId>(object, "source_leaf_id")?,
            harness_revision_id: parse_id!(
                HarnessRevisionId,
                required_string(object, "harness_revision_id")?
            ),
            harness_snapshot_id: parse_id!(
                HarnessSnapshotId,
                required_string(object, "harness_snapshot_id")?
            ),
            model_harness_profile: parse_id!(
                ModelHarnessProfileId,
                required_string(object, "model_harness_profile")?
            ),
            core_run_id: parse_id!(CoreRunId, required_string(object, "core_run_id")?),
            epoch_resume_data: decode_hook_data(required_value(object, "epoch_resume_data")?)?,
        })),
        "epoch_finished" => Ok(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: parse_id!(EpochId, required_string(object, "epoch_id")?),
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            reason: parse_epoch_finish_reason(&required_string(object, "reason")?)?,
        })),
        "step_attempted" => Ok(LaneRecord::StepAttempted(StepAttemptedRecord {
            id: parse_id!(StepId, required_string(object, "id")?),
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            epoch_id: parse_id!(EpochId, required_string(object, "epoch_id")?),
            kind: parse_step_kind(&required_string(object, "step_kind")?)?,
            attempt: u32::try_from(required_u64(object, "attempt")?)
                .map_err(|_| "step attempt exceeds u32".to_string())?,
            result_entry_id: parse_id!(EntryId, required_string(object, "result_entry_id")?),
            reason: optional_string_of(object, "reason")?,
        })),
        "provider_request_started" => Ok(LaneRecord::ProviderRequestStarted(
            ProviderRequestStartedRecord {
                request_id: parse_id!(ProviderRequestId, required_string(object, "request_id")?),
                operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
                epoch_id: parse_id!(EpochId, required_string(object, "epoch_id")?),
                step_id: parse_id!(StepId, required_string(object, "step_id")?),
                physical_attempt: u32::try_from(required_u64(object, "physical_attempt")?)
                    .map_err(|_| "physical attempt exceeds u32".to_string())?,
                model_harness_profile: parse_id!(
                    ModelHarnessProfileId,
                    required_string(object, "model_harness_profile")?
                ),
                request_surface_digest: parse_digest(required_string(
                    object,
                    "request_surface_digest",
                )?)?,
                idempotency_key: optional_string_of(object, "idempotency_key")?,
            },
        )),
        "provider_request_settled" => Ok(LaneRecord::ProviderRequestSettled(
            ProviderRequestSettledRecord {
                request_id: parse_id!(ProviderRequestId, required_string(object, "request_id")?),
                operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
                outcome: required_value(object, "outcome")?.clone(),
                usage: optional_value_of(object, "usage")?
                    .map(decode_usage)
                    .transpose()?,
                response_artifact: optional_artifact_of(object, "response_artifact")?,
                classification: parse_provider_classification(&required_string(
                    object,
                    "classification",
                )?)?,
            },
        )),
        "tool_started" => Ok(LaneRecord::ToolStarted(ToolStartedRecord {
            record_id: parse_id!(RecordId, required_string(object, "record_id")?),
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            epoch_id: parse_id!(EpochId, required_string(object, "epoch_id")?),
            assistant_entry_id: parse_id!(EntryId, required_string(object, "assistant_entry_id")?),
            tool_index: u32::try_from(required_u64(object, "tool_index")?)
                .map_err(|_| "tool index exceeds u32".to_string())?,
            tool_call_id: required_string(object, "tool_call_id")?,
            tool_name: required_string(object, "tool_name")?,
            effective_args: required_value(object, "effective_args")?.clone(),
            result_entry_id: parse_id!(EntryId, required_string(object, "result_entry_id")?),
            replay_policy_at_start: parse_tool_replay_policy(&required_string(
                object,
                "replay_policy_at_start",
            )?)?,
            tool_definition_digest: parse_digest(required_string(
                object,
                "tool_definition_digest",
            )?)?,
            harness_revision_id: parse_id!(
                HarnessRevisionId,
                required_string(object, "harness_revision_id")?
            ),
            idempotency_key: required_string(object, "idempotency_key")?,
        })),
        "queue_enqueued" => Ok(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            queue_item_id: required_string(object, "queue_item_id")?,
        })),
        "queue_cancelled" => Ok(LaneRecord::QueueCancelled(QueueCancelledRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            queue_item_id: required_string(object, "queue_item_id")?,
        })),
        "write_deferred" => Ok(LaneRecord::WriteDeferred(WriteDeferredRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            entry: decode_provisioned_entry(required_value(object, "entry")?)?,
        })),
        "harness_activation_requested" => Ok(LaneRecord::HarnessActivationRequested(
            HarnessActivationRequestedRecord {
                operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
                candidate_id: parse_id!(
                    HarnessCandidateId,
                    required_string(object, "candidate_id")?
                ),
                parent_revision_id: parse_id!(
                    HarnessRevisionId,
                    required_string(object, "parent_revision_id")?
                ),
                proposed_snapshot_id: parse_id!(
                    HarnessSnapshotId,
                    required_string(object, "proposed_snapshot_id")?
                ),
                revision_entry_id: parse_id!(
                    EntryId,
                    required_string(object, "revision_entry_id")?
                ),
            },
        )),
        "usage" => Ok(LaneRecord::Usage(UsageRecord {
            operation_id: parse_id!(OperationId, required_string(object, "operation_id")?),
            request_id: optional_id_of::<ProviderRequestId>(object, "request_id")?,
            usage: decode_usage(required_value(object, "usage")?)?,
        })),
        other => Err(format!("unknown operation record type {other:?}")),
    }
}

fn encode_lane_mutation(mutation: &LaneMutation) -> JsonValue {
    match mutation {
        LaneMutation::Created {
            lane_id,
            base_leaf_id,
        } => JsonValue::object([
            ("type", JsonValue::String("created".into())),
            ("lane_id", string_value(lane_id)),
            ("base_leaf_id", optional_id(base_leaf_id.as_ref())),
        ]),
    }
}

fn decode_lane_mutation(value: &JsonValue) -> Result<LaneMutation, String> {
    let object = object(value)?;
    match required_string(object, "type")?.as_str() {
        "created" => Ok(LaneMutation::Created {
            lane_id: parse_id!(LaneId, required_string(object, "lane_id")?),
            base_leaf_id: optional_id_of::<EntryId>(object, "base_leaf_id")?,
        }),
        other => Err(format!("unknown lane mutation type {other:?}")),
    }
}

fn encode_fact(fact: &SessionFact) -> JsonValue {
    match fact {
        SessionFact::HarnessCatalog(fact) => JsonValue::object([
            ("type", JsonValue::String("harness_catalog".into())),
            (
                "schema_version",
                JsonValue::from(u64::from(fact.schema_version)),
            ),
            ("artifact_id", artifact_value(fact.artifact_id)),
            ("byte_len", JsonValue::from(fact.byte_len)),
        ]),
        SessionFact::ToolSchemaDeviation(fact) => JsonValue::object([
            ("type", JsonValue::String("tool_schema_deviation".into())),
            ("operation_id", string_value(&fact.operation_id)),
            ("epoch_id", string_value(&fact.epoch_id)),
            ("assistant_entry_id", string_value(&fact.assistant_entry_id)),
            ("tool_call_id", JsonValue::String(fact.tool_call_id.clone())),
            ("tool_name", JsonValue::String(fact.tool_name.clone())),
            (
                "model_harness_profile",
                string_value(&fact.model_harness_profile),
            ),
            (
                "arguments_valid_json",
                JsonValue::Bool(fact.arguments_valid_json),
            ),
            (
                "unknown_fields",
                JsonValue::Array(
                    fact.unknown_fields
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            (
                "missing_fields",
                JsonValue::Array(
                    fact.missing_fields
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            (
                "type_mismatches",
                JsonValue::Array(
                    fact.type_mismatches
                        .iter()
                        .map(|mismatch| {
                            JsonValue::object([
                                ("field", JsonValue::String(mismatch.field.clone())),
                                ("expected", JsonValue::String(mismatch.expected.clone())),
                                ("actual", JsonValue::String(mismatch.actual.clone())),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("raw_arguments", encode_payload_ref(&fact.raw_arguments)),
        ]),
        SessionFact::TraceArtifact(fact) => JsonValue::object([
            ("type", JsonValue::String("trace_artifact".into())),
            (
                "schema_version",
                JsonValue::from(u64::from(fact.schema_version)),
            ),
            ("operation_id", string_value(&fact.operation_id)),
            ("epoch_id", string_value(&fact.epoch_id)),
            ("core_run_id", string_value(&fact.core_run_id)),
            (
                "harness_revision_id",
                string_value(&fact.harness_revision_id),
            ),
            (
                "harness_snapshot_id",
                string_value(&fact.harness_snapshot_id),
            ),
            (
                "model_harness_profile",
                string_value(&fact.model_harness_profile),
            ),
            ("artifact_id", artifact_value(fact.artifact_id)),
            ("byte_len", JsonValue::from(fact.byte_len)),
            ("media_type", JsonValue::String(fact.media_type.clone())),
        ]),
        SessionFact::Custom { type_name, payload } => JsonValue::object([
            ("type", JsonValue::String("custom".into())),
            ("type_name", JsonValue::String(type_name.clone())),
            ("payload", payload.clone()),
        ]),
    }
}

fn decode_fact(value: &JsonValue) -> Result<SessionFact, String> {
    let fields = object(value)?;
    match required_string(fields, "type")?.as_str() {
        "harness_catalog" => Ok(SessionFact::HarnessCatalog(HarnessCatalogFact {
            schema_version: u16::try_from(required_u64(fields, "schema_version")?)
                .map_err(|_| "harness catalog schema version exceeds u16".to_string())?,
            artifact_id: parse_artifact(required_string(fields, "artifact_id")?)?,
            byte_len: required_u64(fields, "byte_len")?,
        })),
        "tool_schema_deviation" => {
            let type_mismatches = required_array(fields, "type_mismatches")?
                .iter()
                .map(|value| {
                    let mismatch = object(value)?;
                    Ok(SchemaFieldMismatch {
                        field: required_string(mismatch, "field")?,
                        expected: required_string(mismatch, "expected")?,
                        actual: required_string(mismatch, "actual")?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(SessionFact::ToolSchemaDeviation(ToolSchemaDeviationFact {
                operation_id: parse_id!(OperationId, required_string(fields, "operation_id")?),
                epoch_id: parse_id!(EpochId, required_string(fields, "epoch_id")?),
                assistant_entry_id: parse_id!(
                    EntryId,
                    required_string(fields, "assistant_entry_id")?
                ),
                tool_call_id: required_string(fields, "tool_call_id")?,
                tool_name: required_string(fields, "tool_name")?,
                model_harness_profile: parse_id!(
                    ModelHarnessProfileId,
                    required_string(fields, "model_harness_profile")?
                ),
                arguments_valid_json: required_bool(fields, "arguments_valid_json")?,
                unknown_fields: string_array(required_array(fields, "unknown_fields")?)?,
                missing_fields: string_array(required_array(fields, "missing_fields")?)?,
                type_mismatches,
                raw_arguments: decode_payload_ref(required_value(fields, "raw_arguments")?)?,
            }))
        }
        "trace_artifact" => Ok(SessionFact::TraceArtifact(TraceArtifactFact {
            schema_version: u16::try_from(required_u64(fields, "schema_version")?)
                .map_err(|_| "trace artifact schema version exceeds u16".to_string())?,
            operation_id: parse_id!(OperationId, required_string(fields, "operation_id")?),
            epoch_id: parse_id!(EpochId, required_string(fields, "epoch_id")?),
            core_run_id: parse_id!(CoreRunId, required_string(fields, "core_run_id")?),
            harness_revision_id: parse_id!(
                HarnessRevisionId,
                required_string(fields, "harness_revision_id")?
            ),
            harness_snapshot_id: parse_id!(
                HarnessSnapshotId,
                required_string(fields, "harness_snapshot_id")?
            ),
            model_harness_profile: parse_id!(
                ModelHarnessProfileId,
                required_string(fields, "model_harness_profile")?
            ),
            artifact_id: parse_artifact(required_string(fields, "artifact_id")?)?,
            byte_len: required_u64(fields, "byte_len")?,
            media_type: required_string(fields, "media_type")?,
        })),
        "custom" => Ok(SessionFact::Custom {
            type_name: required_string(fields, "type_name")?,
            payload: required_value(fields, "payload")?.clone(),
        }),
        other => Err(format!("unknown session fact type {other:?}")),
    }
}

fn encode_assistant_tool_call(call: &AssistantToolCall) -> JsonValue {
    JsonValue::object([
        ("id", JsonValue::String(call.id.clone())),
        ("name", JsonValue::String(call.name.clone())),
        ("arguments", call.arguments.clone()),
    ])
}

fn decode_assistant_tool_call(value: &JsonValue) -> Result<AssistantToolCall, String> {
    let object = object(value)?;
    Ok(AssistantToolCall {
        id: required_string(object, "id")?,
        name: required_string(object, "name")?,
        arguments: required_value(object, "arguments")?.clone(),
    })
}

fn encode_payload_ref(value: &PayloadRef) -> JsonValue {
    match value {
        PayloadRef::Inline(value) => JsonValue::object([
            ("kind", JsonValue::String("inline".into())),
            ("value", value.clone()),
        ]),
        PayloadRef::Artifact {
            artifact_id,
            byte_len,
            media_type,
        } => JsonValue::object([
            ("kind", JsonValue::String("artifact".into())),
            ("artifact_id", artifact_value(*artifact_id)),
            ("byte_len", JsonValue::from(*byte_len)),
            ("media_type", JsonValue::String(media_type.clone())),
        ]),
    }
}

fn decode_payload_ref(value: &JsonValue) -> Result<PayloadRef, String> {
    let object = object(value)?;
    match required_string(object, "kind")?.as_str() {
        "inline" => Ok(PayloadRef::Inline(required_value(object, "value")?.clone())),
        "artifact" => Ok(PayloadRef::Artifact {
            artifact_id: parse_artifact(required_string(object, "artifact_id")?)?,
            byte_len: required_u64(object, "byte_len")?,
            media_type: required_string(object, "media_type")?,
        }),
        other => Err(format!("unknown payload reference kind {other:?}")),
    }
}

fn encode_usage(usage: &Usage) -> JsonValue {
    JsonValue::object([
        ("input_tokens", optional_u64(usage.input_tokens)),
        ("output_tokens", optional_u64(usage.output_tokens)),
        ("reasoning_tokens", optional_u64(usage.reasoning_tokens)),
        ("cache_read_tokens", optional_u64(usage.cache_read_tokens)),
        ("cache_write_tokens", optional_u64(usage.cache_write_tokens)),
        ("cost", optional_string(usage.cost.as_deref())),
    ])
}

fn decode_usage(value: &JsonValue) -> Result<Usage, String> {
    let object = object(value)?;
    Ok(Usage {
        input_tokens: optional_u64_of(object, "input_tokens")?,
        output_tokens: optional_u64_of(object, "output_tokens")?,
        reasoning_tokens: optional_u64_of(object, "reasoning_tokens")?,
        cache_read_tokens: optional_u64_of(object, "cache_read_tokens")?,
        cache_write_tokens: optional_u64_of(object, "cache_write_tokens")?,
        cost: optional_string_of(object, "cost")?,
    })
}

fn encode_provisioned_entry(entry: &ProvisionedEntry) -> JsonValue {
    JsonValue::object([
        ("id", string_value(&entry.id)),
        ("body", encode_entry(&entry.body)),
    ])
}

fn decode_provisioned_entry(value: &JsonValue) -> Result<ProvisionedEntry, String> {
    let object = object(value)?;
    Ok(ProvisionedEntry {
        id: parse_id!(EntryId, required_string(object, "id")?),
        body: decode_entry(required_value(object, "body")?)?,
    })
}

fn encode_hook_data(data: &BTreeMap<StableHookId, JsonValue>) -> JsonValue {
    JsonValue::Object(
        data.iter()
            .map(|(key, value)| (key.as_str().into(), value.clone()))
            .collect(),
    )
}

fn decode_hook_data(value: &JsonValue) -> Result<BTreeMap<StableHookId, JsonValue>, String> {
    object(value)?
        .iter()
        .map(|(key, value)| Ok((parse_id!(StableHookId, key.clone()), value.clone())))
        .collect()
}

fn encode_operation_kind(kind: &OperationKind) -> JsonValue {
    match kind {
        OperationKind::Run => JsonValue::object([("kind", JsonValue::String("run".into()))]),
        OperationKind::Other(name) => JsonValue::object([
            ("kind", JsonValue::String("other".into())),
            ("name", JsonValue::String(name.clone())),
        ]),
    }
}

fn decode_operation_kind(value: &JsonValue) -> Result<OperationKind, String> {
    let object = object(value)?;
    match required_string(object, "kind")?.as_str() {
        "run" => Ok(OperationKind::Run),
        "other" => Ok(OperationKind::Other(required_string(object, "name")?)),
        other => Err(format!("unknown operation kind {other:?}")),
    }
}

fn encode_operation_outcome(outcome: &OperationOutcome) -> JsonValue {
    match outcome {
        OperationOutcome::Completed => {
            JsonValue::object([("kind", JsonValue::String("completed".into()))])
        }
        OperationOutcome::Aborted => {
            JsonValue::object([("kind", JsonValue::String("aborted".into()))])
        }
        OperationOutcome::Failed { code } => JsonValue::object([
            ("kind", JsonValue::String("failed".into())),
            ("code", JsonValue::String(code.clone())),
        ]),
    }
}

fn decode_operation_outcome(value: &JsonValue) -> Result<OperationOutcome, String> {
    let object = object(value)?;
    match required_string(object, "kind")?.as_str() {
        "completed" => Ok(OperationOutcome::Completed),
        "aborted" => Ok(OperationOutcome::Aborted),
        "failed" => Ok(OperationOutcome::Failed {
            code: required_string(object, "code")?,
        }),
        other => Err(format!("unknown operation outcome {other:?}")),
    }
}

fn memory_visibility_name(value: MemoryVisibility) -> &'static str {
    match value {
        MemoryVisibility::ModelVisible => "model_visible",
        MemoryVisibility::ExternalOnly => "external_only",
    }
}

fn parse_memory_visibility(value: &str) -> Result<MemoryVisibility, String> {
    match value {
        "model_visible" => Ok(MemoryVisibility::ModelVisible),
        "external_only" => Ok(MemoryVisibility::ExternalOnly),
        _ => Err(format!("unknown memory visibility {value:?}")),
    }
}

fn memory_retention_name(value: MemoryRetention) -> &'static str {
    match value {
        MemoryRetention::Session => "session",
        MemoryRetention::Checkpoint => "checkpoint",
    }
}

fn parse_memory_retention(value: &str) -> Result<MemoryRetention, String> {
    match value {
        "session" => Ok(MemoryRetention::Session),
        "checkpoint" => Ok(MemoryRetention::Checkpoint),
        _ => Err(format!("unknown memory retention {value:?}")),
    }
}

fn step_kind_name(value: StepKind) -> &'static str {
    match value {
        StepKind::Assistant => "assistant",
        StepKind::Compaction => "compaction",
    }
}

fn parse_step_kind(value: &str) -> Result<StepKind, String> {
    match value {
        "assistant" => Ok(StepKind::Assistant),
        "compaction" => Ok(StepKind::Compaction),
        _ => Err(format!("unknown step kind {value:?}")),
    }
}

fn epoch_finish_reason_name(value: &EpochFinishReason) -> &'static str {
    match value {
        EpochFinishReason::Settled => "settled",
        EpochFinishReason::ActivationPending => "activation_pending",
        EpochFinishReason::Interrupted => "interrupted",
    }
}

fn parse_epoch_finish_reason(value: &str) -> Result<EpochFinishReason, String> {
    match value {
        "settled" => Ok(EpochFinishReason::Settled),
        "activation_pending" => Ok(EpochFinishReason::ActivationPending),
        "interrupted" => Ok(EpochFinishReason::Interrupted),
        _ => Err(format!("unknown epoch finish reason {value:?}")),
    }
}

fn tool_replay_policy_name(value: ToolReplayPolicy) -> &'static str {
    match value {
        ToolReplayPolicy::Never => "never",
        ToolReplayPolicy::Safe => "safe",
    }
}

fn parse_tool_replay_policy(value: &str) -> Result<ToolReplayPolicy, String> {
    match value {
        "never" => Ok(ToolReplayPolicy::Never),
        "safe" => Ok(ToolReplayPolicy::Safe),
        _ => Err(format!("unknown tool replay policy {value:?}")),
    }
}

fn provider_classification_name(value: &ProviderSettlementClassification) -> &'static str {
    match value {
        ProviderSettlementClassification::Completed => "completed",
        ProviderSettlementClassification::Retryable => "retryable",
        ProviderSettlementClassification::Discarded => "discarded",
        ProviderSettlementClassification::Interrupted => "interrupted",
    }
}

fn parse_provider_classification(value: &str) -> Result<ProviderSettlementClassification, String> {
    match value {
        "completed" => Ok(ProviderSettlementClassification::Completed),
        "retryable" => Ok(ProviderSettlementClassification::Retryable),
        "discarded" => Ok(ProviderSettlementClassification::Discarded),
        "interrupted" => Ok(ProviderSettlementClassification::Interrupted),
        _ => Err(format!(
            "unknown provider settlement classification {value:?}"
        )),
    }
}

trait ParseOpaqueId: Sized {
    fn parse_opaque(value: String) -> Result<Self, String>;
}

macro_rules! impl_parse_opaque_id {
    ($($name:ty),+ $(,)?) => {
        $(
            impl ParseOpaqueId for $name {
                fn parse_opaque(value: String) -> Result<Self, String> {
                    Self::new(value).map_err(|error| error.to_string())
                }
            }
        )+
    };
}

impl_parse_opaque_id!(
    SessionId,
    LaneId,
    EntryId,
    RecordId,
    OperationId,
    EpochId,
    StepId,
    ProviderRequestId,
    HarnessTreeId,
    HarnessSnapshotId,
    HarnessRevisionId,
    HarnessCandidateId,
    ModelHarnessProfileId,
    ExperimentId,
    FailureSignatureId,
    StableHookId,
    ArtifactPolicyId,
    CoreRunId,
);

fn object(value: &JsonValue) -> Result<&BTreeMap<String, JsonValue>, String> {
    value
        .as_object()
        .ok_or_else(|| "value must be a JSON object".into())
}

fn required_value<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a JsonValue, String> {
    object
        .get(field)
        .ok_or_else(|| format!("missing required field {field:?}"))
}

fn required_string(object: &BTreeMap<String, JsonValue>, field: &str) -> Result<String, String> {
    required_value(object, field)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("field {field:?} must be a string"))
}

fn required_u64(object: &BTreeMap<String, JsonValue>, field: &str) -> Result<u64, String> {
    required_value(object, field)?
        .as_u64()
        .ok_or_else(|| format!("field {field:?} must be an unsigned integer"))
}

fn required_bool(object: &BTreeMap<String, JsonValue>, field: &str) -> Result<bool, String> {
    required_value(object, field)?
        .as_bool()
        .ok_or_else(|| format!("field {field:?} must be a boolean"))
}

fn required_array<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<&'a [JsonValue], String> {
    required_value(object, field)?
        .as_array()
        .ok_or_else(|| format!("field {field:?} must be an array"))
}

fn required_metadata(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Metadata, String> {
    required_value(object, field)?
        .as_object()
        .cloned()
        .ok_or_else(|| format!("field {field:?} must be an object"))
}

fn optional_value_of<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<&'a JsonValue>, String> {
    match object.get(field) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(value) => Ok(Some(value)),
    }
}

fn optional_string_of(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<String>, String> {
    optional_value_of(object, field)?
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("field {field:?} must be a string or null"))
        })
        .transpose()
}

fn optional_u64_of(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<u64>, String> {
    optional_value_of(object, field)?
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| format!("field {field:?} must be an unsigned integer or null"))
        })
        .transpose()
}

fn optional_id_of<T: ParseOpaqueId>(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<T>, String> {
    optional_string_of(object, field)?
        .map(T::parse_opaque)
        .transpose()
}

fn optional_artifact_of(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<Option<ArtifactId>, String> {
    optional_string_of(object, field)?
        .map(parse_artifact)
        .transpose()
}

fn string_array(values: &[JsonValue]) -> Result<Vec<String>, String> {
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "array item must be a string".into())
        })
        .collect()
}

fn string_value(value: &impl ToString) -> JsonValue {
    JsonValue::String(value.to_string())
}

fn digest_value(value: Digest) -> JsonValue {
    JsonValue::String(value.to_hex())
}

fn artifact_value(value: ArtifactId) -> JsonValue {
    JsonValue::String(value.to_hex())
}

fn optional_value(value: Option<JsonValue>) -> JsonValue {
    value.unwrap_or(JsonValue::Null)
}

fn optional_string(value: Option<&str>) -> JsonValue {
    value
        .map(|value| JsonValue::String(value.into()))
        .unwrap_or(JsonValue::Null)
}

fn optional_u64(value: Option<u64>) -> JsonValue {
    value.map(JsonValue::from).unwrap_or(JsonValue::Null)
}

fn optional_id(value: Option<&impl ToString>) -> JsonValue {
    value.map(string_value).unwrap_or(JsonValue::Null)
}

fn optional_artifact(value: Option<ArtifactId>) -> JsonValue {
    value.map(artifact_value).unwrap_or(JsonValue::Null)
}

fn parse_digest(value: String) -> Result<Digest, String> {
    Digest::from_hex(&value).map_err(|error| error.to_string())
}

fn parse_artifact(value: String) -> Result<ArtifactId, String> {
    ArtifactId::from_hex(&value).map_err(|error| error.to_string())
}
