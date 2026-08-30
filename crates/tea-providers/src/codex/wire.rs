//! Fixed ChatGPT Codex OAuth and Responses wire-contract constants.

/// Stable Tea provider identifier.
pub const PROVIDER_ID: &str = "codex";
/// Picker-visible provider label.
pub const PROVIDER_LABEL: &str = "Codex (ChatGPT subscription)";

/// Fixed direct Codex API root. Subscription credentials never cross to an
/// OpenAI Platform API origin.
pub const CODEX_API_ROOT: &str = "https://chatgpt.com/backend-api/codex";
/// Fixed direct Responses endpoint.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Fixed OAuth issuer.
pub const OAUTH_ISSUER: &str = "https://auth.openai.com";
/// Public native OAuth client identifier. This is intentionally not a secret.
pub const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Browser authorization endpoint.
pub const OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// OAuth token endpoint.
pub const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// OAuth revocation endpoint.
pub const OAUTH_REVOCATION_URL: &str = "https://auth.openai.com/oauth/revoke";
/// Primary allowlisted browser callback.
pub const OAUTH_PRIMARY_REDIRECT: &str = "http://localhost:1455/auth/callback";
/// Fallback allowlisted browser callback.
pub const OAUTH_FALLBACK_REDIRECT: &str = "http://localhost:1457/auth/callback";
/// Device authorization initiation endpoint.
pub const OAUTH_DEVICE_USER_CODE_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
/// Device authorization polling endpoint.
pub const OAUTH_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// User-facing device verification page.
pub const OAUTH_DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
/// Redirect URI used only to exchange a completed device authorization code.
pub const OAUTH_DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";

/// Tea's honest Codex backend originator identity.
pub const ORIGINATOR: &str = "tea";
/// Pinned experimental Responses negotiation header value.
pub const RESPONSES_BETA: &str = "responses=experimental";

/// Direct Responses request header names. Keeping these centralized makes the
/// pinned wire contract auditable without dispersing identity spellings.
pub const HEADER_AUTHORIZATION: &str = "Authorization";
pub const HEADER_ACCOUNT_ID: &str = "ChatGPT-Account-ID";
pub const HEADER_ORIGINATOR: &str = "originator";
pub const HEADER_VERSION: &str = "version";
pub const HEADER_USER_AGENT: &str = "User-Agent";
pub const HEADER_OPENAI_BETA: &str = "OpenAI-Beta";
pub const HEADER_ACCEPT: &str = "Accept";
pub const HEADER_CONTENT_TYPE: &str = "Content-Type";
pub const HEADER_SESSION_ID: &str = "session-id";
pub const HEADER_CLIENT_REQUEST_ID: &str = "x-client-request-id";

/// Codex package version whose Responses contract Tea implements.
///
/// Provenance: OpenAI Codex commit `63d213884daea50e4f74efc192cdc44f549b67d5`
/// (`codex-rs/Cargo.toml` declares the workspace package version). Update this
/// value only with the contract fixtures and tests that establish a new wire
/// baseline.
pub const CODEX_WIRE_COMPAT_VERSION: &str = "0.0.0";

/// Maximum retained trusted-host provider response diagnostic prefix.
pub const MAX_DIAGNOSTIC_RESPONSE_BYTES: usize = 2_048;
/// Maximum one complete SSE record before a provider protocol error is raised.
pub const MAX_SSE_RECORD_BYTES: usize = 1_048_576;

/// Build Tea's honest client identifier without borrowing a first-party name.
pub fn tea_user_agent() -> String {
    format!("tea/{}", env!("CARGO_PKG_VERSION"))
}
