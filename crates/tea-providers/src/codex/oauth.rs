//! Direct ChatGPT OAuth authorization-code, PKCE, device, and token helpers.

use super::credentials::SecretString;
use super::wire::{
    OAUTH_AUTHORIZE_URL, OAUTH_CLIENT_ID, OAUTH_DEVICE_REDIRECT, OAUTH_DEVICE_TOKEN_URL,
    OAUTH_DEVICE_USER_CODE_URL, OAUTH_DEVICE_VERIFICATION_URL, OAUTH_FALLBACK_REDIRECT,
    OAUTH_ISSUER, OAUTH_PRIMARY_REDIRECT, OAUTH_REVOCATION_URL, OAUTH_TOKEN_URL, ORIGINATOR,
};
use crate::json::JsonValue;
use crate::scheduler::CancellationToken;
use crate::transport_runtime::client as http_client;
use base64::Engine as _;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tea_http::{TransportRequest, TransportResponse};

const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(900);
const CALLBACK_MAX_BYTES: usize = 16_384;

const QUERY_COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Randomness source injected for deterministic OAuth tests.
pub trait RandomSource: Send + Sync {
    /// Fill `output` with cryptographically secure random bytes in production.
    fn fill(&self, output: &mut [u8]) -> Result<(), OAuthError>;
}

/// Operating-system cryptographic random source.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsRandomSource;

impl RandomSource for OsRandomSource {
    fn fill(&self, output: &mut [u8]) -> Result<(), OAuthError> {
        getrandom::fill(output).map_err(|_| OAuthError::Randomness)
    }
}

/// Narrow synchronous OAuth HTTP boundary backed by `tea-http` in production.
pub trait OAuthHttpClient: Send + Sync {
    /// Execute one OAuth request through Tea's shared HTTP transport.
    fn send(
        &self,
        request: TransportRequest,
        cancellation: &CancellationToken,
    ) -> Result<TransportResponse, OAuthError>;
}

/// Production OAuth transport; it owns no endpoint selection beyond the fixed
/// URLs in this module.
#[derive(Clone, Copy, Debug, Default)]
pub struct TeaOAuthHttpClient;

impl OAuthHttpClient for TeaOAuthHttpClient {
    fn send(
        &self,
        request: TransportRequest,
        cancellation: &CancellationToken,
    ) -> Result<TransportResponse, OAuthError> {
        http_client()
            .send_blocking(request, cancellation)
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    OAuthError::Cancelled
                } else {
                    let _ = error;
                    OAuthError::Transport
                }
            })
    }
}

/// PKCE verifier, challenge, and callback state for exactly one authorization.
#[derive(Clone, Eq, PartialEq)]
pub struct PkceAuthorization {
    /// High-entropy verifier retained only until code exchange.
    pub verifier: String,
    /// S256 base64url-no-pad verifier digest sent in the browser URL.
    pub challenge: String,
    /// Independent callback-CSRF state.
    pub state: String,
}

impl fmt::Debug for PkceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PkceAuthorization")
            .field("verifier", &"[redacted]")
            .field("challenge", &"[redacted]")
            .field("state", &"[redacted]")
            .finish()
    }
}

impl PkceAuthorization {
    /// Generate an authorization state from the supplied secure random source.
    pub fn generate(random: &dyn RandomSource) -> Result<Self, OAuthError> {
        let mut verifier_bytes = [0_u8; 48];
        let mut state_bytes = [0_u8; 32];
        random.fill(&mut verifier_bytes)?;
        random.fill(&mut state_bytes)?;
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_bytes);
        Ok(Self {
            verifier,
            challenge,
            state,
        })
    }
}

/// Browser URL and exact callback metadata for one OAuth authorization.
#[derive(Clone, Eq, PartialEq)]
pub struct BrowserAuthorizationRequest {
    /// Fixed allowlisted callback used in the authorization URL.
    pub redirect_uri: String,
    /// OAuth authorization URL safe to print to a terminal.
    pub authorization_url: String,
    /// PKCE material retained only until exchange completion.
    pub pkce: PkceAuthorization,
}

impl fmt::Debug for BrowserAuthorizationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserAuthorizationRequest")
            .field("redirect_uri", &self.redirect_uri)
            .field("authorization_url", &"[redacted OAuth URL]")
            .field("pkce", &self.pkce)
            .finish()
    }
}

/// Build the exact least-privilege Tea browser authorization URL.
pub fn browser_authorization_request(
    redirect_uri: impl Into<String>,
    random: &dyn RandomSource,
) -> Result<BrowserAuthorizationRequest, OAuthError> {
    let redirect_uri = redirect_uri.into();
    if redirect_uri != OAUTH_PRIMARY_REDIRECT && redirect_uri != OAUTH_FALLBACK_REDIRECT {
        return Err(OAuthError::UnsafeRedirect);
    }
    let pkce = PkceAuthorization::generate(random)?;
    let query = form_encode(&[
        ("response_type", "code"),
        ("client_id", OAUTH_CLIENT_ID),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", pkce.state.as_str()),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]);
    Ok(BrowserAuthorizationRequest {
        redirect_uri,
        authorization_url: format!("{OAUTH_AUTHORIZE_URL}?{query}"),
        pkce,
    })
}

/// Parsed callback parameters before CSRF validation.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct OAuthCallback {
    /// Authorization code, if supplied.
    pub code: Option<String>,
    /// Returned CSRF state, if supplied.
    pub state: Option<String>,
    /// OAuth error code, if supplied.
    pub error: Option<String>,
    /// Provider error description, bounded only for safe terminal display.
    pub error_description: Option<String>,
}

impl fmt::Debug for OAuthCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCallback")
            .field("code", &self.code.as_ref().map(|_| "[redacted]"))
            .field("state", &self.state.as_ref().map(|_| "[redacted]"))
            .field("error", &self.error)
            .field(
                "error_description",
                &self.error_description.as_ref().map(|_| "[bounded]"),
            )
            .finish()
    }
}

/// Parse a complete callback URL, query string, or `code#state` manual value.
pub fn parse_callback(value: &str) -> Result<OAuthCallback, OAuthError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(OAuthError::MissingCode);
    }
    if value.len() > CALLBACK_MAX_BYTES {
        return Err(OAuthError::CallbackTooLarge);
    }
    // A manually copied authorization code is opaque and can itself contain
    // `=` padding. Recognize the documented `code#state` form before trying
    // to interpret it as a query string.
    if !value.contains('?') {
        if let Some((code, state)) = value.split_once('#') {
            return Ok(OAuthCallback {
                code: Some(code.to_owned()),
                state: Some(state.to_owned()),
                ..OAuthCallback::default()
            });
        }
        if !value.contains('&')
            && !value.starts_with("code=")
            && !value.starts_with("state=")
            && !value.starts_with("error=")
        {
            return Ok(OAuthCallback {
                code: Some(value.to_owned()),
                ..OAuthCallback::default()
            });
        }
    }
    let query = if let Some((_, query)) = value.split_once('?') {
        query.split('#').next().unwrap_or_default()
    } else {
        value.split('#').next().unwrap_or_default()
    };
    let mut callback = OAuthCallback::default();
    for pair in query.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_component(key)?;
        let value = decode_component(value)?;
        match key.as_str() {
            "code" => callback.code = Some(value),
            "state" => callback.state = Some(value),
            "error" => callback.error = Some(bound_error_code(value)),
            "error_description" => callback.error_description = Some(bound_description(value)),
            _ => {}
        }
    }
    Ok(callback)
}

/// Validate callback route, OAuth errors, state, and the required code.
pub fn validate_callback(
    callback: OAuthCallback,
    expected_state: &str,
) -> Result<String, OAuthError> {
    if let Some(error) = callback.error {
        return Err(OAuthError::AuthorizationDenied {
            code: error,
            description: callback.error_description,
        });
    }
    if callback.state.as_deref() != Some(expected_state) {
        return Err(OAuthError::StateMismatch);
    }
    callback
        .code
        .filter(|code| !code.is_empty())
        .ok_or(OAuthError::MissingCode)
}

/// Token material returned by the OAuth server before account extraction.
#[derive(Clone, Debug)]
pub struct TokenGrant {
    /// Bearer token for direct Codex Responses requests.
    pub access_token: SecretString,
    /// Rotating refresh token when the response supplied one.
    pub refresh_token: Option<SecretString>,
    /// Lifetime in seconds from the exchange/refresh response.
    pub expires_in_seconds: u64,
    /// Optional ID token used only as an account-ID fallback, then discarded.
    pub id_token: Option<SecretString>,
}

/// Device-code initiation response.
#[derive(Clone, Eq, PartialEq)]
pub struct DeviceAuthorization {
    /// Device authorization identity used only for polling.
    pub device_auth_id: String,
    /// Human-entered verification code.
    pub user_code: String,
    /// Fixed browser verification page for the displayed user code.
    pub verification_url: &'static str,
    /// Server-recommended polling interval.
    pub interval: Duration,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("device_auth_id", &"[redacted]")
            .field("user_code", &"[redacted]")
            .field("verification_url", &self.verification_url)
            .field("interval", &self.interval)
            .finish()
    }
}

/// Result of one device-token poll.
#[derive(Clone, Eq, PartialEq)]
pub enum DevicePollResult {
    /// User has not completed verification yet.
    Pending,
    /// The server requested slower polling.
    SlowDown,
    /// Authorization-code material ready for the ordinary token exchange.
    Authorized {
        /// One-time authorization code.
        authorization_code: String,
        /// PKCE verifier paired with that code.
        code_verifier: String,
    },
}

impl fmt::Debug for DevicePollResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("DevicePollResult::Pending"),
            Self::SlowDown => formatter.write_str("DevicePollResult::SlowDown"),
            Self::Authorized { .. } => {
                formatter.write_str("DevicePollResult::Authorized([redacted])")
            }
        }
    }
}

/// Direct fixed-origin OAuth client.
#[derive(Clone)]
pub struct CodexOAuthClient {
    transport: Arc<dyn OAuthHttpClient>,
    random: Arc<dyn RandomSource>,
}

impl fmt::Debug for CodexOAuthClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexOAuthClient")
            .field("issuer", &OAUTH_ISSUER)
            .finish_non_exhaustive()
    }
}

impl Default for CodexOAuthClient {
    fn default() -> Self {
        Self::new(Arc::new(TeaOAuthHttpClient), Arc::new(OsRandomSource))
    }
}

impl CodexOAuthClient {
    /// Construct an OAuth client from explicit transport and randomness seams.
    pub fn new(transport: Arc<dyn OAuthHttpClient>, random: Arc<dyn RandomSource>) -> Self {
        Self { transport, random }
    }

    /// Construct browser authorization metadata for one allowlisted callback.
    pub fn browser_request(
        &self,
        redirect_uri: impl Into<String>,
    ) -> Result<BrowserAuthorizationRequest, OAuthError> {
        browser_authorization_request(redirect_uri, self.random.as_ref())
    }

    /// Exchange a browser authorization code with the exact browser redirect URI.
    pub fn exchange_browser_code(
        &self,
        code: &str,
        request: &BrowserAuthorizationRequest,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        self.exchange_code(
            code,
            &request.pkce.verifier,
            &request.redirect_uri,
            cancellation,
        )
    }

    /// Exchange a device-flow authorization code through the shared code path.
    pub fn exchange_device_code(
        &self,
        code: &str,
        verifier: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        self.exchange_code(code, verifier, OAUTH_DEVICE_REDIRECT, cancellation)
    }

    /// Exchange a one-time OAuth code with PKCE verifier.
    pub fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        if cancellation.is_cancelled() {
            return Err(OAuthError::Cancelled);
        }
        if code.is_empty() || verifier.is_empty() {
            return Err(OAuthError::MissingCode);
        }
        if !matches!(
            redirect_uri,
            OAUTH_PRIMARY_REDIRECT | OAUTH_FALLBACK_REDIRECT | OAUTH_DEVICE_REDIRECT
        ) {
            return Err(OAuthError::UnsafeRedirect);
        }
        let body = form_encode(&[
            ("grant_type", "authorization_code"),
            ("client_id", OAUTH_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ]);
        let response = self.transport.send(
            TransportRequest::post(OAUTH_TOKEN_URL, body, OAUTH_REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json"),
            cancellation,
        )?;
        parse_token_response(response, true)
    }

    /// Exchange a rotating refresh token. A response may omit a replacement
    /// refresh token; callers retain the old one in that case.
    pub fn refresh(
        &self,
        refresh_token: &SecretString,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        if cancellation.is_cancelled() {
            return Err(OAuthError::Cancelled);
        }
        let body = form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose()),
            ("client_id", OAUTH_CLIENT_ID),
        ]);
        let response = self.transport.send(
            TransportRequest::post(OAUTH_TOKEN_URL, body, OAUTH_REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json"),
            cancellation,
        )?;
        parse_token_response(response, false)
    }

    /// Start ChatGPT's native Codex device flow.
    pub fn start_device(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<DeviceAuthorization, OAuthError> {
        if cancellation.is_cancelled() {
            return Err(OAuthError::Cancelled);
        }
        let body = JsonValue::object([("client_id", JsonValue::String(OAUTH_CLIENT_ID.into()))])
            .to_json_string()
            .map_err(|_| OAuthError::MalformedResponse)?;
        let response = self.transport.send(
            TransportRequest::post(OAUTH_DEVICE_USER_CODE_URL, body, OAUTH_REQUEST_TIMEOUT)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json"),
            cancellation,
        )?;
        if !(200..300).contains(&response.status_code) {
            return Err(OAuthError::HttpStatus(response.status_code));
        }
        let body = parse_json(&response.body)?;
        let object = body.as_object().ok_or(OAuthError::MalformedResponse)?;
        let device_auth_id = device_field(object, "device_auth_id", 4_096)?;
        let user_code = device_user_code(object)?;
        // The first-party client accepts both numeric and JSON-string interval
        // values. Keep that wire tolerance while bounding Tea's polling rate.
        let interval = optional_interval_seconds(object)?.unwrap_or(5).clamp(1, 60);
        Ok(DeviceAuthorization {
            device_auth_id,
            user_code,
            verification_url: OAUTH_DEVICE_VERIFICATION_URL,
            interval: Duration::from_secs(interval),
        })
    }

    /// Poll one device-flow status update without sleeping.
    pub fn poll_device(
        &self,
        device: &DeviceAuthorization,
        cancellation: &CancellationToken,
    ) -> Result<DevicePollResult, OAuthError> {
        if cancellation.is_cancelled() {
            return Err(OAuthError::Cancelled);
        }
        let body = JsonValue::object([
            (
                "device_auth_id",
                JsonValue::String(device.device_auth_id.clone()),
            ),
            ("user_code", JsonValue::String(device.user_code.clone())),
        ])
        .to_json_string()
        .map_err(|_| OAuthError::MalformedResponse)?;
        let response = self.transport.send(
            TransportRequest::post(OAUTH_DEVICE_TOKEN_URL, body, OAUTH_REQUEST_TIMEOUT)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json"),
            cancellation,
        )?;
        if matches!(response.status_code, 403 | 404) {
            if let Ok(body) = parse_json(&response.body)
                && let Some(object) = body.as_object()
                && let Some(error) = oauth_error_code(object)
            {
                return device_poll_error(error);
            }
            return Ok(DevicePollResult::Pending);
        }
        let body = parse_json(&response.body)?;
        let object = body.as_object().ok_or(OAuthError::MalformedResponse)?;
        if let Some(error) = oauth_error_code(object) {
            return device_poll_error(error);
        }
        if !(200..300).contains(&response.status_code) {
            return Err(OAuthError::HttpStatus(response.status_code));
        }
        Ok(DevicePollResult::Authorized {
            authorization_code: string_field(object, "authorization_code")?,
            code_verifier: string_field(object, "code_verifier")?,
        })
    }

    /// Poll until the user completes device authorization or its finite
    /// deadline/cancellation settles.
    pub fn wait_for_device_authorization(
        &self,
        device: &DeviceAuthorization,
        cancellation: &CancellationToken,
    ) -> Result<(String, String), OAuthError> {
        let deadline = Instant::now() + DEVICE_LOGIN_TIMEOUT;
        let mut interval = device.interval;
        loop {
            if cancellation.is_cancelled() {
                return Err(OAuthError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(OAuthError::DeviceTimedOut);
            }
            match self.poll_device(device, cancellation)? {
                DevicePollResult::Pending => {}
                DevicePollResult::SlowDown => {
                    interval = interval
                        .saturating_add(Duration::from_secs(5))
                        .min(Duration::from_secs(60));
                }
                DevicePollResult::Authorized {
                    authorization_code,
                    code_verifier,
                } => return Ok((authorization_code, code_verifier)),
            }
            wait_with_cancellation(interval, cancellation)?;
        }
    }

    /// Attempt OAuth token revocation. Callers intentionally remove local
    /// credentials even when this request fails.
    pub fn revoke(
        &self,
        token: &SecretString,
        cancellation: &CancellationToken,
    ) -> Result<(), OAuthError> {
        let body = form_encode(&[("token", token.expose()), ("client_id", OAUTH_CLIENT_ID)]);
        let response = self.transport.send(
            TransportRequest::post(OAUTH_REVOCATION_URL, body, OAUTH_REQUEST_TIMEOUT)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json"),
            cancellation,
        )?;
        if !(200..300).contains(&response.status_code) {
            return Err(OAuthError::HttpStatus(response.status_code));
        }
        Ok(())
    }
}

/// Bound one callback listener to the primary or fallback allowlisted port.
pub struct LoopbackBrowserAuthorization {
    listener: TcpListener,
    request: BrowserAuthorizationRequest,
}

impl fmt::Debug for LoopbackBrowserAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackBrowserAuthorization")
            .field("redirect_uri", &self.request.redirect_uri)
            .field("authorization_url", &"[redacted OAuth URL]")
            .finish_non_exhaustive()
    }
}

/// Browser login mode after probing the two allowlisted callback ports.
pub enum BrowserAuthorizationMode {
    /// A bounded loopback callback listener is ready.
    Loopback(LoopbackBrowserAuthorization),
    /// Neither allowlisted port was available; validate a pasted callback.
    Manual(BrowserAuthorizationRequest),
}

impl CodexOAuthClient {
    /// Bind the primary then fallback callback listener without choosing an
    /// arbitrary port. A manual flow remains available if both are occupied.
    pub fn begin_browser_authorization(&self) -> Result<BrowserAuthorizationMode, OAuthError> {
        for (address, redirect) in [
            ("127.0.0.1:1455", OAUTH_PRIMARY_REDIRECT),
            ("127.0.0.1:1457", OAUTH_FALLBACK_REDIRECT),
        ] {
            if let Ok(listener) = TcpListener::bind(address) {
                listener
                    .set_nonblocking(true)
                    .map_err(|_| OAuthError::CallbackServer)?;
                return Ok(BrowserAuthorizationMode::Loopback(
                    LoopbackBrowserAuthorization {
                        listener,
                        request: self.browser_request(redirect)?,
                    },
                ));
            }
        }
        Ok(BrowserAuthorizationMode::Manual(
            self.browser_request(OAUTH_PRIMARY_REDIRECT)?,
        ))
    }
}

impl LoopbackBrowserAuthorization {
    /// URL to print or launch in a browser.
    pub fn authorization_url(&self) -> &str {
        &self.request.authorization_url
    }

    /// Wait for one bounded, state-validated browser callback.
    pub fn wait_for_callback(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<String, OAuthError> {
        let deadline = Instant::now() + BROWSER_LOGIN_TIMEOUT;
        loop {
            if cancellation.is_cancelled() {
                return Err(OAuthError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(OAuthError::BrowserTimedOut);
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    match handle_callback_connection(&mut stream, &self.request) {
                        // A loopback port can receive speculative browser
                        // probes or unrelated local traffic. A malformed
                        // route must never be accepted, but it also should
                        // not let that traffic cancel a still-valid login.
                        Err(OAuthError::CallbackMalformed | OAuthError::CallbackTooLarge) => {}
                        outcome => return outcome,
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return Err(OAuthError::CallbackServer),
            }
        }
    }

    /// Exchange the callback's one-time code with the retained PKCE verifier.
    pub fn exchange(
        &self,
        oauth: &CodexOAuthClient,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        oauth.exchange_browser_code(code, &self.request, cancellation)
    }
}

impl BrowserAuthorizationMode {
    /// URL that a terminal may print even when browser launching is disabled.
    pub fn authorization_url(&self) -> &str {
        match self {
            Self::Loopback(flow) => flow.authorization_url(),
            Self::Manual(request) => &request.authorization_url,
        }
    }

    /// Exchange a manually pasted callback after state validation.
    pub fn complete_manual(
        &self,
        oauth: &CodexOAuthClient,
        callback: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        let request = match self {
            Self::Manual(request) => request,
            Self::Loopback(flow) => &flow.request,
        };
        let code = validate_callback(parse_callback(callback)?, &request.pkce.state)?;
        oauth.exchange_browser_code(&code, request, cancellation)
    }

    /// Complete the manual fallback from a separately pasted code and state.
    /// This keeps state validation intact when a browser or terminal cannot
    /// conveniently preserve the complete loopback callback URL.
    pub fn complete_manual_parts(
        &self,
        oauth: &CodexOAuthClient,
        code: &str,
        state: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, OAuthError> {
        let request = match self {
            Self::Manual(request) => request,
            Self::Loopback(flow) => &flow.request,
        };
        let code = validate_callback(
            OAuthCallback {
                code: Some(code.trim().to_owned()),
                state: Some(state.trim().to_owned()),
                ..OAuthCallback::default()
            },
            &request.pkce.state,
        )?;
        oauth.exchange_browser_code(&code, request, cancellation)
    }
}

/// Start the platform browser without a shell. A caller may treat failure as
/// nonfatal because the authorization URL is always printable.
pub fn launch_browser(url: &str) -> Result<(), OAuthError> {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result: Result<std::process::Child, std::io::Error> =
        Err(std::io::Error::other("unsupported platform"));
    result.map(|_| ()).map_err(|_| OAuthError::BrowserLaunch)
}

fn handle_callback_connection(
    stream: &mut TcpStream,
    request: &BrowserAuthorizationRequest,
) -> Result<String, OAuthError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| OAuthError::CallbackServer)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|_| OAuthError::CallbackServer)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > CALLBACK_MAX_BYTES {
            write_callback_response(stream, false)?;
            return Err(OAuthError::CallbackTooLarge);
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n")
            || bytes.windows(2).any(|window| window == b"\n\n")
        {
            break;
        }
    }
    let request_line = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.lines().next())
        .ok_or(OAuthError::CallbackMalformed)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let callback_path = target.split_once('?').map_or(target, |(path, _)| path);
    if method != "GET" || callback_path != "/auth/callback" {
        write_callback_response(stream, false)?;
        return Err(OAuthError::CallbackMalformed);
    }
    let callback = match parse_callback(target) {
        Ok(callback) => callback,
        Err(error) => {
            write_callback_response(stream, false)?;
            return Err(error);
        }
    };
    let result = validate_callback(callback, &request.pkce.state);
    write_callback_response(stream, result.is_ok())?;
    result
}

fn write_callback_response(stream: &mut TcpStream, success: bool) -> Result<(), OAuthError> {
    let body = if success {
        "<!doctype html><meta charset=\"utf-8\"><title>Tea login complete</title><p>Tea login complete. You may close this window.</p>"
    } else {
        "<!doctype html><meta charset=\"utf-8\"><title>Tea login failed</title><p>Tea login failed. Return to the terminal.</p>"
    };
    let status = if success { "200 OK" } else { "400 Bad Request" };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|_| OAuthError::CallbackServer)?;
    stream.flush().map_err(|_| OAuthError::CallbackServer)
}

fn parse_token_response(
    response: TransportResponse,
    require_refresh: bool,
) -> Result<TokenGrant, OAuthError> {
    if !(200..300).contains(&response.status_code) {
        return Err(classify_token_status(
            response.status_code,
            &response.body,
            // Only a refresh grant can safely collapse an otherwise opaque
            // definitive 4xx into the persistent re-login state. A browser
            // authorization-code exchange should retain its narrower error.
            !require_refresh,
        ));
    }
    let value = parse_json(&response.body)?;
    let object = value.as_object().ok_or(OAuthError::MalformedResponse)?;
    let access_token = SecretString::new(string_field(object, "access_token")?)
        .map_err(|_| OAuthError::MalformedResponse)?;
    let refresh_token = object
        .get("refresh_token")
        .and_then(JsonValue::as_str)
        .map(|value| SecretString::new(value.to_owned()).map_err(|_| OAuthError::MalformedResponse))
        .transpose()?;
    if require_refresh && refresh_token.is_none() {
        return Err(OAuthError::MalformedResponse);
    }
    let expires_in_seconds = object
        .get("expires_in")
        .and_then(JsonValue::as_u64)
        .filter(|value| *value > 0)
        .ok_or(OAuthError::MalformedResponse)?;
    let id_token = object
        .get("id_token")
        .and_then(JsonValue::as_str)
        .map(|value| SecretString::new(value.to_owned()).map_err(|_| OAuthError::MalformedResponse))
        .transpose()?;
    Ok(TokenGrant {
        access_token,
        refresh_token,
        expires_in_seconds,
        id_token,
    })
}

fn classify_token_status(status: u16, body: &[u8], refresh_grant: bool) -> OAuthError {
    let code = parse_json(body).ok().and_then(|value| {
        value
            .as_object()
            .and_then(oauth_error_code)
            .map(str::to_owned)
    });
    if matches!(status, 400 | 401 | 403)
        && (code.as_deref().is_some_and(|value| {
            matches!(
                value,
                "expired_token"
                    | "invalid_grant"
                    | "invalid_refresh_token"
                    | "invalid_token"
                    | "invalidated"
                    | "refresh_token_expired"
                    | "refresh_token_reused"
                    | "token_expired"
                    | "token_reused"
                    | "token_revoked"
            )
        }) || refresh_grant)
        {
            return OAuthError::PermanentRefresh;
        }
    OAuthError::HttpStatus(status)
}

fn parse_json(bytes: &[u8]) -> Result<JsonValue, OAuthError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| JsonValue::parse(value).ok())
        .ok_or(OAuthError::MalformedResponse)
}

fn string_field(
    object: &std::collections::BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<String, OAuthError> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(OAuthError::MalformedResponse)
}

fn device_field(
    object: &std::collections::BTreeMap<String, JsonValue>,
    field: &str,
    maximum_bytes: usize,
) -> Result<String, OAuthError> {
    let value = string_field(object, field)?;
    if value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(OAuthError::MalformedResponse);
    }
    Ok(value)
}

fn device_user_code(
    object: &std::collections::BTreeMap<String, JsonValue>,
) -> Result<String, OAuthError> {
    let value = object
        .get("user_code")
        .or_else(|| object.get("usercode"))
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(OAuthError::MalformedResponse)?;
    if value.len() > 128 || value.chars().any(char::is_control) {
        return Err(OAuthError::MalformedResponse);
    }
    Ok(value.to_owned())
}

fn device_poll_error(error: &str) -> Result<DevicePollResult, OAuthError> {
    match error {
        "deviceauth_authorization_pending" => Ok(DevicePollResult::Pending),
        "slow_down" => Ok(DevicePollResult::SlowDown),
        _ => Err(OAuthError::DeviceDenied),
    }
}

fn optional_interval_seconds(
    object: &std::collections::BTreeMap<String, JsonValue>,
) -> Result<Option<u64>, OAuthError> {
    let Some(value) = object.get("interval") else {
        return Ok(None);
    };
    if let Some(seconds) = value.as_u64() {
        return Ok(Some(seconds));
    }
    value
        .as_str()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Some)
        .ok_or(OAuthError::MalformedResponse)
}

fn oauth_error_code(object: &std::collections::BTreeMap<String, JsonValue>) -> Option<&str> {
    object.get("error").and_then(|error| {
        error
            .as_str()
            .or_else(|| error.get("code").and_then(JsonValue::as_str))
    })
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                utf8_percent_encode(key, QUERY_COMPONENT),
                utf8_percent_encode(value, QUERY_COMPONENT)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn decode_component(value: &str) -> Result<String, OAuthError> {
    percent_decode_str(&value.replace('+', " "))
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| OAuthError::CallbackMalformed)
}

fn bound_description(value: String) -> String {
    // The callback description is arbitrary, unauthenticated provider text.
    // Do not retain it: it can itself contain a one-time code or other opaque
    // secret that generic bearer/JWT redaction cannot reliably recognize.
    let _ = value;
    "provider supplied an authorization error description".into()
}

fn bound_error_code(value: String) -> String {
    let code = value
        .chars()
        .take(128)
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .collect::<String>();
    let code = code.to_ascii_lowercase();
    match code.as_str() {
        // Retain only standard OAuth/OIDC error names. Callback values are
        // remote input, so an arbitrary syntactically-safe string could still
        // be a one-time credential or other opaque secret.
        "access_denied"
        | "consent_required"
        | "interaction_required"
        | "invalid_client"
        | "invalid_grant"
        | "invalid_request"
        | "invalid_scope"
        | "login_required"
        | "server_error"
        | "temporarily_unavailable"
        | "unauthorized_client"
        | "unsupported_response_type" => code,
        _ => "authorization_error".into(),
    }
}

fn wait_with_cancellation(
    delay: Duration,
    cancellation: &CancellationToken,
) -> Result<(), OAuthError> {
    let started = Instant::now();
    while started.elapsed() < delay {
        if cancellation.is_cancelled() {
            return Err(OAuthError::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(20).min(delay.saturating_sub(started.elapsed())));
    }
    if cancellation.is_cancelled() {
        Err(OAuthError::Cancelled)
    } else {
        Ok(())
    }
}

/// Bounded OAuth failure classification. None of these variants retain token,
/// code, verifier, or callback values.
#[derive(Clone, Eq, PartialEq)]
pub enum OAuthError {
    /// The operating-system random source failed.
    Randomness,
    /// A redirect outside the two allowlisted callback values was requested.
    UnsafeRedirect,
    /// The callback did not carry a usable authorization code.
    MissingCode,
    /// Callback CSRF state did not match the generated state.
    StateMismatch,
    /// OAuth rejected authorization without retaining untrusted prose as instructions.
    AuthorizationDenied {
        /// Stable OAuth error code.
        code: String,
        /// Bounded human-facing description, when supplied.
        description: Option<String>,
    },
    /// The OAuth response was structurally malformed.
    MalformedResponse,
    /// A trusted OAuth endpoint returned a status code.
    HttpStatus(u16),
    /// Refresh is permanently invalid and requires a fresh login.
    PermanentRefresh,
    /// The direct OAuth transport failed before a trusted response.
    Transport,
    /// The caller cancelled the operation.
    Cancelled,
    /// Callback listener setup or I/O failed.
    CallbackServer,
    /// Callback request exceeded the bounded parser limit.
    CallbackTooLarge,
    /// Callback request line or route was invalid.
    CallbackMalformed,
    /// Browser login deadline elapsed.
    BrowserTimedOut,
    /// The platform browser command could not be started.
    BrowserLaunch,
    /// Device flow was denied or returned a definitive error.
    DeviceDenied,
    /// Device-flow deadline elapsed.
    DeviceTimedOut,
}

impl fmt::Debug for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthorizationDenied { code, .. } => formatter
                .debug_struct("OAuthError::AuthorizationDenied")
                .field("code", code)
                .field("description", &"[bounded and redacted]")
                .finish(),
            Self::HttpStatus(status) => formatter
                .debug_tuple("OAuthError::HttpStatus")
                .field(status)
                .finish(),
            Self::Randomness => formatter.write_str("OAuthError::Randomness"),
            Self::UnsafeRedirect => formatter.write_str("OAuthError::UnsafeRedirect"),
            Self::MissingCode => formatter.write_str("OAuthError::MissingCode"),
            Self::StateMismatch => formatter.write_str("OAuthError::StateMismatch"),
            Self::MalformedResponse => formatter.write_str("OAuthError::MalformedResponse"),
            Self::PermanentRefresh => formatter.write_str("OAuthError::PermanentRefresh"),
            Self::Transport => formatter.write_str("OAuthError::Transport"),
            Self::Cancelled => formatter.write_str("OAuthError::Cancelled"),
            Self::CallbackServer => formatter.write_str("OAuthError::CallbackServer"),
            Self::CallbackTooLarge => formatter.write_str("OAuthError::CallbackTooLarge"),
            Self::CallbackMalformed => formatter.write_str("OAuthError::CallbackMalformed"),
            Self::BrowserTimedOut => formatter.write_str("OAuthError::BrowserTimedOut"),
            Self::BrowserLaunch => formatter.write_str("OAuthError::BrowserLaunch"),
            Self::DeviceDenied => formatter.write_str("OAuthError::DeviceDenied"),
            Self::DeviceTimedOut => formatter.write_str("OAuthError::DeviceTimedOut"),
        }
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Randomness => formatter.write_str("secure OAuth randomness failed"),
            Self::UnsafeRedirect => formatter.write_str("OAuth redirect is not allowlisted"),
            Self::MissingCode => {
                formatter.write_str("OAuth callback did not include an authorization code")
            }
            Self::StateMismatch => formatter.write_str("OAuth callback state did not match"),
            Self::AuthorizationDenied { code, description } => {
                write!(formatter, "OAuth authorization failed: {code}")?;
                if let Some(description) = description {
                    write!(formatter, " ({description})")?;
                }
                Ok(())
            }
            Self::MalformedResponse => {
                formatter.write_str("OAuth server returned an invalid response")
            }
            Self::HttpStatus(status) => write!(formatter, "OAuth server returned HTTP {status}"),
            Self::PermanentRefresh => {
                formatter.write_str("Codex login has expired or was revoked; log in again")
            }
            Self::Transport => formatter.write_str("OAuth transport failed"),
            Self::Cancelled => formatter.write_str("OAuth operation was cancelled"),
            Self::CallbackServer => formatter.write_str("OAuth callback server failed"),
            Self::CallbackTooLarge => formatter.write_str("OAuth callback request was too large"),
            Self::CallbackMalformed => formatter.write_str("OAuth callback was malformed"),
            Self::BrowserTimedOut => formatter.write_str("OAuth browser login timed out"),
            Self::BrowserLaunch => {
                formatter.write_str("could not open a browser; use the printed OAuth URL")
            }
            Self::DeviceDenied => formatter.write_str("Codex device authorization was denied"),
            Self::DeviceTimedOut => formatter.write_str("Codex device authorization timed out"),
        }
    }
}

impl std::error::Error for OAuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct DeterministicRandom(u8);

    impl RandomSource for DeterministicRandom {
        fn fill(&self, output: &mut [u8]) -> Result<(), OAuthError> {
            for (index, byte) in output.iter_mut().enumerate() {
                *byte = self.0.wrapping_add(index as u8);
            }
            Ok(())
        }
    }

    struct ScriptedHttp {
        responses: Mutex<VecDeque<(u16, &'static [u8])>>,
    }

    impl ScriptedHttp {
        fn new(responses: impl IntoIterator<Item = (u16, &'static [u8])>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }
    }

    impl OAuthHttpClient for ScriptedHttp {
        fn send(
            &self,
            _request: TransportRequest,
            _cancellation: &CancellationToken,
        ) -> Result<TransportResponse, OAuthError> {
            let (status_code, body) = self
                .responses
                .lock()
                .expect("scripted OAuth response queue")
                .pop_front()
                .expect("OAuth request should have a scripted response");
            Ok(TransportResponse {
                status_code,
                headers: Vec::new(),
                body: body.to_vec(),
            })
        }
    }

    fn client(responses: impl IntoIterator<Item = (u16, &'static [u8])>) -> CodexOAuthClient {
        CodexOAuthClient::new(
            Arc::new(ScriptedHttp::new(responses)),
            Arc::new(DeterministicRandom(7)),
        )
    }

    #[test]
    fn pkce_matches_rfc7636_known_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn browser_request_has_exact_least_privilege_parameters() {
        let request =
            browser_authorization_request(OAUTH_PRIMARY_REDIRECT, &DeterministicRandom(7)).unwrap();
        assert!(request.authorization_url.starts_with(OAUTH_AUTHORIZE_URL));
        assert!(
            request
                .authorization_url
                .contains("scope=openid%20profile%20email%20offline_access")
        );
        assert!(request.authorization_url.contains("originator=tea"));
        assert!(!request.authorization_url.contains("api.connectors"));
        assert!(
            request
                .authorization_url
                .contains("code_challenge_method=S256")
        );
    }

    #[test]
    fn token_exchange_rejects_an_unallowlisted_redirect_before_transport() {
        let cancellation = CancellationToken::new();
        let client = client([(
            200,
            br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#.as_slice(),
        )]);
        assert!(matches!(
            client.exchange_code(
                "authorization-code",
                "verifier",
                "http://127.0.0.1:9999/auth/callback",
                &cancellation,
            ),
            Err(OAuthError::UnsafeRedirect)
        ));
    }

    #[test]
    fn form_encoding_pins_the_token_exchange_and_refresh_contract() {
        assert_eq!(
            form_encode(&[
                ("grant_type", "authorization_code"),
                ("client_id", OAUTH_CLIENT_ID),
                ("code", "code+/="),
                ("code_verifier", "verifier value"),
                ("redirect_uri", OAUTH_PRIMARY_REDIRECT),
            ]),
            format!(
                "grant_type=authorization_code&client_id={OAUTH_CLIENT_ID}&code=code%2B%2F%3D&code_verifier=verifier%20value&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            ),
        );
        assert_eq!(
            form_encode(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", "refresh value"),
                ("client_id", OAUTH_CLIENT_ID),
            ]),
            format!(
                "grant_type=refresh_token&refresh_token=refresh%20value&client_id={OAUTH_CLIENT_ID}",
            ),
        );
    }

    #[test]
    fn callback_rejects_state_mismatch_and_provider_error() {
        let mismatch =
            parse_callback("http://localhost:1455/auth/callback?code=x&state=no").unwrap();
        assert_eq!(
            validate_callback(mismatch, "yes"),
            Err(OAuthError::StateMismatch)
        );
        let error = parse_callback("?error=access_denied&error_description=No+thanks").unwrap();
        assert!(matches!(
            validate_callback(error, "yes"),
            Err(OAuthError::AuthorizationDenied { .. })
        ));
    }

    #[test]
    fn manual_code_and_state_are_accepted() {
        let callback = parse_callback("code-value#state-value").unwrap();
        assert_eq!(
            validate_callback(callback, "state-value").unwrap(),
            "code-value"
        );
    }

    #[test]
    fn malformed_loopback_callback_returns_a_failure_page() {
        use std::net::Shutdown;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let address = listener.local_addr().expect("listener address");
        let request = BrowserAuthorizationRequest {
            redirect_uri: OAUTH_PRIMARY_REDIRECT.into(),
            authorization_url: "https://auth.example/".into(),
            pkce: PkceAuthorization {
                verifier: "verifier".into(),
                challenge: "challenge".into(),
                state: "expected-state".into(),
            },
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept callback connection");
            handle_callback_connection(&mut stream, &request)
        });

        let mut client = TcpStream::connect(address).expect("connect callback client");
        client
            .write_all(
                b"GET /auth/callback?code=%FF&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .expect("send malformed callback");
        client.shutdown(Shutdown::Write).expect("finish request");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("read callback response");

        assert_eq!(
            server.join().expect("callback server thread"),
            Err(OAuthError::CallbackMalformed)
        );
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("Tea login failed"));
    }

    #[test]
    fn callback_and_oauth_debug_output_redact_ephemeral_credentials() {
        let pkce = PkceAuthorization {
            verifier: "verifier-secret".into(),
            challenge: "challenge-secret".into(),
            state: "state-secret".into(),
        };
        let request = BrowserAuthorizationRequest {
            redirect_uri: OAUTH_PRIMARY_REDIRECT.into(),
            authorization_url: "https://auth.example/?state=state-secret".into(),
            pkce: pkce.clone(),
        };
        let callback = OAuthCallback {
            code: Some("authorization-code-secret".into()),
            state: Some("state-secret".into()),
            error: None,
            error_description: None,
        };
        let device = DeviceAuthorization {
            device_auth_id: "device-auth-secret".into(),
            user_code: "user-code-secret".into(),
            verification_url: OAUTH_DEVICE_VERIFICATION_URL,
            interval: Duration::from_secs(1),
        };
        let authorized = DevicePollResult::Authorized {
            authorization_code: "authorization-code-secret".into(),
            code_verifier: "verifier-secret".into(),
        };
        let debug = format!("{pkce:?} {request:?} {callback:?} {device:?} {authorized:?}");
        for secret in [
            "verifier-secret",
            "challenge-secret",
            "state-secret",
            "authorization-code-secret",
            "device-auth-secret",
            "user-code-secret",
        ] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
    }

    #[test]
    fn authorization_error_description_is_bounded_and_redacted() {
        let callback =
            parse_callback("?error=access_denied&error_description=Bearer+aaaaaa.bbbbbb.cccccc")
                .expect("callback parameters parse");
        let error = validate_callback(callback, "state").expect_err("provider denial is terminal");
        let rendered = error.to_string();
        let debug = format!("{error:?}");
        assert!(!rendered.contains("aaaaaa.bbbbbb.cccccc"));
        assert!(!debug.contains("aaaaaa.bbbbbb.cccccc"));
    }

    #[test]
    fn authorization_error_description_never_retains_a_plain_authorization_code() {
        let callback = parse_callback(
            "?error=access_denied&error_description=authorization-code-plain-secret",
        )
        .expect("callback parameters parse");
        let error = validate_callback(callback, "state").expect_err("provider denial is terminal");
        assert!(
            !error
                .to_string()
                .contains("authorization-code-plain-secret")
        );
        assert!(!format!("{error:?}").contains("authorization-code-plain-secret"));
    }

    #[test]
    fn authorization_error_code_never_retains_an_arbitrary_callback_value() {
        let callback = parse_callback("?error=authorization-code-plain-secret").unwrap();
        let error = validate_callback(callback, "expected-state").unwrap_err();
        assert!(
            !error
                .to_string()
                .contains("authorization-code-plain-secret")
        );
        assert!(!format!("{error:?}").contains("authorization-code-plain-secret"));
    }

    #[test]
    fn token_exchange_requires_a_refresh_token_but_refresh_can_omit_rotation() {
        let cancellation = CancellationToken::new();
        let browser =
            browser_authorization_request(OAUTH_PRIMARY_REDIRECT, &DeterministicRandom(7))
                .expect("browser request");
        let client = client([
            (
                200,
                br#"{"access_token":"access","refresh_token":"rotated","expires_in":3600}"#
                    .as_slice(),
            ),
            (
                200,
                br#"{"access_token":"refreshed","expires_in":3600}"#.as_slice(),
            ),
        ]);

        let exchange = client
            .exchange_browser_code("one-time-code", &browser, &cancellation)
            .expect("browser exchange");
        assert_eq!(
            exchange.refresh_token,
            Some(SecretString::new("rotated").unwrap())
        );
        let refresh = client
            .refresh(
                &SecretString::new("existing-refresh").unwrap(),
                &cancellation,
            )
            .expect("refresh response without a new refresh token");
        assert_eq!(refresh.refresh_token, None);
    }

    #[test]
    fn device_poll_distinguishes_pending_slow_down_and_authorization() {
        let cancellation = CancellationToken::new();
        let device = DeviceAuthorization {
            device_auth_id: "device-id".into(),
            user_code: "ABCD-EFGH".into(),
            verification_url: OAUTH_DEVICE_VERIFICATION_URL,
            interval: Duration::from_secs(1),
        };
        let client = client([
            (403, br#"{}"#.as_slice()),
            (200, br#"{"error":"slow_down"}"#.as_slice()),
            (
                200,
                br#"{"authorization_code":"authorization-code","code_verifier":"device-verifier"}"#
                    .as_slice(),
            ),
        ]);

        assert_eq!(
            client
                .poll_device(&device, &cancellation)
                .expect("pending poll"),
            DevicePollResult::Pending,
        );
        assert_eq!(
            client
                .poll_device(&device, &cancellation)
                .expect("slow poll"),
            DevicePollResult::SlowDown,
        );
        assert_eq!(
            client
                .poll_device(&device, &cancellation)
                .expect("authorized poll"),
            DevicePollResult::Authorized {
                authorization_code: "authorization-code".into(),
                code_verifier: "device-verifier".into(),
            },
        );
    }

    #[test]
    fn device_flow_accepts_the_upstream_string_interval_and_nested_error_code() {
        let cancellation = CancellationToken::new();
        let client = client([
            (
                200,
                br#"{"device_auth_id":"device-id","user_code":"ABCD-EFGH","interval":"5"}"#
                    .as_slice(),
            ),
            (
                200,
                br#"{"error":{"code":"deviceauth_authorization_pending"}}"#.as_slice(),
            ),
        ]);
        let device = client
            .start_device(&cancellation)
            .expect("string device interval should be accepted");
        assert_eq!(device.interval, Duration::from_secs(5));
        assert_eq!(
            client
                .poll_device(&device, &cancellation)
                .expect("nested pending error should be accepted"),
            DevicePollResult::Pending,
        );
    }

    #[test]
    fn device_flow_rejects_control_characters_before_terminal_rendering() {
        let cancellation = CancellationToken::new();
        let client = client([(
            200,
            br#"{"device_auth_id":"device-id","user_code":"ABCD\nEFGH"}"#.as_slice(),
        )]);
        assert!(matches!(
            client.start_device(&cancellation),
            Err(OAuthError::MalformedResponse)
        ));
    }

    #[test]
    fn device_poll_treats_a_parseable_403_denial_as_terminal() {
        let cancellation = CancellationToken::new();
        let client = client([(403, br#"{"error":"access_denied"}"#.as_slice())]);
        let device = DeviceAuthorization {
            device_auth_id: "device-id".into(),
            user_code: "ABCD-EFGH".into(),
            verification_url: OAUTH_DEVICE_VERIFICATION_URL,
            interval: Duration::from_secs(1),
        };
        assert!(matches!(
            client.poll_device(&device, &cancellation),
            Err(OAuthError::DeviceDenied)
        ));
    }

    #[test]
    fn permanent_refresh_status_never_exposes_the_response_body() {
        let cancellation = CancellationToken::new();
        let client = client([(
            400,
            br#"{"error":"invalid_grant","access_token":"secret"}"#.as_slice(),
        )]);
        let error = client
            .refresh(&SecretString::new("refresh-token").unwrap(), &cancellation)
            .expect_err("invalid grant must require a new login");
        assert_eq!(error, OAuthError::PermanentRefresh);
        assert!(!error.to_string().contains("secret"));
        assert!(!format!("{error:?}").contains("refresh-token"));
    }

    #[test]
    fn nested_or_opaque_refresh_grant_failures_require_a_new_login() {
        let cancellation = CancellationToken::new();
        let nested = client([(
            400,
            br#"{"error":{"code":"refresh_token_reused"}}"#.as_slice(),
        )]);
        assert!(matches!(
            nested.refresh(&SecretString::new("refresh-token").unwrap(), &cancellation),
            Err(OAuthError::PermanentRefresh)
        ));

        let opaque = client([(401, br#"{}"#.as_slice())]);
        assert!(matches!(
            opaque.refresh(&SecretString::new("refresh-token").unwrap(), &cancellation),
            Err(OAuthError::PermanentRefresh)
        ));
    }
}
