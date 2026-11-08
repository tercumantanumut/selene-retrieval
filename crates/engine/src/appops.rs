//! App-facing operations wired by the M2 tester (integration of M2-W / M2-A / M2-R work):
//! the wizard's pre-scan estimate (M2-A R1), pause / resume / cancel, `add_folder` approvals,
//! and dismissing a notice (M2-R O5). The app reaches them through app-RPC ([`crate::rpc`]); the
//! MCP `add_folder` tool (off by default) goes through [`EngineHandle::request_add_folder`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sr_catalog::repo::approvals::ApprovalRecord;
use sr_common::config::FolderSettings;
use sr_common::error::{ClientKind, Error, ErrorCode};
use sr_common::ids::{ApprovalId, FolderId};
use tokio_util::sync::CancellationToken;

use crate::EngineHandle;

/// Above this many files the wizard suggests a smaller folder (U12, R-UX14).
pub const ESTIMATE_MAX_FILES: u64 = 500_000;
/// Wall-clock budget of the pre-scan; the counts are then lower bounds.
pub const ESTIMATE_BUDGET: Duration = Duration::from_secs(4);
/// Measured first-index speeds (chunks/s; docs/04 R-UX14: CPU Minimum 7.5-11/s, DirectML 650-676/s).
const CHUNKS_PER_S_GPU: f64 = 600.0;
const CHUNKS_PER_S_CPU: f64 = 8.0;
/// 400-token chunks (Amendment 3.5) are about 1.6 KB of English text or code.
const TEXT_BYTES_PER_CHUNK: f64 = 1_600.0;

/// `folders.estimate` reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FolderEstimateOut {
    pub files: u64,
    pub text_bytes: u64,
    /// Counts are lower bounds (very large folder or time budget reached).
    pub capped: bool,
    pub est_chunks: u64,
    /// Seconds for the first full pass on this computer (active execution provider).
    pub eta_s: u64,
    /// Indexing runs on the graphics card.
    pub gpu: bool,
}

/// The Advanced settings the M2 app can change (02 §2.5; Amendment 2: "Use graphics card"; the
/// MCP `add_folder` and `sync_folder` switches). The tier override itself goes through `search_model.choose`, which
/// does the download + rebuild.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdvancedSettings {
    /// Auto (true, default) or Off. Takes effect for new embedding sessions (next start).
    pub use_graphics_card: bool,
    /// Connected apps may *ask* to add folders (each request needs approval). Off by default.
    pub mcp_add_folder_enabled: bool,
    /// Connected apps may ask to check a folder for changes now (Amendment 16). Off by default.
    pub mcp_sync_folder_enabled: bool,
    /// `[search] tier_override` from settings.toml (`None` = automatic).
    pub tier_override: Option<sr_common::tier::Tier>,
    /// A changed value only applies after Selene Retrieval restarts (`x-restart`).
    pub restart_needed: bool,
    /// Re-rank the top results with a local model (downloads it; off by default, M3-Q).
    pub rerank_enabled: bool,
    /// Diversity (MMR lambda 0-1); `None` = off (default).
    pub mmr_lambda: Option<f32>,
    /// Where the reranker is (download, ready, failed).
    pub reranker: crate::rerank::RerankerStatus,
}

/// `settings.update_advanced` params: only the fields given change.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AdvancedUpdate {
    #[serde(default)]
    pub use_graphics_card: Option<bool>,
    #[serde(default)]
    pub mcp_add_folder_enabled: Option<bool>,
    #[serde(default)]
    pub mcp_sync_folder_enabled: Option<bool>,
    /// M3-Q: turning it on is the consent to download the reranker (size shown in the label).
    #[serde(default)]
    pub rerank_enabled: Option<bool>,
    /// M3-Q: `null` = off, a number in 0..=1 = on with that lambda; absent = unchanged.
    #[serde(default, deserialize_with = "some_value")]
    pub mmr_lambda: Option<serde_json::Value>,
}

/// Keeps an explicit JSON `null` as `Some(Value::Null)` (absent stays `None`).
fn some_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}

/// One pending `add_folder` request, as the app shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApprovalOut {
    pub approval_id: ApprovalId,
    pub client: ClientKind,
    pub client_name: Option<String>,
    /// The folder the client asked for (canonical; the user's own path).
    pub path: String,
    pub expires_at: i64,
}

impl From<&ApprovalRecord> for ApprovalOut {
    fn from(a: &ApprovalRecord) -> Self {
        Self {
            approval_id: a.id,
            client: a.client_kind,
            client_name: a.client_name.clone(),
            path: a.root_path.to_string_lossy().into_owned(),
            expires_at: a.expires_at,
        }
    }
}

fn eta_secs(files: u64, text_bytes: u64, gpu: bool) -> (u64, u64) {
    #[allow(clippy::cast_precision_loss)]
    let chunks = (text_bytes as f64 / TEXT_BYTES_PER_CHUNK).max(files as f64);
    let rate = if gpu {
        CHUNKS_PER_S_GPU
    } else {
        CHUNKS_PER_S_CPU
    };
    // Scan + sandboxed extraction: about 2 ms per file.
    #[allow(clippy::cast_precision_loss)]
    let secs = chunks / rate + files as f64 * 0.002;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (chunks.ceil() as u64, secs.ceil() as u64)
    }
}

/// The same refusal mapping as [`EngineHandle::add_folder`], so the wizard's estimate refuses a
/// folder with exactly the message (and "Choose a subfolder" action) the add would give.
fn map_add_error(e: Error, raw: &str) -> Error {
    let base = Path::new(raw.trim_end_matches(['\\', '/']))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());
    match e.code {
        ErrorCode::PathRefused | ErrorCode::PathDenied => {
            Error::new(ErrorCode::PathRefused).with_path(PathBuf::from(raw))
        }
        ErrorCode::NotFound | ErrorCode::FolderUnavailable => {
            Error::new(ErrorCode::FolderUnavailable).with_name(base)
        }
        // M5-E: outside the roots an administrator allows.
        ErrorCode::ManagedByPolicy => Error::new(ErrorCode::ManagedByPolicy).with_name(base),
        _ => e,
    }
}

impl EngineHandle {
    /// Wizard pre-scan (M2-A R1): real filters, default folder settings, the active execution
    /// provider's measured speed. Read-only and bounded ([`ESTIMATE_BUDGET`],
    /// [`ESTIMATE_MAX_FILES`]).
    pub async fn estimate_folder(&self, path: &str) -> Result<FolderEstimateOut, Error> {
        let raw = path.trim().to_string();
        if raw.is_empty() || !Path::new(&raw).is_absolute() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("absolute path required"));
        }
        let rec = self.inner().sync.reconciler().clone();
        let cancel = CancellationToken::new();
        let timer = {
            let c = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(ESTIMATE_BUDGET).await;
                c.cancel();
            })
        };
        let root = PathBuf::from(&raw);
        let c2 = cancel.clone();
        let r = tokio::task::spawn_blocking(move || {
            rec.estimate(&root, &FolderSettings::default(), ESTIMATE_MAX_FILES, &c2)
        })
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        timer.abort();
        let est = r.map_err(|e| map_add_error(e, &raw))?;
        let gpu = crate::provider_on_gpu(&self.inner().provider);
        let (est_chunks, eta_s) = eta_secs(est.files, est.text_bytes, gpu);
        Ok(FolderEstimateOut {
            files: est.files,
            text_bytes: est.text_bytes,
            capped: est.capped,
            est_chunks,
            eta_s,
            gpu,
        })
    }

    /// Pause a folder: running jobs are released (no attempt used), queued ones held.
    pub async fn pause_folder(&self, id: FolderId) -> Result<(), Error> {
        self.inner().sync.pause(id).await?;
        self.refresh_statuses(Some(vec![id])).await;
        Ok(())
    }

    /// Resume a paused folder (a catch-up scan runs). Returns whether a scan was queued.
    pub async fn resume_folder(&self, id: FolderId) -> Result<bool, Error> {
        let scanned = self.inner().sync.resume(id).await?;
        self.refresh_statuses(Some(vec![id])).await;
        Ok(scanned)
    }

    /// Stop the current run of a folder (status `partial`; the next sync finishes it).
    pub async fn cancel_folder(&self, id: FolderId) -> Result<(usize, usize), Error> {
        let r = self.inner().sync.cancel(id).await?;
        self.refresh_statuses(Some(vec![id])).await;
        Ok((r.dropped, r.stopped))
    }

    /// Pending `add_folder` requests from connected apps.
    pub async fn pending_approvals(&self) -> Result<Vec<ApprovalOut>, Error> {
        Ok(self
            .inner()
            .sync
            .pending_approvals()
            .await?
            .iter()
            .map(ApprovalOut::from)
            .collect())
    }

    /// The user allowed a request: the folder is added through the normal path (first scan).
    pub async fn approve(&self, id: ApprovalId) -> Result<FolderId, Error> {
        let rec = self.inner().sync.approval(id).await.ok();
        let f = self.inner().sync.approve(id).await?;
        self.revalidate_roots().await?;
        if let Some(r) = rec {
            self.note_approval(&r, true, Some(f)).await;
        }
        Ok(f)
    }

    pub async fn deny(&self, id: ApprovalId) -> Result<(), Error> {
        let rec = self.inner().sync.approval(id).await.ok();
        self.inner().sync.deny(id).await?;
        if let Some(r) = rec {
            self.note_approval(&r, false, None).await;
        }
        Ok(())
    }

    /// Activity lines for a decided folder request (page 30).
    async fn note_approval(&self, r: &ApprovalRecord, allowed: bool, folder: Option<FolderId>) {
        use sr_catalog::repo::activity::NewActivity;
        use sr_common::api::ActivityKind;
        let client = r
            .client_name
            .clone()
            .unwrap_or_else(|| sr_common::api::default_client_name(&r.client_id));
        let path = r.root_path.to_string_lossy().into_owned();
        let kind = if allowed {
            ActivityKind::ApprovalAllowed
        } else {
            ActivityKind::ApprovalDeclined
        };
        self.record_activity(
            NewActivity::new(kind)
                .client(r.client_kind, client)
                .text(path),
        )
        .await;
        if let Some(fid) = folder {
            let name = r.root_path.file_name().map_or_else(
                || r.root_path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            self.record_activity(NewActivity::new(ActivityKind::FolderAdded).folder(fid, name))
                .await;
        }
    }

    /// Whether the MCP `add_folder` tool is listed (Advanced → integrations; off by default).
    pub fn mcp_add_folder_enabled(&self) -> bool {
        self.inner().sync.mcp_add_folder_enabled()
    }

    /// Whether the MCP `sync_folder` tool is listed (Advanced → integrations; off by default).
    pub fn mcp_sync_folder_enabled(&self) -> bool {
        self.inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .integrations
            .mcp_sync_folder_enabled
    }

    /// MCP `add_folder` (only when turned on): validated first, then a pending approval the user
    /// decides in the app (10 min expiry). Returns the `ApprovalRequired` error the tool shows.
    pub async fn request_add_folder(
        &self,
        client_id: &str,
        client: ClientKind,
        path: &str,
    ) -> Result<Error, Error> {
        let raw = path.trim();
        if raw.is_empty() || !Path::new(raw).is_absolute() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("absolute path required"));
        }
        // M3 tester follow-up: approval cards name the app ("Claude Desktop", an HTTP tool's own
        // name), not "An app".
        let id = client_id.to_string();
        let client_name = self
            .inner()
            .cat
            .read_async(move |c| sr_catalog::repo::clients::client(c, &id))
            .await
            .ok()
            .flatten()
            .map_or_else(
                || sr_common::api::default_client_name(client_id),
                |r| r.display_name,
            );
        let t = self
            .inner()
            .sync
            .request_add_folder(sr_sync::McpAddFolder {
                client_id: client_id.to_string(),
                client_kind: client,
                client_name: Some(client_name),
                path: PathBuf::from(raw),
                collection_id: self.inner().default_collection,
            })
            .await
            .map_err(|e| {
                if e.code == ErrorCode::ApprovalDenied {
                    e
                } else {
                    map_add_error(e, raw)
                }
            })?;
        // The daemon pushed ApprovalRequested; launching the app when it is closed is R-UX12.
        Ok(t.to_error())
    }

    /// Advanced settings as the app shows them.
    pub fn advanced_settings(&self) -> AdvancedSettings {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AdvancedSettings {
            use_graphics_card: s.search.use_graphics_card,
            mcp_add_folder_enabled: self.inner().sync.mcp_add_folder_enabled(),
            mcp_sync_folder_enabled: s.integrations.mcp_sync_folder_enabled,
            tier_override: s.search.tier_override,
            restart_needed: false,
            rerank_enabled: s.ranking.rerank_enabled,
            mmr_lambda: s.ranking.mmr_lambda,
            reranker: self.reranker_status(),
        }
    }

    /// Change Advanced settings: `settings.toml` is rewritten atomically (unknown keys kept, the
    /// previous file kept as `.bak`), `SettingsChanged` is pushed. The `add_folder` and
    /// `sync_folder` switches apply at once (turning `add_folder` off expires pending requests); "Use graphics card" applies to new
    /// embedding sessions, i.e. after a restart (`restart_needed`).
    pub async fn update_advanced(&self, u: AdvancedUpdate) -> Result<AdvancedSettings, Error> {
        // M5-E: switches an administrator locked are refused plainly (the app greys them out).
        if u.mcp_add_folder_enabled == Some(true) && !self.inner().policy.mcp_add_folder_allowed() {
            return Err(crate::policy::refused("MCP add_folder"));
        }
        if u.mcp_sync_folder_enabled == Some(true) && !self.inner().policy.mcp_sync_folder_allowed()
        {
            return Err(crate::policy::refused("MCP sync_folder"));
        }
        if u.rerank_enabled == Some(true) {
            self.check_download_policy("reranker")?;
        }
        // Serialised with every other settings write (no lost updates).
        let w = self.inner().settings_write.lock().await;
        let mut next = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut restart = false;
        if let Some(v) = u.use_graphics_card {
            restart |= v != next.search.use_graphics_card;
            next.search.use_graphics_card = v;
        }
        if let Some(v) = u.mcp_add_folder_enabled {
            next.integrations.mcp_add_folder_enabled = v;
        }
        if let Some(v) = u.mcp_sync_folder_enabled {
            next.integrations.mcp_sync_folder_enabled = v;
        }
        let rerank_changed = u
            .rerank_enabled
            .is_some_and(|v| v != next.ranking.rerank_enabled);
        if let Some(v) = u.rerank_enabled {
            next.ranking.rerank_enabled = v;
        }
        let mmr_changed = u.mmr_lambda.is_some();
        match u.mmr_lambda {
            None => {}
            Some(serde_json::Value::Null) => next.ranking.mmr_lambda = None,
            Some(v) => {
                let l = v
                    .as_f64()
                    .filter(|l| (0.0..=1.0).contains(l))
                    .ok_or_else(|| {
                        Error::new(ErrorCode::InvalidInput)
                            .with_detail("mmr_lambda must be null or between 0 and 1")
                    })?;
                #[allow(clippy::cast_possible_truncation)]
                {
                    next.ranking.mmr_lambda = Some(l as f32);
                }
            }
        }
        self.inner().policy.apply(&mut next);
        let path = self.inner().layout.settings.clone();
        let to_save = next.clone();
        tokio::task::spawn_blocking(move || sr_common::config::save_atomic(&path, &to_save))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        *self
            .inner()
            .settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        drop(w);
        if let Some(v) = u.mcp_add_folder_enabled {
            self.inner().sync.set_mcp_add_folder_enabled(v).await?;
        }
        // Ranking changes apply to the next search (no restart).
        if rerank_changed {
            self.sync_reranker(false);
        } else if mmr_changed {
            self.apply_ranking();
        }
        self.inner()
            .bus
            .publish(sr_common::events::EngineEvent::SettingsChanged);
        let mut out = self.advanced_settings();
        out.restart_needed = restart;
        Ok(out)
    }

    /// Hide a notice until the next start (M2-R O5). Returns how many were removed.
    pub fn dismiss_notice(&self, code: Option<ErrorCode>) -> usize {
        let mut n = self
            .inner()
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = n.len();
        n.retain(|x| code.is_some_and(|c| c != x.code));
        before - n.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_follows_the_execution_provider() {
        let (chunks, cpu) = eta_secs(2_000, 20_000_000, false);
        let (_, gpu) = eta_secs(2_000, 20_000_000, true);
        assert_eq!(chunks, 12_500);
        assert!(cpu > 50 * gpu, "cpu {cpu} gpu {gpu}");
        // Tiny files still count one chunk each.
        assert_eq!(eta_secs(10, 10, false).0, 10);
    }
}
