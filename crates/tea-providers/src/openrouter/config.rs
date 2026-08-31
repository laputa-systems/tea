//! Explicit OpenRouter configuration contracts.

use super::super::retry::RetryPolicy;
use super::transport::COMPLETIONS_URL;
use std::{
    collections::BTreeSet,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tea_protocol::JsonValue;

/// The two safe route identifiers OpenRouter can return in response headers.
///
/// This deliberately excludes every other response header, including values
/// that may be request- or account-specific. A missing field is an honest
/// absence of observation, not a default route inference.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenRouterReturnedRoute {
    /// OpenRouter's returned model identifier, when its response exposed one.
    pub model: Option<String>,
    /// OpenRouter's selected upstream provider, when its response exposed one.
    pub provider: Option<String>,
}

impl OpenRouterReturnedRoute {
    /// Whether OpenRouter exposed either whitelisted route identifier.
    pub fn is_observed(&self) -> bool {
        self.model.is_some() || self.provider.is_some()
    }
}

/// A deliberately narrow inspection seam for an OpenRouter request and its
/// whitelisted returned route.
///
/// The provider never enables this by itself. Hosts that own private evidence
/// can opt in, then persist or inspect exact JSON request bytes and only the
/// `x-openrouter-provider` / `x-openrouter-model` response values. Credentials
/// and all other HTTP headers remain outside this boundary. Keeping the request
/// capture at the payload/send boundary is important: callers must not
/// reconstruct a request from higher-level model state after the fact.
#[derive(Clone, Default)]
pub struct OpenRouterRequestCapture {
    payloads: Arc<Mutex<Vec<Vec<u8>>>>,
    returned_route: Arc<Mutex<OpenRouterReturnedRoute>>,
}

impl OpenRouterRequestCapture {
    /// Record one exact serialized request payload before HTTP headers are added.
    pub fn observe(&self, payload: &[u8]) {
        self.payloads
            .lock()
            .expect("OpenRouter request capture mutex poisoned")
            .push(payload.to_vec());
    }

    /// Return a stable snapshot without consuming evidence needed by another
    /// post-run observer.
    pub fn payloads(&self) -> Vec<Vec<u8>> {
        self.payloads
            .lock()
            .expect("OpenRouter request capture mutex poisoned")
            .clone()
    }

    /// Record the latest whitelisted OpenRouter route headers, if present.
    ///
    /// This mirrors Pi's response observation: a response with either route
    /// header replaces the prior observed route, while a response without both
    /// leaves the prior observation intact.
    pub fn observe_response_headers(&self, headers: &[(String, String)]) {
        let mut provider = None;
        let mut model = None;
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("x-openrouter-provider") {
                provider = Some(value.clone());
            }
            if name.eq_ignore_ascii_case("x-openrouter-model") {
                model = Some(value.clone());
            }
        }
        if provider.is_some() || model.is_some() {
            *self
                .returned_route
                .lock()
                .expect("OpenRouter request capture mutex poisoned") = OpenRouterReturnedRoute {
                model,
                provider,
            };
        }
    }

    /// Return the latest whitelisted route observation without consuming it.
    pub fn returned_route(&self) -> OpenRouterReturnedRoute {
        self.returned_route
            .lock()
            .expect("OpenRouter request capture mutex poisoned")
            .clone()
    }
}

/// Error raised when explicit OpenRouter configuration violates an adapter invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenRouterConfigError {
    /// A required caller-supplied text value was empty.
    EmptyField(&'static str),
    /// The maximum output token cap was zero.
    ZeroMaxTokens,
    /// The sampling temperature was outside OpenRouter's supported range.
    InvalidTemperature,
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
            Self::InvalidTemperature => {
                formatter.write_str("OpenRouter temperature must be finite and between zero and two")
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
#[derive(Clone)]
pub struct OpenRouterConfig {
    pub(super) api_key: String,
    pub(super) model: String,
    // This stays unset for every production configuration. The narrowly scoped test-support
    // feature can replace it with a loopback HTTP/1.1 fixture endpoint for real-binary tests.
    pub(super) test_completion_url: Option<String>,
    /// Optional provider request hint. `None` leaves output length to the provider/model.
    pub(super) max_tokens: Option<u64>,
    /// Optional sampling temperature. `None` leaves sampling to the provider/model.
    pub(super) temperature: Option<f64>,
    /// Optional deterministic sampling seed.
    pub(super) seed: Option<u64>,
    pub(super) request_timeout: Duration,
    pub(super) stall_timeout: Duration,
    pub(super) retry_policy: RetryPolicy,
    // An explicit policy is optional so regular Tea/OpenRouter operation keeps
    // its established `require_parameters` behavior. The shootout supplies a
    // controlled policy to both native harnesses instead of changing that
    // production default.
    pub(super) provider_routing: Option<JsonValue>,
    /// Optional model-facing tool allowlist. The host may retain additional
    /// durable execution tools while exposing only a closed subset to the
    /// provider request.
    pub(super) model_tool_allowlist: Option<BTreeSet<String>>,
    pub(super) request_capture: Option<OpenRouterRequestCapture>,
}

impl OpenRouterConfig {
    /// Configure one OpenRouter model with the evaluation default output cap of 1024 tokens.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            test_completion_url: None,
            max_tokens: None,
            temperature: None,
            seed: None,
            request_timeout: Duration::from_secs(300),
            stall_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::standard(),
            provider_routing: None,
            model_tool_allowlist: None,
            request_capture: None,
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

    /// Set the provider-facing sampling temperature.
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set the provider-facing deterministic sampling seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
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

    /// Override the OpenRouter routing object for this explicit provider
    /// instance. This is used by controlled evaluations; normal callers may
    /// leave it unset and retain the adapter's existing tool-routing default.
    pub fn with_provider_routing(mut self, provider_routing: JsonValue) -> Self {
        self.provider_routing = Some(provider_routing);
        self
    }

    /// Restrict the model-facing tool definitions for this explicit provider
    /// instance while retaining the host's complete execution registry.
    pub fn with_model_tool_allowlist<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.model_tool_allowlist = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Observe exact serialized payloads at the final pre-HTTP boundary.
    pub fn with_request_capture(mut self, request_capture: OpenRouterRequestCapture) -> Self {
        self.request_capture = Some(request_capture);
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
        if self
            .temperature
            .is_some_and(|temperature| {
                !temperature.is_finite() || !(0.0..=2.0).contains(&temperature)
            })
        {
            return Err(OpenRouterConfigError::InvalidTemperature);
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
            .field("temperature", &self.temperature)
            .field("seed", &self.seed)
            .field("request_timeout", &self.request_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .field("retry_policy", &self.retry_policy)
            .field("provider_routing", &self.provider_routing)
            .field(
                "request_capture",
                &self.request_capture.as_ref().map(|_| "enabled"),
            )
            .finish()
    }
}
