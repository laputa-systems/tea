//! Explicit caller-owned Anthropic configuration.

use super::catalog::AnthropicCompat;
use crate::scheduler::ModelPricing;
use crate::RetryPolicy;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Fixed public API origin.
pub const API_ORIGIN: &str = "https://api.anthropic.com";
/// API version header value.
pub const API_VERSION: &str = "2023-06-01";

/// Prompt-cache retention requested for cache breakpoints.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheRetention {
    /// Send no cache breakpoints.
    None,
    /// Five-minute ephemeral entries.
    #[default]
    Short,
    /// One-hour entries, where the model supports them.
    Long,
}

/// How thinking text is returned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThinkingDisplay {
    /// Summarized visible thinking.
    #[default]
    Summarized,
    /// No visible thinking text; signatures still return for continuity.
    Omitted,
}

impl ThinkingDisplay {
    pub(crate) const fn as_wire(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// Exact request bodies observed at the send boundary, for tests and
/// trusted diagnostics. Credentials are headers and never appear here.
#[derive(Clone, Default)]
pub struct AnthropicRequestCapture {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

/// One captured request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedRequest {
    /// Exact body bytes.
    pub body: Vec<u8>,
    /// The `anthropic-beta` header, when sent.
    pub beta_header: Option<String>,
}

impl AnthropicRequestCapture {
    pub(crate) fn observe(&self, body: &[u8], beta_header: Option<&str>) {
        self.requests
            .lock()
            .expect("Anthropic request capture mutex poisoned")
            .push(CapturedRequest {
                body: body.to_vec(),
                beta_header: beta_header.map(str::to_owned),
            });
    }

    /// Captured requests in send order.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests
            .lock()
            .expect("Anthropic request capture mutex poisoned")
            .clone()
    }
}

impl fmt::Debug for AnthropicRequestCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AnthropicRequestCapture").finish_non_exhaustive()
    }
}

/// Explicit configuration for one Anthropic model.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) compat: AnthropicCompat,
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output_tokens: u32,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) cache_retention: CacheRetention,
    pub(crate) thinking_display: ThinkingDisplay,
    pub(crate) temperature: Option<f64>,
    pub(crate) request_timeout: Duration,
    pub(crate) stall_timeout: Duration,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) maximum_retry_delay: Duration,
    pub(crate) request_capture: Option<AnthropicRequestCapture>,
    pub(crate) user_agent: String,
    pub(crate) test_origin: Option<String>,
}

impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicConfig")
            .field("api_key", &"[redacted]")
            .field("model", &self.model)
            .field("compat", &self.compat)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("cache_retention", &self.cache_retention)
            .finish_non_exhaustive()
    }
}

/// Invalid explicit configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnthropicConfigError {
    /// A required value was empty.
    EmptyField(&'static str),
    /// The API key cannot be sent safely as a header.
    UnsafeApiKey,
    /// The output-token ceiling was zero.
    ZeroMaxTokens,
    /// A timeout was zero.
    ZeroTimeout,
    /// The temperature was outside `0.0..=1.0`.
    InvalidTemperature,
}

impl fmt::Display for AnthropicConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "Anthropic {field} must not be empty"),
            Self::UnsafeApiKey => formatter.write_str("Anthropic API key contains a line break"),
            Self::ZeroMaxTokens => formatter.write_str("Anthropic max output tokens must be positive"),
            Self::ZeroTimeout => formatter.write_str("Anthropic timeouts must be positive"),
            Self::InvalidTemperature => {
                formatter.write_str("Anthropic temperature must be between 0 and 1")
            }
        }
    }
}

impl std::error::Error for AnthropicConfigError {}

impl AnthropicConfig {
    /// Configure one model. Listed models carry their catalog switches and
    /// limits; any other model uses [`AnthropicCompat::conservative`] until the
    /// caller states its capabilities with [`Self::with_compat`].
    pub fn try_new(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, AnthropicConfigError> {
        let api_key = api_key.into();
        let model = model.into();
        if api_key.trim().is_empty() {
            return Err(AnthropicConfigError::EmptyField("API key"));
        }
        if api_key.contains(['\r', '\n']) {
            return Err(AnthropicConfigError::UnsafeApiKey);
        }
        if model.trim().is_empty() {
            return Err(AnthropicConfigError::EmptyField("model"));
        }
        let listed = super::catalog::model(&model);
        Ok(Self {
            api_key,
            compat: listed.map_or(AnthropicCompat::conservative(), |model| model.compat),
            context_window: listed.map(|model| model.context_window),
            max_output_tokens: listed.map_or(4_096, |model| model.max_output_tokens),
            pricing: listed.and_then(|model| model.pricing).map(|pricing| pricing.to_pricing()),
            model,
            cache_retention: CacheRetention::Short,
            thinking_display: ThinkingDisplay::Summarized,
            temperature: None,
            request_timeout: Duration::from_secs(600),
            stall_timeout: Duration::from_secs(120),
            retry_policy: RetryPolicy::standard(),
            maximum_retry_delay: Duration::from_secs(60),
            request_capture: None,
            user_agent: format!("tea/{}", env!("CARGO_PKG_VERSION")),
            test_origin: None,
        })
    }

    /// Replace the behavior switches for this model.
    pub fn with_compat(mut self, compat: AnthropicCompat) -> Self {
        self.compat = compat;
        self
    }

    /// State the context capacity.
    pub fn with_context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Replace the default output-token ceiling.
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Result<Self, AnthropicConfigError> {
        if tokens == 0 {
            return Err(AnthropicConfigError::ZeroMaxTokens);
        }
        self.max_output_tokens = tokens;
        Ok(self)
    }

    /// State exact token prices in decimal dollars per million tokens.
    pub fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Select cache retention.
    pub fn with_cache_retention(mut self, retention: CacheRetention) -> Self {
        self.cache_retention = retention;
        self
    }

    /// Select visible-thinking display.
    pub fn with_thinking_display(mut self, display: ThinkingDisplay) -> Self {
        self.thinking_display = display;
        self
    }

    /// Request a sampling temperature where the model and thinking mode allow it.
    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, AnthropicConfigError> {
        if !(0.0..=1.0).contains(&temperature) {
            return Err(AnthropicConfigError::InvalidTemperature);
        }
        self.temperature = Some(temperature);
        Ok(self)
    }

    /// Replace request and stall timeouts.
    pub fn with_timeouts(
        mut self,
        request: Duration,
        stall: Duration,
    ) -> Result<Self, AnthropicConfigError> {
        if request.is_zero() || stall.is_zero() {
            return Err(AnthropicConfigError::ZeroTimeout);
        }
        self.request_timeout = request;
        self.stall_timeout = stall;
        Ok(self)
    }

    /// Replace the bounded retry policy.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Fail instead of retrying when the server requests a longer delay.
    pub fn with_maximum_retry_delay(mut self, delay: Duration) -> Self {
        self.maximum_retry_delay = delay;
        self
    }

    /// Capture exact request bodies.
    pub fn with_request_capture(mut self, capture: AnthropicRequestCapture) -> Self {
        self.request_capture = Some(capture);
        self
    }

    /// Point requests at a loopback fixture origin. Test support only.
    #[cfg(any(test, feature = "provider-anthropic-test-support"))]
    pub fn with_test_origin(mut self, origin: impl Into<String>) -> Self {
        self.test_origin = Some(origin.into());
        self
    }

    /// Configured model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Behavior switches.
    pub fn compat(&self) -> AnthropicCompat {
        self.compat
    }

    /// Exact prices, when known.
    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    pub(crate) fn messages_url(&self) -> String {
        format!(
            "{}/v1/messages",
            self.test_origin.as_deref().unwrap_or(API_ORIGIN)
        )
    }
}
