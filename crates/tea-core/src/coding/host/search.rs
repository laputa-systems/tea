//! Dependency-free bounded glob matching and workspace search helpers.

use super::contract::{OperationError, SearchResult, SearchTruncation};
use crate::scheduler::CancellationToken;
use std::path::Path;

/// Glob patterns share the established workspace-path input bound. Keeping
/// this finite also bounds the matcher frontier for model-controlled input.
pub(crate) const MAX_GLOB_PATTERN_BYTES: usize = 4096;

/// A deliberately small glob matcher sufficient for Tea's file-oriented
/// patterns. `*` matches within one path component, `**` crosses components,
/// and `?` matches one character. Invalid patterns are rejected at the tool
/// boundary rather than silently broadening the search.
#[derive(Clone, Debug)]
pub(crate) struct GlobMatcher {
    tokens: Vec<GlobToken>,
}

#[derive(Clone, Copy, Debug)]
enum GlobToken {
    Literal(u8),
    AnyCharacter,
    ComponentWildcard,
    PathWildcard,
    DirectoryWildcard,
}

impl GlobMatcher {
    pub(crate) fn new(pattern: &str) -> Result<Self, OperationError> {
        if pattern.is_empty() || pattern.contains('\0') {
            return Err(OperationError::new(
                "glob pattern cannot be empty or contain NUL",
            ));
        }
        if pattern.len() > MAX_GLOB_PATTERN_BYTES {
            return Err(OperationError::new(format!(
                "glob pattern exceeds the {MAX_GLOB_PATTERN_BYTES} byte limit",
            )));
        }
        let pattern = pattern.replace('\\', "/");
        let bytes = pattern.as_bytes();
        let mut tokens = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'*' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                    if index + 2 < bytes.len() && bytes[index + 2] == b'/' {
                        tokens.push(GlobToken::DirectoryWildcard);
                        index += 3;
                    } else {
                        tokens.push(GlobToken::PathWildcard);
                        index += 2;
                    }
                }
                b'*' => {
                    tokens.push(GlobToken::ComponentWildcard);
                    index += 1;
                }
                b'?' => {
                    tokens.push(GlobToken::AnyCharacter);
                    index += 1;
                }
                byte => {
                    tokens.push(GlobToken::Literal(byte));
                    index += 1;
                }
            }
        }
        Ok(Self { tokens })
    }

    /// Execute the glob as a finite-state machine. The active-state frontier
    /// has one entry per pattern token, so matching is O(pattern × path) with
    /// no recursive backtracking or model-controlled call depth.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        let candidate = candidate.replace('\\', "/");
        let token_count = self.tokens.len();
        let mut active = vec![false; token_count + 1];
        let mut fresh_directory_wildcards = vec![false; token_count + 1];
        active[0] = true;
        if matches!(self.tokens.first(), Some(GlobToken::DirectoryWildcard)) {
            fresh_directory_wildcards[0] = true;
        }
        self.expand_epsilon(&mut active, &mut fresh_directory_wildcards);

        for byte in candidate.bytes() {
            let mut next = vec![false; token_count + 1];
            let mut next_fresh_directory_wildcards = vec![false; token_count + 1];
            for (index, token) in self.tokens.iter().enumerate() {
                if !active[index] {
                    continue;
                }
                match token {
                    GlobToken::Literal(expected) if *expected == byte => self.activate(
                        &mut next,
                        &mut next_fresh_directory_wildcards,
                        index + 1,
                    ),
                    GlobToken::AnyCharacter => self.activate(
                        &mut next,
                        &mut next_fresh_directory_wildcards,
                        index + 1,
                    ),
                    GlobToken::ComponentWildcard if byte != b'/' => next[index] = true,
                    GlobToken::PathWildcard => next[index] = true,
                    GlobToken::DirectoryWildcard => {
                        next[index] = true;
                        if byte == b'/' {
                            self.activate(
                                &mut next,
                                &mut next_fresh_directory_wildcards,
                                index + 1,
                            );
                        }
                    }
                    _ => {}
                }
            }
            self.expand_epsilon(&mut next, &mut next_fresh_directory_wildcards);
            active = next;
        }
        active[token_count]
    }

    fn activate(&self, active: &mut [bool], fresh_directory_wildcards: &mut [bool], index: usize) {
        active[index] = true;
        if matches!(self.tokens.get(index), Some(GlobToken::DirectoryWildcard)) {
            fresh_directory_wildcards[index] = true;
        }
    }

    fn expand_epsilon(&self, active: &mut [bool], fresh_directory_wildcards: &mut [bool]) {
        // All epsilon transitions move forward, so one left-to-right pass
        // reaches their complete closure without a recursive worklist.
        for index in 0..self.tokens.len() {
            if !active[index] {
                continue;
            }
            match self.tokens[index] {
                GlobToken::ComponentWildcard | GlobToken::PathWildcard => {
                    self.activate(active, fresh_directory_wildcards, index + 1)
                }
                _ => {}
            }
            // `**/` matches zero complete path components only before that
            // wildcard consumes any path text. Its consumed state remains
            // active until it observes a slash, preventing `**/a` from
            // incorrectly matching a basename such as `fooa`.
            if matches!(self.tokens[index], GlobToken::DirectoryWildcard)
                && fresh_directory_wildcards[index]
            {
                self.activate(active, fresh_directory_wildcards, index + 1);
            }
        }
    }
}

/// Search below `root` without recursive filesystem traversal. The explicit
/// stack avoids filesystem-depth call growth, keeps result selection stable,
/// and observes cancellation between directory entries.
pub(crate) fn walk_files(
    root: &Path,
    matcher: &GlobMatcher,
    max_results: usize,
    max_output_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<SearchResult, OperationError> {
    let mut directories = vec![root.to_path_buf()];
    let mut matches = Vec::new();
    let mut output_bytes = 0_usize;
    let mut checked_entries = 0_usize;

    while let Some(current) = directories.pop() {
        check_cancelled(cancellation)?;
        let mut entries = std::fs::read_dir(&current)
            .map_err(|error| OperationError::new(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| OperationError::new(error.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut child_directories = Vec::new();

        for entry in entries {
            checked_entries = checked_entries.saturating_add(1);
            if checked_entries % 64 == 0 {
                check_cancelled(cancellation)?;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            // `DirEntry::metadata` follows symlinks. A nested directory
            // symlink could therefore cross the already-confined search root,
            // so search treats every symlink as outside its traversable
            // workspace tree.
            let file_type = entry
                .file_type()
                .map_err(|error| OperationError::new(error.to_string()))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                child_directories.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !matcher.matches(&relative) && !matcher.matches(&name) {
                continue;
            }
            if matches.len() >= max_results {
                return Ok(SearchResult {
                    matches,
                    truncation: SearchTruncation::ResultLimit,
                });
            }
            let next_output_bytes = output_bytes
                .saturating_add(relative.len())
                .saturating_add(usize::from(!matches.is_empty()));
            if next_output_bytes > max_output_bytes {
                return Ok(SearchResult {
                    matches,
                    truncation: SearchTruncation::ByteBudget,
                });
            }
            output_bytes = next_output_bytes;
            matches.push(relative);
        }
        // The LIFO stack visits alphabetically earlier directories first.
        child_directories.reverse();
        directories.extend(child_directories);
    }

    Ok(SearchResult {
        matches,
        truncation: SearchTruncation::Complete,
    })
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), OperationError> {
    if cancellation.is_cancelled() {
        Err(OperationError::new("cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matcher_preserves_component_and_path_wildcards_without_backtracking() {
        let component = GlobMatcher::new("src/*.rs").expect("component glob parses");
        assert!(component.matches("src/lib.rs"));
        assert!(!component.matches("src/nested/lib.rs"));

        let path = GlobMatcher::new("src/**/*.rs").expect("path glob parses");
        assert!(path.matches("src/lib.rs"));
        assert!(path.matches("src/nested/lib.rs"));
        assert!(!path.matches("other/lib.rs"));

        let zero_or_more_directories = GlobMatcher::new("**/a").expect("directory glob parses");
        assert!(zero_or_more_directories.matches("a"));
        assert!(zero_or_more_directories.matches("nested/a"));
        assert!(!zero_or_more_directories.matches("fooa"));

        // The old recursive matcher explores an exponential tree for this
        // repeated wildcard shape. Correctness here establishes the same
        // language with the finite-state frontier above.
        let adversarial = GlobMatcher::new("*a*a*a*a*a*a*a*a*a*a*a*a*b")
            .expect("adversarial glob parses");
        assert!(adversarial.matches("aaaaaaaaaaaaab"));
        assert!(!adversarial.matches("aaaaaaaaaaaaac"));
    }

    #[test]
    fn glob_pattern_length_is_bounded() {
        assert!(GlobMatcher::new(&"*".repeat(MAX_GLOB_PATTERN_BYTES)).is_ok());
        assert!(GlobMatcher::new(&"*".repeat(MAX_GLOB_PATTERN_BYTES + 1)).is_err());
    }
}
