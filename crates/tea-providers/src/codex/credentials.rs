//! Secret-bearing Codex credential types and explicit persistence boundary.

use crate::json::JsonValue;
use crate::scheduler::CancellationToken;
use rustix::fs::{CWD, FlockOperation, Mode, OFlags, flock, openat};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CREDENTIAL_VERSION: u64 = 1;
const MAX_TIMESTAMP_UNIX_MS: u64 = 4_102_444_800_000; // 2100-01-01T00:00:00Z
// A credential record has a fixed, very small v1 schema. Bound reads before
// parsing so a malformed local file cannot turn a login/status check into an
// unbounded allocation.
const MAX_CREDENTIAL_RECORD_BYTES: u64 = 64 * 1024;
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// A string that intentionally redacts itself in diagnostic formatting.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    /// Construct one nonempty secret string.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CredentialError::EmptySecret);
        }
        if value.chars().any(char::is_control) {
            return Err(CredentialError::UnsafeSecret);
        }
        Ok(Self(value))
    }

    /// Borrow the secret only at the immediate protocol boundary.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

/// One validated Tea-owned v1 Codex credential record.
#[derive(Clone, Eq, PartialEq)]
pub struct CodexCredential {
    access_token: SecretString,
    refresh_token: SecretString,
    expires_at_unix_ms: u64,
    account_id: String,
    obtained_at_unix_ms: u64,
}

impl CodexCredential {
    /// Construct one validated current credential snapshot.
    pub fn new(
        access_token: SecretString,
        refresh_token: SecretString,
        expires_at_unix_ms: u64,
        account_id: impl Into<String>,
        obtained_at_unix_ms: u64,
    ) -> Result<Self, CredentialError> {
        let account_id = account_id.into();
        if account_id.trim().is_empty() || account_id.chars().any(char::is_control) {
            return Err(CredentialError::InvalidAccountId);
        }
        if !(1..=MAX_TIMESTAMP_UNIX_MS).contains(&expires_at_unix_ms)
            || !(1..=MAX_TIMESTAMP_UNIX_MS).contains(&obtained_at_unix_ms)
        {
            return Err(CredentialError::InvalidTimestamp);
        }
        Ok(Self {
            access_token,
            refresh_token,
            expires_at_unix_ms,
            account_id,
            obtained_at_unix_ms,
        })
    }

    /// Borrow the bearer token at the immediate request boundary.
    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    /// Borrow the rotating refresh token at the immediate OAuth boundary.
    pub(crate) fn refresh_token(&self) -> &SecretString {
        &self.refresh_token
    }

    /// Absolute access-token expiry timestamp.
    pub fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }

    /// ChatGPT account identity required by the Codex backend.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Timestamp at which this credential snapshot was acquired.
    pub fn obtained_at_unix_ms(&self) -> u64 {
        self.obtained_at_unix_ms
    }

    /// Replace the access token while retaining the original refresh token when
    /// a conforming refresh response omits a new rotated value.
    pub(crate) fn refreshed(
        &self,
        access_token: SecretString,
        refresh_token: Option<SecretString>,
        expires_at_unix_ms: u64,
        account_id: String,
        obtained_at_unix_ms: u64,
    ) -> Result<Self, CredentialError> {
        Self::new(
            access_token,
            refresh_token.unwrap_or_else(|| self.refresh_token.clone()),
            expires_at_unix_ms,
            account_id,
            obtained_at_unix_ms,
        )
    }

    fn encode(&self) -> Result<Vec<u8>, CredentialError> {
        JsonValue::object([
            ("version", JsonValue::from(CREDENTIAL_VERSION)),
            ("provider", JsonValue::String("codex".into())),
            (
                "access_token",
                JsonValue::String(self.access_token.expose().to_owned()),
            ),
            (
                "refresh_token",
                JsonValue::String(self.refresh_token.expose().to_owned()),
            ),
            (
                "expires_at_unix_ms",
                JsonValue::from(self.expires_at_unix_ms),
            ),
            ("account_id", JsonValue::String(self.account_id.clone())),
            (
                "obtained_at_unix_ms",
                JsonValue::from(self.obtained_at_unix_ms),
            ),
        ])
        .to_json_string_pretty()
        .map(|text| text.into_bytes())
        .map_err(|_| CredentialError::MalformedRecord)
    }

    fn decode(bytes: &[u8]) -> Result<Self, CredentialError> {
        let text = std::str::from_utf8(bytes).map_err(|_| CredentialError::MalformedRecord)?;
        let value = JsonValue::parse(text).map_err(|_| CredentialError::MalformedRecord)?;
        let object = value.as_object().ok_or(CredentialError::MalformedRecord)?;
        let version = object
            .get("version")
            .and_then(JsonValue::as_u64)
            .ok_or(CredentialError::MalformedRecord)?;
        if version != CREDENTIAL_VERSION {
            return Err(CredentialError::UnsupportedVersion(version));
        }
        if object.get("provider").and_then(JsonValue::as_str) != Some("codex") {
            return Err(CredentialError::MalformedRecord);
        }
        let access_token = object
            .get("access_token")
            .and_then(JsonValue::as_str)
            .ok_or(CredentialError::MalformedRecord)?;
        let refresh_token = object
            .get("refresh_token")
            .and_then(JsonValue::as_str)
            .ok_or(CredentialError::MalformedRecord)?;
        let expires_at_unix_ms = object
            .get("expires_at_unix_ms")
            .and_then(JsonValue::as_u64)
            .ok_or(CredentialError::MalformedRecord)?;
        let account_id = object
            .get("account_id")
            .and_then(JsonValue::as_str)
            .ok_or(CredentialError::MalformedRecord)?;
        let obtained_at_unix_ms = object
            .get("obtained_at_unix_ms")
            .and_then(JsonValue::as_u64)
            .ok_or(CredentialError::MalformedRecord)?;
        Self::new(
            SecretString::new(access_token.to_owned())?,
            SecretString::new(refresh_token.to_owned())?,
            expires_at_unix_ms,
            account_id.to_owned(),
            obtained_at_unix_ms,
        )
    }
}

impl fmt::Debug for CodexCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexCredential")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .field("account_id", &abbreviate_account_id(&self.account_id))
            .field("obtained_at_unix_ms", &self.obtained_at_unix_ms)
            .finish()
    }
}

/// Credential persistence failure without secret material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialError {
    /// A required secret was empty.
    EmptySecret,
    /// A secret cannot safely become an HTTP header value.
    UnsafeSecret,
    /// The account identifier was empty or unsafe.
    InvalidAccountId,
    /// A stored timestamp was absent, zero, or implausible.
    InvalidTimestamp,
    /// The record did not match Tea's sole supported shape.
    MalformedRecord,
    /// The record declared another schema version.
    UnsupportedVersion(u64),
    /// No Tea-owned credential exists at the explicit path.
    NotFound,
    /// The configured credential path is unsafe.
    UnsafePath,
    /// A filesystem operation failed without retaining sensitive details.
    Io,
    /// Refresh serialization could not be acquired before its finite deadline.
    LockTimeout,
    /// The refresh wait was cancelled.
    Cancelled,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySecret => formatter.write_str("Codex credential contains an empty secret"),
            Self::UnsafeSecret => {
                formatter.write_str("Codex credential secret contains a line break")
            }
            Self::InvalidAccountId => formatter.write_str("Codex credential account ID is invalid"),
            Self::InvalidTimestamp => {
                formatter.write_str("Codex credential timestamps are invalid")
            }
            Self::MalformedRecord => formatter.write_str("Codex credential record is malformed"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "Codex credential version {version} is unsupported"
                )
            }
            Self::NotFound => formatter.write_str("Codex login is required"),
            Self::UnsafePath => formatter.write_str("Codex credential path is unsafe"),
            Self::Io => formatter.write_str("Codex credential storage failed"),
            Self::LockTimeout => formatter.write_str("Codex credential refresh lock timed out"),
            Self::Cancelled => formatter.write_str("Codex credential operation was cancelled"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Held cross-process credential-refresh serialization token.
pub trait CredentialRefreshLock: Send {}

/// Explicit credential persistence and refresh-lock boundary owned by the host.
pub trait CredentialStore: Send + Sync {
    /// Load the current record, if one exists.
    fn load(&self) -> Result<Option<CodexCredential>, CredentialError>;
    /// Atomically replace the current record with a fully validated snapshot.
    fn save(&self, credential: &CodexCredential) -> Result<(), CredentialError>;
    /// Remove the local Tea-owned record.
    fn remove(&self) -> Result<(), CredentialError>;
    /// Serialize a rotating-token refresh against other Tea processes.
    fn acquire_refresh_lock(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Box<dyn CredentialRefreshLock>, CredentialError>;
    /// Explicit path for status output, when persistence has one.
    fn path(&self) -> Option<&Path>;
}

/// File-backed Tea-owned credential store at an explicit host-selected path.
#[derive(Clone, Debug)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// Bind a store to one explicit `auth/codex.json` path. No home-directory
    /// lookup or ambient Codex credential reuse occurs here.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Borrow the explicit credential path.
    pub fn credential_path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    fn ensure_parent(&self) -> Result<&Path, CredentialError> {
        let parent = self.path.parent().ok_or(CredentialError::UnsafePath)?;
        fs::create_dir_all(parent).map_err(|_| CredentialError::Io)?;
        let metadata = fs::symlink_metadata(parent).map_err(|_| CredentialError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CredentialError::UnsafePath);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|_| CredentialError::Io)?;
        }
        Ok(parent)
    }

    fn ensure_regular_target(&self) -> Result<bool, CredentialError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    Err(CredentialError::UnsafePath)
                } else {
                    Ok(true)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(CredentialError::Io),
        }
    }

    fn temporary_path(&self) -> Result<PathBuf, CredentialError> {
        let parent = self.path.parent().ok_or(CredentialError::UnsafePath)?;
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| CredentialError::Io)?;
        let suffix = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(parent.join(format!(".codex-{suffix}.tmp")))
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self) -> Result<Option<CodexCredential>, CredentialError> {
        if !self.ensure_regular_target()? {
            return Ok(None);
        }
        // O_NOFOLLOW closes the check-to-open race on the supported Unix
        // targets. The explicit metadata check above gives a clear error on
        // platforms that cannot make the same kernel guarantee.
        let file = openat(
            CWD,
            &self.path,
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| CredentialError::UnsafePath)?;
        let mut file = File::from(file);
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_CREDENTIAL_RECORD_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| CredentialError::Io)?;
        if bytes.len() as u64 > MAX_CREDENTIAL_RECORD_BYTES {
            return Err(CredentialError::MalformedRecord);
        }
        CodexCredential::decode(&bytes).map(Some)
    }

    fn save(&self, credential: &CodexCredential) -> Result<(), CredentialError> {
        let parent = self.ensure_parent()?;
        let _ = self.ensure_regular_target()?;
        let payload = credential.encode()?;
        let temporary = self.temporary_path()?;
        let result = (|| {
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).map_err(|_| CredentialError::Io)?;
            file.write_all(&payload).map_err(|_| CredentialError::Io)?;
            file.sync_all().map_err(|_| CredentialError::Io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                    .map_err(|_| CredentialError::Io)?;
            }
            fs::rename(&temporary, &self.path).map_err(|_| CredentialError::Io)?;
            // `rename` is the atomic externally visible commit. The file was
            // already flushed before it, so an advisory directory-sync failure
            // cannot safely be reported as a failed replacement: callers would
            // otherwise believe the previous valid record still exists.
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn remove(&self) -> Result<(), CredentialError> {
        match self.ensure_regular_target()? {
            false => Ok(()),
            true => fs::remove_file(&self.path).map_err(|_| CredentialError::Io),
        }
    }

    fn acquire_refresh_lock(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Box<dyn CredentialRefreshLock>, CredentialError> {
        self.ensure_parent()?;
        let path = self.lock_path();
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(CredentialError::UnsafePath);
        }
        // As with the credential record itself, retain the metadata check for
        // a clear error and use O_NOFOLLOW to close the check-to-open race on
        // supported Unix targets. The lock is Tea-owned and contains no
        // secret material, but following a hostile link would still violate
        // the explicit credential boundary.
        let file = openat(
            CWD,
            &path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| CredentialError::UnsafePath)?;
        let file = File::from(file);
        let started = Instant::now();
        loop {
            if cancellation.is_cancelled() {
                return Err(CredentialError::Cancelled);
            }
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Box::new(FileRefreshLock(file))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= LOCK_WAIT_TIMEOUT {
                        return Err(CredentialError::LockTimeout);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return Err(CredentialError::Io),
            }
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

struct FileRefreshLock(File);

impl CredentialRefreshLock for FileRefreshLock {}

impl Drop for FileRefreshLock {
    fn drop(&mut self) {
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

/// In-memory credential store for deterministic adapter and OAuth tests.
#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    credential: Arc<Mutex<Option<CodexCredential>>>,
}

impl InMemoryCredentialStore {
    /// Construct one empty explicit in-memory credential store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed deterministic test credentials.
    pub fn with_credential(credential: CodexCredential) -> Self {
        Self {
            credential: Arc::new(Mutex::new(Some(credential))),
        }
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn load(&self) -> Result<Option<CodexCredential>, CredentialError> {
        Ok(self
            .credential
            .lock()
            .map_err(|_| CredentialError::Io)?
            .clone())
    }

    fn save(&self, credential: &CodexCredential) -> Result<(), CredentialError> {
        *self.credential.lock().map_err(|_| CredentialError::Io)? = Some(credential.clone());
        Ok(())
    }

    fn remove(&self) -> Result<(), CredentialError> {
        *self.credential.lock().map_err(|_| CredentialError::Io)? = None;
        Ok(())
    }

    fn acquire_refresh_lock(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Box<dyn CredentialRefreshLock>, CredentialError> {
        if cancellation.is_cancelled() {
            return Err(CredentialError::Cancelled);
        }
        Ok(Box::new(NoopRefreshLock))
    }

    fn path(&self) -> Option<&Path> {
        None
    }
}

struct NoopRefreshLock;
impl CredentialRefreshLock for NoopRefreshLock {}

/// Render a safely abbreviated account identity for a terminal status line.
pub fn abbreviate_account_id(account_id: &str) -> String {
    let characters = account_id.chars().collect::<Vec<_>>();
    if characters.len() <= 8 {
        return "[redacted]".into();
    }
    let prefix = characters[..4].iter().collect::<String>();
    let suffix = characters[characters.len() - 4..]
        .iter()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tea-codex-credential-test-{}-{sequence}",
                std::process::id(),
            ));
            fs::create_dir(&path).expect("credential test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn credential(access: &str, refresh: &str) -> CodexCredential {
        CodexCredential::new(
            SecretString::new(access).expect("fixture access token"),
            SecretString::new(refresh).expect("fixture refresh token"),
            2_000_000_000_000,
            "acct_12345678",
            1_900_000_000_000,
        )
        .expect("fixture credential")
    }

    #[test]
    fn v1_round_trip_is_validated_and_secret_debug_is_redacted() {
        let credential = credential("access-secret", "refresh-secret");
        let decoded = CodexCredential::decode(&credential.encode().expect("encode"))
            .expect("decode v1 credential");
        assert_eq!(decoded, credential);
        let diagnostic = format!("{decoded:?}");
        assert!(!diagnostic.contains("access-secret"));
        assert!(!diagnostic.contains("refresh-secret"));
        assert!(!diagnostic.contains("acct_12345678"));
    }

    #[test]
    fn rejects_an_unknown_schema_before_any_secret_is_accepted() {
        let error = CodexCredential::decode(
            br#"{"version":2,"provider":"codex","access_token":"x","refresh_token":"y","expires_at_unix_ms":1,"account_id":"acct","obtained_at_unix_ms":1}"#,
        )
        .expect_err("unsupported credential version must be rejected");
        assert_eq!(error, CredentialError::UnsupportedVersion(2));
    }

    #[test]
    fn credential_values_reject_every_http_control_character() {
        assert_eq!(
            SecretString::new("access\tsecret"),
            Err(CredentialError::UnsafeSecret),
        );
        assert_eq!(
            CodexCredential::new(
                SecretString::new("access").expect("safe access fixture"),
                SecretString::new("refresh").expect("safe refresh fixture"),
                2_000_000_000_000,
                "acct\u{0000}unsafe",
                1_900_000_000_000,
            ),
            Err(CredentialError::InvalidAccountId),
        );
    }

    #[test]
    fn file_store_replaces_only_with_a_complete_v1_record() {
        let directory = TestDirectory::new();
        let path = directory.path().join("auth").join("codex.json");
        let store = FileCredentialStore::new(&path);
        let first = credential("first-access", "first-refresh");
        let second = credential("second-access", "second-refresh");

        store.save(&first).expect("first credential write");
        store.save(&second).expect("atomic replacement write");

        assert_eq!(store.load().expect("load replacement"), Some(second));
        let record = fs::read_to_string(path).expect("read persisted v1 record");
        assert!(!record.contains("first-access"));
        assert!(!record.contains("first-refresh"));
    }

    #[test]
    fn oversized_credential_file_is_rejected_without_unbounded_read() {
        let directory = TestDirectory::new();
        let path = directory.path().join("auth").join("codex.json");
        let store = FileCredentialStore::new(&path);
        store
            .save(&credential("access", "refresh"))
            .expect("seed valid credential");
        fs::write(&path, vec![b'x'; MAX_CREDENTIAL_RECORD_BYTES as usize + 1])
            .expect("replace fixture with oversized content");

        assert_eq!(store.load(), Err(CredentialError::MalformedRecord));
    }

    #[cfg(unix)]
    #[test]
    fn file_store_sets_private_modes_and_never_follows_credential_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = TestDirectory::new();
        let path = directory.path().join("auth").join("codex.json");
        let store = FileCredentialStore::new(&path);
        store
            .save(&credential("access", "refresh"))
            .expect("write private credential");

        let parent_mode = fs::metadata(path.parent().expect("auth parent"))
            .expect("auth metadata")
            .permissions()
            .mode()
            & 0o777;
        let record_mode = fs::metadata(&path)
            .expect("record metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o700);
        assert_eq!(record_mode, 0o600);

        let outside = directory.path().join("outside.json");
        fs::write(&outside, b"outside remains intact").expect("outside fixture");
        fs::remove_file(&path).expect("replace fixture path with symlink");
        symlink(&outside, &path).expect("credential symlink fixture");

        assert_eq!(store.load(), Err(CredentialError::UnsafePath));
        assert_eq!(
            store.save(&credential("other-access", "other-refresh")),
            Err(CredentialError::UnsafePath),
        );
        assert_eq!(
            fs::read_to_string(outside).expect("outside content remains readable"),
            "outside remains intact",
        );
    }
}
