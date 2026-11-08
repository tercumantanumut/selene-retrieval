//! "Claude (sign in)" / "Codex (sign in)" in the app (M5-C, Amendments 10.2 / 10.3), on the
//! wizard step "Get written answers (optional)" (page 20) and Providers & Keys (page 21).
//!
//! If Claude Code (or Codex) is signed in on this PC, Selene Retrieval uses that sign-in: the
//! card says "Using your Claude Code sign-in: {email} ({plan})" with [Check again]. Otherwise it
//! shows one state with one action:
//!
//! * not installed → "Claude Code isn't installed on this computer" + [Get Claude Code] (the
//!   provider list's own `https://` page, opened through the `open_link` allow list);
//! * not signed in → "Claude Code isn't signed in" + [Sign in] (the service opens the CLI's own
//!   sign-in window, which opens the browser itself);
//! * signing in → "Finish signing in in the window that opened" + [Check again].
//!
//! The status is read when a card opens, on [Check again] and when the app window regains
//! focus, and is pushed by the service when the sign-in window closes (`cli_signin.changed`).
//! Nothing polls. No token exists anywhere in Selene Retrieval; only the state, the account
//! email and the plan reach the UI.
//!
//! Service contract (privileged, R31): `cli_signin.status { provider, refresh }` →
//! `sr_common::api::CliSigninStatus`; `cli_signin.start { provider }` → the same plus `started`.

#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::error::{Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::UiError;

/// The provider-list ids that sign in through the user's own CLI.
pub const PROVIDER_IDS: &[&str] = &[
    sr_common::api::CLAUDE_SIGNIN_PROVIDER,
    sr_common::api::CODEX_SIGNIN_PROVIDER,
];

const MAX_EMAIL_CHARS: usize = 320;
const MAX_TEXT_CHARS: usize = 60;

/// Whether `id` is a CLI sign-in provider.
pub fn is_cli_provider(id: &str) -> bool {
    PROVIDER_IDS.contains(&id)
}

/// Where the sign-in stands (the service's `CliSigninState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CliSignInState {
    /// The tool isn't on this computer.
    NotInstalled,
    /// Installed, not signed in.
    SignedOut,
    /// Its sign-in window is open.
    SigningIn,
    /// Signed in: Selene Retrieval uses that sign-in.
    SignedIn,
}

/// One CLI sign-in provider's state for its card. Never carries a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CliSignInStatus {
    /// `claudecode` | `codex`.
    pub provider: String,
    pub state: CliSignInState,
    pub signed_in: bool,
    /// The account's email, when the tool reports one (Claude).
    pub email: Option<String>,
    /// "Max", "Pro", "ChatGPT", …
    pub plan: Option<String>,
    /// The user's tool: "Claude Code" / "Codex".
    pub tool: String,
    /// The tool's own download page (from the provider list; the only address the card opens).
    pub install_url: Option<String>,
    /// The status check itself failed (timeout, unreadable answer): "Couldn't check" + [Check again].
    pub check_failed: bool,
}

/// [Sign in]: what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CliSignInStart {
    pub status: CliSignInStatus,
    /// The tool's sign-in window opened.
    pub started: bool,
}

fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

/// The tool name for `provider` (from the shared table; never from the wire).
fn tool_of(provider: &str) -> String {
    sr_common::api::cli_signin_provider(provider).map_or_else(String::new, |p| p.tool.to_owned())
}

/// The provider list's own page for `provider`, if it is on the `open_link` allow list.
fn install_url(provider: &str) -> Option<String> {
    crate::providers::catalog()
        .iter()
        .find(|p| p.id == provider)
        .and_then(|p| p.key_url.clone())
}

/// Reads a service status (also nested as `{ status: {...} }`) for `provider`.
pub fn status_from(provider: &str, v: &Value) -> CliSignInStatus {
    let v = v.get("status").filter(|s| s.is_object()).unwrap_or(v);
    let state = match v.get("state").and_then(Value::as_str) {
        Some("signed_in") => CliSignInState::SignedIn,
        Some("signing_in") => CliSignInState::SigningIn,
        Some("not_installed") => CliSignInState::NotInstalled,
        _ if v.get("cli_found").and_then(Value::as_bool) == Some(false) => {
            CliSignInState::NotInstalled
        }
        _ => CliSignInState::SignedOut,
    };
    let signed_in = state == CliSignInState::SignedIn
        && v.get("authenticated").and_then(Value::as_bool) == Some(true);
    let state = if state == CliSignInState::SignedIn && !signed_in {
        CliSignInState::SignedOut
    } else {
        state
    };
    let email = text(v, "email", MAX_EMAIL_CHARS).filter(|e| e.contains('@'));
    CliSignInStatus {
        provider: provider.to_owned(),
        state,
        signed_in,
        email: if signed_in { email } else { None },
        plan: if signed_in {
            text(v, "plan", MAX_TEXT_CHARS)
        } else {
            None
        },
        tool: tool_of(provider),
        install_url: install_url(provider),
        check_failed: v.get("error").is_some_and(|e| !e.is_null()),
    }
}

fn check_provider(provider: &str) -> Result<(), UiError> {
    if is_cli_provider(provider) {
        Ok(())
    } else {
        Err(UiError::from(
            Error::new(ErrorCode::InvalidInput).with_detail("not a sign-in provider"),
        ))
    }
}

/// Reads the state; `refresh` asks the tool again now ([Check again], window focus).
pub async fn status<R: Rpc>(
    rpc: &R,
    provider: &str,
    refresh: bool,
) -> Result<CliSignInStatus, UiError> {
    check_provider(provider)?;
    rpc.call(
        method::CLI_SIGNIN_STATUS,
        json!({ "provider": provider, "refresh": refresh }),
    )
    .await
    .map(|v| status_from(provider, &v))
    .map_err(UiError::from)
}

/// [Sign in]: the service opens the tool's own sign-in window (it opens the browser itself).
/// The app opens nothing here.
pub async fn start<R: Rpc>(rpc: &R, provider: &str) -> Result<CliSignInStart, UiError> {
    check_provider(provider)?;
    let v = rpc
        .call(method::CLI_SIGNIN_START, json!({ "provider": provider }))
        .await
        .map_err(UiError::from)?;
    Ok(CliSignInStart {
        status: status_from(provider, &v),
        started: v.get("started").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    #[tokio::test]
    async fn reads_the_service_contract_shapes() {
        let rpc = FakeRpc::default();
        let p = sr_common::api::cli_signin_provider("claudecode").unwrap();
        let mut st =
            sr_common::api::CliSigninStatus::new(p, sr_common::api::CliSigninState::SignedIn);
        st.email = Some("c@example.com".into());
        st.plan = Some("Max".into());
        st.cli_version = Some("2.1.303".into());
        rpc.reply(Ok(serde_json::to_value(&st).unwrap()));
        let got = status(&rpc, "claudecode", true).await.unwrap();
        assert!(got.signed_in);
        assert_eq!(got.state, CliSignInState::SignedIn);
        assert_eq!(got.email.as_deref(), Some("c@example.com"));
        assert_eq!(got.plan.as_deref(), Some("Max"));
        assert_eq!(got.tool, "Claude Code");
        assert!(!got.check_failed);
        assert_eq!(
            rpc.calls()[0],
            (
                "cli_signin.status".to_owned(),
                json!({"provider": "claudecode", "refresh": true})
            )
        );

        let p = sr_common::api::cli_signin_provider("codex").unwrap();
        let st =
            sr_common::api::CliSigninStatus::new(p, sr_common::api::CliSigninState::NotInstalled);
        rpc.reply(Ok(serde_json::to_value(&st).unwrap()));
        let got = status(&rpc, "codex", false).await.unwrap();
        assert_eq!(got.state, CliSignInState::NotInstalled);
        assert_eq!(got.tool, "Codex");
        assert!(!got.signed_in && got.email.is_none());
    }

    #[tokio::test]
    async fn only_sign_in_providers_and_no_stale_account() {
        let rpc = FakeRpc::default();
        assert!(status(&rpc, "openai", false).await.is_err());
        assert!(start(&rpc, "").await.is_err());
        assert!(rpc.calls().is_empty(), "nothing sent for other ids");
        // Not signed in: an email in the reply is not shown.
        rpc.reply(Ok(json!({"authenticated": false, "state": "signed_out",
            "email": "stale@example.com", "plan": "Pro", "error": "Timeout"})));
        let got = status(&rpc, "claudecode", false).await.unwrap();
        assert_eq!(got.email, None);
        assert_eq!(got.plan, None);
        assert!(got.check_failed);
        // "signed_in" without `authenticated` is not trusted.
        rpc.reply(Ok(json!({"state": "signed_in"})));
        assert!(!status(&rpc, "claudecode", false).await.unwrap().signed_in);
    }

    #[tokio::test]
    async fn start_opens_nothing_in_the_app() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(
            json!({"authenticated": false, "state": "signing_in", "cli_found": true,
            "started": true}),
        ));
        let r = start(&rpc, "codex").await.unwrap();
        assert!(r.started);
        assert_eq!(r.status.state, CliSignInState::SigningIn);
        assert_eq!(rpc.calls()[0].0, "cli_signin.start");
        assert_eq!(rpc.calls()[0].1, json!({"provider": "codex"}));
    }
}
