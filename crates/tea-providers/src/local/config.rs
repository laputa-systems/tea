//! Explicit local OpenAI-compatible configuration contracts.

use super::LAGUNA_XS_2_1_MODEL;
use std::fmt;
use std::time::Duration;

/// Configuration failure at the local provider boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalConfigError {
    /// A required caller-supplied text value was empty.
    EmptyField(&'static str),
    /// The base URL must use an HTTP or HTTPS URL accepted by the shared transport.
    InvalidBaseUrl,
    /// The maximum output-token cap was zero.
    ZeroMaxTokens,
    /// A sampling value was not finite or was outside the server's accepted range.
    InvalidSampling(&'static str),
    /// The request timeout was zero.
    ZeroRequestTimeout,
}

impl fmt::Display for LocalConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "local {field} must not be empty"),
            Self::InvalidBaseUrl => {
                formatter.write_str("local base URL must start with http:// or https://")
            }
            Self::ZeroMaxTokens => {
                formatter.write_str("local max tokens must be greater than zero")
            }
            Self::InvalidSampling(field) => {
                write!(formatter, "local {field} must be finite and non-negative")
            }
            Self::ZeroRequestTimeout => {
                formatter.write_str("local request timeout must be greater than zero")
            }
        }
    }
}

impl std::error::Error for LocalConfigError {}

/// Caller-owned configuration for [`LocalProvider`].
#[derive(Clone, PartialEq)]
pub struct LocalConfig {
    pub(super) base_url: String,
    pub(super) model: String,
    pub(super) max_tokens: u64,
    pub(super) temperature: f64,
    pub(super) top_p: f64,
    pub(super) min_p: f64,
    pub(super) enable_thinking: bool,
    pub(super) request_timeout: Duration,
}

impl fmt::Debug for LocalConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("min_p", &self.min_p)
            .field("enable_thinking", &self.enable_thinking)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl LocalConfig {
    /// Configure a local OpenAI-compatible model with Laguna-compatible defaults.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            max_tokens: 4_096,
            temperature: 1.0,
            top_p: 1.0,
            min_p: 0.0,
            enable_thinking: true,
            request_timeout: Duration::from_secs(300),
        }
    }

    /// Configure the oMLX Laguna XS 2.1 5-bit endpoint with its known request defaults.
    pub fn laguna_xs_2_1(base_url: impl Into<String>) -> Self {
        Self::new(base_url, LAGUNA_XS_2_1_MODEL)
    }

    /// Borrow the configured local API root.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Borrow the configured model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the maximum number of generated tokens.
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Replace the sampling temperature.
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = temperature;
        self
    }

    /// Replace nucleus sampling probability.
    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = top_p;
        self
    }

    /// Replace minimum probability sampling threshold.
    pub fn with_min_p(mut self, min_p: f64) -> Self {
        self.min_p = min_p;
        self
    }

    /// Enable or disable the model's chat-template reasoning mode.
    pub fn with_thinking(mut self, enabled: bool) -> Self {
        self.enable_thinking = enabled;
        self
    }

    /// Replace the complete request timeout used by the local transport.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Validate the explicit configuration before constructing a provider.
    pub fn validate(&self) -> Result<(), LocalConfigError> {
        if self.base_url.trim().is_empty() {
            return Err(LocalConfigError::EmptyField("base URL"));
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err(LocalConfigError::InvalidBaseUrl);
        }
        if self.model.trim().is_empty() {
            return Err(LocalConfigError::EmptyField("model"));
        }
        if self.max_tokens == 0 {
            return Err(LocalConfigError::ZeroMaxTokens);
        }
        for (field, value) in [
            ("temperature", self.temperature),
            ("top_p", self.top_p),
            ("min_p", self.min_p),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(LocalConfigError::InvalidSampling(field));
            }
        }
        if self.request_timeout.is_zero() {
            return Err(LocalConfigError::ZeroRequestTimeout);
        }
        Ok(())
    }

    /// Construct and validate one explicit local configuration.
    pub fn try_new(
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LocalConfigError> {
        let config = Self::new(base_url, model);
        config.validate()?;
        Ok(config)
    }
}
