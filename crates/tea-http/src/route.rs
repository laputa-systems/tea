//! Route authority and shared route-local scheduling state.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::client::RetryPolicy;

/// The HTTP methods understood by the small extension-facing transport.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HttpMethod {
    /// HTTP POST.
    Post,
}

impl HttpMethod {
    /// Parse the stable extension-facing method spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "POST" => Some(Self::Post),
            _ => None,
        }
    }

    /// Return the wire method spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
        }
    }
}

/// A fixed scheme and authority selected by trusted host composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    scheme: String,
    authority: String,
}

impl Origin {
    /// Create a fixed origin after validating that it cannot contain a path,
    /// query, fragment, or user information.
    pub fn new(scheme: impl Into<String>, authority: impl Into<String>) -> Result<Self, RouteError> {
        let scheme = scheme.into();
        let authority = authority.into();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(RouteError::InvalidOrigin("scheme must be http or https".into()));
        }
        if authority.is_empty()
            || authority.contains(['/', '?', '#', '@'])
            || authority.chars().any(char::is_whitespace)
        {
            return Err(RouteError::InvalidOrigin(
                "authority must be a bare host with an optional port".into(),
            ));
        }
        Ok(Self { scheme, authority })
    }

    /// Construct an HTTPS origin.
    pub fn https(authority: impl Into<String>) -> Result<Self, RouteError> {
        Self::new("https", authority)
    }

    /// Render an absolute URI only from a separately authorized path.
    pub(crate) fn uri(&self, path: &str) -> String {
        format!("{}://{}{}", self.scheme, self.authority, path)
    }

    /// A stable textual representation suitable for host binding identities.
    pub fn as_str(&self) -> String {
        format!("{}://{}", self.scheme, self.authority)
    }
}

/// Shared rate controls applied beneath every request on one route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RatePolicy {
    /// Maximum requests actively using this route at once.
    pub max_in_flight: NonZeroUsize,
    /// Optional minimum distance between request starts. `None` leaves rate
    /// selection to the upstream provider while preserving concurrency limits.
    pub minimum_interval: Option<Duration>,
}

impl RatePolicy {
    /// Create a route policy with an explicit non-zero concurrency ceiling.
    pub const fn new(max_in_flight: NonZeroUsize, minimum_interval: Option<Duration>) -> Self {
        Self {
            max_in_flight,
            minimum_interval,
        }
    }
}

/// Trusted policy for one named route. The name is an extension-visible route
/// selector, not a host or URL supplied by the extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Route {
    name: String,
    origin: Origin,
    allowed: BTreeSet<(HttpMethod, String)>,
    timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
    retry: RetryPolicy,
    rate: RatePolicy,
}

impl Route {
    /// Begin a named route with deliberately explicit transport bounds.
    pub fn new(
        name: impl Into<String>,
        origin: Origin,
        timeout: Duration,
        max_request_bytes: usize,
        max_response_bytes: usize,
        retry: RetryPolicy,
        rate: RatePolicy,
    ) -> Result<Self, RouteError> {
        let name = name.into();
        if name.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'))
        {
            return Err(RouteError::InvalidRouteName(name));
        }
        if timeout.is_zero() || max_request_bytes == 0 || max_response_bytes == 0 {
            return Err(RouteError::InvalidBounds);
        }
        Ok(Self {
            name,
            origin,
            allowed: BTreeSet::new(),
            timeout,
            max_request_bytes,
            max_response_bytes,
            retry,
            rate,
        })
    }

    /// Permit exactly one method/path pair on this fixed origin.
    pub fn allow(mut self, method: HttpMethod, path: impl Into<String>) -> Result<Self, RouteError> {
        let path = path.into();
        if !valid_path(&path) {
            return Err(RouteError::InvalidPath(path));
        }
        self.allowed.insert((method, path));
        Ok(self)
    }

    /// Route name available to the extension capability.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn authorize(&self, method: HttpMethod, path: &str) -> Result<(), RouteError> {
        if !valid_path(path) {
            return Err(RouteError::InvalidPath(path.into()));
        }
        if !self.allowed.iter().any(|(allowed_method, allowed_path)| *allowed_method == method && allowed_path == path) {
            return Err(RouteError::ForbiddenRequest {
                method: method.as_str().into(),
                path: path.into(),
            });
        }
        Ok(())
    }

    pub(crate) fn origin(&self) -> &Origin {
        &self.origin
    }

    pub(crate) const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) const fn max_request_bytes(&self) -> usize {
        self.max_request_bytes
    }

    pub(crate) const fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub(crate) const fn retry(&self) -> RetryPolicy {
        self.retry
    }

    pub(crate) const fn rate(&self) -> RatePolicy {
        self.rate
    }

    /// Return the stable host-selected route semantics named by a durable
    /// capability binding. It excludes mutable runtime handles and secrets.
    pub fn semantic_identity(&self) -> String {
        let allowed = self
            .allowed
            .iter()
            .map(|(method, path)| format!("{} {}", method.as_str(), path))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "network.http/v1;route={};origin={};allowed={allowed};request_bytes={};response_bytes={};timeout_ms={};retries={};backoff_initial_ms={};backoff_max_ms={};in_flight={};minimum_interval_ms={}",
            self.name,
            self.origin.as_str(),
            self.max_request_bytes,
            self.max_response_bytes,
            self.timeout.as_millis(),
            self.retry.max_retries(),
            self.retry.initial_delay().as_millis(),
            self.retry.max_delay().as_millis(),
            self.rate.max_in_flight,
            self.rate.minimum_interval.map_or(0, |interval| interval.as_millis()),
        )
    }
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['?', '#', '\\'])
        && !path.chars().any(char::is_whitespace)
}

/// Host-side route configuration or authorization failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteError {
    /// The route name is not portable/stable.
    InvalidRouteName(String),
    /// The origin does not name a bare supported origin.
    InvalidOrigin(String),
    /// The request path would escape the host's path authority boundary.
    InvalidPath(String),
    /// Request/response bounds or deadline are invalid.
    InvalidBounds,
    /// The route does not authorize this extension-visible method/path pair.
    ForbiddenRequest { method: String, path: String },
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRouteName(name) => write!(formatter, "invalid route name {name:?}"),
            Self::InvalidOrigin(message) => write!(formatter, "invalid origin: {message}"),
            Self::InvalidPath(path) => write!(formatter, "invalid route path {path:?}"),
            Self::InvalidBounds => formatter.write_str("route bounds and timeout must be non-zero"),
            Self::ForbiddenRequest { method, path } => {
                write!(formatter, "route does not allow {method} {path}")
            }
        }
    }
}

impl std::error::Error for RouteError {}
