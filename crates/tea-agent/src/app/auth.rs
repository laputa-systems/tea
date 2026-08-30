//! Terminal-owned direct Codex authorization commands.
//!
//! The provider crate owns OAuth mechanics and credential serialization; this
//! module owns the Tea state root, user interaction, and process output. No
//! adapter discovers a home directory or ambient credential path itself.

#[cfg(feature = "provider-codex")]
use std::ffi::OsString;
#[cfg(feature = "provider-codex")]
use std::io;
#[cfg(feature = "provider-codex")]
use std::sync::Arc;

use crate::cli::AuthCommand;

use super::error::AppError;
#[cfg(feature = "provider-codex")]
use super::runtime::resolve_tea_home;

/// Execute an explicit Tea-owned provider authorization operation.
pub(crate) fn run_auth_command(command: AuthCommand) -> Result<String, AppError> {
    #[cfg(feature = "provider-codex")]
    {
        run_codex_command(command)
    }
    #[cfg(not(feature = "provider-codex"))]
    {
        let _ = command;
        Err(AppError::Setup(
            "Codex support is not compiled into this tea binary; rebuild with --features provider-codex"
                .into(),
        ))
    }
}

#[cfg(feature = "provider-codex")]
fn run_codex_command(command: AuthCommand) -> Result<String, AppError> {
    use tea_core::scheduler::CancellationToken;
    use tea_providers::codex::{
        launch_browser, BrowserAuthorizationMode, CodexAuthManager, CodexOAuthClient,
        FileCredentialStore,
    };

    let (provider, tea_home) = match &command {
        AuthCommand::Login {
            provider, tea_home, ..
        }
        | AuthCommand::Logout { provider, tea_home }
        | AuthCommand::Status { provider, tea_home } => (provider, tea_home.as_deref()),
    };
    require_codex_provider(provider)?;
    let home = resolve_tea_home(tea_home)?;
    let store = Arc::new(FileCredentialStore::new(
        home.join("auth").join("codex.json"),
    ));
    let manager = CodexAuthManager::with_system_clock(store);
    let cancellation = CancellationToken::new();

    match command {
        AuthCommand::Status { .. } => {
            let status = manager
                .status()
                .map_err(|error| AppError::Setup(error.to_string()))?;
            if !status.logged_in {
                return Ok("Codex: not logged in".into());
            }
            let account = status.account_id.unwrap_or_else(|| "[redacted]".into());
            let renewal = if status.login_required {
                "; login required"
            } else {
                ""
            };
            Ok(format!(
                "Codex: logged in as {account}; expires_at_unix_ms={}{}",
                status.expires_at_unix_ms.unwrap_or_default(),
                renewal
            ))
        }
        AuthCommand::Logout { .. } => {
            manager
                .logout(&cancellation)
                .map_err(|error| AppError::Setup(error.to_string()))?;
            Ok("Codex: logged out; Tea-owned credentials were removed".into())
        }
        AuthCommand::Login {
            device, no_open, ..
        } => {
            let oauth = CodexOAuthClient::default();
            let grant = if device {
                let device = oauth
                    .start_device(&cancellation)
                    .map_err(|error| AppError::Setup(error.to_string()))?;
                println!(
                    "Open {} and enter code: {}",
                    device.verification_url, device.user_code
                );
                let (code, verifier) = oauth
                    .wait_for_device_authorization(&device, &cancellation)
                    .map_err(|error| AppError::Setup(error.to_string()))?;
                oauth
                    .exchange_device_code(&code, &verifier, &cancellation)
                    .map_err(|error| AppError::Setup(error.to_string()))?
            } else {
                let flow = oauth
                    .begin_browser_authorization()
                    .map_err(|error| AppError::Setup(error.to_string()))?;
                println!(
                    "Open this Codex authorization URL:\n{}",
                    flow.authorization_url()
                );
                if !no_open {
                    if let Err(error) = launch_browser(flow.authorization_url()) {
                        eprintln!("tea: {error}; continue with the URL printed above");
                    }
                }
                match &flow {
                    BrowserAuthorizationMode::Loopback(listener) => {
                        let code = listener
                            .wait_for_callback(&cancellation)
                            .map_err(|error| AppError::Setup(error.to_string()))?;
                        listener
                            .exchange(&oauth, &code, &cancellation)
                            .map_err(|error| AppError::Setup(error.to_string()))?
                    }
                    BrowserAuthorizationMode::Manual(_) => {
                        eprintln!(
                            "Neither allowlisted localhost callback port is available. Paste the final callback URL, code#state, or an authorization code, then press Enter:"
                        );
                        let mut pasted = String::new();
                        io::stdin().read_line(&mut pasted).map_err(|_| {
                            AppError::Setup("could not read OAuth callback from stdin".into())
                        })?;
                        if manual_value_is_bare_code(&pasted) {
                            eprintln!(
                                "Paste the callback state paired with that authorization code, then press Enter:"
                            );
                            let mut state = String::new();
                            io::stdin().read_line(&mut state).map_err(|_| {
                                AppError::Setup(
                                    "could not read OAuth callback state from stdin".into(),
                                )
                            })?;
                            flow.complete_manual_parts(&oauth, &pasted, &state, &cancellation)
                                .map_err(|error| AppError::Setup(error.to_string()))?
                        } else {
                            flow.complete_manual(&oauth, &pasted, &cancellation)
                                .map_err(|error| AppError::Setup(error.to_string()))?
                        }
                    }
                }
            };
            let snapshot = manager
                .install_grant(grant, &cancellation)
                .map_err(|error| AppError::Setup(error.to_string()))?;
            // The account ID is deliberately abbreviated by the manager only
            // in status output; do not include it in a login completion line.
            let _ = snapshot;
            Ok("Codex: Tea-owned OAuth login completed".into())
        }
    }
}

#[cfg(feature = "provider-codex")]
fn manual_value_is_bare_code(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.contains(['?', '#', '&'])
        && !value.starts_with("code=")
        && !value.starts_with("state=")
        && !value.starts_with("error=")
}

#[cfg(feature = "provider-codex")]
fn require_codex_provider(provider: &OsString) -> Result<(), AppError> {
    if provider.to_str() == Some("codex") {
        Ok(())
    } else {
        Err(AppError::Setup(
            "`tea auth` currently supports only the `codex` provider".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn codex_status(tea_home: Option<std::path::PathBuf>) -> AuthCommand {
        AuthCommand::Status {
            provider: OsString::from("codex"),
            tea_home,
        }
    }

    #[cfg(feature = "provider-codex")]
    #[test]
    fn status_is_offline_and_nonsecret_when_no_tea_owned_credential_exists() {
        let path = std::env::temp_dir().join(format!(
            "tea-codex-auth-status-missing-{}",
            std::process::id()
        ));
        let output = run_auth_command(codex_status(Some(path)))
            .expect("status must not require OAuth, a browser, or a credential file");
        assert_eq!(output, "Codex: not logged in");
    }

    #[cfg(not(feature = "provider-codex"))]
    #[test]
    fn feature_disabled_binary_reports_that_codex_is_not_compiled() {
        let error = run_auth_command(codex_status(None))
            .expect_err("a feature-disabled binary cannot execute Codex auth");
        assert!(error.to_string().contains("not compiled"));
    }
}
