//! Validated runtime configuration for the direct Codex provider.

use super::auth::CodexAuthManager;
use super::wire::{CODEX_RESPONSES_URL, PROVIDER_ID};
use crate::retry::RetryPolicy;
use std::fmt;
use std::sync::Arc;
#[cfg(any(test, feature = "provider-codex-test-support"))]
use std::sync::Mutex;
use std::time::Duration;

/// Typed Codex Responses text verbosity choices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexTextVerbosity {
    /// Concise default output requested by Tea.
    Low,
}

impl CodexTextVerbosity {
    pub(crate) const fn as_wire(self) -> &'static str {
        match self {
            Self::Low => "low",
        }
    }
}

/// A stripped request record suitable for deterministic test assertions.
#[cfg(any(test, feature = "provider-codex-test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCapturedRequest {
    /// Exact serialized JSON payload.
    pub payload: Vec<u8>,
    /// Headers with authorization values redacted.
    pub headers: Vec<(String, String)>,
}

/// Explicit request-boundary capture for offline contract tests.
#[cfg(any(test, feature = "provider-codex-test-support"))]
#[derive(Clone, Default)]
pub struct CodexRequestCapture {
    requests: Arc<Mutex<Vec<CodexCapturedRequest>>>,
}

#[cfg(any(test, feature = "provider-codex-test-support"))]
impl CodexRequestCapture {
    /// Record one exact body and already-redacted header list.
    pub fn observe(&self, payload: Vec<u8>, headers: Vec<(String, String)>) {
        self.requests
            .lock()
            .expect("Codex request capture mutex poisoned")
            .push(CodexCapturedRequest { payload, headers });
    }

    /// Return a stable owned snapshot of captured requests.
    pub fn requests(&self) -> Vec<CodexCapturedRequest> {
        self.requests
            .lock()
            .expect("Codex request capture mutex poisoned")
            .clone()
    }
}

/// Direct Codex provider construction failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexConfigError {
    /// A required string was empty.
    EmptyField(&'static str),
    /// Complete request deadline cannot be zero.
    ZeroRequestTimeout,
    /// Response progress timeout cannot be zero.
    ZeroStallTimeout,
    /// A test endpoint does not name a loopback origin.
    UnsafeTestEndpoint,
    /// A protocol/header value contained a control character.
    UnsafeHeaderValue(&'static str),
}

impl fmt::Display for CodexConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "Codex {field} must not be empty"),
            Self::ZeroRequestTimeout => {
                formatter.write_str("Codex request timeout must be greater than zero")
            }
            Self::ZeroStallTimeout => {
                formatter.write_str("Codex response stall timeout must be greater than zero")
            }
            Self::UnsafeTestEndpoint => {
                formatter.write_str("Codex test endpoint must be a loopback HTTP origin")
            }
            Self::UnsafeHeaderValue(field) => {
                write!(
                    formatter,
                    "Codex {field} must not contain control characters"
                )
            }
        }
    }
}

impl std::error::Error for CodexConfigError {}

/// Explicit caller-owned configuration for [`super::CodexProvider`].
///
/// Production origins are fixed in `wire.rs`; only private test-support code
/// can inject a validated loopback response origin.
#[derive(Clone)]
pub struct CodexConfig {
    pub(super) model: String,
    pub(super) auth: Arc<CodexAuthManager>,
    pub(super) request_timeout: Duration,
    pub(super) stall_timeout: Duration,
    pub(super) retry_policy: RetryPolicy,
    pub(super) text_verbosity: CodexTextVerbosity,
    #[cfg(any(test, feature = "provider-codex-test-support"))]
    pub(super) request_capture: Option<CodexRequestCapture>,
    test_responses_url: Option<String>,
}

impl CodexConfig {
    /// Configure one explicit Codex model and explicit shared auth manager.
    pub fn new(auth: Arc<CodexAuthManager>, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            auth,
            request_timeout: Duration::from_secs(300),
            stall_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::standard(),
            text_verbosity: CodexTextVerbosity::Low,
            #[cfg(any(test, feature = "provider-codex-test-support"))]
            request_capture: None,
            test_responses_url: None,
        }
    }

    /// Construct and validate explicit configuration in one operation.
    pub fn try_new(
        auth: Arc<CodexAuthManager>,
        model: impl Into<String>,
    ) -> Result<Self, CodexConfigError> {
        let config = Self::new(auth, model);
        config.validate()?;
        Ok(config)
    }

    /// Borrow the configured provider-local model ID.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the complete direct Responses deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Replace the no-progress response deadline.
    pub fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    /// Replace bounded pre-stream retry policy.
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Enable a redacted request capture for deterministic tests.
    #[cfg(any(test, feature = "provider-codex-test-support"))]
    pub fn with_request_capture(mut self, capture: CodexRequestCapture) -> Self {
        self.request_capture = Some(capture);
        self
    }

    /// Inject a loopback-only endpoint for deterministic adapter tests.
    #[cfg(any(test, feature = "provider-codex-test-support"))]
    pub fn with_test_responses_url(mut self, url: impl Into<String>) -> Self {
        self.test_responses_url = Some(url.into());
        self
    }

    pub(super) fn responses_url(&self) -> &str {
        self.test_responses_url
            .as_deref()
            .unwrap_or(CODEX_RESPONSES_URL)
    }

    /// Validate configuration before provider construction or network I/O.
    pub fn validate(&self) -> Result<(), CodexConfigError> {
        if self.model.trim().is_empty() {
            return Err(CodexConfigError::EmptyField("model"));
        }
        if self.request_timeout.is_zero() {
            return Err(CodexConfigError::ZeroRequestTimeout);
        }
        if self.stall_timeout.is_zero() {
            return Err(CodexConfigError::ZeroStallTimeout);
        }
        if self.model.chars().any(char::is_control) {
            return Err(CodexConfigError::UnsafeHeaderValue("model"));
        }
        if let Some(url) = &self.test_responses_url
            && !loopback_test_endpoint(url)
        {
            return Err(CodexConfigError::UnsafeTestEndpoint);
        }
        Ok(())
    }

    /// Provider ID encoded by this configuration family.
    pub const fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }
}

impl fmt::Debug for CodexConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("CodexConfig");
        debug
            .field("model", &self.model)
            .field("auth", &self.auth)
            .field("request_timeout", &self.request_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("retry_policy", &self.retry_policy)
            .field("text_verbosity", &self.text_verbosity)
            .field(
                "test_responses_url",
                &self.test_responses_url.as_ref().map(|_| "loopback"),
            );
        #[cfg(any(test, feature = "provider-codex-test-support"))]
        debug.field(
            "request_capture",
            &self.request_capture.as_ref().map(|_| "enabled"),
        );
        debug.finish()
    }
}

fn loopback_test_endpoint(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let authority = rest
        .find(['/', '?', '#'])
        .map(|index| &rest[..index])
        .unwrap_or(rest);
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_control) {
        return false;
    }
    if let Some(port) = authority.strip_prefix("[::1]") {
        return valid_test_endpoint_port(port);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            matches!(host, "127.0.0.1" | "localhost") && valid_test_endpoint_port_suffix(port)
        }
        None => matches!(authority, "127.0.0.1" | "localhost"),
    }
}

fn valid_test_endpoint_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(valid_test_endpoint_port_suffix)
}

fn valid_test_endpoint_port_suffix(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|port| port != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::credentials::InMemoryCredentialStore;

    #[test]
    fn rejects_unsafe_test_endpoint_before_network_io() {
        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            InMemoryCredentialStore::new(),
        )));
        let config = CodexConfig::new(auth, "gpt-test");
        assert_eq!(config.provider_id(), "codex");
        assert_eq!(config.responses_url(), CODEX_RESPONSES_URL);
    }

    #[test]
    fn rejects_test_endpoint_userinfo_that_only_looks_like_loopback() {
        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            InMemoryCredentialStore::new(),
        )));
        let config = CodexConfig::new(auth, "gpt-test")
            .with_test_responses_url("http://127.0.0.1:1455@non-loopback.invalid/responses");

        assert_eq!(config.validate(), Err(CodexConfigError::UnsafeTestEndpoint));
    }
}
