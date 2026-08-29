//! Compile-time version presentation for the CLI control path.

/// Return the package version and validated seven-character build revision.
pub fn line() -> String {
    crate::build_info::version_line()
}
