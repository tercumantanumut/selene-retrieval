//! "Claude (sign in)" / "Codex (sign in)" in the service (M5-C, Amendments 10.2 / 10.3):
//! `cli_signin.status | start {provider}` and their `claude_signin.*` names for `claudecode`
//! (all privileged, R31), the provider slot for these rows, and the `cli_signin.changed` event.
//!
//! The user's own CLI owns the login (`sr_providers::cli`). The service never reads, stores or
//! refreshes a token; there is nothing to sign out of here (removing the provider only
//! disconnects Selene Retrieval, never the user's Claude Code / Codex login).
//!
//! Status is asked from the CLI when the app asks (opening a page, [Check again], the window
//! regaining focus) and when the CLI's sign-in window closes; a recent answer (30 s) is reused
//! otherwise. Nothing polls.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sr_common::api::{CliSigninStart, CliSigninState, CliSigninStatus};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_providers::cli::{CliKind, CliLlm, CliPath, discover};

use crate::EngineHandle;
use crate::llm::LlmSlot;

/// A status answer is reused for this long (unless the caller asks again).
const CACHE_TTL: Duration = Duration::from_secs(30);
/// A sign-in window still open after this long is no longer reported as "signing in".
const LOGIN_MAX: Duration = Duration::from_secs(15 * 60);
/// The Credential Manager account of the removed OAuth design (M5-E); deleted at start.
const OLD_TOKEN_ACCOUNT: &str = "claude-signin";

/// The engine's CLI sign-in state.
#[derive(Default)]
pub(crate) struct CliSignin {
    cache: Mutex<HashMap<CliKind, (Instant, CliSigninStatus)>>,
    /// Sign-in windows we opened, by provider (when).
    login: Mutex<HashMap<CliKind, Instant>>,
    /// One status check per provider at a time.
    checks: tokio::sync::Mutex<()>,
    /// Tests: a fake CLI instead of the search.
    paths: Mutex<HashMap<CliKind, PathBuf>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn kind_of(provider: &str) -> Result<CliKind, Error> {
    CliKind::from_provider(provider.trim()).ok_or_else(|| {
        Error::new(ErrorCode::InvalidInput).with_detail("provider must be claudecode or codex")
    })
}

/// Whether two statuses differ in anything the app shows.
fn differs(a: &CliSigninStatus, b: &CliSigninStatus) -> bool {
    a.state != b.state
        || a.email != b.email
        || a.plan != b.plan
        || a.cli_found != b.cli_found
        || a.error != b.error
}

impl EngineHandle {
    fn cli(&self) -> &CliSignin {
        &self.inner().cli
    }

    /// Tests only: use the (fake) CLI at `path` for `provider` instead of searching; `None`
    /// restores the search. The path still has to pass the same checks (absolute, .exe/.cmd).
    #[doc(hidden)]
    pub fn set_cli_path(&self, provider: &str, path: Option<PathBuf>) {
        let Some(k) = CliKind::from_provider(provider) else {
            return;
        };
        match path {
            Some(p) => lock(&self.cli().paths).insert(k, p),
            None => lock(&self.cli().paths).remove(&k),
        };
        lock(&self.cli().cache).remove(&k);
    }

    fn find_cli(&self, kind: CliKind) -> Option<CliPath> {
        let over = lock(&self.cli().paths).get(&kind).cloned();
        match over {
            Some(p) => discover::accept(&p),
            None => discover::find(kind),
        }
    }

    fn signing_in(&self, kind: CliKind) -> bool {
        lock(&self.cli().login)
            .get(&kind)
            .is_some_and(|at| at.elapsed() < LOGIN_MAX)
    }

    fn with_login_state(&self, kind: CliKind, mut st: CliSigninStatus) -> CliSigninStatus {
        if !st.authenticated && st.cli_found && self.signing_in(kind) {
            st.state = CliSigninState::SigningIn;
        }
        st
    }

    fn configured_provider(&self) -> Option<String> {
        self.inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .provider
            .provider
            .clone()
    }

    /// `cli_signin.status` (`claude_signin.status` = provider `claudecode`).
    pub async fn cli_signin_status(
        &self,
        provider: &str,
        refresh: bool,
    ) -> Result<CliSigninStatus, Error> {
        let kind = kind_of(provider)?;
        Ok(self.cli_status(kind, refresh).await)
    }

    /// Status with the follow-ups: a changed sign-in of the configured provider reconnects it
    /// (its MCP `context_answer` tool appears or goes).
    pub(crate) async fn cli_status(&self, kind: CliKind, refresh: bool) -> CliSigninStatus {
        let (st, auth_changed) = self.probe(kind, refresh).await;
        if auth_changed && self.configured_provider().as_deref() == Some(kind.provider().id) {
            let _ = self.reload_provider().await;
        }
        st
    }

    /// The status (cached or asked now) and whether `authenticated` changed. Pushes
    /// `cli_signin.changed` when anything shown changed. Never reconnects (no recursion).
    async fn probe(&self, kind: CliKind, refresh: bool) -> (CliSigninStatus, bool) {
        let fresh = |h: &Self| {
            lock(&h.cli().cache)
                .get(&kind)
                .filter(|(at, _)| at.elapsed() < CACHE_TTL)
                .map(|(_, s)| s.clone())
        };
        if !refresh && let Some(s) = fresh(self) {
            return (self.with_login_state(kind, s), false);
        }
        let _one = self.cli().checks.lock().await;
        if !refresh && let Some(s) = fresh(self) {
            return (self.with_login_state(kind, s), false);
        }
        let exe = self.find_cli(kind);
        let st = sr_providers::cli::check_status(kind, exe.as_ref()).await;
        let before = lock(&self.cli().cache)
            .insert(kind, (Instant::now(), st.clone()))
            .map(|(_, s)| s);
        drop(_one);
        if st.authenticated {
            lock(&self.cli().login).remove(&kind);
        }
        let st = self.with_login_state(kind, st);
        tracing::info!(
            target: "sr_engine::cli_signin",
            provider = kind.stem(),
            state = ?st.state,
            version = st.cli_version.as_deref().unwrap_or("-"),
            error = ?st.error,
            "sign-in checked"
        );
        if before.as_ref().is_none_or(|b| differs(b, &st)) {
            self.inner()
                .bus
                .publish(EngineEvent::CliSigninChanged(st.clone()));
        }
        let auth_changed = before
            .as_ref()
            .is_some_and(|b| b.authenticated != st.authenticated);
        (st, auth_changed)
    }

    /// `cli_signin.start`: opens the CLI's own sign-in (`claude auth login` / `codex login`) in
    /// its own window, which opens the browser itself. Already signed in or not installed: no
    /// window (`started: false`). When the window closes, the status is checked again and pushed.
    pub async fn cli_signin_start(&self, provider: &str) -> Result<CliSigninStart, Error> {
        let kind = kind_of(provider)?;
        let st = self.cli_status(kind, true).await;
        if st.authenticated || !st.cli_found {
            return Ok(CliSigninStart {
                status: st,
                started: false,
            });
        }
        if self.signing_in(kind) {
            // One sign-in window at a time; the open one is still waiting.
            return Ok(CliSigninStart {
                status: st,
                started: false,
            });
        }
        let Some(exe) = self.find_cli(kind) else {
            return Err(kind.not_installed());
        };
        let dir = std::env::temp_dir().join("sr-cli-signin");
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        let mut child = sr_providers::cli::start_login(kind, &exe, &dir)?;
        lock(&self.cli().login).insert(kind, Instant::now());
        tracing::info!(target: "sr_engine::cli_signin", provider = kind.stem(), "sign-in window opened");
        let h = self.clone();
        let stop = self.inner().shutdown.clone();
        self.track_task(tokio::spawn(async move {
            tokio::select! {
                () = stop.cancelled() => return,
                _ = tokio::time::timeout(LOGIN_MAX, child.wait()) => {}
            }
            lock(&h.cli().login).remove(&kind);
            let _ = h.cli_status(kind, true).await;
        }));
        let st = self.with_login_state(kind, st);
        self.inner()
            .bus
            .publish(EngineEvent::CliSigninChanged(st.clone()));
        Ok(CliSigninStart {
            status: st,
            started: true,
        })
    }

    /// The chat slot for a CLI sign-in provider: the CLI must be there and signed in (a recent
    /// status is reused). No process runs a prompt here.
    pub(crate) async fn connect_cli(
        &self,
        spec: &sr_providers::ProviderSpec,
        s: &sr_common::config::Settings,
    ) -> Result<LlmSlot, Error> {
        let kind = kind_of(&spec.id)?;
        self.check_provider_policy(&spec.id, None)?;
        let exe = self.find_cli(kind).ok_or_else(|| kind.not_installed())?;
        let (st, _) = self.probe(kind, false).await;
        if !st.authenticated {
            return Err(match st.error {
                Some(code) if st.cli_found => {
                    Error::new(code).with_provider(spec.display_name.clone())
                }
                _ if !st.cli_found => kind.not_installed(),
                _ => kind.not_signed_in("the CLI reports no sign-in"),
            });
        }
        let llm = CliLlm::new(
            kind,
            exe,
            spec.clone(),
            self.privacy_gate(),
            &sr_providers::RemoteSettings::from_settings(s),
        );
        Ok(LlmSlot {
            provider_id: spec.id.clone(),
            display_name: spec.display_name.clone(),
            model: s.provider.default_model.clone().unwrap_or_default(),
            llm: Arc::new(llm),
        })
    }

    /// [Check and save] / [Check again] for a CLI sign-in provider: asks the CLI now; nothing is
    /// sent to the provider. The model list is the provider table's hints.
    pub(crate) async fn check_cli(
        &self,
        spec: &sr_providers::ProviderSpec,
    ) -> Result<(Option<String>, Vec<sr_providers::ModelInfo>), Error> {
        let kind = kind_of(&spec.id)?;
        let (st, _) = self.probe(kind, true).await;
        if !st.cli_found {
            return Err(kind.not_installed());
        }
        if !st.authenticated {
            return Err(match st.error {
                Some(code) => Error::new(code).with_provider(spec.display_name.clone()),
                None => kind.not_signed_in("the CLI reports no sign-in"),
            });
        }
        Ok((
            spec.default_model.clone(),
            sr_providers::remote::discovery::registry_models(spec),
        ))
    }

    /// The token of the removed OAuth design, if an earlier build stored one: deleted (once per
    /// start; nothing else ever reads it).
    pub(crate) fn drop_old_signin_token(&self) {
        if let Err(e) = self.llm_state().secrets.delete(OLD_TOKEN_ACCOUNT) {
            tracing::debug!(target: "sr_engine::cli_signin", code = %e.code, "old sign-in entry not removed");
        }
    }
}
