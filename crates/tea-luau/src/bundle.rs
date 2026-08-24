//! Closed, deterministic Luau policy bundles.
//!
//! This module deliberately contains no filesystem or manifest parser. A host
//! reads a bundle from its own storage, converts the validated records into
//! [`Bundle::from_sources`], and then passes the resulting value into the
//! policy VM. Keeping loading separate from the bundle contract means that a
//! caller cannot accidentally reintroduce cwd, `HOME`, package-registry, or
//! network lookup as a module-resolution fallback.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// The original closed-bundle ABI.
///
/// A bundle declares deterministic named prompt sections, bounded lifecycle
/// callbacks, typed context proposals, and the `before_tool` decision hook.
/// Older append-style declarations are deliberately not accepted.
pub const BUNDLE_ABI_VERSION: u32 = 1;

/// The closed-bundle ABI that adds host commands and idle callbacks.
///
/// Version 2 intentionally leaves the v1 declaration shape unchanged. A v1
/// bundle cannot opt into the new host-facing fields by accident.
pub const BUNDLE_ABI_V2_VERSION: u32 = 2;

/// Whether this build understands a closed-bundle ABI version.
pub const fn supports_bundle_abi(abi_version: u32) -> bool {
    matches!(abi_version, BUNDLE_ABI_VERSION | BUNDLE_ABI_V2_VERSION)
}

/// A canonical, bundle-local module path.
///
/// Paths use `/` separators and never contain `.` or `..` segments. This is
/// intentionally stricter than a host filesystem path: canonical paths make
/// resolution and bundle hashes independent of the host operating system.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModulePath(String);

impl ModulePath {
    /// Validate and construct a canonical bundle-local path.
    pub fn new(path: impl AsRef<str>) -> Result<Self, ModulePathError> {
        let path = path.as_ref();
        if path.is_empty() {
            return Err(ModulePathError::Empty);
        }
        if path.starts_with('/') || path.ends_with('/') {
            return Err(ModulePathError::AbsoluteOrTrailingSeparator {
                path: path.to_owned(),
            });
        }
        if path.contains('\\') {
            return Err(ModulePathError::Backslash {
                path: path.to_owned(),
            });
        }
        if path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(ModulePathError::ControlCharacter {
                path: path.to_owned(),
            });
        }

        for segment in path.split('/') {
            if segment.is_empty() {
                return Err(ModulePathError::EmptySegment {
                    path: path.to_owned(),
                });
            }
            if segment == "." || segment == ".." {
                return Err(ModulePathError::Traversal {
                    path: path.to_owned(),
                });
            }
        }

        // A drive prefix is not a valid bundle path even on Unix, where it
        // would otherwise look like an ordinary filename.
        if path.as_bytes().get(1) == Some(&b':') {
            return Err(ModulePathError::DrivePrefix {
                path: path.to_owned(),
            });
        }

        Ok(Self(path.to_owned()))
    }

    /// Return the canonical slash-separated path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the parent directory, if this is not a root-level module.
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    fn from_segments(segments: &[&str]) -> Result<Self, ModulePathError> {
        Self::new(segments.join("/"))
    }
}

impl AsRef<str> for ModulePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A rejected bundle-local path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModulePathError {
    /// The path contains no module name.
    Empty,
    /// The path begins with `/` or ends with `/`.
    AbsoluteOrTrailingSeparator {
        /// The rejected path.
        path: String,
    },
    /// Backslashes are rejected rather than interpreted according to the host OS.
    Backslash {
        /// The rejected path.
        path: String,
    },
    /// The path contains two adjacent separators.
    EmptySegment {
        /// The rejected path.
        path: String,
    },
    /// The path contains a NUL or another ASCII control character.
    ControlCharacter {
        /// The rejected path.
        path: String,
    },
    /// The path contains `.` or `..`.
    Traversal {
        /// The rejected path.
        path: String,
    },
    /// The path resembles a Windows drive path.
    DrivePrefix {
        /// The rejected path.
        path: String,
    },
}

impl fmt::Display for ModulePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("module path is empty"),
            Self::AbsoluteOrTrailingSeparator { path } => {
                write!(
                    formatter,
                    "module path is absolute or has a trailing separator: {path:?}"
                )
            }
            Self::Backslash { path } => {
                write!(formatter, "module path contains a backslash: {path:?}")
            }
            Self::EmptySegment { path } => {
                write!(formatter, "module path contains an empty segment: {path:?}")
            }
            Self::ControlCharacter { path } => {
                write!(
                    formatter,
                    "module path contains a control character: {path:?}"
                )
            }
            Self::Traversal { path } => {
                write!(
                    formatter,
                    "module path contains a traversal segment: {path:?}"
                )
            }
            Self::DrivePrefix { path } => {
                write!(formatter, "module path has a drive prefix: {path:?}")
            }
        }
    }
}

impl Error for ModulePathError {}

/// A validated capability name from a bundle manifest.
///
/// Names are opaque to this crate but use a stable ASCII form so capability
/// comparisons are deterministic. Hosts decide which names are meaningful.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityName(String);

impl CapabilityName {
    /// Validate and construct a capability name such as `world.mcp.call`.
    pub fn new(name: impl AsRef<str>) -> Result<Self, CapabilityNameError> {
        let name = name.as_ref();
        if name.is_empty() {
            return Err(CapabilityNameError::Empty);
        }
        if name.bytes().any(|byte| !byte.is_ascii()) {
            return Err(CapabilityNameError::NonAscii {
                name: name.to_owned(),
            });
        }
        if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
            return Err(CapabilityNameError::EmptySegment {
                name: name.to_owned(),
            });
        }
        if name.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.')
        }) {
            return Err(CapabilityNameError::InvalidCharacter {
                name: name.to_owned(),
            });
        }
        Ok(Self(name.to_owned()))
    }

    /// Return the manifest's canonical capability spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CapabilityName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CapabilityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A rejected manifest capability name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityNameError {
    /// The name contains no capability.
    Empty,
    /// Capability names are deliberately ASCII-only.
    NonAscii {
        /// The rejected name.
        name: String,
    },
    /// The name contains an empty `.`-separated segment.
    EmptySegment {
        /// The rejected name.
        name: String,
    },
    /// The name contains a character outside `[A-Za-z0-9_.-]`.
    InvalidCharacter {
        /// The rejected name.
        name: String,
    },
}

impl fmt::Display for CapabilityNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("capability name is empty"),
            Self::NonAscii { name } => write!(formatter, "capability name is not ASCII: {name:?}"),
            Self::EmptySegment { name } => {
                write!(formatter, "capability name has an empty segment: {name:?}")
            }
            Self::InvalidCharacter { name } => {
                write!(
                    formatter,
                    "capability name contains an invalid character: {name:?}"
                )
            }
        }
    }
}

impl Error for CapabilityNameError {}

/// The validated, serializable contract that describes one bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleManifest {
    abi_version: u32,
    entrypoint: ModulePath,
    capabilities: Vec<CapabilityName>,
}

impl BundleManifest {
    /// Build a manifest and reject unsupported ABI versions or malformed names.
    ///
    /// Capabilities are sorted and deduplicated, making their order independent
    /// of the source manifest representation and the bundle hash stable.
    pub fn new<I, C>(
        abi_version: u32,
        entrypoint: impl AsRef<str>,
        capabilities: I,
    ) -> Result<Self, ManifestError>
    where
        I: IntoIterator<Item = C>,
        C: AsRef<str>,
    {
        if !supports_bundle_abi(abi_version) {
            return Err(ManifestError::UnsupportedAbiVersion {
                expected: BUNDLE_ABI_V2_VERSION,
                actual: abi_version,
            });
        }
        let entrypoint = ModulePath::new(entrypoint).map_err(ManifestError::InvalidEntrypoint)?;
        let mut parsed = capabilities
            .into_iter()
            .map(|capability| {
                CapabilityName::new(capability).map_err(ManifestError::InvalidCapability)
            })
            .collect::<Result<Vec<_>, _>>()?;
        parsed.sort_unstable();
        for pair in parsed.windows(2) {
            if pair[0] == pair[1] {
                return Err(ManifestError::DuplicateCapability {
                    capability: pair[0].clone(),
                });
            }
        }
        Ok(Self {
            abi_version,
            entrypoint,
            capabilities: parsed,
        })
    }

    /// Return the bundle ABI version.
    pub fn abi_version(&self) -> u32 {
        self.abi_version
    }

    /// Return the entrypoint module path.
    pub fn entrypoint(&self) -> &ModulePath {
        &self.entrypoint
    }

    /// Return sorted, unique capability names.
    pub fn capabilities(&self) -> &[CapabilityName] {
        &self.capabilities
    }
}

/// A rejected bundle manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    /// The host asked for an ABI this crate does not implement.
    UnsupportedAbiVersion {
        /// ABI version understood by this crate.
        expected: u32,
        /// ABI version supplied by the host.
        actual: u32,
    },
    /// The entrypoint is not a safe bundle-local module path.
    InvalidEntrypoint(ModulePathError),
    /// A capability name is malformed.
    InvalidCapability(CapabilityNameError),
    /// Duplicate capability declarations are rejected instead of silently changing intent.
    DuplicateCapability {
        /// The capability that appeared more than once.
        capability: CapabilityName,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedAbiVersion {
                expected: _,
                actual,
            } => {
                write!(
                    formatter,
                    "unsupported bundle ABI version {actual}; supported versions are {BUNDLE_ABI_VERSION} and {BUNDLE_ABI_V2_VERSION}"
                )
            }
            Self::InvalidEntrypoint(error) => {
                write!(formatter, "invalid bundle entrypoint: {error}")
            }
            Self::InvalidCapability(error) => {
                write!(formatter, "invalid bundle capability: {error}")
            }
            Self::DuplicateCapability { capability } => {
                write!(formatter, "duplicate bundle capability: {capability}")
            }
        }
    }
}

impl Error for ManifestError {}

/// A closed bundle of manifest metadata and source modules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bundle {
    manifest: BundleManifest,
    modules: BTreeMap<ModulePath, String>,
    source_hash: u64,
}

impl Bundle {
    /// Build a bundle from already-canonical module paths and source strings.
    pub fn new(
        manifest: BundleManifest,
        modules: BTreeMap<ModulePath, String>,
    ) -> Result<Self, BundleError> {
        if !modules.contains_key(manifest.entrypoint()) {
            return Err(BundleError::MissingEntrypoint {
                entrypoint: manifest.entrypoint().clone(),
            });
        }
        if modules.keys().any(|path| path.as_str().is_empty()) {
            return Err(BundleError::InvalidModulePath {
                error: ModulePathError::Empty,
            });
        }
        let source_hash = calculate_hash(&manifest, &modules);
        Ok(Self {
            manifest,
            modules,
            source_hash,
        })
    }

    /// Build a bundle from `(path, source)` records, validating every path.
    pub fn from_sources<I, P, S>(manifest: BundleManifest, sources: I) -> Result<Self, BundleError>
    where
        I: IntoIterator<Item = (P, S)>,
        P: AsRef<str>,
        S: Into<String>,
    {
        let mut modules = BTreeMap::new();
        for (path, source) in sources {
            let path =
                ModulePath::new(path).map_err(|error| BundleError::InvalidModulePath { error })?;
            if modules.insert(path.clone(), source.into()).is_some() {
                return Err(BundleError::DuplicateModule { path });
            }
        }
        Self::new(manifest, modules)
    }

    /// Return the validated manifest.
    pub fn manifest(&self) -> &BundleManifest {
        &self.manifest
    }

    /// Return all modules in canonical path order.
    pub fn modules(&self) -> &BTreeMap<ModulePath, String> {
        &self.modules
    }

    /// Return source for a canonical module, if the module is in this bundle.
    pub fn module(&self, path: &ModulePath) -> Option<&str> {
        self.modules.get(path).map(String::as_str)
    }

    /// Resolve a root-relative module request inside this bundle.
    pub fn resolve(&self, requested: &str) -> Result<ResolvedModule<'_>, ResolveError> {
        let path = ModulePath::new(requested).map_err(ResolveError::InvalidPath)?;
        self.resolve_path(path)
    }

    /// Resolve a `./` or `../` import relative to a loaded module.
    ///
    /// Imports without a relative prefix are interpreted as bundle-root paths;
    /// they still cannot escape the bundle or access a host filesystem path.
    pub fn resolve_relative(
        &self,
        from: &ModulePath,
        requested: &str,
    ) -> Result<ResolvedModule<'_>, ResolveError> {
        if !self.modules.contains_key(from) {
            return Err(ResolveError::RequesterNotFound { path: from.clone() });
        }
        if requested.starts_with('/') || requested.starts_with('\\') {
            return Err(ResolveError::InvalidPath(
                ModulePathError::AbsoluteOrTrailingSeparator {
                    path: requested.to_owned(),
                },
            ));
        }

        let is_relative = requested == "."
            || requested == ".."
            || requested.starts_with("./")
            || requested.starts_with("../");
        let mut segments = if is_relative {
            from.parent()
                .map(|parent| parent.split('/').collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for segment in requested.split('/') {
            match segment {
                "" => {
                    return Err(ResolveError::InvalidPath(ModulePathError::EmptySegment {
                        path: requested.to_owned(),
                    }))
                }
                "." => {}
                ".." => {
                    if !is_relative {
                        return Err(ResolveError::InvalidPath(ModulePathError::Traversal {
                            path: requested.to_owned(),
                        }));
                    }
                    if segments.pop().is_none() {
                        return Err(ResolveError::EscapesBundle {
                            from: from.clone(),
                            requested: requested.to_owned(),
                        });
                    }
                }
                segment => segments.push(segment),
            }
        }
        let path = ModulePath::from_segments(&segments).map_err(ResolveError::InvalidPath)?;
        self.resolve_path(path)
    }

    fn resolve_path(&self, path: ModulePath) -> Result<ResolvedModule<'_>, ResolveError> {
        let Some(source) = self.modules.get(&path) else {
            return Err(ResolveError::NotFound { path });
        };
        Ok(ResolvedModule { path, source })
    }

    /// Return the deterministic source/dependency identity of this bundle.
    ///
    /// This is an FNV-1a 64-bit content identity, not a cryptographic digest.
    /// The hash includes the ABI, entrypoint, sorted capabilities, every
    /// canonical module path, and length-prefixed UTF-8 source bytes.
    pub fn source_hash(&self) -> u64 {
        self.source_hash
    }

    /// Return [`source_hash`](Self::source_hash) in a stable hexadecimal form.
    pub fn source_hash_hex(&self) -> String {
        format!("{:016x}", self.source_hash)
    }
}

/// A resolved bundle module borrowed from its bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModule<'a> {
    /// Canonical module path.
    pub path: ModulePath,
    /// UTF-8 Luau source.
    pub source: &'a str,
}

/// A bundle construction failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BundleError {
    /// A source record used an unsafe path.
    InvalidModulePath {
        /// The path validation failure.
        error: ModulePathError,
    },
    /// Two source records named the same canonical module.
    DuplicateModule {
        /// The module path that appeared more than once.
        path: ModulePath,
    },
    /// The manifest entrypoint has no source record.
    MissingEntrypoint {
        /// Manifest entrypoint absent from the source set.
        entrypoint: ModulePath,
    },
}

impl fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidModulePath { error } => {
                write!(formatter, "invalid bundle module path: {error}")
            }
            Self::DuplicateModule { path } => write!(formatter, "duplicate bundle module: {path}"),
            Self::MissingEntrypoint { entrypoint } => {
                write!(formatter, "bundle entrypoint is not present: {entrypoint}")
            }
        }
    }
}

impl Error for BundleError {}

/// A module lookup or import-resolution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    /// The requested path is not safe.
    InvalidPath(ModulePathError),
    /// The import would leave the bundle root.
    EscapesBundle {
        /// Module attempting the request.
        from: ModulePath,
        /// Request that would escape the bundle root.
        requested: String,
    },
    /// The importing module is not part of this bundle.
    RequesterNotFound {
        /// Requester absent from the bundle.
        path: ModulePath,
    },
    /// The path is safe but absent from this bundle.
    NotFound {
        /// Safe canonical path absent from the bundle.
        path: ModulePath,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(error) => write!(formatter, "invalid module request: {error}"),
            Self::EscapesBundle { from, requested } => {
                write!(
                    formatter,
                    "module request {requested:?} from {from} escapes the bundle"
                )
            }
            Self::RequesterNotFound { path } => {
                write!(formatter, "requesting module is not in the bundle: {path}")
            }
            Self::NotFound { path } => write!(formatter, "module is not in the bundle: {path}"),
        }
    }
}

impl Error for ResolveError {}

fn calculate_hash(manifest: &BundleManifest, modules: &BTreeMap<ModulePath, String>) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    hash_field(&mut hash, &manifest.abi_version.to_le_bytes());
    hash_field(&mut hash, manifest.entrypoint.as_str().as_bytes());
    for capability in &manifest.capabilities {
        hash_field(&mut hash, capability.as_str().as_bytes());
    }
    for (path, source) in modules {
        hash_field(&mut hash, path.as_str().as_bytes());
        hash_field(&mut hash, source.as_bytes());
    }
    hash
}

fn hash_field(hash: &mut u64, bytes: &[u8]) {
    // Length-prefix every field so concatenation cannot create an equivalent
    // identity (for example, `ab` + `c` versus `a` + `bc`).
    for byte in (bytes.len() as u64)
        .to_le_bytes()
        .into_iter()
        .chain(bytes.iter().copied())
    {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(capabilities: &[&str]) -> BundleManifest {
        BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", capabilities).unwrap()
    }

    fn bundle() -> Bundle {
        Bundle::from_sources(
            manifest(&["world.mcp.call", "trace.emit"]),
            [
                ("main.luau", "return require('./lib/tool.luau')"),
                ("lib/tool.luau", "return { name = 'tool' }"),
            ],
        )
        .unwrap()
    }

    #[test]
    fn module_paths_reject_traversal_and_absolute_forms() {
        for path in [
            "../main.luau",
            "a/../../main.luau",
            "/main.luau",
            "C:/main.luau",
            "a\\b.luau",
        ] {
            assert!(
                ModulePath::new(path).is_err(),
                "accepted unsafe path {path:?}"
            );
        }
        assert_eq!(ModulePath::new("a/b.luau").unwrap().parent(), Some("a"));
    }

    #[test]
    fn manifest_rejects_bad_abi_entrypoint_and_capabilities() {
        assert!(matches!(
            BundleManifest::new(99, "main.luau", std::iter::empty::<&str>()),
            Err(ManifestError::UnsupportedAbiVersion { .. })
        ));
        assert!(matches!(
            BundleManifest::new(
                BUNDLE_ABI_VERSION,
                "../main.luau",
                std::iter::empty::<&str>()
            ),
            Err(ManifestError::InvalidEntrypoint(_))
        ));
        assert!(matches!(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", ["world..exec"]),
            Err(ManifestError::InvalidCapability(_))
        ));
        assert!(matches!(
            BundleManifest::new(
                BUNDLE_ABI_VERSION,
                "main.luau",
                ["world.exec", "world.exec"]
            ),
            Err(ManifestError::DuplicateCapability { .. })
        ));
        assert!(matches!(
            Bundle::from_sources(manifest(&[]), [("../escape.luau", "return nil")]),
            Err(BundleError::InvalidModulePath { .. })
        ));
    }

    #[test]
    fn bundle_requires_entrypoint_and_resolves_only_known_modules() {
        assert!(matches!(
            Bundle::from_sources(manifest(&[]), [("other.luau", "return nil")]),
            Err(BundleError::MissingEntrypoint { .. })
        ));
        let bundle = bundle();
        let resolved = bundle.resolve("lib/tool.luau").unwrap();
        assert_eq!(resolved.source, "return { name = 'tool' }");
        assert_eq!(
            bundle
                .resolve_relative(&ModulePath::new("main.luau").unwrap(), "./lib/tool.luau")
                .unwrap()
                .path
                .as_str(),
            "lib/tool.luau"
        );
        assert!(matches!(
            bundle.resolve_relative(&ModulePath::new("main.luau").unwrap(), "../../escape.luau"),
            Err(ResolveError::EscapesBundle { .. })
        ));
        assert!(matches!(
            bundle.resolve("missing.luau"),
            Err(ResolveError::NotFound { .. })
        ));
        assert!(matches!(
            bundle.resolve_relative(&ModulePath::new("main.luau").unwrap(), "lib/../main.luau"),
            Err(ResolveError::InvalidPath(ModulePathError::Traversal { .. }))
        ));
    }

    #[test]
    fn source_hash_is_deterministic_and_covers_manifest_and_sources() {
        let first = Bundle::from_sources(
            manifest(&["trace.emit", "world.mcp.call"]),
            [("main.luau", "return 1"), ("lib.luau", "return 2")],
        )
        .unwrap();
        let second = Bundle::from_sources(
            manifest(&["world.mcp.call", "trace.emit"]),
            [("lib.luau", "return 2"), ("main.luau", "return 1")],
        )
        .unwrap();
        assert_eq!(first.source_hash(), second.source_hash());
        assert_eq!(first.source_hash_hex(), second.source_hash_hex());

        let changed = Bundle::from_sources(
            manifest(&["trace.emit", "world.mcp.call"]),
            [("main.luau", "return 9"), ("lib.luau", "return 2")],
        )
        .unwrap();
        assert_ne!(first.source_hash(), changed.source_hash());
    }
}
