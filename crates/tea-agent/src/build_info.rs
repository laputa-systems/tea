//! Compile-time identity for the terminal host binary and its durable sessions.

/// Published package version from the crate manifest.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Seven-character Git revision captured by `build.rs`.
pub const GIT_SHA: &str = env!("TEA_BUILD_GIT_SHA");
/// Immutable session-header key for the package version that created a session.
pub const SESSION_VERSION_METADATA_KEY: &str = "tea.build.version";
/// Immutable session-header key for the Git revision that created a session.
pub const SESSION_GIT_SHA_METADATA_KEY: &str = "tea.build.git_sha";

/// Render the human-readable binary identity used by `tea --version`.
pub fn version_line() -> String {
    format!("tea {PACKAGE_VERSION} (git {GIT_SHA})")
}
