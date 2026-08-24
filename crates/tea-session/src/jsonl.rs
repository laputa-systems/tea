//! Strict append-only JSONL v1 session storage.

use crate::ids::*;
use crate::model::*;
use crate::agents::*;
use crate::store::{
    SessionAppendIndex, SessionClock, SessionError, SessionReader, SessionWriter,
    SystemSessionClock, validate_snapshot, validate_snapshot_append,
};
use crate::{
    ArtifactError, JsonValue, LaneId, SessionVerification, SessionVerificationError, verify_session,
};
#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
))]
use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::fs::{FlockOperation, flock};
#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
))]
use rustix::io::Errno;
#[cfg(test)]
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EXPORT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_HEAD_CACHE_FILE: AtomicU64 = AtomicU64::new(1);
const MAX_SESSION_LINE_BYTES: usize = 1_048_576;

/// Deterministic append-stage interruption used only by the storage matrix.
///
/// The production write path has no fault-injection branch. Tests install one
/// thread-local stage after session creation, so parallel fixtures cannot
/// interrupt each other's file handles.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TestWriteFailpoint {
    BeforeAppend,
    /// Fail after writing exactly this many JSON bytes and before the newline.
    AfterJsonBytes(usize),
    AfterJsonBeforeNewline,
    AfterNewlineBeforeFlush,
    DuringFlush,
    AfterFlushBeforeSync,
    DuringSync,
    AfterSyncBeforeReturn,
}

/// Deterministic session-creation interruption used only by the creation matrix.
///
/// Creation publishes a fully initialized private directory with a no-replace
/// rename. A failure before publication must leave no candidate session; a
/// failure after publication may leave only that valid, reopenable directory.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestCreationFailpoint {
    BeforeTemporaryDirectory,
    AfterTemporaryDirectory,
    AfterLayout,
    AfterHeaderWrite,
    AfterHeadCache,
    BeforeTemporaryDirectorySync,
    AfterTemporaryDirectorySync,
    BeforePublication,
    AfterPublication,
    BeforeParentDirectorySync,
    AfterParentDirectorySync,
}

#[cfg(test)]
thread_local! {
    static TEST_WRITE_FAILPOINT: RefCell<Option<TestWriteFailpoint>> = const { RefCell::new(None) };
    static TEST_CREATION_FAILPOINT: RefCell<Option<TestCreationFailpoint>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestWriteFailpointGuard;

#[cfg(test)]
impl Drop for TestWriteFailpointGuard {
    fn drop(&mut self) {
        TEST_WRITE_FAILPOINT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) fn install_test_write_failpoint(
    failpoint: TestWriteFailpoint,
) -> TestWriteFailpointGuard {
    TEST_WRITE_FAILPOINT.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "a test write failpoint is already installed on this thread"
        );
        *slot.borrow_mut() = Some(failpoint);
    });
    TestWriteFailpointGuard
}

#[cfg(test)]
pub(crate) struct TestCreationFailpointGuard;

#[cfg(test)]
impl Drop for TestCreationFailpointGuard {
    fn drop(&mut self) {
        TEST_CREATION_FAILPOINT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) fn install_test_creation_failpoint(
    failpoint: TestCreationFailpoint,
) -> TestCreationFailpointGuard {
    TEST_CREATION_FAILPOINT.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "a test creation failpoint is already installed on this thread"
        );
        *slot.borrow_mut() = Some(failpoint);
    });
    TestCreationFailpointGuard
}

#[cfg(test)]
fn test_write_failpoint() -> Option<TestWriteFailpoint> {
    TEST_WRITE_FAILPOINT.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
fn test_creation_failpoint() -> Option<TestCreationFailpoint> {
    TEST_CREATION_FAILPOINT.with(|slot| *slot.borrow())
}

#[cfg(test)]
fn interrupt_creation_at(
    failpoint: TestCreationFailpoint,
    path: &Path,
) -> Result<(), SessionError> {
    if test_creation_failpoint() == Some(failpoint) {
        return Err(SessionError::Io {
            path: path.display().to_string(),
            message: format!("injected session creation interruption at {failpoint:?}"),
        });
    }
    Ok(())
}

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

/// Read-only replay evidence for a v1 session directory.
///
/// `torn_tail_offset` identifies only an unterminated final write. Complete
/// malformed lines are never repairable through this API.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInspection {
    /// Fully validated committed prefix.
    pub snapshot: SessionSnapshot,
    /// Byte offset at which an uncommitted final tail begins, if present.
    pub torn_tail_offset: Option<u64>,
}

/// Result of the explicit torn-tail repair operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRepair {
    /// Truncated uncommitted tail offset, if the log had one.
    pub truncated_tail_offset: Option<u64>,
    /// Failure while rebuilding a disposable cache after the authoritative
    /// truncation committed. The repair itself remains successful.
    pub cache_warning: Option<String>,
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
    append_index: SessionAppendIndex,
    durability: DurabilityMode,
    fault: Option<String>,
    cache_warning: Option<String>,
    clock: Arc<dyn SessionClock>,
}

impl Drop for JsonlSession {
    fn drop(&mut self) {
        // `flock` belongs to the open-file description and a descriptor can
        // briefly outlive this owner across `fork` before `exec` applies
        // close-on-exec. Unlock explicitly so dropping the session is the
        // authority boundary even when such an inherited descriptor has not
        // closed yet. Close remains the operating-system fallback on error.
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

impl JsonlSession {
    /// Create a new session directory and its initial v1 header atomically.
    pub fn create(
        directory: impl AsRef<Path>,
        header: SessionHeader,
        durability: DurabilityMode,
    ) -> Result<Self, SessionError> {
        Self::create_with_clock(directory, header, durability, Arc::new(SystemSessionClock))
    }

    /// Create a new session with an explicit commit clock.
    pub fn create_with_clock(
        directory: impl AsRef<Path>,
        mut header: SessionHeader,
        durability: DurabilityMode,
        clock: Arc<dyn SessionClock>,
    ) -> Result<Self, SessionError> {
        if header.kind != "session" || header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::InvalidInput {
                message: "JSONL v1 creation requires a v1 session header".into(),
            });
        }
        seal_header(&mut header)?;
        let directory = directory.as_ref().to_path_buf();
        if directory.exists() {
            return Err(SessionError::Io {
                path: directory.display().to_string(),
                message: "refusing to create a session over an existing directory".into(),
            });
        }
        let parent = directory
            .parent()
            .ok_or_else(|| SessionError::InvalidInput {
                message: "session directory must have a parent directory".into(),
            })?;
        ensure_export_parent(parent)?;
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| SessionError::InvalidInput {
                message: "session directory must have a UTF-8 file name".into(),
            })?;
        let nonce = NEXT_SESSION_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.create-{}-{nonce:016x}",
            std::process::id()
        ));
        let encoded = encode_header(&header).to_json_string().map_err(|error| {
            SessionError::InvalidInput {
                message: format!("session header cannot encode as JSON: {error}"),
            }
        })?;
        let mut created_temporary = false;
        let result = (|| {
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::BeforeTemporaryDirectory, &temporary)?;
            fs::create_dir(&temporary).map_err(|error| io(&temporary, error))?;
            created_temporary = true;
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::AfterTemporaryDirectory, &temporary)?;
            set_private_directory(&temporary)?;
            create_layout(&temporary)?;
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::AfterLayout, &temporary)?;
            let temporary_session_path = temporary.join("session.jsonl");
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary_session_path)
                .map_err(|error| io(&temporary_session_path, error))?;
            acquire_writer_lock(&file, &temporary_session_path)?;
            write_complete_line(&mut file, &encoded, durability, &temporary_session_path)?;
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::AfterHeaderWrite, &temporary)?;
            let snapshot = SessionSnapshot::empty(header);
            let append_index = SessionAppendIndex::empty(snapshot.header());
            write_head_cache(&temporary, &snapshot, durability)?;
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::AfterHeadCache, &temporary)?;
            if durability == DurabilityMode::Strict {
                #[cfg(test)]
                interrupt_creation_at(
                    TestCreationFailpoint::BeforeTemporaryDirectorySync,
                    &temporary,
                )?;
                sync_directory(&temporary)?;
                #[cfg(test)]
                interrupt_creation_at(
                    TestCreationFailpoint::AfterTemporaryDirectorySync,
                    &temporary,
                )?;
            }
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::BeforePublication, &temporary)?;
            publish_directory_noreplace(&temporary, &directory)?;
            #[cfg(test)]
            interrupt_creation_at(TestCreationFailpoint::AfterPublication, &directory)?;
            if durability == DurabilityMode::Strict {
                #[cfg(test)]
                interrupt_creation_at(TestCreationFailpoint::BeforeParentDirectorySync, parent)?;
                sync_directory(parent)?;
                #[cfg(test)]
                interrupt_creation_at(TestCreationFailpoint::AfterParentDirectorySync, parent)?;
            }
            Ok(Self {
                directory: directory.clone(),
                session_path: directory.join("session.jsonl"),
                file,
                snapshot,
                append_index,
                durability,
                fault: None,
                cache_warning: None,
                clock,
            })
        })();
        if result.is_err() && created_temporary && temporary.exists() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    /// Open a fully committed v1 session and acquire its sole writer lock.
    ///
    /// An unterminated tail is intentionally not repaired here; call
    /// [`Self::inspect`] then [`Self::repair_torn_tail`] explicitly.
    pub fn open(
        directory: impl AsRef<Path>,
        durability: DurabilityMode,
    ) -> Result<Self, SessionError> {
        Self::open_with_clock(directory, durability, Arc::new(SystemSessionClock))
    }

    /// Open a v1 session with an explicit clock for subsequent commits.
    pub fn open_with_clock(
        directory: impl AsRef<Path>,
        durability: DurabilityMode,
        clock: Arc<dyn SessionClock>,
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
        reject_unsupported_format(&mut file, &session_path)?;
        let (snapshot, append_index, incomplete_tail_offset) =
            decode_snapshot_stream(&mut file, &session_path)?;
        if let Some(offset) = incomplete_tail_offset {
            return Err(SessionError::RecoveryRequired {
                path: session_path.display().to_string(),
                offset,
            });
        }
        validate_snapshot(&snapshot)?;
        let cache_warning = if Self::head_cache_is_current(&directory, &snapshot) {
            None
        } else {
            write_head_cache(&directory, &snapshot, durability)
                .err()
                .map(|error| error.to_string())
        };
        Ok(Self {
            directory,
            session_path,
            file,
            snapshot,
            append_index,
            durability,
            fault: None,
            cache_warning,
            clock,
        })
    }

    /// Replay a session without acquiring its writer lock or modifying any
    /// file. This is the inspection path for operators and recovery tooling.
    pub fn inspect(directory: impl AsRef<Path>) -> Result<SessionInspection, SessionError> {
        let directory = directory.as_ref().to_path_buf();
        ensure_real_directory(&directory)?;
        ensure_layout(&directory)?;
        let session_path = directory.join("session.jsonl");
        ensure_regular_file(&session_path)?;
        let mut file = File::open(&session_path).map_err(|error| io(&session_path, error))?;
        reject_unsupported_format(&mut file, &session_path)?;
        let (snapshot, _, torn_tail_offset) = decode_snapshot_stream(&mut file, &session_path)?;
        validate_snapshot(&snapshot)?;
        Ok(SessionInspection {
            snapshot,
            torn_tail_offset,
        })
    }

    /// Truncate only an explicitly identified unterminated final tail.
    ///
    /// This obtains the session's writer lock, validates the complete prefix,
    /// synchronizes the truncation in strict mode, and then refreshes the
    /// derived `HEAD` cache. A cache failure is reported separately because
    /// it cannot undo the authoritative repair.
    pub fn repair_torn_tail(
        directory: impl AsRef<Path>,
        durability: DurabilityMode,
    ) -> Result<SessionRepair, SessionError> {
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
        reject_unsupported_format(&mut file, &session_path)?;
        let (snapshot, _, torn_tail_offset) = decode_snapshot_stream(&mut file, &session_path)?;
        validate_snapshot(&snapshot)?;
        if let Some(offset) = torn_tail_offset {
            file.set_len(offset)
                .map_err(|error| io(&session_path, error))?;
            if durability == DurabilityMode::Strict {
                file.sync_data().map_err(|error| io(&session_path, error))?;
            }
        }
        let cache_warning = write_head_cache(&directory, &snapshot, durability)
            .err()
            .map(|error| error.to_string());
        Ok(SessionRepair {
            truncated_tail_offset: torn_tail_offset,
            cache_warning,
        })
    }

    /// Return the explicit session-directory root.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Return the most recent failure while refreshing a disposable cache.
    ///
    /// The JSONL log remains committed and authoritative when this is set.
    pub fn cache_warning(&self) -> Option<&str> {
        self.cache_warning.as_deref()
    }

    /// Open the immutable object store colocated with this v1 session.
    ///
    /// Callers that construct a durable supervisor over this writer should
    /// retain this store, so every `tea-artifact://` locator remains valid
    /// after the JSONL writer reopens.
    pub fn artifact_store(&self) -> Result<crate::FileArtifactStore, crate::ArtifactError> {
        crate::FileArtifactStore::open(self.directory.join("objects"))
    }

    /// Plan and apply immutable artifact collection while this session's sole
    /// writer lock is held. The authoritative snapshot supplies the primary
    /// roots; callers add retained harness, experiment, or export roots.
    pub fn collect_unreferenced_artifacts(
        &mut self,
        additional_roots: impl IntoIterator<Item = crate::ArtifactId>,
        quota: crate::ArtifactQuota,
    ) -> Result<crate::ArtifactGcReport, crate::ArtifactError> {
        if let Some(message) = &self.fault {
            return Err(crate::ArtifactError::InvalidRequest {
                message: format!(
                    "cannot collect artifacts from a faulted session writer: {message}"
                ),
            });
        }
        let store = self.artifact_store()?;
        let plan = crate::plan_artifact_gc(&store, &self.snapshot, additional_roots, quota)?;
        crate::apply_artifact_gc(&store, &plan, quota)
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
        reject_export_with_unresolved_workspace_leases(&snapshot)?;
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
            write_head_cache(&temporary, &snapshot, self.durability)?;

            let destination_store = crate::FileArtifactStore::open(temporary.join("objects"))?;
            for artifact_id in &verification.artifact_roots {
                let mut input = source_store.open_verified_reader(*artifact_id)?;
                let copied =
                    destination_store.put_reader(&mut input, "application/octet-stream")?;
                if copied.artifact_id != *artifact_id {
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
            write_export_manifest(
                &temporary,
                &snapshot,
                &verification,
                &destination_store,
                self.durability,
            )?;
            publish_directory_noreplace(&temporary, &destination)?;
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

    #[cfg(test)]
    pub(crate) fn duplicate_writer_descriptor_for_test(&self) -> File {
        self.file.try_clone().expect("test duplicates writer descriptor")
    }

    /// Return whether `HEAD` is the exact disposable cache derived from this
    /// validated snapshot. The check is read-only, so operator verification
    /// can report cache disagreement without trusting or repairing the cache.
    pub fn head_cache_is_current(directory: impl AsRef<Path>, snapshot: &SessionSnapshot) -> bool {
        head_cache_matches(directory.as_ref(), snapshot)
    }

    fn writable(&self) -> Result<(), SessionError> {
        match &self.fault {
            Some(message) => Err(SessionError::Faulted {
                message: message.clone(),
            }),
            None => Ok(()),
        }
    }

    fn write_mutation(&mut self, mutation: SessionMutation) -> Result<(), SessionError> {
        self.writable()?;
        let mutation = seal_mutation(&self.snapshot, mutation)?;
        let refresh_head = selects_main_harness_revision(&mutation);
        let locally_validated = self.append_index.is_locally_validated_mutation(&mutation)?;
        if !locally_validated {
            // Reject cross-record invalid input before it can become durable,
            // but borrow the prospective payload through the sole pure
            // reducer instead of cloning the complete retained prefix.
            validate_snapshot_append(&self.snapshot, &mutation)?;
        }
        let encoded = encode_mutation(&mutation)
            .to_json_string()
            .map_err(|error| SessionError::InvalidInput {
                message: format!("session mutation cannot encode as JSON: {error}"),
            })?;
        ensure_complete_line_size(&encoded)?;
        if let Err(error) = write_complete_line(
            &mut self.file,
            &encoded,
            self.durability,
            &self.session_path,
        ) {
            if matches!(error, SessionError::IndeterminateWrite { .. }) {
                self.fault = Some(error.to_string());
            }
            return Err(error);
        }
        // `session.jsonl` is authoritative. A cache failure cannot undo its
        // committed prefix, so defer the disposable cache until after the
        // reduced state advances and never report it as a failed commit.
        self.append_index.advance(&mutation);
        self.snapshot.push_mutation(mutation);
        if refresh_head {
            self.cache_warning = write_head_cache(&self.directory, &self.snapshot, self.durability)
                .err()
                .map(|error| error.to_string());
        }
        Ok(())
    }
}

/// Portable exports deliberately exclude operational worktrees. A child in
/// one of these graph states still relies on its lease for deterministic
/// recovery, so exporting only the JSONL/artifact prefix would manufacture a
/// session that cannot safely resume. Terminal worktree cleanup is host state
/// rather than a durable fact, so completed graph states remain exportable.
fn reject_export_with_unresolved_workspace_leases(
    snapshot: &crate::SessionSnapshot,
) -> Result<(), SessionError> {
    let graph = reduce_agent_graph(snapshot)?;
    let unresolved = graph.agents.values().find(|node| {
        matches!(
            node.state,
            AgentState::Spawned | AgentState::Running | AgentState::Finalizing { .. }
        )
    });
    if let Some(node) = unresolved {
        return Err(SessionError::InvalidInput {
            message: format!(
                "cannot export a session with unresolved workspace lease {} for agent {}",
                node.spawned.workspace_lease_id, node.spawned.agent_id
            ),
        });
    }
    Ok(())
}

/// Inspect only the complete header line before any v1 recovery work. This is
/// the clean-slate format boundary: discarded formats cannot trigger record
/// decoding or mutation of the file that contains them.
fn reject_unsupported_format(file: &mut File, path: &Path) -> Result<(), SessionError> {
    let Some(header_line) = read_first_complete_line(file, path)? else {
        return Ok(());
    };
    let Ok(header) = JsonValue::parse(&header_line) else {
        return Ok(());
    };
    let Some(fields) = header.as_object() else {
        return Ok(());
    };
    if fields.get("kind").and_then(JsonValue::as_str) != Some("session") {
        return Ok(());
    }
    let observed_version = fields.get("version").and_then(JsonValue::as_u64);
    if let Some(observed_version) = observed_version
        && observed_version != u64::from(SESSION_FORMAT_VERSION)
    {
        return Err(SessionError::UnsupportedFormat {
            path: path.display().to_string(),
            observed_version: Some(observed_version),
        });
    }
    Ok(())
}

fn read_first_complete_line(file: &mut File, path: &Path) -> Result<Option<String>, SessionError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io(path, error))?;
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match file.read(&mut byte).map_err(|error| io(path, error))? {
            0 if bytes.is_empty() => return Ok(None),
            0 => return Ok(None),
            _ if byte[0] == b'\n' => break,
            _ => {
                if bytes.len() == MAX_SESSION_LINE_BYTES {
                    return Err(SessionError::Format {
                        path: path.display().to_string(),
                        line: 1,
                        offset: 0,
                        sequence: None,
                        mutation_kind: None,
                        message: format!(
                            "session header exceeds the {MAX_SESSION_LINE_BYTES}-byte line limit"
                        ),
                    });
                }
                bytes.push(byte[0]);
            }
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| SessionError::Format {
            path: path.display().to_string(),
            line: 1,
            offset: 0,
            sequence: None,
            mutation_kind: None,
            message: format!("session header is not UTF-8: {error}"),
        })
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
        if self.append_index.contains_entry(&entry.id) {
            return Err(SessionError::InvalidInput {
                message: format!("entry ID {} already materialized", entry.id),
            });
        }
        let parent_id = self.append_index.lane_leaf(lane_id)?;
        let stored = StoredEntry {
            lane_id: lane_id.clone(),
            header: EntryHeader {
                id: entry.id,
                parent_id,
                seq: self.snapshot.next_sequence(),
                timestamp_ms: self.clock.now_ms(),
            },
            body: entry.body,
        };
        self.write_mutation(SessionMutation::Entry(stored.clone()))?;
        Ok(stored)
    }

    fn append_record(&mut self, record: LaneRecord) -> Result<StoredRecord, SessionError> {
        self.writable()?;
        let stored = StoredRecord {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: self.clock.now_ms(),
            record,
        };
        self.write_mutation(SessionMutation::Record(stored.clone()))?;
        Ok(stored)
    }

    fn append_lane_mutation(
        &mut self,
        mutation: LaneMutation,
    ) -> Result<StoredLaneMutation, SessionError> {
        self.writable()?;
        let stored = StoredLaneMutation {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: self.clock.now_ms(),
            mutation,
        };
        self.write_mutation(SessionMutation::Lane(stored.clone()))?;
        Ok(stored)
    }

    fn append_fact(&mut self, fact: SessionFact) -> Result<StoredFact, SessionError> {
        self.writable()?;
        let stored = StoredFact {
            seq: self.snapshot.next_sequence(),
            timestamp_ms: self.clock.now_ms(),
            fact,
        };
        self.write_mutation(SessionMutation::Fact(stored.clone()))?;
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

/// Publish a completed session directory without replacing any destination.
///
/// Ordinary directory rename may replace an empty destination on Unix. That
/// would turn a collision into an overwrite, so v1 uses the platform's
/// no-replace primitive and fails closed when it is unavailable.
fn publish_directory_noreplace(source: &Path, destination: &Path) -> Result<(), SessionError> {
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    ))]
    {
        match renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(()),
            Err(Errno::NOSYS | Errno::INVAL) => Err(SessionError::InvalidInput {
                message:
                    "atomic no-replace directory publication is unavailable on this filesystem"
                        .into(),
            }),
            Err(error) => Err(io(destination, error.into())),
        }
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        target_os = "redox",
    )))]
    {
        let _ = source;
        Err(SessionError::InvalidInput {
            message: "atomic no-replace directory publication is unsupported on this platform"
                .into(),
        })
    }
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
    let mut input = File::open(source).map_err(|error| io(source, error))?;
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
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(|error| io(source, error))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| io(destination, error))?;
    }
    output.flush().map_err(|error| io(destination, error))?;
    if durability == DurabilityMode::Strict {
        output.sync_data().map_err(|error| io(destination, error))?;
    }
    Ok(())
}

fn write_export_manifest(
    directory: &Path,
    snapshot: &SessionSnapshot,
    verification: &SessionVerification,
    artifacts: &dyn crate::ArtifactStore,
    durability: DurabilityMode,
) -> Result<(), SessionExportError> {
    let artifacts = verification
        .artifact_roots
        .iter()
        .map(|artifact_id| {
            artifacts.verify_object(*artifact_id).map(|byte_len| {
                JsonValue::object([
                    ("artifact_id", JsonValue::String(artifact_id.to_hex())),
                    ("byte_len", JsonValue::from(byte_len)),
                ])
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let contents = JsonValue::object([
        ("artifacts", JsonValue::Array(artifacts)),
        ("format", JsonValue::String("tea-session-export-v1".into())),
        ("session_id", string_value(&snapshot.header().session_id)),
        (
            "through_digest",
            JsonValue::String(snapshot.last_digest().to_hex()),
        ),
        ("through_seq", JsonValue::from(snapshot.last_sequence().0)),
    ])
    .to_json_string()
    .map_err(|error| SessionError::InvalidInput {
        message: format!("export manifest cannot encode as JSON: {error}"),
    })?;
    let path = directory.join("export.json");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| io(&path, error))?;
    write_complete_line(&mut file, &contents, durability, &path)?;
    if durability == DurabilityMode::Strict {
        sync_directory(directory)?;
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
    ensure_complete_line_size(json)?;
    #[cfg(test)]
    if let Some(failpoint) = test_write_failpoint() {
        match failpoint {
            TestWriteFailpoint::BeforeAppend => return Err(injected_pre_write_failure(path)),
            TestWriteFailpoint::AfterJsonBytes(byte_count) => {
                let byte_count = byte_count.min(json.len());
                file.write_all(&json.as_bytes()[..byte_count])
                    .map_err(|error| indeterminate_write(path, error))?;
                return Err(if byte_count == 0 {
                    injected_pre_write_failure(path)
                } else {
                    injected_write_failure(path)
                });
            }
            TestWriteFailpoint::AfterJsonBeforeNewline
            | TestWriteFailpoint::AfterNewlineBeforeFlush
            | TestWriteFailpoint::DuringFlush
            | TestWriteFailpoint::AfterFlushBeforeSync
            | TestWriteFailpoint::DuringSync
            | TestWriteFailpoint::AfterSyncBeforeReturn => {}
        }
    }
    file.write_all(json.as_bytes())
        .map_err(|error| indeterminate_write(path, error))?;
    #[cfg(test)]
    if matches!(
        test_write_failpoint(),
        Some(TestWriteFailpoint::AfterJsonBeforeNewline)
    ) {
        return Err(injected_write_failure(path));
    }
    file.write_all(b"\n")
        .map_err(|error| indeterminate_write(path, error))?;
    #[cfg(test)]
    if matches!(
        test_write_failpoint(),
        Some(TestWriteFailpoint::AfterNewlineBeforeFlush)
    ) {
        return Err(injected_write_failure(path));
    }
    #[cfg(test)]
    if matches!(
        test_write_failpoint(),
        Some(TestWriteFailpoint::DuringFlush)
    ) {
        return Err(injected_write_failure(path));
    }
    file.flush()
        .map_err(|error| indeterminate_write(path, error))?;
    #[cfg(test)]
    if matches!(
        test_write_failpoint(),
        Some(TestWriteFailpoint::AfterFlushBeforeSync)
    ) {
        return Err(injected_write_failure(path));
    }
    if durability == DurabilityMode::Strict {
        #[cfg(test)]
        if matches!(test_write_failpoint(), Some(TestWriteFailpoint::DuringSync)) {
            return Err(injected_write_failure(path));
        }
        file.sync_data()
            .map_err(|error| indeterminate_write(path, error))?;
        #[cfg(test)]
        if matches!(
            test_write_failpoint(),
            Some(TestWriteFailpoint::AfterSyncBeforeReturn)
        ) {
            return Err(injected_write_failure(path));
        }
    }
    Ok(())
}

#[cfg(test)]
fn injected_write_failure(path: &Path) -> SessionError {
    indeterminate_write(
        path,
        std::io::Error::other("injected session write interruption"),
    )
}

#[cfg(test)]
fn injected_pre_write_failure(path: &Path) -> SessionError {
    io(
        path,
        std::io::Error::other("injected session write rejection before append"),
    )
}

fn indeterminate_write(path: &Path, error: std::io::Error) -> SessionError {
    SessionError::IndeterminateWrite {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn ensure_complete_line_size(json: &str) -> Result<(), SessionError> {
    if json.len() > MAX_SESSION_LINE_BYTES {
        return Err(SessionError::InvalidInput {
            message: format!("session line exceeds the {MAX_SESSION_LINE_BYTES}-byte line limit"),
        });
    }
    Ok(())
}

fn write_head_cache(
    directory: &Path,
    snapshot: &SessionSnapshot,
    durability: DurabilityMode,
) -> Result<(), SessionError> {
    let destination = directory.join("HEAD");
    let nonce = NEXT_HEAD_CACHE_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".HEAD.{}-{nonce:016x}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| io(&temporary, error))?;
    let contents = head_cache_contents(snapshot)?;
    let result = (|| {
        file.write_all(contents.as_bytes())
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
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// `HEAD` identifies only the selected main-lane revision and the immutable
/// header that names its session. It deliberately does not mirror the latest
/// log prefix: ordinary record commits do not change active-harness selection
/// and must not add a second synchronous replace to the hot append path.
fn head_cache_contents(snapshot: &SessionSnapshot) -> Result<String, SessionError> {
    JsonValue::object([
        (
            "active_harness_revision",
            snapshot
                .active_main_harness_revision()
                .map(string_value)
                .unwrap_or(JsonValue::Null),
        ),
        (
            "header_digest",
            JsonValue::String(snapshot.header().digest.to_hex()),
        ),
        ("session_id", string_value(&snapshot.header().session_id)),
        ("version", JsonValue::from(1_u64)),
    ])
    .to_json_string()
    .map_err(|error| SessionError::InvalidInput {
        message: format!("HEAD cache cannot encode as JSON: {error}"),
    })
}

fn head_cache_matches(directory: &Path, snapshot: &SessionSnapshot) -> bool {
    let destination = directory.join("HEAD");
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            metadata
        }
        Ok(_) | Err(_) => return false,
    };
    // `HEAD` is a fixed-size, derived cache; treating an unexpected large file
    // as stale avoids making session open allocate in proportion to untrusted
    // cache bytes.
    if metadata.len() > 16 * 1024 {
        return false;
    }
    let mut expected = match head_cache_contents(snapshot) {
        Ok(contents) => contents.into_bytes(),
        Err(_) => return false,
    };
    expected.push(b'\n');
    fs::read(destination).is_ok_and(|contents| contents == expected)
}

fn selects_main_harness_revision(mutation: &StoredMutation) -> bool {
    matches!(
        &mutation.mutation,
        SessionMutation::Entry(entry)
            if entry.lane_id == LaneId::main()
                && matches!(entry.body, SessionEntry::HarnessRevisionChanged(_))
    )
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

fn decode_snapshot_stream(
    file: &mut File,
    path: &Path,
) -> Result<(SessionSnapshot, SessionAppendIndex, Option<u64>), SessionError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io(path, error))?;
    let mut lines = BoundedLineReader::new(file, path);
    let header_line = match lines.next_line()? {
        ReadLine::Complete(line) => line,
        ReadLine::End | ReadLine::IncompleteTail { .. } => {
            return Err(SessionError::Format {
                path: path.display().to_string(),
                line: 1,
                offset: 0,
                sequence: None,
                mutation_kind: None,
                message: "session file has no complete header line".into(),
            });
        }
    };
    let header_json = parse_canonical_line(path, &header_line)?;
    let header = decode_header(&header_json)
        .map_err(|message| format_error(path, header_line.line, header_line.offset, message))?;
    let mut snapshot = SessionSnapshot::empty(header);
    let mut append_index = SessionAppendIndex::empty(snapshot.header());
    loop {
        match lines.next_line()? {
            ReadLine::Complete(line) => {
                let value = parse_canonical_line(path, &line)?;
                let mutation = decode_mutation(&value, &snapshot.header().session_id)
                    .map_err(|error| format_mutation_error(path, line.line, line.offset, error))?;
                if mutation.seq != snapshot.next_sequence() {
                    return Err(format_decoded_mutation_error(
                        path,
                        line.line,
                        line.offset,
                        &mutation,
                        format!(
                            "non-consecutive sequence {}; expected {}",
                            mutation.seq.0,
                            snapshot.next_sequence().0
                        ),
                    ));
                }
                if mutation.prev_digest != snapshot.last_digest() {
                    return Err(format_decoded_mutation_error(
                        path,
                        line.line,
                        line.offset,
                        &mutation,
                        "previous digest mismatch".into(),
                    ));
                }
                append_index.advance(&mutation);
                snapshot.push_mutation(mutation);
            }
            ReadLine::End => return Ok((snapshot, append_index, None)),
            ReadLine::IncompleteTail { offset } => {
                return Ok((snapshot, append_index, Some(offset)));
            }
        }
    }
}

struct CompleteLine {
    line: usize,
    offset: u64,
    bytes: Vec<u8>,
}

enum ReadLine {
    Complete(CompleteLine),
    End,
    IncompleteTail { offset: u64 },
}

struct BoundedLineReader<'a> {
    reader: BufReader<&'a mut File>,
    path: &'a Path,
    next_line: usize,
    next_offset: u64,
}

impl<'a> BoundedLineReader<'a> {
    fn new(file: &'a mut File, path: &'a Path) -> Self {
        Self {
            reader: BufReader::new(file),
            path,
            next_line: 1,
            next_offset: 0,
        }
    }

    fn next_line(&mut self) -> Result<ReadLine, SessionError> {
        let line = self.next_line;
        let offset = self.next_offset;
        let mut bytes = Vec::new();
        let mut oversized = false;
        loop {
            let buffer = self.reader.fill_buf().map_err(|error| SessionError::Io {
                path: self.path.display().to_string(),
                message: error.to_string(),
            })?;
            if buffer.is_empty() {
                return if bytes.is_empty() && !oversized {
                    Ok(ReadLine::End)
                } else {
                    Ok(ReadLine::IncompleteTail { offset })
                };
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let content_length = newline.unwrap_or(buffer.len());
            if !oversized {
                if bytes.len().saturating_add(content_length) > MAX_SESSION_LINE_BYTES {
                    oversized = true;
                    bytes.clear();
                } else {
                    bytes.extend_from_slice(&buffer[..content_length]);
                }
            }
            let consumed = newline.map_or(buffer.len(), |position| position + 1);
            self.reader.consume(consumed);
            self.next_offset = self.next_offset.saturating_add(consumed as u64);
            if newline.is_some() {
                self.next_line = self.next_line.saturating_add(1);
                if oversized {
                    return Err(SessionError::Format {
                        path: self.path.display().to_string(),
                        line,
                        offset,
                        sequence: None,
                        mutation_kind: None,
                        message: format!(
                            "session line exceeds the {MAX_SESSION_LINE_BYTES}-byte line limit"
                        ),
                    });
                }
                return Ok(ReadLine::Complete(CompleteLine {
                    line,
                    offset,
                    bytes,
                }));
            }
        }
    }
}

fn parse_canonical_line(path: &Path, line: &CompleteLine) -> Result<JsonValue, SessionError> {
    let source = std::str::from_utf8(&line.bytes).map_err(|error| SessionError::Format {
        path: path.display().to_string(),
        line: line.line,
        offset: line.offset,
        sequence: None,
        mutation_kind: None,
        message: format!("session JSONL is not UTF-8: {error}"),
    })?;
    let value = JsonValue::parse(source).map_err(|error| SessionError::Format {
        path: path.display().to_string(),
        line: line.line,
        offset: line.offset,
        sequence: None,
        mutation_kind: None,
        message: error.to_string(),
    })?;
    let canonical = value
        .to_json_string()
        .map_err(|error| SessionError::Format {
            path: path.display().to_string(),
            line: line.line,
            offset: line.offset,
            sequence: None,
            mutation_kind: None,
            message: format!("could not re-encode session JSON canonically: {error}"),
        })?;
    if canonical.as_bytes() != line.bytes {
        return Err(SessionError::Format {
            path: path.display().to_string(),
            line: line.line,
            offset: line.offset,
            sequence: None,
            mutation_kind: None,
            message: "session line is not canonical JSON".into(),
        });
    }
    Ok(value)
}

fn format_error(path: &Path, line: usize, offset: u64, message: String) -> SessionError {
    SessionError::Format {
        path: path.display().to_string(),
        line,
        offset,
        sequence: None,
        mutation_kind: None,
        message,
    }
}

fn format_mutation_error(
    path: &Path,
    line: usize,
    offset: u64,
    error: MutationDecodeError,
) -> SessionError {
    SessionError::Format {
        path: path.display().to_string(),
        line,
        offset,
        sequence: error.sequence,
        mutation_kind: error.mutation_kind,
        message: error.message,
    }
}

fn format_decoded_mutation_error(
    path: &Path,
    line: usize,
    offset: u64,
    mutation: &StoredMutation,
    message: String,
) -> SessionError {
    SessionError::Format {
        path: path.display().to_string(),
        line,
        offset,
        sequence: Some(mutation.seq),
        mutation_kind: Some(mutation_kind_name(&mutation.mutation).into()),
        message,
    }
}

fn io(path: &Path, error: std::io::Error) -> SessionError {
    SessionError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn encode_unsigned_header(header: &SessionHeader) -> JsonValue {
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

fn encode_header(header: &SessionHeader) -> JsonValue {
    let mut fields = encode_unsigned_header(header)
        .as_object()
        .expect("unsigned session header is an object")
        .clone();
    fields.insert("digest".into(), digest_value(header.digest));
    JsonValue::Object(fields)
}

pub(crate) fn seal_header(header: &mut SessionHeader) -> Result<(), SessionError> {
    header.digest = calculate_header_digest(header)
        .map_err(|message| SessionError::InvalidInput { message })?;
    Ok(())
}

fn calculate_header_digest(header: &SessionHeader) -> Result<Digest, String> {
    let canonical = encode_unsigned_header(header)
        .to_json_string()
        .map_err(|error| format!("session header cannot encode canonically: {error}"))?;
    let mut hasher = CanonicalHashWriter::new("tea-session-header-v1", 1, 1);
    hasher.bytes("canonical_unsigned_header", canonical.as_bytes());
    Ok(hasher.finish())
}

fn decode_header(value: &JsonValue) -> Result<SessionHeader, String> {
    let object = object(value)?;
    require_exact_fields(
        object,
        &[
            "kind",
            "version",
            "session_id",
            "created_at_ms",
            "workspace",
            "metadata",
            "initial_lane",
            "digest",
        ],
        "header",
    )?;
    if required_string(object, "kind")? != "session" {
        return Err("header kind must be `session`".into());
    }
    let version = required_u64(object, "version")?;
    if version != u64::from(SESSION_FORMAT_VERSION) {
        return Err(format!("unsupported session version {version}"));
    }
    let digest = parse_digest(required_string(object, "digest")?)?;
    let header = SessionHeader {
        kind: "session".into(),
        version: SESSION_FORMAT_VERSION,
        session_id: parse_id!(SessionId, required_string(object, "session_id")?),
        created_at_ms: required_u64(object, "created_at_ms")?,
        workspace: required_string(object, "workspace")?,
        metadata: required_metadata(object, "metadata")?,
        initial_lane: parse_id!(LaneId, required_string(object, "initial_lane")?),
        digest,
    };
    if calculate_header_digest(&header)? != digest {
        return Err("header digest mismatch".into());
    }
    Ok(header)
}

pub(crate) fn encode_mutation(mutation: &StoredMutation) -> JsonValue {
    JsonValue::object([
        ("seq", JsonValue::from(mutation.seq.0)),
        ("timestamp_ms", JsonValue::from(mutation.timestamp_ms)),
        ("prev_digest", digest_value(mutation.prev_digest)),
        ("mutation", encode_mutation_payload(&mutation.mutation)),
        ("digest", digest_value(mutation.digest)),
    ])
}

fn encode_mutation_payload(mutation: &SessionMutation) -> JsonValue {
    match mutation {
        SessionMutation::Entry(entry) => JsonValue::object([
            ("kind", JsonValue::String("entry".into())),
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
        SessionMutation::Record(record) => JsonValue::object([
            ("kind", JsonValue::String("record".into())),
            ("payload", encode_record(&record.record)),
        ]),
        SessionMutation::Lane(lane) => JsonValue::object([
            ("kind", JsonValue::String("lane".into())),
            ("payload", encode_lane_mutation(&lane.mutation)),
        ]),
        SessionMutation::Fact(fact) => JsonValue::object([
            ("kind", JsonValue::String("fact".into())),
            ("payload", encode_fact(&fact.fact)),
        ]),
    }
}

pub(crate) fn seal_mutation(
    snapshot: &SessionSnapshot,
    mutation: SessionMutation,
) -> Result<StoredMutation, SessionError> {
    let (seq, timestamp_ms) = mutation_envelope_values(&mutation);
    let prev_digest = snapshot.last_digest();
    let digest = calculate_record_digest(
        &snapshot.header().session_id,
        seq,
        timestamp_ms,
        prev_digest,
        &mutation,
    )
    .map_err(|message| SessionError::InvalidInput { message })?;
    Ok(StoredMutation {
        seq,
        timestamp_ms,
        prev_digest,
        digest,
        mutation,
    })
}

fn mutation_envelope_values(mutation: &SessionMutation) -> (Sequence, u64) {
    match mutation {
        SessionMutation::Entry(entry) => (entry.header.seq, entry.header.timestamp_ms),
        SessionMutation::Record(record) => (record.seq, record.timestamp_ms),
        SessionMutation::Lane(lane) => (lane.seq, lane.timestamp_ms),
        SessionMutation::Fact(fact) => (fact.seq, fact.timestamp_ms),
    }
}

fn calculate_record_digest(
    session_id: &SessionId,
    seq: Sequence,
    timestamp_ms: u64,
    prev_digest: Digest,
    mutation: &SessionMutation,
) -> Result<Digest, String> {
    let canonical_payload = encode_mutation_payload(mutation)
        .to_json_string()
        .map_err(|error| format!("session mutation cannot encode canonically: {error}"))?;
    let mut hasher = CanonicalHashWriter::new("tea-session-record-v1", 1, 1);
    hasher.string("session_id", session_id.as_str());
    hasher.u64("sequence", seq.0);
    hasher.u64("timestamp_ms", timestamp_ms);
    hasher.bytes("previous_digest", prev_digest.as_bytes());
    hasher.bytes("canonical_mutation_payload", canonical_payload.as_bytes());
    Ok(hasher.finish())
}

#[derive(Debug)]
struct MutationDecodeError {
    message: String,
    sequence: Option<Sequence>,
    mutation_kind: Option<String>,
}

impl From<String> for MutationDecodeError {
    fn from(message: String) -> Self {
        Self {
            message,
            sequence: None,
            mutation_kind: None,
        }
    }
}

impl MutationDecodeError {
    fn from_decoded_mutation(message: impl Into<String>, mutation: &StoredMutation) -> Self {
        Self {
            message: message.into(),
            sequence: Some(mutation.seq),
            mutation_kind: Some(mutation_kind_name(&mutation.mutation).into()),
        }
    }
}

fn decode_mutation(
    value: &JsonValue,
    session_id: &SessionId,
) -> Result<StoredMutation, MutationDecodeError> {
    let fields = object(value)?;
    require_exact_fields(
        fields,
        &["seq", "timestamp_ms", "prev_digest", "mutation", "digest"],
        "mutation envelope",
    )?;
    let seq = Sequence(required_u64(fields, "seq")?);
    let timestamp_ms = required_u64(fields, "timestamp_ms")?;
    let prev_digest = parse_digest(required_string(fields, "prev_digest")?)?;
    let digest = parse_digest(required_string(fields, "digest")?)?;
    let mutation = decode_session_mutation(required_value(fields, "mutation")?, seq, timestamp_ms)?;
    if calculate_record_digest(session_id, seq, timestamp_ms, prev_digest, &mutation)? != digest {
        return Err(MutationDecodeError {
            message: "record digest mismatch".into(),
            sequence: Some(seq),
            mutation_kind: Some(mutation_kind_name(&mutation).into()),
        });
    }
    let stored = StoredMutation {
        seq,
        timestamp_ms,
        prev_digest,
        digest,
        mutation,
    };
    // Decoding must be lossless for the closed v1 wire schema. In particular,
    // an unknown nested field must not be silently excluded from the digest
    // calculation just because this decoder has no semantic home for it.
    if encode_mutation(&stored) != *value {
        return Err(MutationDecodeError::from_decoded_mutation(
            "mutation does not match the v1 schema",
            &stored,
        ));
    }
    Ok(stored)
}

fn mutation_kind_name(mutation: &SessionMutation) -> &'static str {
    match mutation {
        SessionMutation::Entry(_) => "entry",
        SessionMutation::Record(_) => "record",
        SessionMutation::Lane(_) => "lane",
        SessionMutation::Fact(_) => "fact",
    }
}

fn decode_session_mutation(
    value: &JsonValue,
    seq: Sequence,
    timestamp_ms: u64,
) -> Result<SessionMutation, String> {
    let fields = object(value)?;
    require_exact_fields(fields, &["kind", "payload"], "mutation")?;
    let kind = required_string(fields, "kind")?;
    let payload = required_value(fields, "payload")?;
    match kind.as_str() {
        "entry" => {
            let payload = object(payload)?;
            let lane_id = parse_id!(LaneId, required_string(payload, "lane_id")?);
            let id = parse_id!(EntryId, required_string(payload, "id")?);
            let parent_id = optional_id_of::<EntryId>(payload, "parent_id")?;
            let body = decode_entry(required_value(payload, "entry")?)?;
            Ok(SessionMutation::Entry(StoredEntry {
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
        "record" => Ok(SessionMutation::Record(StoredRecord {
            seq,
            timestamp_ms,
            record: decode_record(payload)?,
        })),
        "lane" => Ok(SessionMutation::Lane(StoredLaneMutation {
            seq,
            timestamp_ms,
            mutation: decode_lane_mutation(payload)?,
        })),
        "fact" => Ok(SessionMutation::Fact(StoredFact {
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
        SessionFact::SubagentPolicy(fact) => JsonValue::object([
            ("type", JsonValue::String("subagent_policy".into())),
            ("schema_version", JsonValue::from(u64::from(fact.schema_version))),
            (
                "models",
                JsonValue::Array(fact.models.iter().map(encode_subagent_model).collect()),
            ),
            ("max_concurrent", JsonValue::from(u64::from(fact.max_concurrent))),
            (
                "max_total_per_operation",
                JsonValue::from(u64::from(fact.max_total_per_operation)),
            ),
            ("timeout_ms", JsonValue::from(fact.timeout_ms)),
            ("tool_surface_digest", digest_value(fact.tool_surface_digest)),
        ]),
        SessionFact::AgentSpawned(fact) => JsonValue::object([
            ("type", JsonValue::String("agent_spawned".into())),
            ("agent_id", string_value(&fact.agent_id)),
            ("parent_lane_id", string_value(&fact.parent_lane_id)),
            ("parent_operation_id", string_value(&fact.parent_operation_id)),
            ("lane_id", string_value(&fact.lane_id)),
            ("task_name", JsonValue::String(fact.task_name.clone())),
            ("model", encode_subagent_model(&fact.model)),
            ("thinking", JsonValue::String(fact.thinking.clone())),
            (
                "context_mode",
                JsonValue::String(agent_context_mode_name(fact.context_mode).into()),
            ),
            ("base_leaf_id", optional_id(fact.base_leaf_id.as_ref())),
            ("workspace_lease_id", string_value(&fact.workspace_lease_id)),
            (
                "harness_revision_id",
                string_value(&fact.harness_revision_id),
            ),
            (
                "harness_snapshot_id",
                string_value(&fact.harness_snapshot_id),
            ),
            (
                "model_harness_profile_id",
                string_value(&fact.model_harness_profile_id),
            ),
            (
                "spawn_tool_call_id",
                JsonValue::String(fact.spawn_tool_call_id.clone()),
            ),
        ]),
        SessionFact::WorkspaceDelta(fact) => JsonValue::object([
            ("type", JsonValue::String("workspace_delta".into())),
            ("delta_id", string_value(&fact.delta_id)),
            ("agent_id", string_value(&fact.agent_id)),
            ("workspace_lease_id", string_value(&fact.workspace_lease_id)),
            ("base_commit", JsonValue::String(fact.base_commit.clone())),
            ("result_commit", JsonValue::String(fact.result_commit.clone())),
            (
                "changed_paths",
                JsonValue::Array(
                    fact.changed_paths
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            ("patch", encode_payload_ref(&fact.patch)),
        ]),
        SessionFact::AgentTaskFinished(fact) => JsonValue::object([
            ("type", JsonValue::String("agent_task_finished".into())),
            ("agent_id", string_value(&fact.agent_id)),
            ("operation_id", string_value(&fact.operation_id)),
            ("outcome", encode_operation_outcome(&fact.outcome)),
            ("final_entry_id", optional_id(fact.final_entry_id.as_ref())),
            ("report", encode_payload_ref(&fact.report)),
            (
                "workspace_delta_id",
                optional_id(fact.workspace_delta_id.as_ref()),
            ),
        ]),
        SessionFact::WorkspaceDeltaApplied(fact) => JsonValue::object([
            ("type", JsonValue::String("workspace_delta_applied".into())),
            ("delta_id", string_value(&fact.delta_id)),
            ("target_lane_id", string_value(&fact.target_lane_id)),
            ("tool_call_id", JsonValue::String(fact.tool_call_id.clone())),
            (
                "changed_paths",
                JsonValue::Array(
                    fact.changed_paths
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
        ]),
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
        "subagent_policy" => Ok(SessionFact::SubagentPolicy(SubagentPolicyFact {
            schema_version: u16::try_from(required_u64(fields, "schema_version")?)
                .map_err(|_| "subagent policy schema version exceeds u16".to_string())?,
            models: required_array(fields, "models")?
                .iter()
                .map(decode_subagent_model)
                .collect::<Result<Vec<_>, _>>()?,
            max_concurrent: u32::try_from(required_u64(fields, "max_concurrent")?)
                .map_err(|_| "subagent max_concurrent exceeds u32".to_string())?,
            max_total_per_operation: u32::try_from(required_u64(
                fields,
                "max_total_per_operation",
            )?)
            .map_err(|_| "subagent max_total_per_operation exceeds u32".to_string())?,
            timeout_ms: required_u64(fields, "timeout_ms")?,
            tool_surface_digest: parse_digest(required_string(fields, "tool_surface_digest")?)?,
        })),
        "agent_spawned" => Ok(SessionFact::AgentSpawned(AgentSpawnedFact {
            agent_id: parse_id!(AgentId, required_string(fields, "agent_id")?),
            parent_lane_id: parse_id!(LaneId, required_string(fields, "parent_lane_id")?),
            parent_operation_id: parse_id!(
                OperationId,
                required_string(fields, "parent_operation_id")?
            ),
            lane_id: parse_id!(LaneId, required_string(fields, "lane_id")?),
            task_name: required_string(fields, "task_name")?,
            model: decode_subagent_model(required_value(fields, "model")?)?,
            thinking: required_string(fields, "thinking")?,
            context_mode: parse_agent_context_mode(&required_string(fields, "context_mode")?)?,
            base_leaf_id: optional_id_of::<EntryId>(fields, "base_leaf_id")?,
            workspace_lease_id: parse_id!(
                WorkspaceLeaseId,
                required_string(fields, "workspace_lease_id")?
            ),
            harness_revision_id: parse_id!(
                HarnessRevisionId,
                required_string(fields, "harness_revision_id")?
            ),
            harness_snapshot_id: parse_id!(
                HarnessSnapshotId,
                required_string(fields, "harness_snapshot_id")?
            ),
            model_harness_profile_id: parse_id!(
                ModelHarnessProfileId,
                required_string(fields, "model_harness_profile_id")?
            ),
            spawn_tool_call_id: required_string(fields, "spawn_tool_call_id")?,
        })),
        "workspace_delta" => Ok(SessionFact::WorkspaceDelta(WorkspaceDeltaFact {
            delta_id: parse_id!(WorkspaceDeltaId, required_string(fields, "delta_id")?),
            agent_id: parse_id!(AgentId, required_string(fields, "agent_id")?),
            workspace_lease_id: parse_id!(
                WorkspaceLeaseId,
                required_string(fields, "workspace_lease_id")?
            ),
            base_commit: required_string(fields, "base_commit")?,
            result_commit: required_string(fields, "result_commit")?,
            changed_paths: string_array(required_array(fields, "changed_paths")?)?,
            patch: decode_payload_ref(required_value(fields, "patch")?)?,
        })),
        "agent_task_finished" => Ok(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
            agent_id: parse_id!(AgentId, required_string(fields, "agent_id")?),
            operation_id: parse_id!(OperationId, required_string(fields, "operation_id")?),
            outcome: decode_operation_outcome(required_value(fields, "outcome")?)?,
            final_entry_id: optional_id_of::<EntryId>(fields, "final_entry_id")?,
            report: decode_payload_ref(required_value(fields, "report")?)?,
            workspace_delta_id: optional_id_of::<WorkspaceDeltaId>(
                fields,
                "workspace_delta_id",
            )?,
        })),
        "workspace_delta_applied" => Ok(SessionFact::WorkspaceDeltaApplied(
            WorkspaceDeltaAppliedFact {
                delta_id: parse_id!(WorkspaceDeltaId, required_string(fields, "delta_id")?),
                target_lane_id: parse_id!(
                    LaneId,
                    required_string(fields, "target_lane_id")?
                ),
                tool_call_id: required_string(fields, "tool_call_id")?,
                changed_paths: string_array(required_array(fields, "changed_paths")?)?,
            },
        )),
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

fn encode_subagent_model(model: &SubagentModelRecord) -> JsonValue {
    JsonValue::object([
        ("provider", JsonValue::String(model.provider.clone())),
        ("model", JsonValue::String(model.model.clone())),
        ("revision", optional_string(model.revision.as_deref())),
        ("display_name", JsonValue::String(model.display_name.clone())),
        ("context_window", optional_u64(model.context_window)),
    ])
}

fn decode_subagent_model(value: &JsonValue) -> Result<SubagentModelRecord, String> {
    let object = object(value)?;
    Ok(SubagentModelRecord {
        provider: required_string(object, "provider")?,
        model: required_string(object, "model")?,
        revision: optional_string_of(object, "revision")?,
        display_name: required_string(object, "display_name")?,
        context_window: optional_u64_of(object, "context_window")?,
    })
}

fn agent_context_mode_name(value: AgentContextMode) -> &'static str {
    match value {
        AgentContextMode::Task => "task",
        AgentContextMode::Parent => "parent",
    }
}

fn parse_agent_context_mode(value: &str) -> Result<AgentContextMode, String> {
    match value {
        "task" => Ok(AgentContextMode::Task),
        "parent" => Ok(AgentContextMode::Parent),
        _ => Err(format!("unknown agent context mode {value:?}")),
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
        OperationKind::Subagent {
            agent_id,
            parent_operation_id,
        } => JsonValue::object([
            ("kind", JsonValue::String("subagent".into())),
            ("agent_id", string_value(agent_id)),
            ("parent_operation_id", string_value(parent_operation_id)),
        ]),
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
        "subagent" => Ok(OperationKind::Subagent {
            agent_id: parse_id!(AgentId, required_string(object, "agent_id")?),
            parent_operation_id: parse_id!(
                OperationId,
                required_string(object, "parent_operation_id")?
            ),
        }),
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
    AgentId,
    WorkspaceLeaseId,
    WorkspaceDeltaId,
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

fn require_exact_fields(
    object: &BTreeMap<String, JsonValue>,
    expected: &[&str],
    context: &str,
) -> Result<(), String> {
    if object.len() != expected.len() || expected.iter().any(|field| !object.contains_key(*field)) {
        return Err(format!("unknown or missing {context} fields"));
    }
    Ok(())
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
