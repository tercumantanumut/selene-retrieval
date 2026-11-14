//! Tauri commands (typed via tauri-specta → `ui/src/bindings.ts`) and the logic behind them.
//!
//! The logic is generic over [`Rpc`] and plain paths so it is unit-tested without a WebView or a
//! running service; the `#[tauri::command]` wrappers only fetch state and forward.
// `UiError` (title, body, action + label, details) passed 128 bytes with M3's provider actions.
// It is the Tauri IPC error, built at most once per user action and serialised straight to the
// WebView, so its size has no cost; boxing it would only change every command signature.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sr_common::error::{Error, ErrorCode};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt as _;

use crate::apps::{self, ChangeTracker, ConnectCtx};
use crate::connect::claude;
use crate::daemon::{DaemonLink, Rpc, Shared, method};
use crate::diagnostics;
use crate::dto::{
    AboutInfo, AdvancedView, AnswersView, AppView, AuthKind, ClaudeState, ClientApp, ClientsView,
    ConnectResult, ConnectStatus, ConsentPurpose, ConsentRow, Estimate, KeyCheck, LicenseText,
    ModelOption, ProviderOption, ProviderStatus, SaveResult, TierName, UiAction, UiError,
    UpdateInfo,
};
use crate::{estimate, prefs, providers};

/// Everything commands need, managed by Tauri.
pub struct AppCtx {
    pub link: Arc<DaemonLink>,
    pub shared: Shared,
    /// The user's folders + our bridge, for every Connect button (Claude, Cursor, VS Code).
    pub connect: ConnectCtx,
    /// Last seen app states, to tell "Claude removed our entry" from our own Disconnect.
    pub tracker: Mutex<ChangeTracker>,
    /// Keeps the client config watcher alive (rebuilt after a connect creates the folder).
    pub watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// The app's own config folder (`prefs.json`); `None` if Windows didn't give us one.
    pub prefs_dir: Option<PathBuf>,
}

impl AppCtx {
    fn prefs(&self) -> prefs::Prefs {
        self.prefs_dir
            .as_deref()
            .map(prefs::load)
            .unwrap_or_default()
    }

    fn mark_autostart_decided(&self) {
        if let Some(d) = &self.prefs_dir {
            let mut p = prefs::load(d);
            if !p.autostart_decided {
                p.autostart_decided = true;
                prefs::save(d, &p);
            }
        }
    }
}

impl AppCtx {
    pub fn tracker(&self) -> std::sync::MutexGuard<'_, ChangeTracker> {
        self.tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claude's DS-5.5 state, including "Claude removed our entry" (R-UX23).
    pub fn claude_status(&self) -> ConnectStatus {
        let ext = self.tracker().changed_externally(ClientApp::ClaudeDesktop);
        claude_status(&self.connect, ext)
    }
}

// ---------------------------------------------------------------------------------------------
// Logic (unit-tested)

fn bad_path() -> UiError {
    UiError::new(
        "Choose a folder",
        "That doesn't look like a folder on this computer.",
        UiAction::ChooseFolder,
        "Choose a folder",
        "InvalidInput · app",
    )
}

/// Adds a folder; indexing starts on its own and progress arrives as events.
/// Returns the service's `FolderInfo` reply (`folder_id`, `name`, `path`, ...).
pub async fn add_folder<R: Rpc>(rpc: &R, path: &str) -> Result<Value, UiError> {
    let p = path.trim();
    if p.is_empty() || !Path::new(p).is_absolute() {
        return Err(bad_path());
    }
    rpc.call(method::FOLDERS_ADD, json!({ "path": p }))
        .await
        .map_err(UiError::from)
}

pub async fn remove_folder<R: Rpc>(rpc: &R, folder_id: &str) -> Result<(), UiError> {
    rpc.call(method::FOLDERS_REMOVE, json!({ "folder_id": folder_id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

pub async fn sync_folder<R: Rpc>(rpc: &R, folder_id: Option<&str>) -> Result<(), UiError> {
    rpc.call(method::FOLDERS_SYNC, json!({ "folder_id": folder_id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

/// Wizard estimate from the service (`folders.estimate`: real filters, active EP speed). A service
/// older than the method answers "unknown method" (`InvalidInput`): `Ok(None)` = use the local
/// fallback count. A refused or missing folder keeps its plain message and one action.
pub async fn estimate_from_service<R: Rpc>(
    rpc: &R,
    path: &str,
) -> Result<Option<Estimate>, UiError> {
    match rpc
        .call(method::FOLDERS_ESTIMATE, json!({ "path": path.trim() }))
        .await
    {
        Ok(v) => Ok(estimate::from_service(&v)),
        Err(e) if e.code == ErrorCode::InvalidInput => Ok(None),
        Err(e) => Err(UiError::from(e)),
    }
}

/// Pause or resume one folder.
pub async fn set_paused<R: Rpc>(rpc: &R, folder_id: &str, paused: bool) -> Result<(), UiError> {
    let m = if paused {
        method::FOLDERS_PAUSE
    } else {
        method::FOLDERS_RESUME
    };
    rpc.call(m, json!({ "folder_id": folder_id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

/// The user's decision on an `add_folder` request from a connected app.
pub async fn decide_approval<R: Rpc>(rpc: &R, id: &str, allow: bool) -> Result<(), UiError> {
    let m = if allow {
        method::APPROVALS_APPROVE
    } else {
        method::APPROVALS_DENY
    };
    rpc.call(m, json!({ "approval_id": id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

/// [Use it] (Settings → Search quality). The click is the consent to download (Amendment 1);
/// `allow_metered` = "Download anyway" on a metered connection. Progress arrives as events.
pub async fn use_recommended_model<R: Rpc>(rpc: &R, allow_metered: bool) -> Result<(), UiError> {
    rpc.call(
        method::SEARCH_MODEL_USE_RECOMMENDED,
        json!({ "allow_metered": allow_metered }),
    )
    .await
    .map(|_| ())
    .map_err(UiError::from)
}

fn advanced_view(v: &Value) -> AdvancedView {
    AdvancedView {
        use_graphics_card: v
            .get("use_graphics_card")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        allow_app_folder_requests: v
            .get("mcp_add_folder_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allow_app_folder_checks: v
            .get("mcp_sync_folder_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        restart_needed: v
            .get("restart_needed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Settings → Advanced, read once when the page opens (never polled).
pub async fn get_advanced<R: Rpc>(rpc: &R) -> Result<AdvancedView, UiError> {
    rpc.call(method::SETTINGS_ADVANCED, json!({}))
        .await
        .map(|v| advanced_view(&v))
        .map_err(UiError::from)
}

/// Change any of the Advanced switches (`None` = unchanged); returns the saved state.
pub async fn set_advanced<R: Rpc>(
    rpc: &R,
    use_graphics_card: Option<bool>,
    allow_app_folder_requests: Option<bool>,
    allow_app_folder_checks: Option<bool>,
) -> Result<AdvancedView, UiError> {
    rpc.call(
        method::SETTINGS_UPDATE_ADVANCED,
        json!({
            "use_graphics_card": use_graphics_card,
            "mcp_add_folder_enabled": allow_app_folder_requests,
            "mcp_sync_folder_enabled": allow_app_folder_checks,
        }),
    )
    .await
    .map(|v| advanced_view(&v))
    .map_err(UiError::from)
}

/// Advanced → search model: switch to a tier (any direction). Above Basic it downloads the model
/// (the click is the consent) and improves the index in the background; search keeps working.
pub async fn choose_tier<R: Rpc>(rpc: &R, tier: TierName) -> Result<(), UiError> {
    let t = match tier {
        TierName::Basic => "minimum",
        TierName::Better => "standard",
        TierName::Best => "high",
    };
    rpc.call(
        method::SEARCH_MODEL_CHOOSE,
        json!({ "tier": t, "allow_metered": false }),
    )
    .await
    .map(|_| ())
    .map_err(UiError::from)
}

/// Page 12 multilingual offer / Advanced "Multilingual" (M3-Q profile, `search_model.choose`
/// with `model: "multilingual"`): a one-click download; search keeps working while it changes.
pub async fn choose_multilingual<R: Rpc>(rpc: &R) -> Result<(), UiError> {
    rpc.call(
        method::SEARCH_MODEL_CHOOSE,
        json!({ "model": "multilingual", "allow_metered": false }),
    )
    .await
    .map(|_| ())
    .map_err(UiError::from)
}

// ---------------------------------------------------------------------------------------------
// M3: providers, consent, answers (pages 20–23). Wire shapes: docs/research/m3-u-progress.md
// (requests R-P1…R-P3 / R-Q5). The key travels once, from the field to the service over the
// user-only pipe; the service checks it and keeps it in Windows Credential Manager. The app never
// stores, logs or echoes it back (only `key_last4`).

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
}

/// The last 4 characters, never more (even if a service sent more).
fn last4(v: Option<String>) -> String {
    let v = v.unwrap_or_default();
    let n = v.chars().count();
    v.chars().skip(n.saturating_sub(4)).collect()
}

/// `{"code": "<ErrorCode>", "ctx": {...}}` (the RPC error data shape) → the one error shape.
fn ui_error_from_wire(v: &Value) -> Option<UiError> {
    let code: ErrorCode = serde_json::from_value(v.get("code")?.clone()).ok()?;
    let mut e = Error::new(code);
    if let Some(ctx) = v
        .get("ctx")
        .and_then(|c| serde_json::from_value::<sr_common::error::ErrCtx>(c.clone()).ok())
    {
        e = e.with_ctx(ctx);
    }
    Some(UiError::from(e))
}

/// `providers.status` reply → page 21's view.
pub fn provider_status(v: &Value) -> ProviderStatus {
    let Some(id) = s(v, "provider") else {
        return ProviderStatus::none();
    };
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = s(m, "id")?;
            Some(ModelOption {
                name: s(m, "display_name").unwrap_or_else(|| id.clone()),
                id,
            })
        })
        .collect();
    let last_error = v.get("last_error").and_then(ui_error_from_wire);
    let check = match v.get("check").and_then(Value::as_str) {
        Some("working") => KeyCheck::Working,
        Some("failed") => KeyCheck::Failed,
        _ if last_error.is_some() => KeyCheck::Failed,
        _ => KeyCheck::NotChecked,
    };
    ProviderStatus {
        provider_name: Some(providers::display_name(&id)),
        provider: Some(id),
        key_saved: v.get("key_saved").and_then(Value::as_bool).unwrap_or(false),
        key_last4: last4(s(v, "key_last4")),
        address: s(v, "address"),
        check,
        last_error,
        model: s(v, "model"),
        default_model: s(v, "default_model"),
        models,
        remote_embeddings: v
            .get("remote_embeddings")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        remote_rerank: v
            .get("remote_rerank")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

pub async fn get_provider_status<R: Rpc>(rpc: &R) -> Result<ProviderStatus, UiError> {
    rpc.call(method::PROVIDERS_STATUS, json!({}))
        .await
        .map(|v| provider_status(&v))
        .map_err(UiError::from)
}

/// [Check and save] (or [Save without checking] when offline: `check = false`). The click is the
/// consent for `validate_key` and `list_models` with that provider (D-35); nothing is sent to a
/// provider before it. Only providers from the provider list that take a key or an address can be
/// saved; of the account sign-in providers only "Claude (sign in)" and "Codex (sign in)" can be
/// saved (no key: the user's own CLI login is used, M5-C).
pub async fn save_provider<R: Rpc>(
    rpc: &R,
    list: &[ProviderOption],
    provider: &str,
    key: Option<&str>,
    address: Option<&str>,
    check: bool,
) -> Result<SaveResult, UiError> {
    let Some(p) = list.iter().find(|p| p.id == provider) else {
        return Err(UiError::from(
            Error::new(ErrorCode::InvalidInput).with_detail("provider not in the provider list"),
        ));
    };
    let key = key.map(str::trim).filter(|k| !k.is_empty());
    let address = address.map(str::trim).filter(|a| !a.is_empty());
    let params = match p.auth {
        // "Claude (sign in)" / "Codex (sign in)": no key and no address; the service uses the
        // CLI's own sign-in (`providers.save { provider, key: null, address: null, check }`;
        // "isn't installed" / "isn't signed in" errors otherwise).
        AuthKind::Oauth if p.available && crate::cli_signin::is_cli_provider(&p.id) => {
            json!({ "provider": p.id, "key": null, "address": null, "check": check })
        }
        AuthKind::Oauth => {
            return Err(UiError::from(
                Error::new(ErrorCode::InvalidInput)
                    .with_detail("sign-in providers aren't available"),
            ));
        }
        AuthKind::ApiKey => match key {
            Some(k) => json!({ "provider": p.id, "key": k, "address": null, "check": check }),
            None => return Err(UiError::from(Error::new(ErrorCode::InvalidInput))),
        },
        AuthKind::Local => json!({
            "provider": p.id,
            "key": null,
            "address": address.or(p.default_address.as_deref()),
            "check": check,
        }),
    };
    let v = rpc
        .call(method::PROVIDERS_SAVE, params)
        .await
        .map_err(UiError::from)?;
    let status = provider_status(v.get("status").unwrap_or(&v));
    let validated = v
        .get("validated")
        .and_then(Value::as_bool)
        .unwrap_or(status.check == KeyCheck::Working);
    Ok(SaveResult { status, validated })
}

/// Generic "call and return the new provider status" (check again, set model, remove).
pub async fn provider_call<R: Rpc>(
    rpc: &R,
    m: &str,
    params: Value,
) -> Result<ProviderStatus, UiError> {
    rpc.call(m, params)
        .await
        .map(|v| provider_status(v.get("status").unwrap_or(&v)))
        .map_err(UiError::from)
}

fn consent_rows(v: &Value) -> Vec<ConsentRow> {
    let mut rows: Vec<ConsentRow> = Vec::new();
    for r in v.as_array().into_iter().flatten() {
        let Some(purpose) = r
            .get("purpose")
            .and_then(Value::as_str)
            .and_then(ConsentPurpose::from_wire)
        else {
            continue;
        };
        let allowed = r.get("allowed").and_then(Value::as_bool).unwrap_or(false);
        match rows.iter_mut().find(|x| x.purpose == purpose) {
            // `answer` + `iterate` are one row: allowed only if both are.
            Some(x) => x.allowed &= allowed,
            None => rows.push(ConsentRow { purpose, allowed }),
        }
    }
    rows
}

pub async fn get_consents<R: Rpc>(rpc: &R) -> Result<Vec<ConsentRow>, UiError> {
    rpc.call(method::CONSENT_LIST, json!({}))
        .await
        .map(|v| consent_rows(&v))
        .map_err(UiError::from)
}

/// Allow or revoke one purpose for the configured provider. "Allow" is only ever sent after the
/// consent dialog (page 22); revoking needs no dialog.
pub async fn set_consent<R: Rpc>(
    rpc: &R,
    provider: &str,
    purpose: ConsentPurpose,
    allowed: bool,
) -> Result<Vec<ConsentRow>, UiError> {
    rpc.call(
        method::CONSENT_SET,
        json!({ "provider": provider, "purpose": purpose.wire(), "allowed": allowed }),
    )
    .await
    .map(|v| consent_rows(&v))
    .map_err(UiError::from)
}

fn answers_view(v: &Value) -> AnswersView {
    AnswersView {
        answers_enabled: v
            .get("answers_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Deeper answers stay off unless the service says the user turned them on.
        deeper_answers: v
            .get("deeper_answers")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_rounds: v
            .get("max_rounds")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(2),
    }
}

pub async fn get_answers<R: Rpc>(rpc: &R) -> Result<AnswersView, UiError> {
    rpc.call(method::SETTINGS_ANSWERS, json!({}))
        .await
        .map(|v| answers_view(&v))
        .map_err(UiError::from)
}

pub async fn set_answers<R: Rpc>(
    rpc: &R,
    answers_enabled: Option<bool>,
    deeper_answers: Option<bool>,
    max_rounds: Option<u32>,
) -> Result<AnswersView, UiError> {
    // Page 13: 1–5 extra rounds (the service checks the bounds too).
    let max_rounds = max_rounds.map(|n| n.clamp(1, 5));
    rpc.call(
        method::SETTINGS_UPDATE_ANSWERS,
        json!({ "answers_enabled": answers_enabled, "deeper_answers": deeper_answers, "max_rounds": max_rounds }),
    )
    .await
    .map(|v| answers_view(&v))
    .map_err(UiError::from)
}

pub fn claude_status(c: &ConnectCtx, changed_externally: bool) -> ConnectStatus {
    let claude = c.state(ClientApp::ClaudeDesktop);
    ConnectStatus {
        claude,
        snippet: c.snippet(ClientApp::ClaudeDesktop),
        changed_externally: changed_externally && claude != ClaudeState::Connected,
    }
}

pub fn connect_claude(c: &ConnectCtx) -> Result<ConnectResult, UiError> {
    let message = apps::connect_app(c, ClientApp::ClaudeDesktop)?;
    Ok(ConnectResult {
        status: claude_status(c, false),
        message,
    })
}

pub fn disconnect_claude(c: &ConnectCtx) -> Result<ConnectResult, UiError> {
    let message = apps::disconnect_app(c, ClientApp::ClaudeDesktop)?;
    Ok(ConnectResult {
        status: claude_status(c, false),
        message,
    })
}

// ---------------------------------------------------------------------------------------------
// M4: restart, connected apps, About, links (pages 03, 16, 18, 21, 29)

/// How long the shell shows "Restarting the background service…" before falling back to the
/// normal states (the real problem panel, if the service didn't come back).
pub const RESTART_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(60);

/// [Restart now] / [Restart Selene Retrieval] (page 03, R-UX31, OQ-15/24): a real restart.
/// `host.restart` (contract §2) drains the service and exits it; the status loop re-attaches at
/// once, which starts it again through the usual launcher, and pending work resumes as "Catching
/// up after restart". A service that isn't reachable (the code-33 panel) is simply started again;
/// an older service without `host.restart` gets the drain shutdown instead.
pub async fn restart_service<R: Rpc>(
    rpc: &R,
    drain: impl std::future::Future<Output = Result<(), Error>>,
    shared: &Shared,
) -> Result<(), UiError> {
    shared.update(|s| s.set_restarting(true));
    let r = match rpc.call(method::HOST_RESTART, json!({})).await {
        Ok(_) => Ok(()),
        // Not running: re-attaching starts it.
        Err(e) if e.code == ErrorCode::ServiceUnavailable => Ok(()),
        // Older service: unknown method.
        Err(e) if e.code == ErrorCode::InvalidInput => drain.await,
        Err(e) => Err(e),
    };
    if let Err(e) = r {
        shared.update(|s| s.set_restarting(false));
        return Err(UiError::from(e));
    }
    Ok(())
}

/// R34: send the recovery choice; on success the screen gives way to "Restarting the background
/// service…" (the service restarts out of its recovery mode).
pub async fn recover_index<R: Rpc>(
    rpc: &R,
    shared: &Shared,
    choice: crate::policy::RecoverChoice,
) -> Result<(), UiError> {
    crate::policy::recover(rpc, choice).await?;
    shared.update(|s| {
        let a = s.clear_index_missing();
        let b = s.set_restarting(true);
        a || b
    });
    Ok(())
}

/// Bundled licence texts (the `.txt` files in `dir`, at most 64 files of 2 MB each), sorted by name.
pub fn read_licenses(dir: &Path) -> Vec<LicenseText> {
    const MAX: u64 = 2 * 1024 * 1024;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<LicenseText> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX))
        .filter_map(|e| {
            let p = e.path();
            let is_txt = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("txt"));
            if !is_txt {
                return None;
            }
            let name = p.file_stem()?.to_string_lossy().into_owned();
            let text = String::from_utf8_lossy(&std::fs::read(&p).ok()?).into_owned();
            Some(LicenseText { name, text })
        })
        .take(64)
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Where "Save as file…" may write: an absolute `.txt` path whose folder exists (the user picked
/// it in the Windows save dialog; nothing else is accepted from the WebView).
pub fn diagnostics_target(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path.trim());
    let ok = p.is_absolute()
        && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("txt"))
        && p.parent().is_some_and(Path::is_dir)
        && !p.is_dir();
    ok.then_some(p)
}

fn save_failed() -> UiError {
    UiError::new(
        "Couldn't save the file",
        &format!(
            "{} didn't allow the change. Try again.",
            crate::dto::OS_NAME
        ),
        UiAction::Retry,
        "Retry",
        "DiagnosticsSave",
    )
}

fn open_failed() -> UiError {
    UiError::new(
        "Couldn't open it",
        &format!("{} didn't allow it. Try again.", crate::dto::OS_NAME),
        UiAction::Retry,
        "Retry",
        "Open",
    )
}

/// Paths of the added folders (the only places "Show in folder" may point into).
pub fn added_folder_paths(view: &AppView) -> Vec<PathBuf> {
    view.folders
        .iter()
        .filter_map(|f| f.path.as_deref())
        .map(PathBuf::from)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Tauri wrappers (names = TS command names)

pub mod cmd {
    use super::*;
    use crate::activity::{ActivityPageView, EgressPageView};
    use crate::connected::{HttpView, ScopeView, ServiceClient, ServiceClientsView, TokenView};
    use crate::folders::{
        self, ChangeImpact, FieldProblem, FolderDetailView, FolderSettingsView, GroupDeleted,
        GroupRow, SettingsSaved, SkippedPageView,
    };
    use crate::index_move::{IndexLocationView, MoveCheckView};
    use crate::search::{AnswerView, EnhanceView, SearchView};

    /// Current state (the UI also listens to `StateChanged` pushes; this is only the first read).
    #[tauri::command]
    #[specta::specta]
    pub fn get_state(ctx: State<'_, AppCtx>) -> AppView {
        ctx.shared.view()
    }

    /// Wake the re-attach loop now (the "Retry" button on a service problem).
    #[tauri::command]
    #[specta::specta]
    pub fn retry_connection(ctx: State<'_, AppCtx>) {
        ctx.link.retry.notify_one();
    }

    /// Pre-scan for the wizard: the service's own count (`folders.estimate`), or the local
    /// fallback count for an older service.
    #[tauri::command]
    #[specta::specta]
    pub async fn estimate_folder(
        ctx: State<'_, AppCtx>,
        path: String,
    ) -> Result<Estimate, UiError> {
        let p = PathBuf::from(path.trim());
        if !p.is_absolute() || !p.is_dir() {
            return Err(bad_path());
        }
        if let Some(e) = super::estimate_from_service(&*ctx.link, &path).await? {
            return Ok(e);
        }
        let gpu = ctx.shared.gpu();
        let c = tauri::async_runtime::spawn_blocking(move || {
            estimate::count(&p, estimate::MAX_FILES, estimate::TIME_BUDGET)
        })
        .await
        .map_err(|_| UiError::from(Error::new(ErrorCode::Internal)))?;
        Ok(estimate::estimate(&c, gpu))
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn add_folder(ctx: State<'_, AppCtx>, path: String) -> Result<String, UiError> {
        let info = super::add_folder(&*ctx.link, &path).await?;
        ctx.shared.update(|s| s.apply_folder_info(&info));
        Ok(info
            .get("folder_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    /// Removes a folder from search. The card then shows "Removing…" until the service confirms
    /// the purge (`PurgeCompleted`).
    #[tauri::command]
    #[specta::specta]
    pub async fn remove_folder(ctx: State<'_, AppCtx>, folder_id: String) -> Result<(), UiError> {
        super::remove_folder(&*ctx.link, &folder_id).await?;
        ctx.shared.update(|s| s.mark_removing(&folder_id));
        Ok(())
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn sync_folder(
        ctx: State<'_, AppCtx>,
        folder_id: Option<String>,
    ) -> Result<(), UiError> {
        super::sync_folder(&*ctx.link, folder_id.as_deref()).await
    }

    /// Pause (`true`) or resume (`false`) a folder.
    #[tauri::command]
    #[specta::specta]
    pub async fn set_folder_paused(
        ctx: State<'_, AppCtx>,
        folder_id: String,
        paused: bool,
    ) -> Result<(), UiError> {
        super::set_paused(&*ctx.link, &folder_id, paused).await
    }

    /// Allow (`true`) or decline a connected app's request to add a folder.
    #[tauri::command]
    #[specta::specta]
    pub async fn decide_approval(
        ctx: State<'_, AppCtx>,
        approval_id: String,
        allow: bool,
    ) -> Result<(), UiError> {
        super::decide_approval(&*ctx.link, &approval_id, allow).await?;
        ctx.shared.update(|s| s.remove_approval(&approval_id));
        crate::daemon::refresh_approvals(&*ctx.link, &ctx.shared).await;
        Ok(())
    }

    /// Hide the service's notices until its next start (M2-R O5).
    #[tauri::command]
    #[specta::specta]
    pub async fn dismiss_notices(ctx: State<'_, AppCtx>) -> Result<(), UiError> {
        ctx.link
            .call(method::NOTICES_DISMISS, json!({}))
            .await
            .map_err(UiError::from)?;
        ctx.shared.update(|s| s.clear_notices());
        Ok(())
    }

    /// [Use it]: download the recommended search model and improve the index in the background.
    #[tauri::command]
    #[specta::specta]
    pub async fn use_recommended_model(
        ctx: State<'_, AppCtx>,
        allow_metered: bool,
    ) -> Result<(), UiError> {
        super::use_recommended_model(&*ctx.link, allow_metered).await?;
        ctx.shared.update(|s| {
            s.model_change_started();
            true
        });
        Ok(())
    }

    /// Settings → Advanced switches (read once when the page opens).
    #[tauri::command]
    #[specta::specta]
    pub async fn get_advanced_settings(ctx: State<'_, AppCtx>) -> Result<AdvancedView, UiError> {
        super::get_advanced(&*ctx.link).await
    }

    /// Change "Use graphics card", "Let connected apps ask to add folders" and/or "Let connected
    /// apps check folders for changes".
    #[tauri::command]
    #[specta::specta]
    pub async fn set_advanced_settings(
        ctx: State<'_, AppCtx>,
        use_graphics_card: Option<bool>,
        allow_app_folder_requests: Option<bool>,
        allow_app_folder_checks: Option<bool>,
    ) -> Result<AdvancedView, UiError> {
        super::set_advanced(
            &*ctx.link,
            use_graphics_card,
            allow_app_folder_requests,
            allow_app_folder_checks,
        )
        .await
    }

    /// Advanced → search model: Basic / Better / Best (downloads above Basic; consent = click).
    #[tauri::command]
    #[specta::specta]
    pub async fn choose_search_tier(ctx: State<'_, AppCtx>, tier: TierName) -> Result<(), UiError> {
        super::choose_tier(&*ctx.link, tier).await?;
        if tier != TierName::Basic {
            ctx.shared.update(|s| {
                s.model_change_started();
                true
            });
        }
        Ok(())
    }

    /// Page 12 multilingual offer / Advanced → search model "Multilingual" (download, consent = click).
    #[tauri::command]
    #[specta::specta]
    pub async fn choose_multilingual_search(ctx: State<'_, AppCtx>) -> Result<(), UiError> {
        super::choose_multilingual(&*ctx.link).await?;
        ctx.shared.update(|s| {
            s.model_change_started();
            true
        });
        Ok(())
    }

    /// Stop a search model change (it resumes from where it stopped if started again).
    #[tauri::command]
    #[specta::specta]
    pub async fn cancel_model_change(ctx: State<'_, AppCtx>) -> Result<(), UiError> {
        ctx.link
            .call(method::SEARCH_MODEL_CANCEL, json!({}))
            .await
            .map_err(UiError::from)?;
        ctx.shared.update(|s| {
            s.model_change_cancelled();
            true
        });
        Ok(())
    }

    #[tauri::command]
    #[specta::specta]
    pub fn get_claude_status(ctx: State<'_, AppCtx>) -> ConnectStatus {
        ctx.claude_status()
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn connect_claude(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
    ) -> Result<ConnectResult, UiError> {
        ctx.tracker()
            .own_write(ClientApp::ClaudeDesktop, std::time::Instant::now());
        let r = super::connect_claude(&ctx.connect);
        crate::watch_client_configs(&app);
        if r.is_ok() {
            crate::activity::note(
                &*ctx.link,
                true,
                sr_common::error::ClientKind::ClaudeDesktop,
            )
            .await;
        }
        r
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn disconnect_claude(ctx: State<'_, AppCtx>) -> Result<ConnectResult, UiError> {
        ctx.tracker()
            .own_write(ClientApp::ClaudeDesktop, std::time::Instant::now());
        let r = super::disconnect_claude(&ctx.connect);
        if r.is_ok() {
            crate::activity::note(
                &*ctx.link,
                false,
                sr_common::error::ClientKind::ClaudeDesktop,
            )
            .await;
        }
        r
    }

    /// Text for the clipboard (the UI copies it). No secrets, queries, names or paths.
    #[tauri::command]
    #[specta::specta]
    pub async fn get_diagnostics(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
    ) -> Result<String, UiError> {
        let daemon = if ctx.link.info().is_some() {
            ctx.link.call(method::DIAGNOSTICS, json!({})).await.ok()
        } else {
            None
        };
        let (pid, proto) = ctx.link.info().unzip();
        let installs = claude::detect(&ctx.connect.claude_roots());
        let inp = diagnostics::Inputs {
            app_version: env!("CARGO_PKG_VERSION"),
            os: os_line(),
            daemon_pid: pid,
            daemon_proto: proto,
            daemon,
            claude: Some(ctx.connect.state(ClientApp::ClaudeDesktop)),
            claude_variants: installs
                .iter()
                .map(|i| match i.variant {
                    claude::Variant::Msix => "msix",
                    claude::Variant::Standard => "standard",
                })
                .collect(),
            autostart: app.autolaunch().is_enabled().ok(),
        };
        let home = app.path().home_dir().ok();
        Ok(diagnostics::build(
            &inp,
            &ctx.shared.view(),
            home.as_deref(),
        ))
    }

    fn os_line() -> String {
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
    }

    #[tauri::command]
    #[specta::specta]
    pub fn get_about(app: AppHandle, ctx: State<'_, AppCtx>) -> AboutInfo {
        let data_dir = sr_client::naming::resolve_data_dir(None)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        AboutInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            data_dir,
            start_with_windows: app.autolaunch().is_enabled().unwrap_or(false),
            autostart_decided: ctx.prefs().autostart_decided,
        }
    }

    /// "Start with Windows" (HKCU Run → `sr-app --background`). Returns the new state. Records
    /// that a choice was made, so the wizard never overrides it later (OQ-16).
    #[tauri::command]
    #[specta::specta]
    pub fn set_start_with_windows(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
        enabled: bool,
    ) -> Result<bool, UiError> {
        let al = app.autolaunch();
        let r = if enabled { al.enable() } else { al.disable() };
        r.map_err(|e| {
            tracing::warn!(err = %e, "autostart change failed");
            UiError::new(
                "Couldn't change this setting",
                &format!(
                    "{} didn't allow the change. Try again.",
                    crate::dto::OS_NAME
                ),
                UiAction::Retry,
                "Retry",
                "Autostart",
            )
        })?;
        ctx.mark_autostart_decided();
        Ok(al.is_enabled().unwrap_or(enabled))
    }

    // -----------------------------------------------------------------------------------------
    // M3: providers, consent, answers

    /// The AI providers that can be offered (from the generated provider list, never hard-coded).
    #[tauri::command]
    #[specta::specta]
    pub fn get_providers() -> Vec<ProviderOption> {
        providers::catalog().to_vec()
    }

    /// Providers & Keys: the configured provider, its key state (last 4 characters only) and
    /// models. Read when the page opens or after an action; never polled.
    #[tauri::command]
    #[specta::specta]
    pub async fn get_provider_status(ctx: State<'_, AppCtx>) -> Result<ProviderStatus, UiError> {
        super::get_provider_status(&*ctx.link).await
    }

    /// [Check and save]: validate the key live, list the models, pick the default, keep the key
    /// in Windows Credential Manager. `check = false` is "Save without checking" (offline).
    #[tauri::command]
    #[specta::specta]
    pub async fn save_provider(
        ctx: State<'_, AppCtx>,
        provider: String,
        key: Option<String>,
        address: Option<String>,
        check: bool,
    ) -> Result<SaveResult, UiError> {
        super::save_provider(
            &*ctx.link,
            providers::catalog(),
            &provider,
            key.as_deref(),
            address.as_deref(),
            check,
        )
        .await
    }

    /// [Check again]: re-validate the saved key (a user action, not a poll).
    #[tauri::command]
    #[specta::specta]
    pub async fn check_provider(ctx: State<'_, AppCtx>) -> Result<ProviderStatus, UiError> {
        super::provider_call(&*ctx.link, method::PROVIDERS_CHECK, json!({})).await
    }

    /// "Model for answers": `None` = automatic. Saves at once.
    #[tauri::command]
    #[specta::specta]
    pub async fn set_provider_model(
        ctx: State<'_, AppCtx>,
        model: Option<String>,
    ) -> Result<ProviderStatus, UiError> {
        super::provider_call(
            &*ctx.link,
            method::PROVIDERS_SET_MODEL,
            json!({ "model": model }),
        )
        .await
    }

    /// [Remove provider…] → [Remove]: the key is deleted from Credential Manager, consents are
    /// dropped and the answer tools are hidden again.
    #[tauri::command]
    #[specta::specta]
    pub async fn remove_provider(ctx: State<'_, AppCtx>) -> Result<ProviderStatus, UiError> {
        super::provider_call(&*ctx.link, method::PROVIDERS_REMOVE, json!({})).await
    }

    /// "Allowed to send" (page 21): one row per purpose.
    #[tauri::command]
    #[specta::specta]
    pub async fn get_consents(ctx: State<'_, AppCtx>) -> Result<Vec<ConsentRow>, UiError> {
        super::get_consents(&*ctx.link).await
    }

    /// Allow (after the consent dialog) or revoke one purpose for `provider`.
    #[tauri::command]
    #[specta::specta]
    pub async fn set_consent(
        ctx: State<'_, AppCtx>,
        provider: String,
        purpose: ConsentPurpose,
        allowed: bool,
    ) -> Result<Vec<ConsentRow>, UiError> {
        let rows = super::set_consent(&*ctx.link, &provider, purpose, allowed).await?;
        ctx.shared.update(|s| s.clear_consent_request());
        Ok(rows)
    }

    /// The consent dialog was declined: stop asking until the service needs it again.
    #[tauri::command]
    #[specta::specta]
    pub fn dismiss_consent_request(ctx: State<'_, AppCtx>) {
        ctx.shared.update(|s| s.clear_consent_request());
    }

    /// Settings → Answers (only shown once a provider exists).
    #[tauri::command]
    #[specta::specta]
    pub async fn get_answers_settings(ctx: State<'_, AppCtx>) -> Result<AnswersView, UiError> {
        super::get_answers(&*ctx.link).await
    }

    /// Change "Let connected apps ask for answers…" and/or "Deeper answers". Saves at once; the
    /// service tells connected apps through `tools/list_changed`.
    #[tauri::command]
    #[specta::specta]
    pub async fn set_answers_settings(
        ctx: State<'_, AppCtx>,
        answers_enabled: Option<bool>,
        deeper_answers: Option<bool>,
        max_rounds: Option<u32>,
    ) -> Result<AnswersView, UiError> {
        super::set_answers(&*ctx.link, answers_enabled, deeper_answers, max_rounds).await
    }

    // -----------------------------------------------------------------------------------------
    // M4: restart, connected apps, About, links

    /// [Restart now] / [Restart Selene Retrieval]: drain, then start the service again.
    #[tauri::command]
    #[specta::specta]
    pub async fn restart_service(ctx: State<'_, AppCtx>) -> Result<(), UiError> {
        super::restart_service(&*ctx.link, ctx.link.shutdown_drain(), &ctx.shared).await?;
        ctx.link.reset().await;
        ctx.link.retry.notify_one();
        let shared = ctx.shared.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(RESTART_GIVE_UP).await;
            shared.update(|s| s.set_restarting(false));
        });
        Ok(())
    }

    /// Connected apps (page 29): one card per one-click app + the "Other app" setup text, with
    /// "Last used" from the service when it can be reached.
    #[tauri::command]
    #[specta::specta]
    pub async fn get_clients(ctx: State<'_, AppCtx>) -> Result<ClientsView, UiError> {
        let last = crate::connected::list(&*ctx.link)
            .await
            .map(|v| crate::connected::last_used(&v))
            .unwrap_or_default();
        Ok(apps::clients_view(&ctx.connect, &ctx.tracker(), &last))
    }

    /// [Connect Cursor] / [Connect VS Code] / [Connect Claude Desktop]. Returns the plain next
    /// step; the cards update by push (`ClientsChanged`).
    #[tauri::command]
    #[specta::specta]
    pub async fn connect_client(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
        client: ClientApp,
    ) -> Result<String, UiError> {
        ctx.tracker().own_write(client, std::time::Instant::now());
        let r = apps::connect_app(&ctx.connect, client);
        crate::watch_client_configs(&app);
        crate::push_clients(&app);
        if r.is_ok() {
            crate::activity::note(&*ctx.link, true, client_kind(client)).await;
        }
        r
    }

    /// [Disconnect]: removes only our entry from that app's settings.
    #[tauri::command]
    #[specta::specta]
    pub async fn disconnect_client(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
        client: ClientApp,
    ) -> Result<String, UiError> {
        ctx.tracker().own_write(client, std::time::Instant::now());
        let r = apps::disconnect_app(&ctx.connect, client);
        crate::push_clients(&app);
        if r.is_ok() {
            crate::activity::note(&*ctx.link, false, client_kind(client)).await;
        }
        r
    }

    fn client_kind(c: ClientApp) -> sr_common::error::ClientKind {
        match c {
            ClientApp::ClaudeDesktop => sr_common::error::ClientKind::ClaudeDesktop,
            ClientApp::Cursor => sr_common::error::ClientKind::Cursor,
            ClientApp::VsCode => sr_common::error::ClientKind::VsCode,
        }
    }

    /// [Repair…] → [Repair] (OQ-46): keep the unreadable file next to it, write a fresh one with
    /// only Selene Retrieval in it.
    #[tauri::command]
    #[specta::specta]
    pub fn repair_client(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
        client: ClientApp,
    ) -> Result<String, UiError> {
        ctx.tracker().own_write(client, std::time::Instant::now());
        let r = apps::repair_app(&ctx.connect, client, std::time::SystemTime::now());
        crate::watch_client_configs(&app);
        crate::push_clients(&app);
        r
    }

    /// "View licences" (page 18): the texts bundled in the install folder's `licenses`.
    #[tauri::command]
    #[specta::specta]
    pub fn get_licenses() -> Vec<LicenseText> {
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("licenses")))
            .map(|d| super::read_licenses(&d))
            .unwrap_or_default()
    }

    /// "Save as file…" (page 18, OQ-30): the same redacted text as "Copy diagnostics", written to
    /// the `.txt` file the user picked in the Windows save dialog.
    #[tauri::command]
    #[specta::specta]
    pub async fn save_diagnostics(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
        path: String,
    ) -> Result<(), UiError> {
        let target = super::diagnostics_target(&path).ok_or_else(super::save_failed)?;
        let text = get_diagnostics(app, ctx).await?;
        std::fs::write(&target, text).map_err(|e| {
            tracing::warn!(kind = ?e.kind(), "diagnostics not saved");
            super::save_failed()
        })
    }

    /// "Where do I get a key?" (page 21): opens a provider's key page from the provider list.
    #[tauri::command]
    #[specta::specta]
    pub fn open_link(url: String) -> Result<(), UiError> {
        crate::opener::open_link(&url, &providers::key_urls()).map_err(|_| super::open_failed())
    }

    /// [Show in folder] (page 26, OQ-40): selects a search result in Explorer. Only files inside
    /// an added folder; the file itself is never opened or run.
    #[tauri::command]
    #[specta::specta]
    pub fn show_in_folder(ctx: State<'_, AppCtx>, path: String) -> Result<(), UiError> {
        let roots = super::added_folder_paths(&ctx.shared.view());
        crate::opener::show_in_folder(Path::new(&path), &roots).map_err(|_| super::open_failed())
    }

    // -----------------------------------------------------------------------------------------
    // Amendment 13: "Create a folder for me" (docs/07-default-folder-plan.md)

    /// `FOLDERID_Documents` through Tauri's path resolver (`SHGetKnownFolderPath`, follows
    /// OneDrive / folder redirection). Debug builds: `SR_DOCUMENTS_DIR` (tests; ignored in
    /// release, like every dev hook), so a dev run never writes into the real Documents.
    fn documents_dir(app: &AppHandle) -> Option<PathBuf> {
        if let Some(d) = sr_client::naming::dev_env("SR_DOCUMENTS_DIR") {
            return Some(PathBuf::from(d));
        }
        app.path().document_dir().ok().filter(|p| p.is_absolute())
    }

    /// One [Create a folder for me] at a time (a double click sees the first one's folder).
    static CREATING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Whether to offer [Create a folder for me] (read again when the pushed folder list or the
    /// policy changes; never on a timer).
    #[tauri::command]
    #[specta::specta]
    pub fn get_default_folder(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
    ) -> crate::default_folder::DefaultFolderOffer {
        let view = ctx.shared.view();
        crate::default_folder::offer(
            documents_dir(&app).as_deref(),
            &crate::default_folder::added_folders(&view),
            crate::default_folder::roots_managed(&view),
        )
    }

    /// [Create a folder for me]: makes `Documents\Selene Retrieval` (or finds it), adds it like
    /// any folder, and opens it in File Explorer. Takes no path: the app computes it.
    #[tauri::command]
    #[specta::specta]
    pub async fn create_default_folder(
        app: AppHandle,
        ctx: State<'_, AppCtx>,
    ) -> Result<crate::default_folder::DefaultFolderResult, UiError> {
        use crate::default_folder as df;
        let _one = CREATING.lock().await;
        let view = ctx.shared.view();
        let docs = documents_dir(&app);
        let added = df::added_folders(&view);
        // Fail closed: if Windows can't say, treat the app as elevated (nothing is made).
        let elevated = sr_client::sys::process_elevated().unwrap_or(true);
        let input = df::CreateInput {
            docs: docs.as_deref(),
            added: &added,
            roots_managed: df::roots_managed(&view),
            elevated,
            timeout: df::FS_TIMEOUT,
        };
        let (mut r, info) = df::create(&*ctx.link, input).await?;
        if let Some(info) = &info {
            ctx.shared.update(|s| s.apply_folder_info(info));
            // One catch-up check after the watcher has armed (R45): a file dropped into the
            // just-opened folder right away is never left for the hourly check.
            let link = ctx.link.clone();
            let id = r.folder_id.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(df::CATCH_UP_AFTER).await;
                df::catch_up(&*link, &id).await;
            });
        }
        let path = PathBuf::from(&r.path);
        r.in_onedrive = df::in_onedrive(&path, &df::onedrive_roots());
        // The folder's own path, or the added folder it is part of (Documents).
        let mut roots = super::added_folder_paths(&ctx.shared.view());
        if info.is_some() {
            roots.push(path.clone());
        }
        r.opened = tauri::async_runtime::spawn_blocking(move || {
            crate::opener::open_folder(&path, &roots).is_ok()
        })
        .await
        .unwrap_or(false);
        tracing::info!(target: "sr_app::default_folder", created = r.created, already_added = r.already_added, opened = r.opened, "default folder ready");
        Ok(r)
    }

    /// [Open folder] (folder details, wizard): opens an added folder in File Explorer. The path
    /// comes from the app's own folder list, never from the page.
    #[tauri::command]
    #[specta::specta]
    pub fn open_folder(ctx: State<'_, AppCtx>, folder_id: String) -> Result<(), UiError> {
        let view = ctx.shared.view();
        let roots = super::added_folder_paths(&view);
        let path = view
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .and_then(|f| f.path.clone())
            .ok_or_else(super::open_failed)?;
        crate::opener::open_folder(Path::new(&path), &roots).map_err(|_| super::open_failed())
    }

    // -----------------------------------------------------------------------------------------
    // M4: folders (pages 24, 25, 31)

    /// Folder detail (page 25): read when the page opens and after a pushed change of the
    /// folder's state; never on a timer.
    #[tauri::command]
    #[specta::specta]
    pub async fn get_folder_detail(
        ctx: State<'_, AppCtx>,
        folder_id: String,
    ) -> Result<FolderDetailView, UiError> {
        folders::folder_detail(&*ctx.link, &folder_id).await
    }

    /// "Show all" skipped files (up to 500 per page).
    #[tauri::command]
    #[specta::specta]
    pub async fn get_skipped_files(
        ctx: State<'_, AppCtx>,
        folder_id: String,
        offset: u32,
        limit: u32,
        max_file_bytes: u32,
    ) -> Result<SkippedPageView, UiError> {
        folders::skipped_files(&*ctx.link, &folder_id, offset, limit, max_file_bytes).await
    }

    /// [Retry] one file or [Retry all] (`file_ids = null`).
    #[tauri::command]
    #[specta::specta]
    pub async fn retry_files(
        ctx: State<'_, AppCtx>,
        folder_id: String,
        file_ids: Option<Vec<String>>,
    ) -> Result<u32, UiError> {
        folders::retry_files(&*ctx.link, &folder_id, file_ids).await
    }

    /// What saving `after` would do (the UI confirms re-reads first, page 25).
    #[tauri::command]
    #[specta::specta]
    pub fn folder_change_impact(
        before: FolderSettingsView,
        after: FolderSettingsView,
    ) -> ChangeImpact {
        folders::change_impact(&before, &after)
    }

    /// Live field checks (DS-4.18), in plain words.
    #[tauri::command]
    #[specta::specta]
    pub fn check_folder_settings(settings: FolderSettingsView) -> Vec<FieldProblem> {
        folders::validate(&settings)
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn update_folder_settings(
        ctx: State<'_, AppCtx>,
        folder_id: String,
        settings: FolderSettingsView,
    ) -> Result<SettingsSaved, UiError> {
        folders::update_settings(&*ctx.link, &folder_id, &settings).await
    }

    /// [Stop] (page 25, OQ-39): the folder shows "Stopped" until [Continue].
    #[tauri::command]
    #[specta::specta]
    pub async fn stop_folder(ctx: State<'_, AppCtx>, folder_id: String) -> Result<(), UiError> {
        folders::cancel_folder(&*ctx.link, &folder_id).await?;
        ctx.shared.update(|s| s.mark_stopped(&folder_id));
        Ok(())
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn get_groups(ctx: State<'_, AppCtx>) -> Result<Vec<GroupRow>, UiError> {
        folders::list_groups(&*ctx.link).await
    }

    fn name_problem(text: String) -> UiError {
        UiError::new(
            "Check the name",
            &text,
            UiAction::None,
            "",
            "InvalidInput · name",
        )
    }

    /// [New group] → [Create]. A taken or empty name gets its plain field text.
    #[tauri::command]
    #[specta::specta]
    pub async fn create_group(ctx: State<'_, AppCtx>, name: String) -> Result<GroupRow, UiError> {
        let existing = folders::list_groups(&*ctx.link).await?;
        if let Some(p) = folders::group_name_problem(&name, &existing, None) {
            return Err(name_problem(p));
        }
        folders::create_group(&*ctx.link, &name).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn rename_group(
        ctx: State<'_, AppCtx>,
        group_id: String,
        name: String,
    ) -> Result<GroupRow, UiError> {
        let existing = folders::list_groups(&*ctx.link).await?;
        if let Some(p) = folders::group_name_problem(&name, &existing, Some(&group_id)) {
            return Err(name_problem(p));
        }
        folders::rename_group(&*ctx.link, &group_id, &name).await
    }

    /// [Delete…] → [Delete]: its folders move to "My folders"; nothing leaves search.
    #[tauri::command]
    #[specta::specta]
    pub async fn delete_group(
        ctx: State<'_, AppCtx>,
        group_id: String,
    ) -> Result<GroupDeleted, UiError> {
        folders::delete_group(&*ctx.link, &group_id).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn move_folder_to_group(
        ctx: State<'_, AppCtx>,
        folder_id: String,
        group_id: String,
    ) -> Result<(), UiError> {
        folders::move_folder(&*ctx.link, &folder_id, &group_id).await
    }

    // -----------------------------------------------------------------------------------------
    // M4: search, Ask, Prompt Enhancer (pages 26–28)

    /// A search the user asked for (Enter / [Search]); never as-you-type.
    #[tauri::command]
    #[specta::specta]
    pub async fn search(
        ctx: State<'_, AppCtx>,
        query: String,
        folder_ids: Option<Vec<String>>,
        max_results: Option<u32>,
    ) -> Result<SearchView, UiError> {
        crate::search::search(&*ctx.link, &query, folder_ids, max_results).await
    }

    /// [Ask]: a written answer with citations. Progress lines arrive as `ServicePush`
    /// `request_progress` signals for `request_id`.
    #[tauri::command]
    #[specta::specta]
    pub async fn ask(
        ctx: State<'_, AppCtx>,
        question: String,
        folder_ids: Option<Vec<String>>,
        request_id: String,
    ) -> Result<AnswerView, UiError> {
        crate::search::ask(&*ctx.link, &question, folder_ids, &request_id).await
    }

    /// [Stop] for Ask / Prompt Enhancer: no further provider calls (nothing more is billed).
    #[tauri::command]
    #[specta::specta]
    pub async fn cancel_request(
        ctx: State<'_, AppCtx>,
        request_id: String,
    ) -> Result<bool, UiError> {
        crate::search::cancel_request(&*ctx.link, &request_id).await
    }

    /// [Improve prompt] (page 28): only the prompt and the scope are sent.
    #[tauri::command]
    #[specta::specta]
    pub async fn enhance_prompt(
        ctx: State<'_, AppCtx>,
        prompt: String,
        folder_ids: Option<Vec<String>>,
        request_id: String,
    ) -> Result<EnhanceView, UiError> {
        crate::search::enhance(&*ctx.link, &prompt, folder_ids, &request_id).await
    }

    // -----------------------------------------------------------------------------------------
    // M4: Activity (page 30)

    #[tauri::command]
    #[specta::specta]
    pub async fn get_activity(
        ctx: State<'_, AppCtx>,
        before_id: Option<f64>,
    ) -> Result<ActivityPageView, UiError> {
        crate::activity::list(&*ctx.link, before_id).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn get_egress(
        ctx: State<'_, AppCtx>,
        before_id: Option<f64>,
        provider: Option<String>,
    ) -> Result<EgressPageView, UiError> {
        crate::activity::egress(&*ctx.link, before_id, provider).await
    }

    // -----------------------------------------------------------------------------------------
    // M4: per-app access and local HTTP (page 29 Advanced)

    #[tauri::command]
    #[specta::specta]
    pub async fn get_service_clients(
        ctx: State<'_, AppCtx>,
    ) -> Result<ServiceClientsView, UiError> {
        crate::connected::list(&*ctx.link).await
    }

    /// "Folder access per app": All my folders, or only some folder groups.
    #[tauri::command]
    #[specta::specta]
    pub async fn set_client_scope(
        ctx: State<'_, AppCtx>,
        client_id: String,
        scope: ScopeView,
    ) -> Result<ServiceClient, UiError> {
        crate::connected::set_scope(&*ctx.link, &client_id, &scope).await
    }

    /// "Allow other local tools to connect over HTTP" (applies at once).
    #[tauri::command]
    #[specta::specta]
    pub async fn set_http_enabled(
        ctx: State<'_, AppCtx>,
        enabled: bool,
    ) -> Result<HttpView, UiError> {
        crate::connected::set_http(&*ctx.link, enabled).await
    }

    /// [Add a tool…]: a new password for one local tool, shown once.
    #[tauri::command]
    #[specta::specta]
    pub async fn create_tool_token(
        ctx: State<'_, AppCtx>,
        name: String,
    ) -> Result<TokenView, UiError> {
        crate::connected::create_token(&*ctx.link, &name).await
    }

    /// Copy the setup text again = a new password (the old one stops working).
    #[tauri::command]
    #[specta::specta]
    pub async fn rotate_tool_token(
        ctx: State<'_, AppCtx>,
        client_id: String,
    ) -> Result<TokenView, UiError> {
        crate::connected::rotate_token(&*ctx.link, &client_id).await
    }

    /// [Revoke…] → [Revoke]: the tool loses access at once.
    #[tauri::command]
    #[specta::specta]
    pub async fn revoke_tool(ctx: State<'_, AppCtx>, client_id: String) -> Result<(), UiError> {
        crate::connected::revoke(&*ctx.link, &client_id).await
    }

    // -----------------------------------------------------------------------------------------
    // M4: move the search index (page 32)

    #[tauri::command]
    #[specta::specta]
    pub async fn get_index_location(ctx: State<'_, AppCtx>) -> Result<IndexLocationView, UiError> {
        crate::index_move::status(&*ctx.link).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn check_index_target(
        ctx: State<'_, AppCtx>,
        target: String,
    ) -> Result<MoveCheckView, UiError> {
        crate::index_move::check(&*ctx.link, &target).await
    }

    /// [Move]: the service stops for a moment, moves the index and starts again.
    #[tauri::command]
    #[specta::specta]
    pub async fn move_index(ctx: State<'_, AppCtx>, target: String) -> Result<(), UiError> {
        crate::index_move::start(&*ctx.link, &target).await?;
        ctx.shared.update(|s| s.set_restarting(true));
        Ok(())
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn cancel_index_move(ctx: State<'_, AppCtx>) -> Result<bool, UiError> {
        crate::index_move::cancel(&*ctx.link).await
    }

    /// R34 "Search index not found": [Move the index back] or [Start fresh]. The service leaves
    /// its recovery mode and starts again; the shell shows "Restarting the background service…"
    /// until the new run's snapshot arrives (pushed).
    #[tauri::command]
    #[specta::specta]
    pub async fn recover_index(
        ctx: State<'_, AppCtx>,
        choice: crate::policy::RecoverChoice,
    ) -> Result<(), UiError> {
        super::recover_index(&*ctx.link, &ctx.shared, choice).await?;
        ctx.link.reset().await;
        ctx.link.retry.notify_one();
        let shared = ctx.shared.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(RESTART_GIVE_UP).await;
            shared.update(|s| s.set_restarting(false));
        });
        Ok(())
    }

    /// [Remove…] → [Remove] on Connected apps (contract §4 `clients.forget`): a stale app leaves
    /// the list. Returns whether the service had it.
    #[tauri::command]
    #[specta::specta]
    pub async fn forget_client(ctx: State<'_, AppCtx>, client_id: String) -> Result<bool, UiError> {
        crate::connected::forget(&*ctx.link, &client_id).await
    }

    // -----------------------------------------------------------------------------------------
    // M5-C: "Claude (sign in)" / "Codex (sign in)" reuse the local Claude Code / Codex login
    // (Amendments 10.2 / 10.3; pages 20 and 21). No token exists in Selene Retrieval.

    /// The sign-in state of `provider` (`claudecode` | `codex`). Read when a card opens; with
    /// `refresh` the tool is asked again now ([Check again], the window regaining focus).
    #[tauri::command]
    #[specta::specta]
    pub async fn cli_signin_status(
        ctx: State<'_, AppCtx>,
        provider: String,
        refresh: bool,
    ) -> Result<crate::cli_signin::CliSignInStatus, UiError> {
        crate::cli_signin::status(&*ctx.link, &provider, refresh).await
    }

    /// [Sign in]: the service opens the tool's own sign-in window (it opens the browser
    /// itself). The app opens nothing.
    #[tauri::command]
    #[specta::specta]
    pub async fn cli_signin_start(
        ctx: State<'_, AppCtx>,
        provider: String,
    ) -> Result<crate::cli_signin::CliSignInStart, UiError> {
        crate::cli_signin::start(&*ctx.link, &provider).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, UiError> {
        crate::updater::check(&app).await
    }

    #[tauri::command]
    #[specta::specta]
    pub async fn install_update(app: AppHandle, ctx: State<'_, AppCtx>) -> Result<(), UiError> {
        crate::updater::install(&app, &ctx.link).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;
    use std::fs;

    /// An absolute folder and a home folder on this platform.
    #[cfg(windows)]
    const DOCS: &str = r"C:\Docs";
    #[cfg(windows)]
    const HOME: &str = r"C:\Users\a";
    #[cfg(not(windows))]
    const DOCS: &str = "/Users/a/Docs";
    #[cfg(not(windows))]
    const HOME: &str = "/Users/a";

    #[tokio::test]
    async fn add_folder_forwards_and_maps_errors() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"folder_id": "01JABC", "name": "Docs"})));
        assert_eq!(
            add_folder(&rpc, &format!(" {DOCS} ")).await,
            Ok(json!({"folder_id": "01JABC", "name": "Docs"}))
        );
        assert_eq!(
            rpc.calls(),
            vec![("folders.add".to_owned(), json!({"path": DOCS}))]
        );

        rpc.reply(Err(Error::new(ErrorCode::PathRefused)
            .with_path(HOME)
            .with_detail("dangerous root")));
        let e = add_folder(&rpc, HOME).await.unwrap_err();
        assert_eq!(e.title, "This folder can't be added");
        assert!(matches!(e.action, UiAction::ChooseSubfolder { .. }));
        assert_eq!(e.action_label, "Choose a subfolder");

        // Relative or empty paths never reach the service.
        let before = rpc.calls().len();
        assert_eq!(
            add_folder(&rpc, "docs").await.unwrap_err().action,
            UiAction::ChooseFolder
        );
        assert_eq!(
            add_folder(&rpc, "  ").await.unwrap_err().action,
            UiAction::ChooseFolder
        );
        assert_eq!(rpc.calls().len(), before);
    }

    #[tokio::test]
    async fn estimate_uses_the_service_and_falls_back_for_an_older_one() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(
            json!({"files": 2000, "text_bytes": 20_000_000, "capped": false,
            "est_chunks": 12500, "eta_s": 40, "gpu": true}),
        ));
        let e = estimate_from_service(&rpc, r" C:\Docs ")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e.files, 2000);
        assert_eq!(e.eta_s, 40);
        assert!(e.text.starts_with("About 2,000 files."), "{}", e.text);
        assert_eq!(
            rpc.calls(),
            vec![("folders.estimate".to_owned(), json!({"path": r"C:\Docs"}))]
        );
        // Older service: unknown method -> local fallback.
        rpc.reply(Err(
            Error::new(ErrorCode::InvalidInput).with_detail("unknown method")
        ));
        assert_eq!(estimate_from_service(&rpc, r"C:\Docs").await, Ok(None));
        // A refused folder keeps its message and "Choose a subfolder".
        rpc.reply(Err(
            Error::new(ErrorCode::PathRefused).with_path(r"C:\Users\a")
        ));
        let e = estimate_from_service(&rpc, r"C:\Users\a")
            .await
            .unwrap_err();
        assert!(matches!(e.action, UiAction::ChooseSubfolder { .. }));
    }

    #[tokio::test]
    async fn pause_approvals_and_model_change_forward() {
        let rpc = FakeRpc::default();
        set_paused(&rpc, "01F", true).await.unwrap();
        set_paused(&rpc, "01F", false).await.unwrap();
        decide_approval(&rpc, "01A", true).await.unwrap();
        decide_approval(&rpc, "01B", false).await.unwrap();
        use_recommended_model(&rpc, false).await.unwrap();
        assert_eq!(
            rpc.calls(),
            vec![
                ("folders.pause".to_owned(), json!({"folder_id": "01F"})),
                ("folders.resume".to_owned(), json!({"folder_id": "01F"})),
                (
                    "approvals.approve".to_owned(),
                    json!({"approval_id": "01A"})
                ),
                ("approvals.deny".to_owned(), json!({"approval_id": "01B"})),
                (
                    "search_model.use_recommended".to_owned(),
                    json!({"allow_metered": false})
                ),
            ]
        );
        rpc.reply(Err(Error::new(ErrorCode::DiskFull)));
        let e = use_recommended_model(&rpc, false).await.unwrap_err();
        assert_eq!(e.title, "Not enough disk space");
    }

    #[tokio::test]
    async fn advanced_settings_forward_and_map() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(
            json!({"use_graphics_card": true, "mcp_add_folder_enabled": false,
            "mcp_sync_folder_enabled": false, "tier_override": null, "restart_needed": false}),
        ));
        let a = get_advanced(&rpc).await.unwrap();
        assert!(a.use_graphics_card && !a.allow_app_folder_requests && !a.restart_needed);
        assert!(!a.allow_app_folder_checks);
        rpc.reply(Ok(
            json!({"use_graphics_card": false, "mcp_add_folder_enabled": false,
            "mcp_sync_folder_enabled": false, "tier_override": null, "restart_needed": true}),
        ));
        let a = set_advanced(&rpc, Some(false), None, None).await.unwrap();
        assert!(!a.use_graphics_card && a.restart_needed);
        // "Let connected apps check folders for changes": forwarded alone, the saved state comes
        // back (an older service without the field reads as off).
        rpc.reply(Ok(
            json!({"use_graphics_card": false, "mcp_add_folder_enabled": false,
            "mcp_sync_folder_enabled": true, "tier_override": null, "restart_needed": false}),
        ));
        let a = set_advanced(&rpc, None, None, Some(true)).await.unwrap();
        assert!(a.allow_app_folder_checks && !a.allow_app_folder_requests);
        assert!(!advanced_view(&json!({})).allow_app_folder_checks);
        choose_tier(&rpc, TierName::Better).await.unwrap();
        assert_eq!(
            rpc.calls(),
            vec![
                ("settings.advanced".to_owned(), json!({})),
                (
                    "settings.update_advanced".to_owned(),
                    json!({"use_graphics_card": false, "mcp_add_folder_enabled": null,
                           "mcp_sync_folder_enabled": null})
                ),
                (
                    "settings.update_advanced".to_owned(),
                    json!({"use_graphics_card": null, "mcp_add_folder_enabled": null,
                           "mcp_sync_folder_enabled": true})
                ),
                (
                    "search_model.choose".to_owned(),
                    json!({"tier": "standard", "allow_metered": false})
                ),
            ]
        );
    }

    #[tokio::test]
    async fn remove_and_sync_forward() {
        let rpc = FakeRpc::default();
        remove_folder(&rpc, "01J").await.unwrap();
        sync_folder(&rpc, None).await.unwrap();
        rpc.reply(Err(Error::new(ErrorCode::ServiceUnavailable)));
        let e = sync_folder(&rpc, Some("01J")).await.unwrap_err();
        assert_eq!(e.action, UiAction::RestartApp);
        assert_eq!(
            rpc.calls(),
            vec![
                ("folders.remove".to_owned(), json!({"folder_id": "01J"})),
                ("folders.sync".to_owned(), json!({"folder_id": null})),
                ("folders.sync".to_owned(), json!({"folder_id": "01J"})),
            ]
        );
    }

    /// Placeholder provider list for tests (not the shipped list).
    fn test_list() -> Vec<ProviderOption> {
        vec![
            ProviderOption {
                id: "keyed".into(),
                name: "Keyed AI".into(),
                auth: AuthKind::ApiKey,
                available: true,
                default_address: None,
                model_hint: Some("model-x".into()),
                key_url: None,
                disabled_reason: None,
            },
            ProviderOption {
                id: "signin".into(),
                name: "Sign-in AI".into(),
                auth: AuthKind::Oauth,
                available: false,
                default_address: None,
                model_hint: None,
                key_url: None,
                disabled_reason: Some("pending_vendor_approval".into()),
            },
            ProviderOption {
                id: "here".into(),
                name: "Here".into(),
                auth: AuthKind::Local,
                available: true,
                default_address: Some("http://localhost:1".into()),
                model_hint: None,
                key_url: None,
                disabled_reason: None,
            },
        ]
    }

    #[test]
    fn provider_status_maps_and_never_exposes_more_than_last4() {
        assert_eq!(
            provider_status(&json!({"provider": null})),
            ProviderStatus::none()
        );
        let st = provider_status(&json!({
            "provider": "keyed", "key_saved": true, "key_last4": "sk-THISISTOOLONG1234",
            "check": "working", "model": null, "default_model": "model-x",
            "models": [{"id": "model-x", "display_name": "Model X"}, {"id": "model-y"}],
            "remote_embeddings": false
        }));
        assert_eq!(st.key_last4, "1234");
        assert_eq!(st.check, KeyCheck::Working);
        assert_eq!(st.models.len(), 2);
        assert_eq!(st.models[1].name, "model-y");
        assert_eq!(st.default_model.as_deref(), Some("model-x"));
        let st = provider_status(&json!({
            "provider": "keyed", "key_saved": true, "key_last4": "abcd",
            "last_error": {"code": "ProviderAuth", "ctx": {"provider": "Keyed AI"}}
        }));
        assert_eq!(st.check, KeyCheck::Failed);
        let e = st.last_error.unwrap();
        assert_eq!(e.title, "Your Keyed AI key was rejected");
        assert_eq!(
            e.action,
            UiAction::ReenterKey {
                provider: "Keyed AI".into()
            }
        );
        assert_eq!(e.action_label, "Re-enter key");
    }

    #[tokio::test]
    async fn save_provider_sends_key_once_and_maps_errors() {
        let list = test_list();
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"status": {"provider": "keyed", "key_saved": true,
            "key_last4": "wxyz", "check": "working", "default_model": "model-x"}, "validated": true})));
        let r = save_provider(&rpc, &list, "keyed", Some("  sk-secret-wxyz "), None, true)
            .await
            .unwrap();
        assert!(r.validated);
        assert_eq!(r.status.key_last4, "wxyz");
        assert_eq!(
            rpc.calls(),
            vec![(
                "providers.save".to_owned(),
                json!({"provider": "keyed", "key": "sk-secret-wxyz", "address": null, "check": true})
            )]
        );
        // Local provider: address (default pre-filled), no key.
        save_provider(&rpc, &list, "here", None, None, true)
            .await
            .unwrap();
        assert_eq!(
            rpc.calls()[1].1,
            json!({"provider": "here", "key": null, "address": "http://localhost:1", "check": true})
        );
        // "Claude (sign in)" is saved without a key (the service uses the CLI's own sign-in).
        let mut with_claude = list.clone();
        with_claude.push(ProviderOption {
            id: sr_common::api::CLAUDE_SIGNIN_PROVIDER.into(),
            name: sr_common::api::CLAUDE_SIGNIN_LABEL.into(),
            auth: AuthKind::Oauth,
            available: true,
            default_address: None,
            model_hint: None,
            key_url: None,
            disabled_reason: None,
        });
        rpc.reply(Ok(
            json!({"status": {"provider": "claudecode", "check": "working"}}),
        ));
        save_provider(
            &rpc,
            &with_claude,
            "claudecode",
            Some("ignored"),
            None,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            rpc.calls().last().unwrap().1,
            json!({"provider": "claudecode", "key": null, "address": null, "check": true})
        );
        // Other sign-in providers and unknown ids never reach the service.
        let n = rpc.calls().len();
        assert!(
            save_provider(&rpc, &list, "signin", Some("k"), None, true)
                .await
                .is_err()
        );
        assert!(
            save_provider(&rpc, &list, "made-up", Some("k"), None, true)
                .await
                .is_err()
        );
        assert!(
            save_provider(&rpc, &list, "keyed", Some("   "), None, true)
                .await
                .is_err()
        );
        assert_eq!(rpc.calls().len(), n);
        // Wrong key, quota, offline: the mapped message and one action each.
        for (code, title, action) in [
            (
                ErrorCode::ProviderAuth,
                "Your Keyed AI key was rejected",
                UiAction::ReenterKey {
                    provider: "Keyed AI".into(),
                },
            ),
            (
                ErrorCode::ProviderQuota,
                "Your Keyed AI account is out of credit",
                UiAction::OpenProviders,
            ),
            (ErrorCode::Offline, "You're offline", UiAction::Retry),
            (
                ErrorCode::NetworkBlocked,
                "Connection blocked",
                UiAction::CopyDiagnostics,
            ),
        ] {
            rpc.reply(Err(Error::new(code).with_provider("Keyed AI")));
            let e = save_provider(&rpc, &list, "keyed", Some("k"), None, true)
                .await
                .unwrap_err();
            assert_eq!((e.title.as_str(), &e.action), (title, &action), "{code:?}");
            assert!(!e.action_label.is_empty());
        }
    }

    #[tokio::test]
    async fn consent_and_answers_forward() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!([
            {"purpose": "answer", "allowed": true},
            {"purpose": "iterate", "allowed": false},
            {"purpose": "enhance", "allowed": true},
            {"purpose": "validate_key", "allowed": true}
        ])));
        let rows = get_consents(&rpc).await.unwrap();
        assert_eq!(
            rows,
            vec![
                ConsentRow {
                    purpose: ConsentPurpose::Answer,
                    allowed: false
                },
                ConsentRow {
                    purpose: ConsentPurpose::Enhance,
                    allowed: true
                },
            ],
            "answer + iterate are one row; key checks are not a row"
        );
        set_consent(&rpc, "keyed", ConsentPurpose::EmbedDoc, true)
            .await
            .unwrap();
        rpc.reply(Ok(json!({"answers_enabled": true, "max_rounds": 3})));
        let a = get_answers(&rpc).await.unwrap();
        assert!(
            a.answers_enabled && !a.deeper_answers,
            "deeper answers default off"
        );
        assert_eq!(a.max_rounds, 3);
        set_answers(&rpc, None, Some(true), Some(9)).await.unwrap();
        let calls = rpc.calls();
        assert_eq!(
            calls[1],
            (
                "consent.set".to_owned(),
                json!({"provider": "keyed", "purpose": "embed_doc", "allowed": true})
            )
        );
        assert_eq!(
            calls[3],
            (
                "settings.update_answers".to_owned(),
                json!({"answers_enabled": null, "deeper_answers": true, "max_rounds": 5})
            )
        );
    }

    #[test]
    fn privacy_blocked_offers_allow_for_that_purpose() {
        let mut e = Error::new(ErrorCode::PrivacyBlocked).with_provider("Keyed AI");
        e.ctx.purpose = Some(sr_common::error::Purpose::Iterate);
        let u = UiError::from(e);
        assert_eq!(u.title, "Not sent to Keyed AI");
        assert_eq!(
            u.action,
            UiAction::AllowRemote {
                provider: "Keyed AI".into(),
                purpose: ConsentPurpose::Answer
            }
        );
        assert_eq!(u.action_label, "Allow");
        let u = UiError::from(Error::new(ErrorCode::ProviderNotConfigured));
        assert_eq!(
            (u.action, u.action_label.as_str()),
            (UiAction::AddProvider, "Add a provider")
        );
    }

    fn ctx(t: &tempfile::TempDir) -> ConnectCtx {
        let bridge = t.path().join("app").join("sr-bridge.exe");
        fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        fs::write(&bridge, b"MZ").unwrap();
        let roots = crate::connect::ClientRoots {
            roaming: t.path().join("Roaming"),
            local: t.path().join("Local"),
            home: t.path().join("Home"),
        };
        fs::create_dir_all(&roots.roaming).unwrap();
        fs::create_dir_all(&roots.local).unwrap();
        fs::create_dir_all(&roots.home).unwrap();
        ConnectCtx {
            roots,
            bridge_exe: bridge,
        }
    }

    #[test]
    fn claude_not_installed_is_plain_with_snippet() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        let s = claude_status(&c, false);
        assert_eq!(s.claude, ClaudeState::NotInstalled);
        assert!(s.snippet.contains("selene-retrieval") && s.snippet.contains("sr-bridge.exe"));
        let e = connect_claude(&c).unwrap_err();
        assert_eq!(e.title, "Claude Desktop isn't installed");
        assert_eq!(e.action, UiAction::CopySnippet);
    }

    #[test]
    fn connect_flow_squirrel() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        fs::create_dir_all(c.roots.local.join("AnthropicClaude")).unwrap();
        assert_eq!(claude_status(&c, false).claude, ClaudeState::NotConnected);
        let r = connect_claude(&c).unwrap();
        assert_eq!(r.status.claude, ClaudeState::Connected);
        assert!(r.message.contains("quit it and open it again"));
        assert_eq!(
            connect_claude(&c).unwrap().message,
            "Claude is already connected."
        );
        let r = disconnect_claude(&c).unwrap();
        assert_eq!(r.status.claude, ClaudeState::NotConnected);
    }

    #[test]
    fn unreadable_config_offers_snippet_and_stays_untouched() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(&t);
        fs::create_dir_all(c.roots.local.join("AnthropicClaude")).unwrap();
        let p = c.roots.roaming.join("Claude").join(claude::CONFIG_FILE);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "{ not json").unwrap();
        assert_eq!(claude_status(&c, false).claude, ClaudeState::CantRead);
        let e = connect_claude(&c).unwrap_err();
        assert_eq!(e.action, UiAction::CopySnippet);
        assert_eq!(fs::read_to_string(&p).unwrap(), "{ not json");
    }

    #[test]
    fn missing_bridge_is_an_internal_error() {
        let t = tempfile::tempdir().unwrap();
        let mut c = ctx(&t);
        c.bridge_exe = t.path().join("nope.exe");
        fs::create_dir_all(c.roots.local.join("AnthropicClaude")).unwrap();
        let e = connect_claude(&c).unwrap_err();
        assert_eq!(e.action, UiAction::CopyDiagnostics);
        assert!(!c.roots.roaming.join("Claude").exists(), "nothing written");
    }

    #[tokio::test]
    async fn restart_uses_host_restart_and_falls_back_for_an_older_service() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let drains = AtomicU32::new(0);
        let drain = || async {
            drains.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let (shared, _rx) = Shared::new();

        // Current service: `host.restart`, no drain shutdown.
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"restarting": true})));
        restart_service(&rpc, drain(), &shared).await.unwrap();
        assert_eq!(rpc.calls()[0].0, "host.restart");
        assert_eq!(drains.load(Ordering::SeqCst), 0);
        assert!(
            shared.view().restarting,
            "the shell shows Restarting… until the new run"
        );

        // Older service (unknown method): the drain shutdown instead.
        rpc.reply(Err(
            Error::new(ErrorCode::InvalidInput).with_detail("unknown method host.restart")
        ));
        restart_service(&rpc, drain(), &shared).await.unwrap();
        assert_eq!(drains.load(Ordering::SeqCst), 1);

        // Not running (code-33 panel): re-attaching starts it; no error.
        rpc.reply(Err(Error::new(ErrorCode::ServiceUnavailable)));
        restart_service(&rpc, drain(), &shared).await.unwrap();

        // A real failure: plain error, and the shell stops saying "Restarting…".
        rpc.reply(Err(Error::new(ErrorCode::PermissionDenied)));
        assert!(restart_service(&rpc, drain(), &shared).await.is_err());
        assert!(!shared.view().restarting);
    }

    #[test]
    fn licences_and_diagnostics_paths() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("LICENSE-b.txt"), "B text").unwrap();
        fs::write(t.path().join("LICENSE-a.TXT"), "A text").unwrap();
        fs::write(t.path().join("notes.md"), "not a licence").unwrap();
        let l = read_licenses(t.path());
        assert_eq!(
            l.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            ["LICENSE-a", "LICENSE-b"]
        );
        assert!(read_licenses(&t.path().join("missing")).is_empty());

        let ok = t.path().join("Selene Retrieval diagnostics.txt");
        assert_eq!(diagnostics_target(&ok.to_string_lossy()), Some(ok.clone()));
        // Only an absolute .txt path in an existing folder.
        assert!(diagnostics_target("relative.txt").is_none());
        assert!(diagnostics_target(&t.path().join("x.exe").to_string_lossy()).is_none());
        assert!(
            diagnostics_target(&t.path().join("nope").join("x.txt").to_string_lossy()).is_none()
        );
    }
}
