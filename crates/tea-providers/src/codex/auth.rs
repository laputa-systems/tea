//! Shared, refresh-rotating Codex authentication manager.

use super::credentials::{
    CodexCredential, CredentialError, CredentialStore, SecretString, abbreviate_account_id,
};
use super::oauth::{CodexOAuthClient, OAuthError, TokenGrant};
use crate::scheduler::CancellationToken;
use base64::Engine as _;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REFRESH_SAFETY_WINDOW_MS: u64 = 5 * 60 * 1_000;

/// Injectable wall clock for deterministic authentication tests.
pub trait Clock: Send + Sync {
    /// Return Unix milliseconds.
    fn now_unix_ms(&self) -> Result<u64, AuthError>;
}

/// Production system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> Result<u64, AuthError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::Clock)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
    }
}

/// Request-safe snapshot derived immediately before a direct backend attempt.
#[derive(Clone, Eq, PartialEq)]
pub struct CodexAuthSnapshot {
    /// Bearer token for one direct `chatgpt.com` request.
    pub access_token: SecretString,
    /// Required ChatGPT account identity.
    pub account_id: String,
}

impl fmt::Debug for CodexAuthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAuthSnapshot")
            .field("access_token", &"[redacted]")
            .field("account_id", &abbreviate_account_id(&self.account_id))
            .finish()
    }
}

/// Safe terminal status projection of the current explicit credential record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexAuthStatus {
    /// Whether an explicit Tea-owned credential record exists.
    pub logged_in: bool,
    /// Safely abbreviated account identity when logged in.
    pub account_id: Option<String>,
    /// Stored access-token expiry timestamp when logged in.
    pub expires_at_unix_ms: Option<u64>,
    /// Whether a prior permanent refresh failure requires interactive login.
    pub login_required: bool,
}

#[derive(Default)]
struct RefreshState {
    refreshing: bool,
    login_required: bool,
}

struct AuthInner {
    store: Arc<dyn CredentialStore>,
    oauth: CodexOAuthClient,
    clock: Arc<dyn Clock>,
    refresh: Mutex<RefreshState>,
    refresh_finished: Condvar,
}

/// Tea-owned source of fresh Codex request credentials.
///
/// The manager never discovers a path itself; its caller supplies one explicit
/// store. A provider receives this shared manager and cannot read environment
/// variables, home directories, browser storage, or another Codex client's
/// refresh token.
#[derive(Clone)]
pub struct CodexAuthManager {
    inner: Arc<AuthInner>,
}

impl fmt::Debug for CodexAuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAuthManager")
            .field("credential_path", &self.credential_path())
            .finish_non_exhaustive()
    }
}

impl CodexAuthManager {
    /// Construct an auth manager from explicit credential, OAuth, and clock
    /// dependencies. This constructor performs no I/O.
    pub fn new(
        store: Arc<dyn CredentialStore>,
        oauth: CodexOAuthClient,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(AuthInner {
                store,
                oauth,
                clock,
                refresh: Mutex::new(RefreshState::default()),
                refresh_finished: Condvar::new(),
            }),
        }
    }

    /// Construct a production manager with an explicit Tea-owned store.
    pub fn with_system_clock(store: Arc<dyn CredentialStore>) -> Self {
        Self::new(store, CodexOAuthClient::default(), Arc::new(SystemClock))
    }

    /// Explicit credential path for a safe status display, when file-backed.
    pub fn credential_path(&self) -> Option<&std::path::Path> {
        self.inner.store.path()
    }

    /// Load current status without refreshing or sending network traffic.
    pub fn status(&self) -> Result<CodexAuthStatus, AuthError> {
        let credential = self.inner.store.load().map_err(AuthError::Credential)?;
        let state = self.inner.refresh.lock().map_err(|_| AuthError::Internal)?;
        Ok(match credential {
            Some(credential) => CodexAuthStatus {
                logged_in: true,
                account_id: Some(abbreviate_account_id(credential.account_id())),
                expires_at_unix_ms: Some(credential.expires_at_unix_ms()),
                login_required: state.login_required,
            },
            None => CodexAuthStatus {
                logged_in: false,
                account_id: None,
                expires_at_unix_ms: None,
                login_required: state.login_required,
            },
        })
    }

    /// Persist a fresh browser/device authorization grant and return its first
    /// request-safe snapshot only after atomic persistence succeeds.
    pub fn install_grant(
        &self,
        grant: TokenGrant,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let now = self.inner.clock.now_unix_ms()?;
        let expires_at = expiry_from(now, grant.expires_in_seconds)?;
        let account_id = account_id_from_grant(&grant)?;
        let refresh_token = grant.refresh_token.ok_or(AuthError::MissingRefreshToken)?;
        let credential = CodexCredential::new(
            grant.access_token,
            refresh_token,
            expires_at,
            account_id,
            now,
        )
        .map_err(AuthError::Credential)?;
        self.inner
            .store
            .save(&credential)
            .map_err(AuthError::Credential)?;
        let mut state = self.inner.refresh.lock().map_err(|_| AuthError::Internal)?;
        state.login_required = false;
        Ok(snapshot(&credential))
    }

    /// Return a fresh snapshot, refreshing inside the five-minute safety
    /// window when necessary.
    pub fn snapshot(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        self.snapshot_inner(false, cancellation)
    }

    /// Force exactly one fresh-token path after a pre-stream HTTP 401.
    pub fn force_refresh(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        self.snapshot_inner(true, cancellation)
    }

    /// Remove Tea-owned credentials even if remote revocation fails.
    pub fn logout(&self, cancellation: &CancellationToken) -> Result<(), AuthError> {
        let credential = self.inner.store.load().map_err(AuthError::Credential)?;
        if let Some(credential) = credential {
            let _ = self
                .inner
                .oauth
                .revoke(credential.refresh_token(), cancellation);
        }
        self.inner.store.remove().map_err(AuthError::Credential)?;
        let mut state = self.inner.refresh.lock().map_err(|_| AuthError::Internal)?;
        state.login_required = false;
        Ok(())
    }

    fn snapshot_inner(
        &self,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        if self
            .inner
            .refresh
            .lock()
            .map_err(|_| AuthError::Internal)?
            .login_required
        {
            return Err(AuthError::LoginRequired);
        }
        let credential = self
            .inner
            .store
            .load()
            .map_err(AuthError::Credential)?
            .ok_or(AuthError::LoginRequired)?;
        let now = self.inner.clock.now_unix_ms()?;
        if !force && credential_is_fresh(&credential, now) {
            return Ok(snapshot(&credential));
        }
        self.refresh(credential, force, cancellation)
    }

    fn refresh(
        &self,
        observed: CodexCredential,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            let mut state = self.inner.refresh.lock().map_err(|_| AuthError::Internal)?;
            if state.login_required {
                return Err(AuthError::LoginRequired);
            }
            if !state.refreshing {
                state.refreshing = true;
                drop(state);
                let result = self.refresh_as_leader(observed.clone(), force, cancellation);
                let mut state = self.inner.refresh.lock().map_err(|_| AuthError::Internal)?;
                state.refreshing = false;
                if matches!(
                    result,
                    Err(AuthError::PermanentRefresh | AuthError::LoginRequired)
                ) {
                    state.login_required = true;
                }
                self.inner.refresh_finished.notify_all();
                return result;
            }
            let (state, _) = self
                .inner
                .refresh_finished
                .wait_timeout(state, Duration::from_millis(20))
                .map_err(|_| AuthError::Internal)?;
            drop(state);
            let refreshed = self
                .inner
                .store
                .load()
                .map_err(AuthError::Credential)?
                .ok_or(AuthError::LoginRequired)?;
            let now = self.inner.clock.now_unix_ms()?;
            if credential_is_fresh(&refreshed, now) && refreshed != observed {
                return Ok(snapshot(&refreshed));
            }
        }
    }

    fn refresh_as_leader(
        &self,
        observed: CodexCredential,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<CodexAuthSnapshot, AuthError> {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let _lock = self
            .inner
            .store
            .acquire_refresh_lock(cancellation)
            .map_err(AuthError::Credential)?;
        // Another Tea process may have completed a rotating refresh while this
        // process waited. Reload under the held cross-process lock and use the
        // committed replacement rather than racing its old refresh token.
        let current = self
            .inner
            .store
            .load()
            .map_err(AuthError::Credential)?
            .ok_or(AuthError::LoginRequired)?;
        let now = self.inner.clock.now_unix_ms()?;
        if credential_is_fresh(&current, now) && current != observed {
            return Ok(snapshot(&current));
        }
        if !force && credential_is_fresh(&current, now) {
            return Ok(snapshot(&current));
        }

        let grant = self.refresh_with_transient_retry(current.refresh_token(), cancellation)?;
        let expires_at = expiry_from(now, grant.expires_in_seconds)?;
        let account_id = account_id_from_grant_or_existing(&grant, current.account_id())?;
        let replacement = current
            .refreshed(
                grant.access_token,
                grant.refresh_token,
                expires_at,
                account_id,
                now,
            )
            .map_err(AuthError::Credential)?;
        // The replacement is visible to request code only after this atomic
        // store commit succeeds.
        self.inner
            .store
            .save(&replacement)
            .map_err(AuthError::Credential)?;
        Ok(snapshot(&replacement))
    }

    fn refresh_with_transient_retry(
        &self,
        refresh_token: &SecretString,
        cancellation: &CancellationToken,
    ) -> Result<TokenGrant, AuthError> {
        let mut delay = Duration::from_millis(250);
        for attempt in 0..=2 {
            if cancellation.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            match self.inner.oauth.refresh(refresh_token, cancellation) {
                Ok(grant) => return Ok(grant),
                Err(OAuthError::PermanentRefresh) => return Err(AuthError::PermanentRefresh),
                Err(OAuthError::Cancelled) => return Err(AuthError::Cancelled),
                Err(error) if transient_oauth_error(&error) && attempt < 2 => {
                    wait_with_cancellation(delay, cancellation)?;
                    delay = delay.saturating_mul(2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(AuthError::OAuth(error)),
            }
        }
        Err(AuthError::Internal)
    }
}

fn credential_is_fresh(credential: &CodexCredential, now: u64) -> bool {
    credential.expires_at_unix_ms() > now.saturating_add(REFRESH_SAFETY_WINDOW_MS)
}

fn snapshot(credential: &CodexCredential) -> CodexAuthSnapshot {
    CodexAuthSnapshot {
        access_token: credential.access_token().clone(),
        account_id: credential.account_id().to_owned(),
    }
}

fn expiry_from(now: u64, expires_in_seconds: u64) -> Result<u64, AuthError> {
    now.checked_add(
        expires_in_seconds
            .checked_mul(1_000)
            .ok_or(AuthError::InvalidExpiry)?,
    )
    .ok_or(AuthError::InvalidExpiry)
}

fn account_id_from_grant(grant: &TokenGrant) -> Result<String, AuthError> {
    account_id_from_token(&grant.access_token)
        .or_else(|| grant.id_token.as_ref().and_then(account_id_from_token))
        .ok_or(AuthError::MissingAccountId)
}

fn account_id_from_grant_or_existing(
    grant: &TokenGrant,
    existing: &str,
) -> Result<String, AuthError> {
    Ok(account_id_from_token(&grant.access_token)
        .or_else(|| grant.id_token.as_ref().and_then(account_id_from_token))
        .unwrap_or_else(|| existing.to_owned()))
}

/// Decode an untrusted JWT payload only to obtain the server-issued account
/// routing claim. This is not signature verification or local authorization.
pub fn account_id_from_token(token: &SecretString) -> Option<String> {
    let payload = token.expose().split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let text = std::str::from_utf8(&decoded).ok()?;
    let value = crate::json::JsonValue::parse(text).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(str::to_owned)
}

fn transient_oauth_error(error: &OAuthError) -> bool {
    matches!(
        error,
        OAuthError::Transport
            | OAuthError::HttpStatus(500 | 502 | 503 | 504)
            | OAuthError::CallbackServer
    )
}

fn wait_with_cancellation(
    delay: Duration,
    cancellation: &CancellationToken,
) -> Result<(), AuthError> {
    let started = std::time::Instant::now();
    while started.elapsed() < delay {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(20).min(delay.saturating_sub(started.elapsed())));
    }
    if cancellation.is_cancelled() {
        Err(AuthError::Cancelled)
    } else {
        Ok(())
    }
}

/// Authentication-manager failure without credential values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    /// No Tea-owned current credential is available.
    LoginRequired,
    /// A refresh token was permanently invalidated, reused, expired, or revoked.
    PermanentRefresh,
    /// Interactive token exchange omitted the required rotating refresh token.
    MissingRefreshToken,
    /// Token material did not contain a ChatGPT account ID claim.
    MissingAccountId,
    /// OAuth expiry arithmetic overflowed or was unusable.
    InvalidExpiry,
    /// The caller cancelled refresh or credential work.
    Cancelled,
    /// System clock was unavailable.
    Clock,
    /// Credential persistence boundary failed.
    Credential(CredentialError),
    /// OAuth boundary failed.
    OAuth(OAuthError),
    /// Internal synchronization failed.
    Internal,
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoginRequired | Self::PermanentRefresh => {
                formatter.write_str("Codex login is required; run `tea auth login codex`")
            }
            Self::MissingRefreshToken => {
                formatter.write_str("Codex OAuth response omitted a refresh token")
            }
            Self::MissingAccountId => {
                formatter.write_str("Codex OAuth response did not contain a ChatGPT account ID")
            }
            Self::InvalidExpiry => {
                formatter.write_str("Codex OAuth response had an invalid expiry")
            }
            Self::Cancelled => formatter.write_str("Codex authentication was cancelled"),
            Self::Clock => formatter.write_str("Codex authentication clock failed"),
            Self::Credential(error) => error.fmt(formatter),
            Self::OAuth(error) => error.fmt(formatter),
            Self::Internal => formatter.write_str("Codex authentication synchronization failed"),
        }
    }
}

impl std::error::Error for AuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::credentials::InMemoryCredentialStore;
    use crate::codex::oauth::{OAuthHttpClient, OsRandomSource};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_unix_ms(&self) -> Result<u64, AuthError> {
            Ok(self.0)
        }
    }

    struct ScriptedOAuth {
        responses: Mutex<VecDeque<(u16, &'static [u8])>>,
        calls: AtomicUsize,
        first_call_delay: Option<Duration>,
    }

    impl ScriptedOAuth {
        fn new(
            responses: impl IntoIterator<Item = (u16, &'static [u8])>,
            first_call_delay: Option<Duration>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: AtomicUsize::new(0),
                first_call_delay,
            }
        }
    }

    impl OAuthHttpClient for ScriptedOAuth {
        fn send(
            &self,
            _request: tea_http::TransportRequest,
            _cancellation: &CancellationToken,
        ) -> Result<tea_http::TransportResponse, OAuthError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0
                && let Some(delay) = self.first_call_delay
            {
                std::thread::sleep(delay);
            }
            let (status_code, body) = self
                .responses
                .lock()
                .expect("scripted OAuth response queue")
                .pop_front()
                .expect("refresh must have one scripted response");
            Ok(tea_http::TransportResponse {
                status_code,
                headers: Vec::new(),
                body: body.to_vec(),
            })
        }
    }

    fn expired_credential() -> CodexCredential {
        CodexCredential::new(
            SecretString::new("old-access").expect("fixture access"),
            SecretString::new("old-refresh").expect("fixture refresh"),
            1_000,
            "acct_12345678",
            1,
        )
        .expect("fixture credential")
    }

    fn manager_with(
        store: Arc<InMemoryCredentialStore>,
        oauth: Arc<ScriptedOAuth>,
    ) -> CodexAuthManager {
        CodexAuthManager::new(
            store,
            CodexOAuthClient::new(oauth, Arc::new(OsRandomSource)),
            Arc::new(FixedClock(10_000)),
        )
    }

    #[test]
    fn extracts_account_id_from_untrusted_access_jwt_payload() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1234"}}"#);
        let token = SecretString::new(format!("header.{payload}.signature")).unwrap();
        assert_eq!(account_id_from_token(&token).as_deref(), Some("acct_1234"));
    }

    #[test]
    fn account_id_falls_back_to_a_padded_id_token_payload() {
        let payload = base64::engine::general_purpose::URL_SAFE
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_from_id"}}"#);
        let grant = TokenGrant {
            access_token: SecretString::new("not-a-jwt").unwrap(),
            refresh_token: Some(SecretString::new("refresh").unwrap()),
            expires_in_seconds: 3_600,
            id_token: Some(SecretString::new(format!("header.{payload}.signature")).unwrap()),
        };
        assert_eq!(account_id_from_grant(&grant).unwrap(), "acct_from_id");
    }

    #[test]
    fn refresh_persists_the_complete_replacement_and_retains_omitted_rotation() {
        let store = Arc::new(InMemoryCredentialStore::with_credential(
            expired_credential(),
        ));
        let oauth = Arc::new(ScriptedOAuth::new(
            [(
                200,
                br#"{"access_token":"new-access","expires_in":3600}"#.as_slice(),
            )],
            None,
        ));
        let manager = manager_with(Arc::clone(&store), Arc::clone(&oauth));
        let snapshot = manager
            .snapshot(&CancellationToken::new())
            .expect("expired credential should refresh");

        assert_eq!(snapshot.access_token.expose(), "new-access");
        assert_eq!(snapshot.account_id, "acct_12345678");
        let persisted = store
            .load()
            .expect("read committed replacement")
            .expect("replacement should exist");
        assert_eq!(persisted.access_token().expose(), "new-access");
        assert_eq!(persisted.refresh_token().expose(), "old-refresh");
        assert_eq!(oauth.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_expired_snapshots_share_one_refresh_commit() {
        let store = Arc::new(InMemoryCredentialStore::with_credential(
            expired_credential(),
        ));
        let oauth = Arc::new(ScriptedOAuth::new(
            [(
                200,
                br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
                    .as_slice(),
            )],
            Some(Duration::from_millis(60)),
        ));
        let manager = manager_with(Arc::clone(&store), Arc::clone(&oauth));
        let first = manager.clone();
        let second = manager.clone();
        let first = std::thread::spawn(move || first.snapshot(&CancellationToken::new()));
        let second = std::thread::spawn(move || second.snapshot(&CancellationToken::new()));

        assert_eq!(
            first
                .join()
                .expect("first refresh worker joins")
                .expect("first snapshot"),
            second
                .join()
                .expect("second refresh worker joins")
                .expect("second snapshot"),
        );
        assert_eq!(oauth.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .load()
                .expect("read coordinated replacement")
                .expect("replacement exists")
                .refresh_token()
                .expose(),
            "new-refresh",
        );
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretString::new("secret-value").unwrap();
        let debug = format!("{secret:?}");
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn permanent_refresh_marks_the_manager_login_required_without_replaying_it() {
        let store = Arc::new(InMemoryCredentialStore::with_credential(
            expired_credential(),
        ));
        let oauth = Arc::new(ScriptedOAuth::new(
            [(400, br#"{"error":"invalid_grant"}"#.as_slice())],
            None,
        ));
        let manager = manager_with(store, Arc::clone(&oauth));
        let cancellation = CancellationToken::new();

        assert_eq!(
            manager.snapshot(&cancellation),
            Err(AuthError::PermanentRefresh),
        );
        assert_eq!(oauth.calls.load(Ordering::SeqCst), 1);
        assert!(
            manager
                .status()
                .expect("status remains readable")
                .login_required
        );
        assert_eq!(
            manager.snapshot(&cancellation),
            Err(AuthError::LoginRequired)
        );
        assert_eq!(oauth.calls.load(Ordering::SeqCst), 1);
    }
}
