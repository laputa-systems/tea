//! Workspace path authority for standard coding tools.

use super::contract::OperationError;
use std::path::{Path, PathBuf};

/// An explicit, canonicalized workspace authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Canonicalize an existing directory as a workspace root.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, OperationError> {
        let root = std::fs::canonicalize(path.as_ref()).map_err(|error| {
            OperationError::new(format!("workspace root is not accessible: {error}"))
        })?;
        if !root.is_dir() {
            return Err(OperationError::new("workspace root is not a directory"));
        }
        Ok(Self(root))
    }

    /// Borrow the canonical root path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Resolve an existing path, rejecting absolute paths outside the root and
    /// symlinks that resolve outside it.
    pub fn resolve_existing(&self, input: &str) -> Result<PathBuf, OperationError> {
        let candidate = self.lexically_resolve(input)?;
        let resolved = std::fs::canonicalize(&candidate)
            .map_err(|error| OperationError::new(format!("path is not accessible: {error}")))?;
        self.ensure_inside(&resolved)?;
        Ok(resolved)
    }

    /// Resolve a path that may not exist yet (for `write`). Existing parents
    /// are canonicalized so symlink escapes remain rejected.
    pub fn resolve_for_write(&self, input: &str) -> Result<PathBuf, OperationError> {
        let candidate = self.lexically_resolve(input)?;
        let mut existing = candidate.clone();
        let mut suffix = Vec::new();
        while !existing.exists() {
            let name = existing.file_name().ok_or_else(|| {
                OperationError::new("write path has no existing workspace parent")
            })?;
            suffix.push(name.to_os_string());
            existing.pop();
        }
        let canonical_existing = std::fs::canonicalize(&existing).map_err(|error| {
            OperationError::new(format!("write parent is not accessible: {error}"))
        })?;
        self.ensure_inside(&canonical_existing)?;
        let mut resolved = canonical_existing;
        for component in suffix.iter().rev() {
            resolved.push(component);
        }
        self.ensure_inside(&resolved)?;
        Ok(resolved)
    }

    fn lexically_resolve(&self, input: &str) -> Result<PathBuf, OperationError> {
        if input.is_empty() {
            return Err(OperationError::new("path cannot be empty"));
        }
        let raw = Path::new(input);
        let source = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.0.join(raw)
        };
        let mut result = PathBuf::new();
        for component in source.components() {
            use std::path::Component;
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !result.pop() {
                        return Err(OperationError::new("path escapes the workspace root"));
                    }
                }
                Component::RootDir | Component::Prefix(_) => result.push(component.as_os_str()),
                Component::Normal(value) => result.push(value),
            }
        }
        self.ensure_inside(&result)?;
        Ok(result)
    }

    fn ensure_inside(&self, path: &Path) -> Result<(), OperationError> {
        if path == self.as_path() || path.starts_with(self.as_path()) {
            Ok(())
        } else {
            Err(OperationError::new("path escapes the workspace root"))
        }
    }
}
