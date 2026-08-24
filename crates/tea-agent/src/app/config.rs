//! Strict terminal-only configuration for optional application features.
//!
//! This module intentionally owns neither provider construction nor durable
//! session policy.  It only decodes the global authorization input supplied by
//! the terminal user.  Library crates must never call it.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{Document, Item, Table, Value};

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_CONCURRENT: u32 = 4;
const DEFAULT_MAX_TOTAL_PER_OPERATION: u32 = 16;
const DEFAULT_TIMEOUT_SECONDS: u64 = 900;

#[derive(Clone, Debug, Eq, PartialEq)]
#[derive(Default)]
pub(super) struct TuiConfig {
    pub(super) features: FeatureConfig,
    pub(super) subagents: SubagentTuiConfig,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct FeatureConfig {
    pub(super) subagents: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SubagentTuiConfig {
    pub(super) provider: Option<String>,
    pub(super) models: Option<Vec<String>>,
    pub(super) max_concurrent: NonZeroU32,
    pub(super) max_total_per_operation: NonZeroU32,
    pub(super) timeout: Duration,
}


impl Default for SubagentTuiConfig {
    fn default() -> Self {
        Self {
            provider: None,
            models: None,
            max_concurrent: NonZeroU32::new(DEFAULT_MAX_CONCURRENT)
                .expect("default max concurrency is nonzero"),
            max_total_per_operation: NonZeroU32::new(DEFAULT_MAX_TOTAL_PER_OPERATION)
                .expect("default total spawn limit is nonzero"),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        }
    }
}

/// A bounded configuration failure associated with the terminal-owned path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    path: PathBuf,
    location: Option<ConfigLocation>,
    message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConfigLocation {
    line: usize,
    column: usize,
}

impl ConfigError {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid TUI config {}", self.path.display())?;
        if let Some(location) = self.location {
            write!(
                formatter,
                " at line {}, column {}",
                location.line, location.column
            )?;
        }
        write!(formatter, ": {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Load the terminal configuration rooted at the already-resolved Tea home.
pub(super) fn load_tui_config(tea_home: &Path) -> Result<TuiConfig, ConfigError> {
    let path = tea_home.join("config.toml");
    let initial_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TuiConfig::default())
        }
        Err(error) => {
            return Err(ConfigError::new(
                path,
                None,
                format!("could not inspect config path: {error}"),
            ));
        }
    };
    if initial_metadata.file_type().is_symlink() {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must not be a symlink",
        ));
    }
    if !initial_metadata.is_file() {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must be a regular file",
        ));
    }
    // Open without following a symlink, then validate that the path still
    // names this exact regular file. Reading metadata and reopening by path
    // without the handle identity check would leave a check/use window.
    let mut file = match open_config_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TuiConfig::default())
        }
        Err(error) => {
            return Err(ConfigError::new(
                path,
                None,
                format!("could not open config file: {error}"),
            ));
        }
    };
    let opened_metadata = file.metadata().map_err(|error| {
        ConfigError::new(
            path.clone(),
            None,
            format!("could not inspect open config file: {error}"),
        )
    })?;
    let path_metadata = fs::symlink_metadata(&path).map_err(|error| {
        ConfigError::new(
            path.clone(),
            None,
            format!("could not revalidate config path: {error}"),
        )
    })?;
    if path_metadata.file_type().is_symlink() {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must not be a symlink",
        ));
    }
    if !opened_metadata.is_file() || !path_metadata.is_file() {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must be a regular file",
        ));
    }
    if !metadata_names_open_file(&opened_metadata, &path_metadata) {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml changed while it was being opened",
        ));
    }
    if opened_metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must not exceed 256 KiB",
        ));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len().min(MAX_CONFIG_BYTES) as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ConfigError::new(
                path.clone(),
                None,
                format!("could not read config file: {error}"),
            )
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            path,
            None,
            "config.toml must not exceed 256 KiB",
        ));
    }
    let source = std::str::from_utf8(&bytes).map_err(|error| {
        ConfigError::new(
            path.clone(),
            location_from_offset(&bytes, error.valid_up_to()),
            "config.toml must be valid UTF-8",
        )
    })?;
    if source.trim().is_empty() {
        return Ok(TuiConfig::default());
    }
    parse_tui_config(&path, source)
}

#[cfg(unix)]
fn open_config_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    // `O_NOFOLLOW` closes the precheck/open race and `O_NONBLOCK` ensures a
    // raced special file cannot hang the terminal before handle validation.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    const SAFE_OPEN_FLAGS: i32 = 0x100 | 0x4;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const SAFE_OPEN_FLAGS: i32 = 0x20_000 | 0x800;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux",
        target_os = "android"
    )))]
    const SAFE_OPEN_FLAGS: i32 = 0;

    OpenOptions::new()
        .read(true)
        .custom_flags(SAFE_OPEN_FLAGS)
        .open(path)
}

#[cfg(not(unix))]
fn open_config_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn metadata_names_open_file(opened: &fs::Metadata, path: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    opened.dev() == path.dev() && opened.ino() == path.ino()
}

#[cfg(not(unix))]
fn metadata_names_open_file(_opened: &fs::Metadata, _path: &fs::Metadata) -> bool {
    true
}

impl ConfigError {
    fn new(path: PathBuf, location: Option<ConfigLocation>, message: impl Into<String>) -> Self {
        Self {
            path,
            location,
            message: message.into(),
        }
    }
}

fn parse_tui_config(path: &Path, source: &str) -> Result<TuiConfig, ConfigError> {
    let document = Document::parse(source).map_err(|error| {
        ConfigError::new(
            path.to_path_buf(),
            error
                .span()
                .and_then(|span| location_from_offset(source.as_bytes(), span.start)),
            format!("TOML syntax error: {}", error.message()),
        )
    })?;
    let root = document.as_table();
    reject_unknown_keys(path, source, root, &["features", "subagents"], "root")?;

    let features = match root.get("features") {
        None => FeatureConfig::default(),
        Some(item) => parse_features(path, source, item)?,
    };
    let subagents = match root.get("subagents") {
        None => SubagentTuiConfig::default(),
        Some(item) => parse_subagents(path, source, item, features.subagents)?,
    };
    Ok(TuiConfig {
        features,
        subagents,
    })
}

fn parse_features(path: &Path, source: &str, item: &Item) -> Result<FeatureConfig, ConfigError> {
    let table = item.as_table().ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            "root table [features] must be a TOML table",
        )
    })?;
    reject_unknown_keys(path, source, table, &["subagents"], "[features]")?;
    let subagents = match table.get("subagents") {
        None => false,
        Some(item) => item.as_bool().ok_or_else(|| {
            error_for_item(path, source, item, "[features].subagents must be a boolean")
        })?,
    };
    Ok(FeatureConfig { subagents })
}

fn parse_subagents(
    path: &Path,
    source: &str,
    item: &Item,
    subagents_enabled: bool,
) -> Result<SubagentTuiConfig, ConfigError> {
    let table = item.as_table().ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            "root table [subagents] must be a TOML table",
        )
    })?;
    reject_unknown_keys(
        path,
        source,
        table,
        &[
            "provider",
            "models",
            "max_concurrent",
            "max_total_per_operation",
            "timeout_seconds",
        ],
        "[subagents]",
    )?;

    let provider = optional_string(path, source, table, "provider")?;
    if provider
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(error_for_item(
            path,
            source,
            table.get("provider").expect("provider is present"),
            "[subagents].provider must not be empty",
        ));
    }
    let models = optional_models(path, source, table)?;
    if subagents_enabled && models.as_ref().is_some_and(Vec::is_empty) {
        return Err(error_for_item(
            path,
            source,
            table.get("models").expect("models is present"),
            "[subagents].models must not be empty when subagents are enabled",
        ));
    }

    let max_concurrent = optional_u32(
        path,
        source,
        table,
        "max_concurrent",
        DEFAULT_MAX_CONCURRENT,
        1,
        16,
    )?;
    let max_total_per_operation = optional_u32(
        path,
        source,
        table,
        "max_total_per_operation",
        DEFAULT_MAX_TOTAL_PER_OPERATION,
        1,
        64,
    )?;
    if max_total_per_operation < max_concurrent {
        return Err(error_for_item(
            path,
            source,
            table
                .get("max_total_per_operation")
                .expect("the invalid total value was explicit"),
            "[subagents].max_total_per_operation must be at least max_concurrent",
        ));
    }
    let timeout_seconds = optional_u64(
        path,
        source,
        table,
        "timeout_seconds",
        DEFAULT_TIMEOUT_SECONDS,
        30,
        7_200,
    )?;

    Ok(SubagentTuiConfig {
        provider,
        models,
        max_concurrent: NonZeroU32::new(max_concurrent).expect("minimum max_concurrent is one"),
        max_total_per_operation: NonZeroU32::new(max_total_per_operation)
            .expect("minimum max_total_per_operation is one"),
        timeout: Duration::from_secs(timeout_seconds),
    })
}

fn reject_unknown_keys(
    path: &Path,
    source: &str,
    table: &Table,
    allowed: &[&str],
    table_name: &str,
) -> Result<(), ConfigError> {
    for (key, item) in table.iter() {
        if !allowed.contains(&key) {
            let message = if table_name == "root" {
                format!("unknown root key {key:?}; only [features] and [subagents] are allowed")
            } else {
                format!("unknown {table_name} key {key:?}")
            };
            return Err(error_for_item(path, source, item, message));
        }
    }
    Ok(())
}

fn optional_string(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
) -> Result<Option<String>, ConfigError> {
    let Some(item) = table.get(key) else {
        return Ok(None);
    };
    let value = item.as_str().ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            format!("[subagents].{key} must be a string"),
        )
    })?;
    Ok(Some(value.to_owned()))
}

fn optional_models(
    path: &Path,
    source: &str,
    table: &Table,
) -> Result<Option<Vec<String>>, ConfigError> {
    let Some(item) = table.get("models") else {
        return Ok(None);
    };
    let array = item
        .as_array()
        .ok_or_else(|| error_for_item(path, source, item, "[subagents].models must be an array"))?;
    let mut models = Vec::with_capacity(array.len());
    let mut known = BTreeSet::new();
    for value in array.iter() {
        let model = value.as_str().ok_or_else(|| {
            error_for_value(
                path,
                source,
                value,
                "[subagents].models entries must be strings",
            )
        })?;
        if model.trim().is_empty() {
            return Err(error_for_value(
                path,
                source,
                value,
                "[subagents].models entries must not be empty",
            ));
        }
        if !known.insert(model) {
            return Err(error_for_value(
                path,
                source,
                value,
                format!("duplicate model {model:?} in [subagents].models"),
            ));
        }
        models.push(model.to_owned());
    }
    Ok(Some(models))
}

fn optional_u32(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
    default: u32,
    minimum: u32,
    maximum: u32,
) -> Result<u32, ConfigError> {
    let Some(item) = table.get(key) else {
        return Ok(default);
    };
    let value = item.as_integer().ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            format!("[subagents].{key} must be an integer"),
        )
    })?;
    let value = u32::try_from(value)
        .ok()
        .filter(|value| (*value >= minimum) && (*value <= maximum));
    value.ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            format!("[subagents].{key} must be between {minimum} and {maximum}"),
        )
    })
}

fn optional_u64(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ConfigError> {
    let Some(item) = table.get(key) else {
        return Ok(default);
    };
    let value = item.as_integer().ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            format!("[subagents].{key} must be an integer"),
        )
    })?;
    let value = u64::try_from(value)
        .ok()
        .filter(|value| (*value >= minimum) && (*value <= maximum));
    value.ok_or_else(|| {
        error_for_item(
            path,
            source,
            item,
            format!("[subagents].{key} must be between {minimum} and {maximum}"),
        )
    })
}

fn error_for_item(
    path: &Path,
    source: &str,
    item: &Item,
    message: impl Into<String>,
) -> ConfigError {
    ConfigError::new(
        path.to_path_buf(),
        item.span()
            .and_then(|span| location_from_offset(source.as_bytes(), span.start)),
        message,
    )
}

fn error_for_value(
    path: &Path,
    source: &str,
    value: &Value,
    message: impl Into<String>,
) -> ConfigError {
    ConfigError::new(
        path.to_path_buf(),
        value
            .span()
            .and_then(|span| location_from_offset(source.as_bytes(), span.start)),
        message,
    )
}

fn location_from_offset(source: &[u8], offset: usize) -> Option<ConfigLocation> {
    if offset > source.len() {
        return None;
    }
    let prefix = std::str::from_utf8(&source[..offset]).ok()?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, line)| {
            line.chars().count() + 1
        });
    Some(ConfigLocation { line, column })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tea_home(label: &str) -> std::path::PathBuf {
        static NEXT_HOME: AtomicU64 = AtomicU64::new(1);
        let home = std::env::temp_dir().join(format!(
            "tea-config-{label}-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&home).expect("test Tea home creates");
        home
    }

    fn write_config(home: &Path, source: &str) {
        fs::write(home.join("config.toml"), source).expect("test config writes");
    }

    fn expect_error(home: &Path, source: &str, expected: &str) {
        write_config(home, source);
        let error = load_tui_config(home).expect_err("config should be rejected");
        assert_eq!(error.path(), home.join("config.toml").as_path());
        assert!(
            error.to_string().contains(expected),
            "{error} did not contain {expected:?}"
        );
    }

    #[test]
    fn missing_and_empty_files_use_defaults() {
        let missing = tea_home("missing");
        assert_eq!(
            load_tui_config(&missing).expect("missing defaults"),
            TuiConfig::default()
        );
        assert!(
            !missing.join("config.toml").exists(),
            "loading defaults must not create config.toml"
        );

        let empty = tea_home("empty");
        write_config(&empty, "");
        assert_eq!(
            load_tui_config(&empty).expect("empty defaults"),
            TuiConfig::default()
        );
    }

    #[test]
    fn enabled_and_disabled_examples_preserve_the_declared_policy() {
        let enabled = tea_home("enabled");
        write_config(
            &enabled,
            r#"
[features]
subagents = true

[subagents]
provider = "openrouter"
models = ["openai/gpt-5.6-luna", "inclusionai/ling-3.0-tiny:free"]
max_concurrent = 3
max_total_per_operation = 12
timeout_seconds = 600
"#,
        );
        assert_eq!(
            load_tui_config(&enabled).expect("enabled config"),
            TuiConfig {
                features: FeatureConfig { subagents: true },
                subagents: SubagentTuiConfig {
                    provider: Some("openrouter".into()),
                    models: Some(vec![
                        "openai/gpt-5.6-luna".into(),
                        "inclusionai/ling-3.0-tiny:free".into(),
                    ]),
                    max_concurrent: NonZeroU32::new(3).expect("nonzero"),
                    max_total_per_operation: NonZeroU32::new(12).expect("nonzero"),
                    timeout: Duration::from_secs(600),
                },
            }
        );

        let disabled = tea_home("disabled");
        write_config(&disabled, "[features]\nsubagents = false\n");
        assert_eq!(
            load_tui_config(&disabled).expect("disabled config"),
            TuiConfig::default()
        );
    }

    #[test]
    fn rejects_unknown_root_feature_and_subagent_keys() {
        expect_error(
            &tea_home("unknown-root"),
            "other = true\n",
            "unknown root key",
        );
        expect_error(
            &tea_home("unknown-root-table"),
            "[other]\nenabled = true\n",
            "unknown root key",
        );
        expect_error(
            &tea_home("unknown-feature"),
            "[features]\nother = true\n",
            "unknown [features] key",
        );
        expect_error(
            &tea_home("unknown-subagent"),
            "[subagents]\nother = true\n",
            "unknown [subagents] key",
        );
    }

    #[test]
    fn rejects_duplicate_keys_and_wrong_value_types_with_a_location() {
        let duplicate = tea_home("duplicate");
        write_config(
            &duplicate,
            "[features]\nsubagents = true\nsubagents = false\n",
        );
        let error = load_tui_config(&duplicate).expect_err("duplicate key rejects");
        assert_eq!(error.path(), duplicate.join("config.toml").as_path());
        assert!(error.to_string().contains("line"), "{error}");

        expect_error(
            &tea_home("wrong-feature-type"),
            "[features]\nsubagents = \"yes\"\n",
            "must be a boolean",
        );
        let wrong_provider_type = tea_home("wrong-provider-type");
        write_config(&wrong_provider_type, "[subagents]\nprovider = 3\n");
        let error = load_tui_config(&wrong_provider_type).expect_err("wrong type rejects");
        assert!(error.to_string().contains("must be a string"), "{error}");
        assert!(
            error.to_string().contains("line 2, column"),
            "semantic errors retain parser source locations: {error}"
        );
        expect_error(
            &tea_home("wrong-models-type"),
            "[subagents]\nmodels = \"openai/gpt-5.6-luna\"\n",
            "must be an array",
        );
        expect_error(
            &tea_home("wrong-limit-type"),
            "[subagents]\nmax_concurrent = true\n",
            "must be an integer",
        );
    }

    #[test]
    fn rejects_invalid_model_lists_and_limit_ranges() {
        expect_error(
            &tea_home("duplicate-model"),
            "[subagents]\nmodels = [\"a\", \"a\"]\n",
            "duplicate model",
        );
        expect_error(
            &tea_home("empty-model"),
            "[subagents]\nmodels = [\"   \"]\n",
            "must not be empty",
        );
        expect_error(
            &tea_home("empty-enabled-array"),
            "[features]\nsubagents = true\n[subagents]\nmodels = []\n",
            "must not be empty when subagents are enabled",
        );
        expect_error(
            &tea_home("too-many-active"),
            "[subagents]\nmax_concurrent = 17\n",
            "between 1 and 16",
        );
        expect_error(
            &tea_home("total-below-active"),
            "[subagents]\nmax_concurrent = 5\nmax_total_per_operation = 4\n",
            "at least max_concurrent",
        );
        expect_error(
            &tea_home("timeout-low"),
            "[subagents]\ntimeout_seconds = 29\n",
            "between 30 and 7200",
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_config_symlink() {
        use std::os::unix::fs::symlink;

        let home = tea_home("symlink");
        let target = home.join("target.toml");
        fs::write(&target, "[features]\nsubagents = true\n").expect("target writes");
        symlink(&target, home.join("config.toml")).expect("config symlink creates");
        let error = load_tui_config(&home).expect_err("symlink rejects");
        assert!(
            error.to_string().contains("must not be a symlink"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_config_symlink_without_opening_its_fifo_target() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let home = tea_home("symlink-fifo");
        let fifo = home.join("blocking-fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo starts")
                .success(),
            "fixture FIFO creates"
        );
        symlink(&fifo, home.join("config.toml")).expect("config symlink creates");
        let error = load_tui_config(&home).expect_err("FIFO symlink rejects before open");
        assert!(
            error.to_string().contains("must not be a symlink"),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_file_larger_than_the_config_limit() {
        let home = tea_home("oversized");
        fs::write(
            home.join("config.toml"),
            vec![b'#'; MAX_CONFIG_BYTES as usize + 1],
        )
        .expect("oversized config writes");
        let error = load_tui_config(&home).expect_err("oversized config rejects");
        assert!(error.to_string().contains("256 KiB"), "{error}");
    }
}
