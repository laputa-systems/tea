//! Small host-owned preferences that survive between terminal invocations.
//!
//! The core remains file-system agnostic. This preference stores only the last provider/model
//! identity selected by the terminal host, so startup can restore the user's model without
//! opening an interactive picker.

use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tea_core::ModelDescriptor;
use tea_protocol::{JsonNumber, JsonValue};

const PREFERENCE_FILE: &str = "last-model.json";
const PREFERENCE_VERSION: u64 = 1;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

/// Failures at the host-owned last-model persistence boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreferenceError {
    Io { path: PathBuf, message: String },
    Json { path: PathBuf, message: String },
    Contract { path: PathBuf, message: String },
}

impl std::fmt::Display for PreferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(
                    formatter,
                    "model preference I/O failed at {}: {message}",
                    path.display()
                )
            }
            Self::Json { path, message } => write!(
                formatter,
                "invalid model preference JSON at {}: {message}",
                path.display()
            ),
            Self::Contract { path, message } => write!(
                formatter,
                "invalid model preference at {}: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PreferenceError {}

/// Read the last model selected below one explicit Tea home.
pub(crate) fn load_last_model(
    tea_home: impl AsRef<Path>,
) -> Result<Option<ModelDescriptor>, PreferenceError> {
    let path = tea_home.as_ref().join(PREFERENCE_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() {
            return Err(contract(&path, "model preference cannot be a symlink"));
        }
    }
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(&path, error)),
    };
    let value = JsonValue::parse(&source).map_err(|error| PreferenceError::Json {
        path: path.clone(),
        message: error.to_string(),
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| contract(&path, "root must be a JSON object"))?;
    let version = object
        .get("version")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| contract(&path, "version must be an unsigned integer"))?;
    if version != PREFERENCE_VERSION {
        return Err(contract(
            &path,
            format!("unsupported model preference version {version}"),
        ));
    }
    let provider = required_string(&path, object, "provider")?;
    let model = required_string(&path, object, "model")?;
    Ok(Some(ModelDescriptor {
        provider,
        model,
        revision: None,
    }))
}

/// Atomically persist the last model selected by the terminal host.
pub(crate) fn save_last_model(
    tea_home: impl AsRef<Path>,
    model: &ModelDescriptor,
) -> Result<(), PreferenceError> {
    let tea_home = tea_home.as_ref();
    let path = tea_home.join(PREFERENCE_FILE);
    if model.provider.trim().is_empty() || model.model.trim().is_empty() {
        return Err(contract(&path, "provider and model must not be empty"));
    }
    ensure_home_directory(tea_home)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() {
            return Err(contract(&path, "model preference cannot be a symlink"));
        }
    }
    let value = JsonValue::object([
        ("model", JsonValue::String(model.model.clone())),
        ("provider", JsonValue::String(model.provider.clone())),
        (
            "version",
            JsonValue::Number(JsonNumber::Unsigned(PREFERENCE_VERSION)),
        ),
    ]);
    let source = value
        .to_json_string()
        .map(|source| format!("{source}\n"))
        .map_err(|error| PreferenceError::Json {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let temporary = tea_home.join(format!(
        ".last-model-{}.tmp",
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created_temporary = false;
    let result = (|| {
        let mut file = create_private_temporary(&temporary)?;
        created_temporary = true;
        file.write_all(source.as_bytes())
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|error| io_error(&temporary, error))?;
        drop(file);
        fs::rename(&temporary, &path).map_err(|error| io_error(&path, error))?;
        sync_preference_directory(tea_home)?;
        Ok(())
    })();
    if result.is_err() && created_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_home_directory(path: &Path) -> Result<(), PreferenceError> {
    fs::create_dir_all(path).map_err(|error| io_error(path, error))?;
    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    if !metadata.is_dir() {
        return Err(contract(path, "Tea home must be a directory"));
    }
    Ok(())
}

fn required_string(
    path: &Path,
    object: &std::collections::BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<String, PreferenceError> {
    let value = object
        .get(field)
        .ok_or_else(|| contract(path, format!("{field} is required")))?;
    let value = value
        .as_str()
        .ok_or_else(|| contract(path, format!("{field} must be a string")))?;
    if value.trim().is_empty() {
        return Err(contract(path, format!("{field} must not be empty")));
    }
    Ok(value.to_owned())
}

fn sync_preference_directory(path: &Path) -> Result<(), PreferenceError> {
    let directory = fs::File::open(path).map_err(|error| io_error(path, error))?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io_error(path, error)),
    }
}

fn create_private_temporary(path: &Path) -> Result<fs::File, PreferenceError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| io_error(path, error))
}

fn io_error(path: &Path, error: io::Error) -> PreferenceError {
    PreferenceError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn contract(path: &Path, message: impl Into<String>) -> PreferenceError {
    PreferenceError::Contract {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preference_is_empty_and_round_trip_is_canonical() {
        let root = std::env::temp_dir().join(format!(
            "tea-preference-test-{}",
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let model = ModelDescriptor {
            provider: "local".into(),
            model: "demo".into(),
            revision: None,
        };
        assert_eq!(load_last_model(&root).unwrap(), None);
        save_last_model(&root, &model).unwrap();
        assert_eq!(load_last_model(&root).unwrap(), Some(model));
        assert!(fs::read_to_string(root.join(PREFERENCE_FILE))
            .unwrap()
            .contains(r#""version":1"#));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_preference_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "tea-preference-test-{}",
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(PREFERENCE_FILE),
            r#"{"version":1,"provider":"local"}"#,
        )
        .unwrap();
        assert!(matches!(
            load_last_model(&root),
            Err(PreferenceError::Contract { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temporary_creation_does_not_overwrite_an_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "tea-preference-test-{}",
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let temporary = root.join(".last-model.tmp");
        fs::write(&temporary, b"must not be overwritten").unwrap();

        let error = create_private_temporary(&temporary).unwrap_err();

        assert!(matches!(error, PreferenceError::Io { .. }));
        assert_eq!(fs::read(&temporary).unwrap(), b"must not be overwritten");
        let _ = fs::remove_dir_all(root);
    }
}
