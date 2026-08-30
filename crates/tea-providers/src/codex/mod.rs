//! Direct ChatGPT-subscription Codex Responses provider.
//!
//! This optional adapter implements Tea-owned OAuth and the direct Codex
//! Responses SSE contract. It never shells out to Codex, reuses another
//! Codex credential file, sends a subscription bearer token to an OpenAI
//! Platform API origin, or impersonates a first-party client.

mod auth;
mod config;
mod context;
mod credentials;
mod error;
mod oauth;
mod payload;
mod provider;
mod stream;
mod wire;

pub use auth::{
    AuthError, Clock, CodexAuthManager, CodexAuthSnapshot, CodexAuthStatus, SystemClock,
    account_id_from_token,
};
#[cfg(any(test, feature = "provider-codex-test-support"))]
pub use config::{CodexCapturedRequest, CodexRequestCapture};
pub use config::{CodexConfig, CodexConfigError, CodexTextVerbosity};
pub use context::CodexContextHook;
pub use credentials::{
    CodexCredential, CredentialError, CredentialStore, FileCredentialStore,
    InMemoryCredentialStore, SecretString, abbreviate_account_id,
};
pub use error::{CodexErrorReport, CodexErrorSource};
pub use oauth::{
    BrowserAuthorizationMode, BrowserAuthorizationRequest, CodexOAuthClient, DeviceAuthorization,
    DevicePollResult, OAuthCallback, OAuthError, OAuthHttpClient, OsRandomSource,
    PkceAuthorization, RandomSource, TeaOAuthHttpClient, TokenGrant, browser_authorization_request,
    launch_browser, parse_callback, validate_callback,
};
pub use provider::CodexProvider;
pub use wire::{
    CODEX_API_ROOT, CODEX_WIRE_COMPAT_VERSION, OAUTH_ISSUER, PROVIDER_ID, PROVIDER_LABEL,
};
