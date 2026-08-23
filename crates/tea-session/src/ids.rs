use blake3::Hasher;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// A content digest used by immutable Tea objects.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Hash exact bytes with BLAKE3.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        Self(*blake3::hash(bytes.as_ref()).as_bytes())
    }

    /// Construct a digest from a canonical lowercase or uppercase hex string.
    pub fn from_hex(value: &str) -> Result<Self, DigestError> {
        if value.len() != 64 {
            return Err(DigestError::WrongLength {
                actual: value.len(),
            });
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = hex_value(pair[0]).ok_or(DigestError::InvalidHex { index: index * 2 })?;
            let low = hex_value(pair[1]).ok_or(DigestError::InvalidHex {
                index: index * 2 + 1,
            })?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Return the canonical lowercase hexadecimal spelling.
    pub fn to_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
        }
        output
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Digest")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// A rejected digest spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DigestError {
    /// BLAKE3 digests are always 32 bytes / 64 hexadecimal characters.
    WrongLength { actual: usize },
    /// The indicated hexadecimal byte contains an invalid character.
    InvalidHex { index: usize },
}

impl fmt::Display for DigestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { actual } => {
                write!(
                    formatter,
                    "BLAKE3 digest must contain 64 hexadecimal characters, got {actual}"
                )
            }
            Self::InvalidHex { index } => {
                write!(formatter, "invalid hexadecimal character at index {index}")
            }
        }
    }
}

impl std::error::Error for DigestError {}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A canonical, domain-separated hash stream for durable Tea identities.
///
/// Callers write fields in their documented fixed order. Map callers must use
/// a sorted map or explicitly sort keys before writing them. The byte encoding
/// includes each field name and length so adjacent values cannot collide.
pub struct CanonicalHashWriter {
    hasher: Hasher,
}

impl CanonicalHashWriter {
    /// Start a schema-versioned, domain-separated BLAKE3 stream.
    pub fn new(domain: &str, schema_version: u16, abi_version: u16) -> Self {
        let mut hasher = Hasher::new();
        write_frame(&mut hasher, b"tea-canonical-hash-v1");
        write_frame(&mut hasher, domain.as_bytes());
        hasher.update(&schema_version.to_be_bytes());
        hasher.update(&abi_version.to_be_bytes());
        Self { hasher }
    }

    /// Append a named byte-string field.
    pub fn bytes(&mut self, name: &str, value: impl AsRef<[u8]>) {
        write_frame(&mut self.hasher, name.as_bytes());
        write_frame(&mut self.hasher, value.as_ref());
    }

    /// Append a named UTF-8 string field.
    pub fn string(&mut self, name: &str, value: &str) {
        self.bytes(name, value.as_bytes());
    }

    /// Append a named unsigned integer field in fixed-width big-endian form.
    pub fn u64(&mut self, name: &str, value: u64) {
        self.bytes(name, value.to_be_bytes());
    }

    /// Append a named boolean field.
    pub fn boolean(&mut self, name: &str, value: bool) {
        self.bytes(name, [u8::from(value)]);
    }

    /// Append an explicit variant discriminant rather than relying on an enum's
    /// implementation-defined memory layout.
    pub fn discriminant(&mut self, name: &str, value: u16) {
        self.bytes(name, value.to_be_bytes());
    }

    /// Append a portable normalized path field.
    pub fn normalized_path(&mut self, name: &str, value: &NormalizedPath) {
        self.string(name, value.as_str());
    }

    /// Append a map after sorting its string keys. Values are exact byte
    /// strings; callers encode nested values with their own fixed field order.
    pub fn sorted_byte_map<'a, I>(&mut self, name: &str, entries: I)
    where
        I: IntoIterator<Item = (&'a str, &'a [u8])>,
    {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
        self.u64(name, entries.len() as u64);
        for (key, value) in entries {
            self.string("map_key", key);
            self.bytes("map_value", value);
        }
    }

    /// Finalize the digest.
    pub fn finish(self) -> Digest {
        Digest(*self.hasher.finalize().as_bytes())
    }
}

fn write_frame(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// A monotonically increasing local ID generator for hosts that need a
/// non-content-addressed durable identity. It intentionally does not promise
/// global uniqueness: callers must keep the resulting value in durable state.
#[derive(Debug)]
pub struct IdGenerator {
    next: AtomicU64,
}

impl IdGenerator {
    /// Start generating at one.
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    /// Generate a portable opaque ID beneath a caller-owned kind prefix.
    pub fn next(&self, kind: &str) -> Result<String, IdError> {
        validate_id(kind)?;
        let sequence = self.next.fetch_add(1, Ordering::Relaxed);
        Ok(format!("{kind}-{sequence:016x}"))
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// A rejected opaque durable identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdError {
    /// Empty identifiers blur an absent field with an identity.
    Empty,
    /// Identifiers are deliberately bounded so they can safely appear in a
    /// filesystem-derived session layout and JSONL index.
    TooLong { actual: usize },
    /// A control character or path separator would make an opaque identity
    /// unsafe as a path component or log boundary.
    UnsafeCharacter { character: char },
}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("durable ID cannot be empty"),
            Self::TooLong { actual } => {
                write!(formatter, "durable ID exceeds 200 bytes ({actual})")
            }
            Self::UnsafeCharacter { character } => {
                write!(
                    formatter,
                    "durable ID contains unsafe character {character:?}"
                )
            }
        }
    }
}

impl std::error::Error for IdError {}

/// A canonical portable relative path used in immutable source-tree hashes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NormalizedPath(String);

impl NormalizedPath {
    /// Validate a conservative slash-separated portable relative path.
    pub fn new(value: impl Into<String>) -> Result<Self, NormalizedPathError> {
        let value = value.into();
        if value.is_empty() {
            return Err(NormalizedPathError::Empty);
        }
        if value.starts_with('/')
            || value.starts_with('\\')
            || value.as_bytes().get(1) == Some(&b':')
        {
            return Err(NormalizedPathError::Absolute { path: value });
        }
        if value.contains('\\') || value.bytes().any(|byte| byte == 0) {
            return Err(NormalizedPathError::UnsafeCharacter { path: value });
        }
        for segment in value.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(NormalizedPathError::TraversalOrEmpty { path: value });
            }
            if segment
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
            {
                return Err(NormalizedPathError::UnsafeCharacter { path: value });
            }
        }
        Ok(Self(value))
    }

    /// Borrow the normalized slash-separated spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NormalizedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A rejected source-tree path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizedPathError {
    /// The path has no component.
    Empty,
    /// Absolute and drive-qualified paths are never portable bundle paths.
    Absolute { path: String },
    /// Empty, `.` and `..` components cannot enter a canonical tree hash.
    TraversalOrEmpty { path: String },
    /// A separator, NUL, or nonportable character was supplied.
    UnsafeCharacter { path: String },
}

impl fmt::Display for NormalizedPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("normalized path cannot be empty"),
            Self::Absolute { path } => write!(formatter, "path is absolute: {path:?}"),
            Self::TraversalOrEmpty { path } => {
                write!(
                    formatter,
                    "path has an empty or traversal component: {path:?}"
                )
            }
            Self::UnsafeCharacter { path } => {
                write!(formatter, "path has an unsafe character: {path:?}")
            }
        }
    }
}

impl std::error::Error for NormalizedPathError {}

fn validate_id(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > 200 {
        return Err(IdError::TooLong {
            actual: value.len(),
        });
    }
    if let Some(character) = value
        .chars()
        .find(|character| character.is_control() || matches!(character, '/' | '\\' | ':' | '\0'))
    {
        return Err(IdError::UnsafeCharacter { character });
    }
    Ok(())
}

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Validate and construct an opaque durable identifier.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_id(&value)?;
                Ok(Self(value))
            }

            /// Borrow the stable textual identity.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume this opaque identity into its textual representation.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

opaque_id!(
    /// Identifies one durable session.
    SessionId
);
opaque_id!(
    /// Identifies a lane inside a durable session.
    LaneId
);
opaque_id!(
    /// Identifies an immutable semantic entry.
    EntryId
);
opaque_id!(
    /// Identifies a durable operation-log record.
    RecordId
);
opaque_id!(
    /// Identifies one caller-visible durable operation.
    OperationId
);
opaque_id!(
    /// Identifies one immutable core-run epoch.
    EpochId
);
opaque_id!(
    /// Identifies a retryable model or compaction step.
    StepId
);
opaque_id!(
    /// Identifies one physical provider request.
    ProviderRequestId
);
opaque_id!(
    /// Identifies a durable tool invocation.
    ToolInvocationId
);
opaque_id!(
    /// Identifies an immutable harness source tree.
    HarnessTreeId
);
opaque_id!(
    /// Identifies an immutable complete harness snapshot.
    HarnessSnapshotId
);
opaque_id!(
    /// Identifies an immutable harness revision transition.
    HarnessRevisionId
);
opaque_id!(
    /// Identifies an immutable staged harness candidate.
    HarnessCandidateId
);
opaque_id!(
    /// Identifies an immutable model-harness profile.
    ModelHarnessProfileId
);
opaque_id!(
    /// Identifies an immutable evolution experiment.
    ExperimentId
);
opaque_id!(
    /// Identifies one failure-signature cluster.
    FailureSignatureId
);
opaque_id!(
    /// Identifies a stable hook registration.
    StableHookId
);
opaque_id!(
    /// Identifies an immutable artifact policy.
    ArtifactPolicyId
);
opaque_id!(
    /// Identifies a Tea core run for durable provenance.
    CoreRunId
);

impl LaneId {
    /// The only lane exposed by the first complete durable slice.
    pub fn main() -> Self {
        Self("main".into())
    }
}

/// Identifies immutable artifact bytes by their exact BLAKE3 digest.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactId(Digest);

impl ArtifactId {
    /// Hash exact artifact bytes into their durable identity.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        Self(Digest::from_bytes(bytes))
    }

    /// Parse the canonical digest spelling.
    pub fn from_hex(value: &str) -> Result<Self, DigestError> {
        Digest::from_hex(value).map(Self)
    }

    /// Return the BLAKE3 content digest.
    pub fn digest(self) -> Digest {
        self.0
    }

    /// Return the canonical lowercase hexadecimal spelling.
    pub fn to_hex(self) -> String {
        self.0.to_hex()
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The one session-global ordering assigned inside a successful storage commit.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sequence(pub u64);
