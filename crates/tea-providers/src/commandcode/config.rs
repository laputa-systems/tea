//! Explicit Command Code configuration and error contracts.

use super::super::retry::RetryPolicy;
use std::fmt;

/// Error raised when explicit Command Code configuration violates an adapter invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandCodeConfigError {
    /// A required caller-supplied text value was empty.
    EmptyField(&'static str),
    /// The maximum output token cap was zero.
    ZeroMaxTokens,
    /// The temperature was not finite.
    NonFiniteTemperature,
    /// Command Code only serializes canonical UUID thread identifiers.
    InvalidThreadId,
}

impl fmt::Display for CommandCodeConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "Command Code {field} must not be empty"),
            Self::ZeroMaxTokens => {
                formatter.write_str("Command Code max tokens must be greater than zero")
            }
            Self::NonFiniteTemperature => {
                formatter.write_str("Command Code temperature must be finite")
            }
            Self::InvalidThreadId => {
                formatter.write_str("Command Code thread ID must be a canonical UUID")
            }
        }
    }
}

impl std::error::Error for CommandCodeConfigError {}

/// Boundary at which a Command Code request failed.
///
/// `Gateway` details originated in a remote NDJSON `error` event. `Adapter` details are local
/// validation, serialization, transport, or response-grammar failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandCodeErrorSource {
    /// The gateway sent a terminal NDJSON `error` event.
    Gateway,
    /// The adapter could not form, send, or parse a request/response.
    Adapter,
}

impl CommandCodeErrorSource {
    /// Stable, lower-case spelling for a host log or structured artifact.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Adapter => "adapter",
        }
    }
}

/// Structured diagnostic for the most recent Command Code failure.
///
/// The core's [`ModelStreamEvent::Error`] remains deliberately generic, so a remote provider
/// cannot inject arbitrary text into agent state. Hosts that own a private diagnostic sink can
/// instead read this report with [`CommandCodeProvider::last_error_report`]. The configured API
/// key is redacted from text fields, but gateway messages are still untrusted remote data: do
/// not write them to logs that are visible to an untrusted model or another tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCodeErrorReport {
    /// Whether this came from a gateway event or from the local adapter boundary.
    pub source: CommandCodeErrorSource,
    /// Gateway-provided message or a stable local adapter failure explanation.
    pub message: String,
    /// Gateway HTTP status, when supplied directly or embedded in its message.
    pub status_code: Option<u16>,
    /// Gateway error classification, when present.
    pub error_type: Option<String>,
    /// Gateway error code, when present.
    pub error_code: Option<String>,
    /// Whether the current Command Code client considers the failure retryable.
    ///
    /// The adapter uses this classification for bounded retries before exposing the terminal
    /// error stream; the report preserves the final classification for trusted host diagnostics.
    pub retryable: Option<bool>,
}

impl fmt::Display for CommandCodeErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "source={} message={:?}",
            self.source.as_str(),
            self.message
        )?;
        if let Some(status_code) = self.status_code {
            write!(formatter, " status_code={status_code}")?;
        }
        if let Some(error_type) = &self.error_type {
            write!(formatter, " error_type={error_type:?}")?;
        }
        if let Some(error_code) = &self.error_code {
            write!(formatter, " error_code={error_code:?}")?;
        }
        if let Some(retryable) = self.retryable {
            write!(formatter, " retryable={retryable}")?;
        }
        Ok(())
    }
}

/// Caller-provided host metadata included in a Command Code gateway request.
///
/// These values are deliberately explicit rather than discovered from the local process. A
/// sandboxed host can supply the virtual workspace and platform it has actually authorized.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCodeHostContext {
    pub(super) working_directory: String,
    pub(super) date: String,
    pub(super) environment: String,
}

impl CommandCodeHostContext {
    /// Construct host metadata after rejecting empty values.
    pub fn new(
        working_directory: impl Into<String>,
        date: impl Into<String>,
        environment: impl Into<String>,
    ) -> Result<Self, CommandCodeConfigError> {
        Ok(Self {
            working_directory: nonempty(working_directory.into(), "working directory")?,
            date: nonempty(date.into(), "date")?,
            environment: nonempty(environment.into(), "environment")?,
        })
    }
}

/// Command Code permission mode for one request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CommandCodePermissionMode {
    /// Let the gateway apply its standard permission behavior.
    #[default]
    Standard,
    /// Let the gateway auto-accept permitted operations.
    AutoAccept,
    /// Ask the gateway to plan instead of applying changes.
    Plan,
}

impl CommandCodePermissionMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::AutoAccept => "auto-accept",
            Self::Plan => "plan",
        }
    }
}

/// Caller-owned configuration for [`CommandCodeProvider`].
///
/// The default is the gateway's `agent` mode and a 64,000 token cap, matching the upstream Pi
/// catalog. Credentials are redacted from its [`fmt::Debug`] representation.
#[derive(Clone, PartialEq)]
pub struct CommandCodeConfig {
    pub(super) api_key: String,
    pub(super) model: String,
    pub(super) host: CommandCodeHostContext,
    pub(super) max_tokens: u64,
    pub(super) permission_mode: CommandCodePermissionMode,
    pub(super) thread_id: Option<String>,
    pub(super) mode: String,
    pub(super) temperature: Option<f64>,
    pub(super) zero_data_retention: bool,
    pub(super) project_slug: Option<String>,
    pub(super) taste_learning_enabled: bool,
    pub(super) retry_policy: RetryPolicy,
}

impl CommandCodeConfig {
    /// Configure a Command Code model with explicit credentials and host metadata.
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        host: CommandCodeHostContext,
    ) -> Result<Self, CommandCodeConfigError> {
        Ok(Self {
            api_key: nonempty(api_key.into(), "API key")?,
            model: nonempty(model.into(), "model")?,
            host,
            max_tokens: 64_000,
            permission_mode: CommandCodePermissionMode::Standard,
            thread_id: None,
            mode: "agent".into(),
            temperature: None,
            zero_data_retention: false,
            project_slug: None,
            // Command Code CLI 1.24.0 enables this client feature by default. Embeddings can
            // turn it off explicitly; the adapter never discovers a user preference.
            taste_learning_enabled: true,
            retry_policy: RetryPolicy::standard(),
        })
    }

    /// Borrow the explicitly configured gateway model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the explicit maximum output-token cap.
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Result<Self, CommandCodeConfigError> {
        if max_tokens == 0 {
            return Err(CommandCodeConfigError::ZeroMaxTokens);
        }
        self.max_tokens = max_tokens;
        Ok(self)
    }

    /// Set the gateway permission mode for each request.
    pub fn with_permission_mode(mut self, permission_mode: CommandCodePermissionMode) -> Self {
        self.permission_mode = permission_mode;
        self
    }

    /// Include a caller-owned Command Code thread identifier.
    pub fn with_thread_id(
        mut self,
        thread_id: impl Into<String>,
    ) -> Result<Self, CommandCodeConfigError> {
        let thread_id = nonempty(thread_id.into(), "thread ID")?;
        if !is_canonical_uuid(&thread_id) {
            return Err(CommandCodeConfigError::InvalidThreadId);
        }
        self.thread_id = Some(thread_id);
        Ok(self)
    }

    /// Replace the gateway mode, such as `agent` or a caller-defined mode.
    pub fn with_mode(mut self, mode: impl Into<String>) -> Result<Self, CommandCodeConfigError> {
        self.mode = nonempty(mode.into(), "mode")?;
        Ok(self)
    }

    /// Include a finite temperature in each gateway request.
    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, CommandCodeConfigError> {
        if !temperature.is_finite() {
            return Err(CommandCodeConfigError::NonFiniteTemperature);
        }
        self.temperature = Some(temperature);
        Ok(self)
    }

    /// Opt into the gateway's explicit zero-data-retention request header.
    pub fn with_zero_data_retention(mut self, enabled: bool) -> Self {
        self.zero_data_retention = enabled;
        self
    }

    /// Override the project slug sent to the gateway.
    ///
    /// The default is the final path component of the already-explicit working directory,
    /// matching the current Command Code client without reading the process working directory.
    pub fn with_project_slug(
        mut self,
        project_slug: impl Into<String>,
    ) -> Result<Self, CommandCodeConfigError> {
        self.project_slug = Some(nonempty(project_slug.into(), "project slug")?);
        Ok(self)
    }

    /// Enable or disable Command Code's current taste-learning client feature.
    ///
    /// It defaults to the current Command Code CLI behavior. Callers that do not want to opt
    /// into that gateway feature can set it to `false` before constructing the provider.
    pub fn with_taste_learning_enabled(mut self, enabled: bool) -> Self {
        self.taste_learning_enabled = enabled;
        self
    }

    /// Replace the bounded backoff policy used for replay-safe transport and gateway attempts.
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }
}

impl fmt::Debug for CommandCodeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandCodeConfig")
            .field("api_key", &"[redacted]")
            .field("model", &self.model)
            .field("host", &self.host)
            .field("max_tokens", &self.max_tokens)
            .field("permission_mode", &self.permission_mode)
            .field("thread_id", &self.thread_id)
            .field("mode", &self.mode)
            .field("temperature", &self.temperature)
            .field("zero_data_retention", &self.zero_data_retention)
            .field("project_slug", &self.project_slug)
            .field("taste_learning_enabled", &self.taste_learning_enabled)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}
fn nonempty(value: String, field: &'static str) -> Result<String, CommandCodeConfigError> {
    if value.trim().is_empty() {
        Err(CommandCodeConfigError::EmptyField(field))
    } else {
        Ok(value)
    }
}

/// Match the canonical UUID form accepted by Command Code's current `z.uuid()` gate without
/// taking a UUID crate solely for request validation.
fn is_canonical_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || !matches!(bytes[8], b'-')
        || !matches!(bytes[13], b'-')
        || !matches!(bytes[18], b'-')
        || !matches!(bytes[23], b'-')
    {
        return false;
    }
    bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit())
}

pub(super) fn project_slug_from_working_directory(working_directory: &str) -> &str {
    working_directory
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|component| !component.is_empty())
        // Host context rejects an empty path, so this fallback only covers roots such as `/`.
        .unwrap_or("project")
}
