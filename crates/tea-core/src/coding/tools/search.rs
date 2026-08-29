//! Dependency-free glob and regex-subset search helpers.

use super::contract::OperationError;
use std::path::Path;

/// A deliberately small glob matcher sufficient for Pi's file-oriented
/// patterns. `*` matches within one path component, `**` crosses components,
/// and `?` matches one character. Invalid patterns are rejected at the tool
/// boundary rather than silently broadening the search.
#[derive(Clone, Debug)]
pub(crate) struct GlobMatcher {
    pattern: String,
}

impl GlobMatcher {
    pub(crate) fn new(pattern: &str) -> Result<Self, OperationError> {
        if pattern.is_empty() || pattern.contains('\0') {
            return Err(OperationError::new(
                "glob pattern cannot be empty or contain NUL",
            ));
        }
        Ok(Self {
            pattern: pattern.replace('\\', "/"),
        })
    }

    pub(crate) fn matches(&self, candidate: &str) -> bool {
        glob_match(
            self.pattern.as_bytes(),
            candidate.replace('\\', "/").as_bytes(),
        )
    }
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    fn match_at(pattern: &[u8], text: &[u8], pi: usize, ti: usize) -> bool {
        if pi == pattern.len() {
            return ti == text.len();
        }
        if pattern[pi] == b'*' {
            let double = pi + 1 < pattern.len() && pattern[pi + 1] == b'*';
            let next = if double { pi + 2 } else { pi + 1 };
            if double
                && next < pattern.len()
                && pattern[next] == b'/'
                && match_at(pattern, text, next + 1, ti)
            {
                return true;
            }
            let mut current = ti;
            loop {
                if match_at(pattern, text, next, current) {
                    return true;
                }
                if current == text.len() || (!double && text[current] == b'/') {
                    break;
                }
                current += 1;
            }
            false
        } else if ti < text.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            match_at(pattern, text, pi + 1, ti + 1)
        } else {
            false
        }
    }
    match_at(pattern, text, 0, 0)
}

pub(crate) fn walk_files(
    root: &Path,
    current: &Path,
    matcher: &GlobMatcher,
    limit: usize,
    output: &mut Vec<String>,
) -> Result<(), OperationError> {
    if output.len() >= limit {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(current).map_err(|error| OperationError::new(error.to_string()))?
    {
        if output.len() >= limit {
            break;
        }
        let entry = entry.map_err(|error| OperationError::new(error.to_string()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        // `DirEntry::metadata` follows symlinks. A nested directory symlink
        // could therefore cross the already-confined search root, so search
        // treats every symlink as outside its traversable workspace tree.
        let file_type = entry
            .file_type()
            .map_err(|error| OperationError::new(error.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if file_type.is_dir() {
            walk_files(root, &path, matcher, limit, output)?;
        } else if file_type.is_file() && (matcher.matches(&relative) || matcher.matches(&name)) {
            output.push(relative);
        }
    }
    Ok(())
}
