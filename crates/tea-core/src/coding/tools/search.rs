//! Dependency-free glob and regex-subset search helpers.

use super::contract::{GrepMatch, GrepOptions, OperationError};
use std::path::{Path, PathBuf};

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
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry
            .metadata()
            .map_err(|error| OperationError::new(error.to_string()))?;
        if metadata.is_dir() {
            walk_files(root, &path, matcher, limit, output)?;
        } else if matcher.matches(&relative) || matcher.matches(&name) {
            output.push(relative);
        }
    }
    Ok(())
}

/// Minimal regex-like matcher used by the dependency-free local grep adapter.
/// It intentionally supports literals, `.`, `*`, `^`, and `$`; malformed
/// character classes/escapes are rejected. A host requiring full regex syntax
/// can replace [`CodingOperations::grep_files`] without changing the tool.
#[derive(Clone, Debug)]
pub(crate) struct TinyPattern {
    pattern: String,
    ignore_case: bool,
}

impl TinyPattern {
    pub(crate) fn new(pattern: &str, ignore_case: bool) -> Result<Self, OperationError> {
        if pattern.is_empty() {
            return Err(OperationError::new("pattern cannot be empty"));
        }
        let mut escaped = false;
        let mut class = false;
        for byte in pattern.bytes() {
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'[' => class = true,
                b']' if !class => return Err(OperationError::new("unmatched ] in pattern")),
                b']' => class = false,
                _ => {}
            }
        }
        if escaped || class {
            return Err(OperationError::new(
                "unterminated regex escape or character class",
            ));
        }
        Ok(Self {
            pattern: pattern.to_owned(),
            ignore_case,
        })
    }

    fn matches(&self, text: &str) -> bool {
        let pattern = if self.ignore_case {
            self.pattern.to_lowercase()
        } else {
            self.pattern.clone()
        };
        let text = if self.ignore_case {
            text.to_lowercase()
        } else {
            text.to_owned()
        };
        let anchored_start = pattern.starts_with('^');
        let anchored_end = pattern.ends_with('$') && !pattern.ends_with("\\$");
        let pattern = pattern.strip_prefix('^').unwrap_or(&pattern);
        let pattern = if anchored_end {
            &pattern[..pattern.len() - 1]
        } else {
            pattern
        };
        if anchored_start {
            tiny_match(pattern.as_bytes(), text.as_bytes(), 0, 0, true)
        } else if anchored_end {
            (0..=text.len()).any(|start| {
                tiny_match(pattern.as_bytes(), text.as_bytes(), 0, start, false)
                    && start + pattern_literal_len(pattern.as_bytes()) == text.len()
            })
        } else {
            (0..=text.len())
                .any(|start| tiny_match(pattern.as_bytes(), text.as_bytes(), 0, start, false))
        }
    }
}

fn pattern_literal_len(pattern: &[u8]) -> usize {
    pattern
        .iter()
        .filter(|byte| **byte != b'*' && **byte != b'\\')
        .count()
}

fn tiny_match(pattern: &[u8], text: &[u8], pi: usize, ti: usize, anchored: bool) -> bool {
    if pi == pattern.len() {
        return !anchored || ti <= text.len();
    }
    if pattern[pi] == b'*' {
        let mut current = ti;
        while current <= text.len() {
            if tiny_match(pattern, text, pi + 1, current, anchored) {
                return true;
            }
            if current == text.len() {
                break;
            }
            current += 1;
        }
        return false;
    }
    if ti >= text.len() {
        return false;
    }
    if pattern[pi] == b'.' || pattern[pi] == text[ti] {
        tiny_match(pattern, text, pi + 1, ti + 1, anchored)
    } else {
        false
    }
}

pub(crate) fn local_grep(
    root: &Path,
    pattern: &str,
    options: GrepOptions,
) -> Result<Vec<GrepMatch>, OperationError> {
    let matcher = if options.literal {
        None
    } else {
        Some(TinyPattern::new(pattern, options.ignore_case)?)
    };
    let literal = if options.ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };
    let file_matcher = options.glob.as_deref().map(GlobMatcher::new).transpose()?;
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
    } else {
        collect_files(root, root, file_matcher.as_ref(), &mut files)?;
    }
    files.sort();
    let mut matches = Vec::new();
    for file in files {
        if matches.len() >= options.limit {
            break;
        }
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if matches.len() >= options.limit {
                break;
            }
            let haystack = if options.ignore_case {
                line.to_lowercase()
            } else {
                (*line).to_owned()
            };
            let is_match = if options.literal {
                haystack.contains(&literal)
            } else {
                matcher.as_ref().is_some_and(|value| value.matches(line))
            };
            if is_match {
                let path = file
                    .strip_prefix(root)
                    .ok()
                    .filter(|relative| !relative.as_os_str().is_empty())
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|| {
                        file.file_name()
                            .unwrap_or_else(|| file.as_os_str())
                            .to_string_lossy()
                            .replace('\\', "/")
                    });
                matches.push(GrepMatch {
                    path,
                    line: index + 1,
                    text: line.chars().take(500).collect(),
                });
            }
        }
    }
    Ok(matches)
}

pub(crate) fn collect_files(
    root: &Path,
    current: &Path,
    matcher: Option<&GlobMatcher>,
    output: &mut Vec<PathBuf>,
) -> Result<(), OperationError> {
    for entry in
        std::fs::read_dir(current).map_err(|error| OperationError::new(error.to_string()))?
    {
        let entry = entry.map_err(|error| OperationError::new(error.to_string()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| OperationError::new(error.to_string()))?;
        if metadata.is_dir() {
            collect_files(root, &path, matcher, output)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if matcher.is_none_or(|matcher| matcher.matches(&relative) || matcher.matches(&name)) {
                output.push(path);
            }
        }
    }
    Ok(())
}
