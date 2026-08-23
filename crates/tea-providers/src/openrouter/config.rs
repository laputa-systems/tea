//! Explicit OpenRouter configuration contracts.

use super::super::retry::RetryPolicy;
use super::transport::COMPLETIONS_URL;
use std::{fmt, time::Duration};

/// Error raised when explicit OpenRouter configuration violates an adapter invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenRouterConfigError {
    /// A required caller-supplied text value was empty.
    EmptyField(&'static str),
    /// The maximum output token cap was zero.
    ZeroMaxTokens,
    /// The HTTP request timeout was zero.
    ZeroRequestTimeout,
    /// The response-stall timeout was zero.
    ZeroStallTimeout,
    /// The API key contains a line break and cannot be represented safely in an HTTP header.
    ApiKeyContainsLineBreak,
}

impl fmt::Display for OpenRouterConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "OpenRouter {field} must not be empty"),
            Self::ZeroMaxTokens => {
                formatter.write_str("OpenRouter max tokens must be greater than zero")
            }
            Self::ZeroRequestTimeout => {
                formatter.write_str("OpenRouter request timeout must be greater than zero")
            }
            Self::ZeroStallTimeout => {
                formatter.write_str("OpenRouter stall timeout must be greater than zero")
            }
            Self::ApiKeyContainsLineBreak => {
                formatter.write_str("OpenRouter API key must not contain line breaks")
            }
        }
    }
}

impl std::error::Error for OpenRouterConfigError {}

/// Caller-owned configuration for [`OpenRouterProvider`].
///
/// The API key is supplied directly by the embedding. This adapter never reads an environment
/// variable, a home-directory credential, or a provider configuration file.
#[derive(Clone, Eq, PartialEq)]
pub struct OpenRouterConfig {
    pub(super) api_key: String,
    pub(super) model: String,
    // This stays unset for every production configuration. The narrowly scoped test-support
    // feature can replace it with a loopback HTTP/1.1 fixture endpoint for real-binary tests.
    pub(super) test_completion_url: Option<String>,
    /// Optional provider request hint. `None` leaves output length to the provider/model.
    pub(super) max_tokens: Option<u64>,
    pub(super) request_timeout: Duration,
    pub(super) stall_timeout: Duration,
    pub(super) retry_policy: RetryPolicy,
}

impl OpenRouterConfig {
    /// Configure one OpenRouter model with the evaluation default output cap of 1024 tokens.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            test_completion_url: None,
            max_tokens: None,
            request_timeout: Duration::from_secs(300),
            stall_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::standard(),
        }
    }

    /// Borrow the explicitly configured OpenRouter model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the explicit maximum completion-token cap.
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Replace the bounded HTTP request timeout used by the OpenRouter transport.
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Borrow the configured HTTP request timeout.
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Replace the bounded no-progress timeout used by the OpenRouter transport.
    pub fn with_stall_timeout(mut self, stall_timeout: Duration) -> Self {
        self.stall_timeout = stall_timeout;
        self
    }

    /// Borrow the configured no-progress timeout.
    pub fn stall_timeout(&self) -> Duration {
        self.stall_timeout
    }

    /// Replace the bounded backoff policy used for replay-safe transport attempts.
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Replace the OpenRouter completion URL for an offline test fixture.
    ///
    /// This test-support-only method is not compiled into normal adapter builds. It exists so a
    /// real `tea` binary can be verified against a local, deterministic HTTP/1.1 server
    /// without supplying credentials to or contacting an external provider.
    #[cfg(any(test, feature = "provider-openrouter-test-support"))]
    pub fn with_test_completion_url(mut self, url: impl Into<String>) -> Self {
        self.test_completion_url = Some(url.into());
        self
    }

    pub(super) fn completion_url(&self) -> &str {
        self.test_completion_url
            .as_deref()
            .unwrap_or(COMPLETIONS_URL)
    }

    /// Validate configuration before a host admits it for provider use.
    pub fn validate(&self) -> Result<(), OpenRouterConfigError> {
        if self.api_key.trim().is_empty() {
            return Err(OpenRouterConfigError::EmptyField("API key"));
        }
        if self.model.trim().is_empty() {
            return Err(OpenRouterConfigError::EmptyField("model"));
        }
        if self.max_tokens == Some(0) {
            return Err(OpenRouterConfigError::ZeroMaxTokens);
        }
        if self.request_timeout.is_zero() {
            return Err(OpenRouterConfigError::ZeroRequestTimeout);
        }
        if self.stall_timeout.is_zero() {
            return Err(OpenRouterConfigError::ZeroStallTimeout);
        }
        if self.api_key.contains(['\n', '\r']) {
            return Err(OpenRouterConfigError::ApiKeyContainsLineBreak);
        }
        Ok(())
    }

    /// Construct and validate explicit configuration in one operation.
    pub fn try_new(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, OpenRouterConfigError> {
        let config = Self::new(api_key, model);
        config.validate()?;
        Ok(config)
    }
}

impl fmt::Debug for OpenRouterConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterConfig")
            .field("api_key", &"[redacted]")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("request_timeout", &self.request_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}
