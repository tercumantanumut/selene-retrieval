//! App-RPC methods served over the pipe for `kind=app` sessions (the Tauri app and the hidden
//! `sr-daemon admin` CLI). `host.*` and `events.*` are handled by the pipe server itself.
//!
//! Folder administration lives here and **only** here (R-UX1: app UI / admin CLI; the MCP
//! `add_folder` tool is off by default, M2+ with approval). Bad parameters → `InvalidInput`.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sr_common::api::{self, methods as m4};
use sr_common::error::{ClientKind, Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::ids::{FileId, FolderId, GroupId};

use crate::EngineHandle;
use crate::files::{GrepRequest, ReadFileRequest};
use crate::handle::SearchArgs;

pub const STATUS: &str = "engine.status";
pub const DIAGNOSTICS: &str = "engine.diagnostics";
pub const WAIT_IDLE: &str = "engine.wait_idle";
pub const FOLDERS_LIST: &str = "folders.list";
pub const FOLDERS_ADD: &str = "folders.add";
pub const FOLDERS_REMOVE: &str = "folders.remove";
pub const FOLDERS_SYNC: &str = "folders.sync";
pub const SEARCH: &str = "search";
pub const FILES_READ: &str = "files.read";
pub const FILES_GREP: &str = "files.grep";
// M2 (wired by the M2 tester): wizard estimate, folder controls, approvals, notices, search model.
pub const FOLDERS_ESTIMATE: &str = "folders.estimate";
pub const FOLDERS_PAUSE: &str = "folders.pause";
pub const FOLDERS_RESUME: &str = "folders.resume";
pub const FOLDERS_CANCEL: &str = "folders.cancel";
pub const APPROVALS_LIST: &str = "approvals.list";
pub const APPROVALS_APPROVE: &str = "approvals.approve";
pub const APPROVALS_DENY: &str = "approvals.deny";
pub const NOTICES_DISMISS: &str = "notices.dismiss";
pub const SEARCH_MODEL_STATUS: &str = "search_model.status";
pub const SEARCH_MODEL_USE_RECOMMENDED: &str = "search_model.use_recommended";
pub const SEARCH_MODEL_CHOOSE: &str = "search_model.choose";
pub const SEARCH_MODEL_CANCEL: &str = "search_model.cancel";
pub const SETTINGS_ADVANCED: &str = "settings.advanced";
pub const SETTINGS_UPDATE_ADVANCED: &str = "settings.update_advanced";
// M3-Q (M3-U requests R-Q4 / R-Q5): AI provider, consent, answers, reranker, answer/enhance.
pub const PROVIDERS_STATUS: &str = "providers.status";
pub const PROVIDERS_SAVE: &str = "providers.save";
pub const PROVIDERS_CHECK: &str = "providers.check";
pub const PROVIDERS_SET_MODEL: &str = "providers.set_model";
pub const PROVIDERS_REMOVE: &str = "providers.remove";
pub const CONSENT_LIST: &str = "consent.list";
pub const CONSENT_SET: &str = "consent.set";
pub const SETTINGS_ANSWERS: &str = "settings.answers";
pub const SETTINGS_UPDATE_ANSWERS: &str = "settings.update_answers";
pub const ANSWER: &str = "answer";
pub const ENHANCE_PROMPT: &str = "enhance_prompt";
pub const RERANKER_STATUS: &str = "reranker.status";
pub const RERANKER_DOWNLOAD: &str = "reranker.download";

/// Every method name (for the app's typed client and tests).
pub const METHODS: &[&str] = &[
    STATUS,
    DIAGNOSTICS,
    WAIT_IDLE,
    FOLDERS_LIST,
    FOLDERS_ADD,
    FOLDERS_REMOVE,
    FOLDERS_SYNC,
    SEARCH,
    FILES_READ,
    FILES_GREP,
    FOLDERS_ESTIMATE,
    FOLDERS_PAUSE,
    FOLDERS_RESUME,
    FOLDERS_CANCEL,
    APPROVALS_LIST,
    APPROVALS_APPROVE,
    APPROVALS_DENY,
    NOTICES_DISMISS,
    SEARCH_MODEL_STATUS,
    SEARCH_MODEL_USE_RECOMMENDED,
    SEARCH_MODEL_CHOOSE,
    SEARCH_MODEL_CANCEL,
    SETTINGS_ADVANCED,
    SETTINGS_UPDATE_ADVANCED,
    PROVIDERS_STATUS,
    PROVIDERS_SAVE,
    PROVIDERS_CHECK,
    PROVIDERS_SET_MODEL,
    PROVIDERS_REMOVE,
    CONSENT_LIST,
    CONSENT_SET,
    SETTINGS_ANSWERS,
    SETTINGS_UPDATE_ANSWERS,
    ANSWER,
    ENHANCE_PROMPT,
    RERANKER_STATUS,
    RERANKER_DOWNLOAD,
];

/// Every method this router serves: [`METHODS`] plus the M4 set (`sr_common::api::methods::ALL`).
pub fn all_methods() -> Vec<&'static str> {
    METHODS.iter().chain(m4::ALL.iter()).copied().collect()
}

fn params<T: DeserializeOwned>(v: Value) -> Result<T, Error> {
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v)
        .map_err(|e| Error::new(ErrorCode::InvalidInput).with_detail(e.to_string()))
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, Error> {
    serde_json::to_value(v).map_err(|e| Error::new(ErrorCode::Internal).with_source(e))
}

#[derive(Deserialize)]
struct AddParams {
    path: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct FolderParams {
    folder_id: FolderId,
}

#[derive(Deserialize, Default)]
struct OptFolderParams {
    #[serde(default)]
    folder_id: Option<FolderId>,
}

#[derive(Deserialize)]
struct PathParams {
    path: String,
}

#[derive(Deserialize)]
struct ApprovalParams {
    approval_id: sr_common::ids::ApprovalId,
}

#[derive(Deserialize, Default)]
struct NoticeParams {
    /// `None` = every notice.
    #[serde(default)]
    code: Option<ErrorCode>,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(feature = "local"), allow(dead_code))]
struct ModelParams {
    /// Advanced override (`search_model.choose`); the Basic [Use it] needs none.
    #[serde(default)]
    tier: Option<sr_common::tier::Tier>,
    /// `"multilingual"` (M3-Q): the multilingual profile instead of a tier.
    #[serde(default)]
    model: Option<String>,
    /// "Download anyway" on a metered connection.
    #[serde(default)]
    allow_metered: bool,
}

#[derive(Deserialize, Default)]
struct ModelChoice {
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize, Default)]
struct WaitParams {
    #[serde(default)]
    timeout_ms: Option<u64>,
}

// ------------------------------------------------------------------------------------- M4 params

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

#[derive(Deserialize)]
struct GroupParams {
    group_id: GroupId,
}

#[derive(Deserialize)]
struct GroupRenameParams {
    group_id: GroupId,
    name: String,
}

#[derive(Deserialize)]
struct MoveFolderParams {
    folder_id: FolderId,
    group_id: GroupId,
}

#[derive(Deserialize)]
struct ClientParams {
    client_id: String,
}

#[derive(Deserialize)]
struct ScopeParams {
    client_id: String,
    scope: api::ClientScope,
}

#[derive(Deserialize)]
struct SnippetParams {
    kind: ClientKind,
    #[serde(default)]
    client_id: Option<String>,
}

#[derive(Deserialize)]
struct EnabledParams {
    enabled: bool,
}

#[derive(Deserialize)]
struct SkippedParams {
    folder_id: FolderId,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct RetryParams {
    folder_id: FolderId,
    #[serde(default)]
    file_ids: Option<Vec<FileId>>,
}

#[derive(Deserialize)]
struct FolderSettingsParams {
    folder_id: FolderId,
    settings: sr_common::config::FolderSettings,
}

#[derive(Deserialize)]
struct RequestIdParams {
    request_id: String,
}

#[derive(Deserialize, Default)]
struct PageParams {
    #[serde(default)]
    before_id: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Deserialize)]
struct NoteParams {
    kind: api::ActivityKind,
    client_kind: ClientKind,
}

#[derive(Deserialize)]
struct TargetParams {
    target: String,
}

/// `answer` from the app: the M3 arguments plus an optional `request_id` (M4).
#[derive(Deserialize)]
struct AppAnswerParams {
    #[serde(flatten)]
    args: crate::answers::AnswerArgs,
    #[serde(default)]
    request_id: Option<String>,
}

/// `enhance_prompt` from the app: the M3 arguments plus an optional `request_id` (M4).
#[derive(Deserialize)]
struct AppEnhanceParams {
    #[serde(flatten)]
    args: crate::answers::EnhanceArgs,
    #[serde(default)]
    request_id: Option<String>,
}

fn progress_event(id: &str, kind: api::RequestKind, phase: api::RequestPhase) -> EngineEvent {
    EngineEvent::RequestProgress(api::RequestProgress {
        request_id: id.to_string(),
        kind,
        phase,
        round: None,
        max: None,
    })
}

/// Dispatch one app-RPC call (no pipe session: in-process callers and tests).
pub async fn dispatch(h: &EngineHandle, method: &str, p: Value) -> Result<Value, Error> {
    dispatch_session(h, method, p, None).await
}

/// Dispatch one app-RPC call made on pipe session `session` (its `answer` / `enhance_prompt`
/// calls stop when the session closes).
pub async fn dispatch_session(
    h: &EngineHandle,
    method: &str,
    p: Value,
    session: Option<u64>,
) -> Result<Value, Error> {
    if m4::ALL.contains(&method) {
        return match dispatch_m4(h, method, p, session).await {
            Some(v) => v,
            None => {
                Err(Error::new(ErrorCode::InvalidInput)
                    .with_detail(format!("unknown method {method}")))
            }
        };
    }
    match method {
        STATUS => {
            let s = h.status_async().await;
            to_value(&s)
        }
        DIAGNOSTICS => Ok(h.diagnostics()),
        WAIT_IDLE => {
            let w: WaitParams = params(p)?;
            let ms = w.timeout_ms.unwrap_or(60_000).min(30 * 60_000);
            let idle = h.wait_idle(std::time::Duration::from_millis(ms)).await;
            Ok(json!({ "idle": idle }))
        }
        FOLDERS_LIST => to_value(&h.list_folders().await?),
        FOLDERS_ADD => {
            let a: AddParams = params(p)?;
            to_value(&h.add_folder(&a.path, a.name).await?)
        }
        FOLDERS_REMOVE => {
            let f: FolderParams = params(p)?;
            h.remove_folder(f.folder_id).await?;
            Ok(json!({ "removed": true }))
        }
        FOLDERS_SYNC => {
            let f: OptFolderParams = params(p)?;
            let ids = h.sync_folder(f.folder_id).await?;
            Ok(json!({ "queued": ids }))
        }
        SEARCH => to_value(&h.search(params::<SearchArgs>(p)?).await?),
        FILES_READ => to_value(&h.read_file(params::<ReadFileRequest>(p)?).await?),
        FILES_GREP => to_value(&h.grep(params::<GrepRequest>(p)?).await?),
        FOLDERS_ESTIMATE => {
            let a: PathParams = params(p)?;
            to_value(&h.estimate_folder(&a.path).await?)
        }
        FOLDERS_PAUSE => {
            let f: FolderParams = params(p)?;
            h.pause_folder(f.folder_id).await?;
            Ok(json!({ "paused": true }))
        }
        FOLDERS_RESUME => {
            let f: FolderParams = params(p)?;
            let scan = h.resume_folder(f.folder_id).await?;
            Ok(json!({ "resumed": true, "scan_queued": scan }))
        }
        FOLDERS_CANCEL => {
            let f: FolderParams = params(p)?;
            let (dropped, stopped) = h.cancel_folder(f.folder_id).await?;
            Ok(json!({ "dropped": dropped, "stopped": stopped }))
        }
        APPROVALS_LIST => to_value(&h.pending_approvals().await?),
        APPROVALS_APPROVE => {
            let a: ApprovalParams = params(p)?;
            let folder = h.approve(a.approval_id).await?;
            Ok(json!({ "folder_id": folder }))
        }
        APPROVALS_DENY => {
            let a: ApprovalParams = params(p)?;
            h.deny(a.approval_id).await?;
            Ok(json!({ "denied": true }))
        }
        NOTICES_DISMISS => {
            let n: NoticeParams = params(p)?;
            Ok(json!({ "dismissed": h.dismiss_notice(n.code) }))
        }
        SEARCH_MODEL_STATUS => to_value(&h.search_model_status().await?),
        SETTINGS_ADVANCED => to_value(&h.advanced_settings()),
        SETTINGS_UPDATE_ADVANCED => {
            let u: crate::appops::AdvancedUpdate = params(p)?;
            to_value(&h.update_advanced(u).await?)
        }
        SEARCH_MODEL_CANCEL => {
            h.cancel_search_model_change().await?;
            Ok(json!({ "cancelled": true }))
        }
        RERANKER_STATUS => to_value(&h.reranker_status()),
        RERANKER_DOWNLOAD => {
            // Retry, or "Download anyway" on a metered connection.
            let m: ModelParams = params(p)?;
            to_value(&h.reranker_download(m.allow_metered))
        }
        PROVIDERS_STATUS => to_value(&h.providers_status()),
        PROVIDERS_SAVE => to_value(&h.save_provider(params(p)?).await?),
        PROVIDERS_CHECK => Ok(json!({ "status": h.check_provider().await? })),
        PROVIDERS_SET_MODEL => {
            let m: ModelChoice = params(p)?;
            Ok(json!({ "status": h.set_provider_model(m.model).await? }))
        }
        PROVIDERS_REMOVE => Ok(json!({ "status": h.remove_provider().await? })),
        CONSENT_LIST => to_value(&h.consent_list().await?),
        CONSENT_SET => to_value(&h.consent_set(params(p)?).await?),
        SETTINGS_ANSWERS => to_value(&h.answers_settings()),
        SETTINGS_UPDATE_ANSWERS => to_value(&h.update_answers(params(p)?).await?),
        // The app's Ask / Prompt Enhancer pages (M4) use the same engine calls as MCP, with a
        // `request_id` for `request.progress` and `requests.cancel`; closing the session stops
        // them too (nothing more is billed).
        ANSWER => {
            let a: AppAnswerParams = params(p)?;
            let guard = h.begin_request(a.request_id.as_deref(), session)?;
            let progress: std::sync::Arc<dyn Fn(sr_core::answer::AnswerPhase) + Send + Sync> =
                match a.request_id.clone() {
                    Some(id) => {
                        let bus = h.bus_arc();
                        bus.publish(progress_event(
                            &id,
                            api::RequestKind::Answer,
                            api::RequestPhase::Searching,
                        ));
                        std::sync::Arc::new(move |ph| {
                            let (phase, round, max) = match ph {
                                sr_core::answer::AnswerPhase::ReadingFiles { .. } => {
                                    (api::RequestPhase::ReadingFiles, None, None)
                                }
                                sr_core::answer::AnswerPhase::Writing => {
                                    (api::RequestPhase::Writing, None, None)
                                }
                                sr_core::answer::AnswerPhase::SearchingAgain { round, max } => {
                                    (api::RequestPhase::SearchingAgain, Some(round), Some(max))
                                }
                            };
                            bus.publish(EngineEvent::RequestProgress(api::RequestProgress {
                                request_id: id.clone(),
                                kind: api::RequestKind::Answer,
                                phase,
                                round,
                                max,
                            }));
                        })
                    }
                    None => std::sync::Arc::new(|_| {}),
                };
            let r = h.answer(a.args, progress, guard.token()).await;
            match r {
                Ok(v) => to_value(&v),
                Err(e) if guard.token().is_cancelled() && e.code != ErrorCode::Cancelled => {
                    Err(Error::new(ErrorCode::Cancelled).with_detail(e.to_string()))
                }
                Err(e) => Err(e),
            }
        }
        ENHANCE_PROMPT => {
            let a: AppEnhanceParams = params(p)?;
            let guard = h.begin_request(a.request_id.as_deref(), session)?;
            let bus = h.bus_arc();
            let rid = a.request_id.clone();
            let report = move |phase: api::RequestPhase| {
                if let Some(id) = &rid {
                    bus.publish(progress_event(id, api::RequestKind::Enhance, phase));
                }
            };
            let r = h
                .enhance_prompt_with_progress(a.args, Some(&report), guard.token())
                .await;
            match r {
                Ok(v) => to_value(&v),
                Err(e) if guard.token().is_cancelled() && e.code != ErrorCode::Cancelled => {
                    Err(Error::new(ErrorCode::Cancelled).with_detail(e.to_string()))
                }
                Err(e) => Err(e),
            }
        }
        #[cfg(feature = "local")]
        SEARCH_MODEL_USE_RECOMMENDED => {
            // The click is the consent (Amendment 1): nothing downloads without it.
            let m: ModelParams = params(p)?;
            let tier = h.use_recommended_search_model(m.allow_metered).await?;
            Ok(json!({ "tier": tier }))
        }
        #[cfg(feature = "local")]
        SEARCH_MODEL_CHOOSE => {
            let m: ModelParams = params(p)?;
            if m.model.as_deref() == Some(crate::reembed::MULTILINGUAL_INTENT) {
                h.choose_multilingual_search_model(m.allow_metered).await?;
                return Ok(json!({ "model": crate::reembed::MULTILINGUAL_INTENT }));
            }
            let tier = m.tier.ok_or_else(|| {
                Error::new(ErrorCode::InvalidInput).with_detail("tier is required")
            })?;
            h.choose_search_model(tier, m.allow_metered).await?;
            Ok(json!({ "tier": tier }))
        }
        other => {
            Err(Error::new(ErrorCode::InvalidInput).with_detail(format!("unknown method {other}")))
        }
    }
}

/// M5-C: `cli_signin.{status,start} {provider, refresh}`; `claude_signin.*` = provider
/// `claudecode` (the names the app first bound to).
#[cfg(feature = "remote")]
async fn cli_signin_call(h: &EngineHandle, method: &str, p: Value) -> Result<Value, Error> {
    let a: sr_common::api::CliSigninParams = params(p)?;
    let provider = match method {
        m4::CLAUDE_SIGNIN_START | m4::CLAUDE_SIGNIN_STATUS => {
            sr_common::api::CLAUDE_SIGNIN_PROVIDER.to_string()
        }
        _ => a
            .provider
            .clone()
            .unwrap_or_else(|| sr_common::api::CLAUDE_SIGNIN_PROVIDER.to_string()),
    };
    match method {
        m4::CLI_SIGNIN_START | m4::CLAUDE_SIGNIN_START => {
            to_value(&h.cli_signin_start(&provider).await?)
        }
        _ => to_value(&h.cli_signin_status(&provider, a.refresh).await?),
    }
}

#[cfg(not(feature = "remote"))]
async fn cli_signin_call(_h: &EngineHandle, _method: &str, _p: Value) -> Result<Value, Error> {
    Err(Error::new(ErrorCode::ProviderNotConfigured)
        .with_provider(sr_common::api::CLAUDE_SIGNIN_LABEL)
        .with_detail("built without remote providers"))
}

/// The M4 methods (`docs/research/m4-rpc-contract.md`). `None` = not an M4 method.
async fn dispatch_m4(
    h: &EngineHandle,
    method: &str,
    p: Value,
    _session: Option<u64>,
) -> Option<Result<Value, Error>> {
    let r = match method {
        m4::GROUPS_LIST => h.groups_list().await.map(|g| json!({ "groups": g })),
        m4::GROUPS_CREATE => match params::<NameParams>(p) {
            Ok(a) => h.create_group(a.name).await.and_then(|g| to_value(&g)),
            Err(e) => Err(e),
        },
        m4::GROUPS_RENAME => match params::<GroupRenameParams>(p) {
            Ok(a) => h
                .rename_group(a.group_id, a.name)
                .await
                .and_then(|g| to_value(&g)),
            Err(e) => Err(e),
        },
        m4::GROUPS_DELETE => match params::<GroupParams>(p) {
            Ok(a) => h.delete_group(a.group_id).await.map(
                |(moved, empty)| json!({ "moved_folders": moved, "clients_without_access": empty }),
            ),
            Err(e) => Err(e),
        },
        m4::GROUPS_MOVE_FOLDER => match params::<MoveFolderParams>(p) {
            Ok(a) => h
                .move_folder_to_group(a.folder_id, a.group_id)
                .await
                .map(|()| json!({ "folder_id": a.folder_id, "group_id": a.group_id })),
            Err(e) => Err(e),
        },
        m4::CLIENTS_LIST => h.clients_list().await.and_then(|v| to_value(&v)),
        m4::CLIENTS_SET_SCOPE => match params::<ScopeParams>(p) {
            Ok(a) => h
                .set_client_scope(a.client_id, a.scope)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::CLIENTS_FORGET => match params::<ClientParams>(p) {
            Ok(a) => h
                .forget_client(a.client_id)
                .await
                .map(|removed| json!({ "removed": removed })),
            Err(e) => Err(e),
        },
        m4::CLIENTS_SNIPPET => match params::<SnippetParams>(p) {
            Ok(a) => h
                .client_snippet(a.kind, a.client_id)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::HTTP_STATUS => to_value(&h.http_status()),
        m4::HTTP_SET_ENABLED => match params::<EnabledParams>(p) {
            Ok(a) => h
                .http_set_enabled(a.enabled)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::HTTP_CREATE_TOKEN => match params::<NameParams>(p) {
            Ok(a) => h.http_create_token(a.name).await.and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::HTTP_ROTATE_TOKEN => match params::<ClientParams>(p) {
            Ok(a) => h
                .http_rotate_token(a.client_id)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::HTTP_REVOKE => match params::<ClientParams>(p) {
            Ok(a) => h
                .http_revoke(a.client_id)
                .await
                .map(|revoked| json!({ "revoked": revoked })),
            Err(e) => Err(e),
        },
        m4::FOLDERS_DETAIL => match params::<FolderParams>(p) {
            Ok(a) => h
                .folder_detail(a.folder_id)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::FOLDERS_SKIPPED => match params::<SkippedParams>(p) {
            Ok(a) => h
                .folder_skipped(a.folder_id, a.offset, a.limit)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::FOLDERS_RETRY_FILES => match params::<RetryParams>(p) {
            Ok(a) => h
                .retry_files(a.folder_id, a.file_ids)
                .await
                .map(|n| json!({ "queued": n })),
            Err(e) => Err(e),
        },
        m4::FOLDERS_UPDATE_SETTINGS => match params::<FolderSettingsParams>(p) {
            Ok(a) => h
                .update_folder_settings(a.folder_id, a.settings)
                .await
                .and_then(|(reindex, detail)| {
                    Ok(json!({ "reindex": reindex, "detail": to_value(&detail)? }))
                }),
            Err(e) => Err(e),
        },
        m4::REQUESTS_CANCEL => match params::<RequestIdParams>(p) {
            Ok(a) => Ok(json!({ "cancelled": h.cancel_request(&a.request_id) })),
            Err(e) => Err(e),
        },
        m4::ACTIVITY_LIST => match params::<PageParams>(p) {
            Ok(a) => h
                .activity_list(a.before_id, a.limit)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::ACTIVITY_EGRESS => match params::<PageParams>(p) {
            Ok(a) => h
                .activity_egress(a.before_id, a.limit, a.provider)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::ACTIVITY_NOTE => match params::<NoteParams>(p) {
            Ok(a) => h
                .activity_note(a.kind, a.client_kind)
                .await
                .map(|id| json!({ "id": id })),
            Err(e) => Err(e),
        },
        m4::INDEX_LOCATION_STATUS => h.index_location_status().await.and_then(|v| to_value(&v)),
        m4::INDEX_LOCATION_CHECK => match params::<TargetParams>(p) {
            Ok(a) => h
                .index_location_check(a.target)
                .await
                .and_then(|v| to_value(&v)),
            Err(e) => Err(e),
        },
        m4::INDEX_LOCATION_MOVE => match params::<TargetParams>(p) {
            Ok(a) => h
                .index_location_move(a.target)
                .await
                .map(|to| json!({ "started": true, "index_path": to.display().to_string() })),
            Err(e) => Err(e),
        },
        m4::INDEX_LOCATION_CANCEL => h
            .index_location_cancel()
            .await
            .map(|c| json!({ "cancelled": c })),
        // R34 (M5-E): served by sr-daemon's recovery mode; a running engine has its index.
        m4::INDEX_LOCATION_RECOVER => Err(Error::new(ErrorCode::InvalidInput)
            .with_detail("the search index is in place; there is nothing to recover")),
        // M5-E: what an administrator's policy locks.
        m4::POLICY_STATUS => to_value(&h.policy_status()),
        // M5-C (Amendments 10.2 / 10.3): CLI sign-in. Privileged (R31), checked by the caller.
        m4::CLI_SIGNIN_START
        | m4::CLI_SIGNIN_STATUS
        | m4::CLAUDE_SIGNIN_START
        | m4::CLAUDE_SIGNIN_STATUS => cli_signin_call(h, method, p).await,
        _ => return None,
    };
    Some(r)
}
