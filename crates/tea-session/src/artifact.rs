use crate::{ArtifactId, ArtifactPolicyId};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MAX_PAGE_BYTES: usize = 1_048_576;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

/// Deterministic immutable-publication interruption used only by the artifact
/// storage matrix. Production publication has no failpoint branch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestArtifactWriteFailpoint {
    BeforeTemporaryCreation,
    BeforeFileSync,
    AfterFileSync,
    BeforePublication,
    AfterPublication,
    BeforeDirectorySync,
    AfterDirectorySync,
}

#[cfg(test)]
thread_local! {
    static TEST_ARTIFACT_WRITE_FAILPOINT: RefCell<Option<TestArtifactWriteFailpoint>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestArtifactWriteFailpointGuard;

#[cfg(test)]
impl Drop for TestArtifactWriteFailpointGuard {
    fn drop(&mut self) {
        TEST_ARTIFACT_WRITE_FAILPOINT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) fn install_test_artifact_write_failpoint(
    failpoint: TestArtifactWriteFailpoint,
) -> TestArtifactWriteFailpointGuard {
    TEST_ARTIFACT_WRITE_FAILPOINT.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "an artifact write failpoint is already installed on this thread"
        );
        *slot.borrow_mut() = Some(failpoint);
    });
    TestArtifactWriteFailpointGuard
}

#[cfg(test)]
fn test_artifact_write_failpoint() -> Option<TestArtifactWriteFailpoint> {
    TEST_ARTIFACT_WRITE_FAILPOINT.with(|slot| *slot.borrow())
}

/// Explicit retention and direct-reader bounds for model-readable artifacts.
///
/// A policy describes bytes *after* any host redaction. The artifact store
/// never invents a redaction fallback: callers must redact before `put` when
/// `redact_before_persist` is set, or reject the result rather than retaining
/// a secret solely to make recovery convenient.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPolicy {
    /// Stable policy identity recorded by semantic entries.
    pub policy_id: ArtifactPolicyId,
    /// Whether the retained object may be exposed through model-facing tools.
    pub model_readable: bool,
    /// Whether the caller must apply its configured redactor before storage.
    pub redact_before_persist: bool,
    /// Maximum canonical payload size that may remain inside a session entry.
    pub maximum_inline_bytes: usize,
    /// Maximum direct artifact page a model-facing reader may return.
    pub maximum_page_bytes: usize,
}

impl Default for ArtifactPolicy {
    fn default() -> Self {
        Self {
            policy_id: ArtifactPolicyId::new("tea-recoverable-v1")
                .expect("built-in artifact policy ID is valid"),
            model_readable: true,
            redact_before_persist: false,
            maximum_inline_bytes: 8_192,
            maximum_page_bytes: 4_096,
        }
    }
}

impl ArtifactPolicy {
    /// Reject bounds that cannot produce a safe direct reader response.
    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.maximum_inline_bytes == 0 {
            return Err(ArtifactError::InvalidRequest {
                message: "artifact inline limit must be greater than zero".into(),
            });
        }
        if self.maximum_page_bytes == 0 || self.maximum_page_bytes > MAX_PAGE_BYTES {
            return Err(ArtifactError::InvalidRequest {
                message: format!("artifact page limit must be within 1..={MAX_PAGE_BYTES} bytes"),
            });
        }
        Ok(())
    }
}

/// Exact immutable object metadata returned after successful persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactDescriptor {
    /// Content-addressed BLAKE3 identity.
    pub artifact_id: ArtifactId,
    /// Exact retained bytes.
    pub byte_len: u64,
    /// Host-supplied stable media type.
    pub media_type: String,
}

/// Content-free inventory metadata for one immutable artifact object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactInventoryItem {
    /// Content-addressed object identity.
    pub artifact_id: ArtifactId,
    /// Exact byte length observed in the object store.
    pub byte_len: u64,
}

/// A bounded direct artifact page. It deliberately does not enter the normal
/// tool-result spill/projection path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPage {
    /// Offset of the first returned byte.
    pub offset: u64,
    /// Exact requested bytes, up to the caller's bounded limit.
    pub bytes: Vec<u8>,
    /// Whether no more bytes remain after this page.
    pub eof: bool,
}

/// Literal-query result for a bounded artifact search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactMatch {
    /// Byte offset of the matching literal.
    pub offset: u64,
    /// Bounded context beginning before or at the match.
    pub context: Vec<u8>,
}

/// Failures at the immutable artifact boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactError {
    /// Requested object is absent.
    NotFound { artifact_id: ArtifactId },
    /// A path entry violates the no-symlink/private-object contract.
    UnsafePath { path: String, message: String },
    /// Existing bytes disagree with their claimed content identity.
    Corruption {
        artifact_id: ArtifactId,
        message: String,
    },
    /// Caller requested an invalid page/search bound.
    InvalidRequest { message: String },
    /// Filesystem or memory store failure.
    Io { path: String, message: String },
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { artifact_id } => {
                write!(formatter, "artifact {artifact_id} was not found")
            }
            Self::UnsafePath { path, message } => {
                write!(formatter, "unsafe artifact path {path}: {message}")
            }
            Self::Corruption {
                artifact_id,
                message,
            } => write!(formatter, "artifact {artifact_id} is corrupt: {message}"),
            Self::InvalidRequest { message } => {
                write!(formatter, "invalid artifact request: {message}")
            }
            Self::Io { path, message } => {
                write!(formatter, "artifact I/O failed at {path}: {message}")
            }
        }
    }
}

impl std::error::Error for ArtifactError {}

/// Immutable content-addressed object store.
pub trait ArtifactStore: Send + Sync {
    /// Persist exact bytes before returning their durable identity.
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactDescriptor, ArtifactError>;

    /// Load exact immutable bytes.
    fn get(&self, artifact_id: ArtifactId) -> Result<Vec<u8>, ArtifactError>;

    /// Rehash one immutable object and return its exact byte length without
    /// exposing content to the caller. Backends may implement this as a
    /// streaming verification path.
    fn verify_object(&self, artifact_id: ArtifactId) -> Result<u64, ArtifactError> {
        let bytes = self.get(artifact_id)?;
        if ArtifactId::from_bytes(&bytes) != artifact_id {
            return Err(ArtifactError::Corruption {
                artifact_id,
                message: "artifact bytes do not match their content-addressed identity".into(),
            });
        }
        Ok(bytes.len() as u64)
    }

    /// List immutable object identities and sizes for an explicit GC pass.
    ///
    /// Stores that do not support inventory should leave the default typed
    /// rejection in place; a host must then avoid pretending a GC run was
    /// completed.
    fn inventory(&self) -> Result<Vec<ArtifactInventoryItem>, ArtifactError> {
        Err(ArtifactError::InvalidRequest {
            message: "artifact store does not support inventory".into(),
        })
    }

    /// Remove one object selected by a previously validated GC plan.
    ///
    /// This operation is intentionally not best effort. A store verifies the
    /// target remains a real object under its own root before deletion and
    /// returns an error rather than silently skipping an unsafe path.
    fn remove(&self, _artifact_id: ArtifactId) -> Result<(), ArtifactError> {
        Err(ArtifactError::InvalidRequest {
            message: "artifact store does not support explicit removal".into(),
        })
    }

    /// Read a bounded page directly from immutable bytes.
    fn read_page(
        &self,
        artifact_id: ArtifactId,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<ArtifactPage, ArtifactError> {
        validate_bound(maximum_bytes)?;
        let bytes = self.get(artifact_id)?;
        let offset = offset.min(bytes.len() as u64) as usize;
        let end = offset.saturating_add(maximum_bytes).min(bytes.len());
        Ok(ArtifactPage {
            offset: offset as u64,
            bytes: bytes[offset..end].to_vec(),
            eof: end == bytes.len(),
        })
    }

    /// Search a literal byte query with bounded result count and context.
    fn search_literal(
        &self,
        artifact_id: ArtifactId,
        query: &[u8],
        maximum_results: usize,
        context_bytes: usize,
    ) -> Result<Vec<ArtifactMatch>, ArtifactError> {
        validate_search_bounds(query, maximum_results, context_bytes)?;
        let bytes = self.get(artifact_id)?;
        let mut matches = Vec::new();
        let mut cursor = 0_usize;
        while cursor.saturating_add(query.len()) <= bytes.len() && matches.len() < maximum_results {
            let Some(relative) = bytes[cursor..]
                .windows(query.len())
                .position(|window| window == query)
            else {
                break;
            };
            let offset = cursor + relative;
            let start = offset.saturating_sub(context_bytes);
            let end = offset
                .saturating_add(query.len())
                .saturating_add(context_bytes)
                .min(bytes.len());
            matches.push(ArtifactMatch {
                offset: offset as u64,
                context: bytes[start..end].to_vec(),
            });
            cursor = offset.saturating_add(query.len());
        }
        Ok(matches)
    }
}

/// In-memory reference object store for backend-neutral fixtures.
#[derive(Clone, Default)]
pub struct MemoryArtifactStore {
    objects: Arc<Mutex<BTreeMap<ArtifactId, Vec<u8>>>>,
}

impl fmt::Debug for MemoryArtifactStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.objects.lock().expect("artifact mutex poisoned").len();
        formatter
            .debug_struct("MemoryArtifactStore")
            .field("object_count", &count)
            .finish()
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactDescriptor, ArtifactError> {
        validate_media_type(media_type)?;
        let artifact_id = ArtifactId::from_bytes(bytes);
        let mut objects = self.objects.lock().expect("artifact mutex poisoned");
        match objects.get(&artifact_id) {
            Some(existing) if existing != bytes => {
                return Err(ArtifactError::Corruption {
                    artifact_id,
                    message: "existing object has different bytes for the same digest".into(),
                });
            }
            Some(_) => {}
            None => {
                objects.insert(artifact_id, bytes.to_vec());
            }
        }
        Ok(ArtifactDescriptor {
            artifact_id,
            byte_len: bytes.len() as u64,
            media_type: media_type.into(),
        })
    }

    fn get(&self, artifact_id: ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        self.objects
            .lock()
            .expect("artifact mutex poisoned")
            .get(&artifact_id)
            .cloned()
            .ok_or(ArtifactError::NotFound { artifact_id })
    }

    fn inventory(&self) -> Result<Vec<ArtifactInventoryItem>, ArtifactError> {
        Ok(self
            .objects
            .lock()
            .expect("artifact mutex poisoned")
            .iter()
            .map(|(artifact_id, bytes)| ArtifactInventoryItem {
                artifact_id: *artifact_id,
                byte_len: bytes.len() as u64,
            })
            .collect())
    }

    fn remove(&self, artifact_id: ArtifactId) -> Result<(), ArtifactError> {
        let removed = self
            .objects
            .lock()
            .expect("artifact mutex poisoned")
            .remove(&artifact_id);
        if removed.is_none() {
            return Err(ArtifactError::NotFound { artifact_id });
        }
        Ok(())
    }
}

/// Filesystem implementation rooted at one explicit session `objects/` directory.
#[derive(Clone, Debug)]
pub struct FileArtifactStore {
    root: PathBuf,
}

impl FileArtifactStore {
    /// Create or open an explicit objects directory. The caller owns the
    /// enclosing session location; this type never discovers a home directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let root = root.as_ref().to_path_buf();
        ensure_directory(&root)?;
        ensure_directory(&root.join("blake3"))?;
        Ok(Self { root })
    }

    /// Return the caller-selected immutable object root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, artifact_id: ArtifactId) -> PathBuf {
        let digest = artifact_id.to_hex();
        self.root.join("blake3").join(&digest[..2]).join(digest)
    }

    /// Stream one immutable object to a private temporary file while hashing
    /// it, then publish the completed identity without overwriting an
    /// existing object. The input never has to be materialized in memory by
    /// the store.
    pub fn put_reader(
        &self,
        reader: &mut dyn Read,
        media_type: &str,
    ) -> Result<ArtifactDescriptor, ArtifactError> {
        validate_media_type(media_type)?;
        let object_root = self.root.join("blake3");
        ensure_directory(&object_root)?;
        let nonce = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary =
            object_root.join(format!(".{}-{nonce:016x}.artifact.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let result = (|| {
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::BeforeTemporaryCreation)
            ) {
                return Err(injected_artifact_write_failure(&temporary));
            }
            let mut file = options
                .open(&temporary)
                .map_err(|error| io(&temporary, error))?;
            let mut hasher = blake3::Hasher::new();
            let mut byte_len = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = reader
                    .read(&mut buffer)
                    .map_err(|error| ArtifactError::Io {
                        path: "artifact input stream".into(),
                        message: error.to_string(),
                    })?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
                byte_len = byte_len.saturating_add(count as u64);
                file.write_all(&buffer[..count])
                    .map_err(|error| io(&temporary, error))?;
            }
            file.flush().map_err(|error| io(&temporary, error))?;
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::BeforeFileSync)
            ) {
                return Err(injected_artifact_write_failure(&temporary));
            }
            file.sync_all().map_err(|error| io(&temporary, error))?;
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::AfterFileSync)
            ) {
                return Err(injected_artifact_write_failure(&temporary));
            }
            drop(file);

            let artifact_id = ArtifactId::from_hex(hasher.finalize().to_hex().as_str())
                .expect("BLAKE3 produces a canonical artifact digest");
            let destination = self.object_path(artifact_id);
            let parent = destination
                .parent()
                .expect("content-addressed destination always has a parent");
            ensure_directory(parent)?;
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::BeforePublication)
            ) {
                return Err(injected_artifact_write_failure(&temporary));
            }
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {
                    #[cfg(test)]
                    if matches!(
                        test_artifact_write_failpoint(),
                        Some(TestArtifactWriteFailpoint::AfterPublication)
                    ) {
                        return Err(injected_artifact_write_failure(&destination));
                    }
                    fs::remove_file(&temporary).map_err(|error| io(&temporary, error))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_existing_identity(&destination, artifact_id)?;
                    fs::remove_file(&temporary).map_err(|error| io(&temporary, error))?;
                }
                Err(error) => return Err(io(&destination, error)),
            }
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::BeforeDirectorySync)
            ) {
                return Err(injected_artifact_write_failure(&destination));
            }
            sync_directory(parent)?;
            sync_directory(&object_root)?;
            #[cfg(test)]
            if matches!(
                test_artifact_write_failpoint(),
                Some(TestArtifactWriteFailpoint::AfterDirectorySync)
            ) {
                return Err(injected_artifact_write_failure(&destination));
            }
            verify_existing_identity(&destination, artifact_id)?;
            Ok(ArtifactDescriptor {
                artifact_id,
                byte_len,
                media_type: media_type.into(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// Open a verified immutable object for streaming consumption.
    pub fn open_verified_reader(&self, artifact_id: ArtifactId) -> Result<File, ArtifactError> {
        let path = self.object_path(artifact_id);
        let _ = verify_existing_identity(&path, artifact_id)?;
        File::open(&path).map_err(|error| io(&path, error))
    }
}

#[cfg(test)]
fn injected_artifact_write_failure(path: &Path) -> ArtifactError {
    io(
        path,
        std::io::Error::other("injected artifact publication interruption"),
    )
}

impl ArtifactStore for FileArtifactStore {
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactDescriptor, ArtifactError> {
        let mut reader = Cursor::new(bytes);
        self.put_reader(&mut reader, media_type)
    }

    fn get(&self, artifact_id: ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        let path = self.object_path(artifact_id);
        let metadata = fs::symlink_metadata(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ArtifactError::NotFound { artifact_id },
            _ => io(&path, error),
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ArtifactError::UnsafePath {
                path: path.display().to_string(),
                message: "artifact object must be a regular non-symlink file".into(),
            });
        }
        let bytes = fs::read(&path).map_err(|error| io(&path, error))?;
        if ArtifactId::from_bytes(&bytes) != artifact_id {
            return Err(ArtifactError::Corruption {
                artifact_id,
                message: "object bytes do not match content-addressed filename".into(),
            });
        }
        Ok(bytes)
    }

    fn verify_object(&self, artifact_id: ArtifactId) -> Result<u64, ArtifactError> {
        let path = self.object_path(artifact_id);
        verify_existing_identity(&path, artifact_id)
    }

    fn read_page(
        &self,
        artifact_id: ArtifactId,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<ArtifactPage, ArtifactError> {
        validate_bound(maximum_bytes)?;
        let path = self.object_path(artifact_id);
        let length = verify_existing_identity(&path, artifact_id)?;
        let offset = offset.min(length);
        let available = length.saturating_sub(offset);
        let count = available.min(maximum_bytes as u64) as usize;
        let mut object = File::open(&path).map_err(|error| io(&path, error))?;
        object
            .seek(SeekFrom::Start(offset))
            .map_err(|error| io(&path, error))?;
        let mut bytes = vec![0_u8; count];
        object
            .read_exact(&mut bytes)
            .map_err(|error| io(&path, error))?;
        Ok(ArtifactPage {
            offset,
            eof: count as u64 == available,
            bytes,
        })
    }

    fn search_literal(
        &self,
        artifact_id: ArtifactId,
        query: &[u8],
        maximum_results: usize,
        context_bytes: usize,
    ) -> Result<Vec<ArtifactMatch>, ArtifactError> {
        validate_search_bounds(query, maximum_results, context_bytes)?;
        let path = self.object_path(artifact_id);
        let length = verify_existing_identity(&path, artifact_id)?;
        let mut file = File::open(&path).map_err(|error| io(&path, error))?;
        let mut matches = Vec::new();
        let mut carry = Vec::new();
        let mut consumed = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        while matches.len() < maximum_results {
            let count = file.read(&mut buffer).map_err(|error| io(&path, error))?;
            if count == 0 {
                break;
            }
            let chunk_start = consumed;
            consumed = consumed.saturating_add(count as u64);
            let carry_len = carry.len();
            let mut scan = carry;
            scan.extend_from_slice(&buffer[..count]);
            let base_offset = chunk_start.saturating_sub(carry_len as u64);
            let first_new_start = carry_len.saturating_sub(query.len().saturating_sub(1));
            let mut cursor = first_new_start;
            while cursor.saturating_add(query.len()) <= scan.len()
                && matches.len() < maximum_results
            {
                let Some(relative) = scan[cursor..]
                    .windows(query.len())
                    .position(|window| window == query)
                else {
                    break;
                };
                let start = cursor + relative;
                let offset = base_offset.saturating_add(start as u64);
                matches.push(ArtifactMatch {
                    offset,
                    context: read_search_context(
                        &path,
                        length,
                        offset,
                        query.len(),
                        context_bytes,
                    )?,
                });
                cursor = start.saturating_add(query.len());
            }
            let keep = query.len().saturating_sub(1).min(scan.len());
            carry = scan[scan.len() - keep..].to_vec();
        }
        Ok(matches)
    }

    fn inventory(&self) -> Result<Vec<ArtifactInventoryItem>, ArtifactError> {
        let object_root = self.root.join("blake3");
        let root_metadata =
            fs::symlink_metadata(&object_root).map_err(|error| io(&object_root, error))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(ArtifactError::UnsafePath {
                path: object_root.display().to_string(),
                message: "artifact object root must be a real directory".into(),
            });
        }
        let mut inventory = Vec::new();
        let buckets = fs::read_dir(&object_root).map_err(|error| io(&object_root, error))?;
        for bucket in buckets {
            let bucket = bucket.map_err(|error| io(&object_root, error))?;
            let bucket_path = bucket.path();
            let bucket_name = bucket.file_name().to_string_lossy().into_owned();
            if is_private_artifact_temporary_name(&bucket_name) {
                // A process can die after private temporary creation and
                // before publication. These names never participate in the
                // immutable object namespace, so inventory and GC must not
                // mistake one for a malformed digest bucket.
                continue;
            }
            let metadata =
                fs::symlink_metadata(&bucket_path).map_err(|error| io(&bucket_path, error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ArtifactError::UnsafePath {
                    path: bucket_path.display().to_string(),
                    message: "artifact bucket must be a real directory".into(),
                });
            }
            if bucket_name.len() != 2 || !bucket_name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(ArtifactError::UnsafePath {
                    path: bucket_path.display().to_string(),
                    message: "artifact bucket name must be two hexadecimal characters".into(),
                });
            }
            for entry in fs::read_dir(&bucket_path).map_err(|error| io(&bucket_path, error))? {
                let entry = entry.map_err(|error| io(&bucket_path, error))?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let artifact_id =
                    ArtifactId::from_hex(&name).map_err(|_| ArtifactError::UnsafePath {
                        path: path.display().to_string(),
                        message: "artifact filename must be a canonical BLAKE3 digest".into(),
                    })?;
                if artifact_id.to_hex()[..2] != bucket_name || self.object_path(artifact_id) != path
                {
                    return Err(ArtifactError::UnsafePath {
                        path: path.display().to_string(),
                        message: "artifact path does not match its content-addressed identity"
                            .into(),
                    });
                }
                let metadata = fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(ArtifactError::UnsafePath {
                        path: path.display().to_string(),
                        message: "artifact object must be a regular non-symlink file".into(),
                    });
                }
                inventory.push(ArtifactInventoryItem {
                    artifact_id,
                    byte_len: metadata.len(),
                });
            }
        }
        inventory.sort_by_key(|item| item.artifact_id);
        Ok(inventory)
    }

    fn remove(&self, artifact_id: ArtifactId) -> Result<(), ArtifactError> {
        let path = self.object_path(artifact_id);
        // Read and hash before the destructive operation. This makes a path
        // swap/corrupt object fail closed rather than deleting an arbitrary
        // neighbor selected through a forged filename.
        let _ = verify_existing_identity(&path, artifact_id)?;
        fs::remove_file(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ArtifactError::NotFound { artifact_id },
            _ => io(&path, error),
        })?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }
}

fn is_private_artifact_temporary_name(name: &str) -> bool {
    let Some(name) = name.strip_prefix('.') else {
        return false;
    };
    let Some(name) = name.strip_suffix(".artifact.tmp") else {
        return false;
    };
    let Some((process_id, nonce)) = name.split_once('-') else {
        return false;
    };
    !process_id.is_empty()
        && process_id.bytes().all(|byte| byte.is_ascii_digit())
        && nonce.len() == 16
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_media_type(media_type: &str) -> Result<(), ArtifactError> {
    if media_type.is_empty() || media_type.len() > 200 || media_type.chars().any(char::is_control) {
        return Err(ArtifactError::InvalidRequest {
            message: "artifact media type must be a bounded non-control string".into(),
        });
    }
    Ok(())
}

fn validate_bound(maximum_bytes: usize) -> Result<(), ArtifactError> {
    if maximum_bytes == 0 || maximum_bytes > MAX_PAGE_BYTES {
        return Err(ArtifactError::InvalidRequest {
            message: format!("artifact page bound must be within 1..={MAX_PAGE_BYTES} bytes"),
        });
    }
    Ok(())
}

fn validate_search_bounds(
    query: &[u8],
    maximum_results: usize,
    context_bytes: usize,
) -> Result<(), ArtifactError> {
    if query.is_empty() || query.len() > MAX_PAGE_BYTES {
        return Err(ArtifactError::InvalidRequest {
            message: "literal artifact query must contain 1..=1048576 bytes".into(),
        });
    }
    if maximum_results == 0 || maximum_results > 1_000 || context_bytes > MAX_PAGE_BYTES {
        return Err(ArtifactError::InvalidRequest {
            message: "artifact search bounds exceed the durable reader limits".into(),
        });
    }
    Ok(())
}

fn read_search_context(
    path: &Path,
    length: u64,
    offset: u64,
    query_len: usize,
    context_bytes: usize,
) -> Result<Vec<u8>, ArtifactError> {
    let start = offset.saturating_sub(context_bytes as u64);
    let end = offset
        .saturating_add(query_len as u64)
        .saturating_add(context_bytes as u64)
        .min(length);
    let count = end.saturating_sub(start) as usize;
    let mut file = File::open(path).map_err(|error| io(path, error))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| io(path, error))?;
    let mut context = vec![0_u8; count];
    file.read_exact(&mut context)
        .map_err(|error| io(path, error))?;
    Ok(context)
}

fn verify_existing_identity(path: &Path, artifact_id: ArtifactId) -> Result<u64, ArtifactError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ArtifactError::NotFound { artifact_id },
        _ => io(path, error),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArtifactError::UnsafePath {
            path: path.display().to_string(),
            message: "artifact object must be a regular non-symlink file".into(),
        });
    }
    let mut file = File::open(path).map_err(|error| io(path, error))?;
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| io(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        byte_len = byte_len.saturating_add(count as u64);
    }
    let observed = ArtifactId::from_hex(hasher.finalize().to_hex().as_str())
        .expect("BLAKE3 produces a canonical artifact digest");
    if observed != artifact_id {
        return Err(ArtifactError::Corruption {
            artifact_id,
            message: "existing object bytes do not match its content-addressed identity".into(),
        });
    }
    Ok(byte_len)
}

fn ensure_directory(path: &Path) -> Result<(), ArtifactError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(ArtifactError::UnsafePath {
            path: path.display().to_string(),
            message: "artifact directory must be a real directory, not a symlink".into(),
        });
    }
    fs::create_dir_all(path).map_err(|error| io(path, error))?;
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArtifactError::UnsafePath {
            path: path.display().to_string(),
            message: "artifact directory became a non-directory or symlink".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io(path, error))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    let directory = File::open(path).map_err(|error| io(path, error))?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        // Some supported filesystems do not allow syncing directories. The
        // object file itself was already synced; preserve that successful
        // durable prefix rather than pretending this platform supports more.
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

fn io(path: &Path, error: std::io::Error) -> ArtifactError {
    ArtifactError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}
