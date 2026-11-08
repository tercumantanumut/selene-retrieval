//! [`EngineHandle`]: the one API the MCP server, app-RPC, the admin CLI and the eval bench use.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sr_catalog::repo::{files, folders, jobs, meta, outbox, profiles, status};
use sr_catalog::{RunTrigger, Store};
use sr_common::config::FolderSettings;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{
    CatchUp, EngineEvent, EngineStatus, FolderStatus, FolderStatusSummary, HostState, Notice,
    Snapshot,
};
use sr_common::ids::FolderId;
use sr_common::tier::Tier;
use sr_core::index::StoreRegistry;
use sr_core::index::writer::DrainReport;
use sr_core::query::{Scores, SearchMode, SearchRequest};

use crate::files::{GrepRequest, GrepResult, ReadFileRequest, ReadFileResult};
use crate::hw::TierChoices;
use crate::sandboxfs::SandboxFs;
use crate::tasks::{self, MaintReport};
use crate::{EventBus, Inner, StartupReport, folder_roots, run_blocking};

/// Cheap clonable handle shared by every front end.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("data_dir", &self.inner.layout.root)
            .field("epoch", &self.inner.epoch)
            .finish_non_exhaustive()
    }
}

/// One folder as shown to clients (moved to `sr_common::api` in M4 so the app shares it).
pub use sr_common::api::FolderInfo;

/// Search arguments (after MCP argument parsing; aliases resolved).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchArgs {
    pub query: String,
    pub max_results: Option<u32>,
    pub min_score: Option<f32>,
    pub folder_ids: Option<Vec<FolderId>>,
    pub mode: Option<SearchMode>,
    /// Collapse identical text into one hit with `also_at` (docs/06 change 4); `None` = on.
    #[serde(default)]
    pub collapse_duplicates: Option<bool>,
}

/// One hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHitOut {
    /// Absolute path (display form); `read_file` accepts it as is.
    pub path: String,
    /// Folder-relative path, `/`-separated.
    pub rel_path: String,
    pub folder_id: FolderId,
    pub folder_name: String,
    /// The folder's root (display form), so clients can print it once (docs/06 change 2).
    #[serde(default)]
    pub folder_root: String,
    pub line_start: u32,
    pub line_end: u32,
    pub snippet: String,
    /// Fused (RRF) score; ranks are what matter.
    pub score: f32,
    pub scores: Scores,
    /// Other places with exactly this text (collapsed duplicates, docs/06 change 4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_at: Vec<AlsoAtOut>,
    /// In-process only (answers drop a deeper-round copy of a text they already have).
    #[serde(skip)]
    pub text_hash: Option<sr_common::ids::TextHash>,
}

/// Another copy of a hit's text, with its own lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlsoAtOut {
    pub folder_id: FolderId,
    pub folder_name: String,
    pub folder_root: String,
    /// Folder-relative path, `/`-separated.
    pub rel_path: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// Search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchOutput {
    pub hits: Vec<SearchHitOut>,
    pub total_ms: f64,
    pub dropped_stale: usize,
    /// `true` when no folder has been added yet (an empty result is then expected).
    pub no_folders: bool,
    /// `true` when nothing matched closely enough (R-UX18): `hits` is empty on purpose and the
    /// client says "Nothing closely matched …" instead of showing unrelated passages.
    #[serde(default)]
    pub no_good_match: bool,
    /// Best dense cosine / BM25 score of any candidate (diagnostics, calibration).
    #[serde(default)]
    pub top_dense: Option<f32>,
    #[serde(default)]
    pub top_bm25: Option<f32>,
    /// M4: the calling app's scope names no folder (MCP only; contract §4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_access: bool,
    /// The stale-hit lookup cap stopped the search early (docs/06 issue 2; diagnostics).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hydrate_capped: bool,
}

/// A removed folder still being purged.
struct RemovingFolder {
    id: FolderId,
    name: String,
    path: String,
}

/// Longest snippet returned per hit.
pub const MAX_SNIPPET_CHARS: usize = 4000;

fn blocking_err(e: tokio::task::JoinError) -> Error {
    Error::new(ErrorCode::Internal).with_source(e)
}

impl EngineHandle {
    pub(crate) fn from_inner(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }

    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    pub fn bus_arc(&self) -> Arc<EventBus> {
        self.inner.bus.clone()
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.layout.root
    }

    pub fn epoch(&self) -> i64 {
        self.inner.epoch
    }

    pub fn host_state(&self) -> HostState {
        *self
            .inner
            .host
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn startup_report(&self) -> StartupReport {
        self.inner
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The catalog (diagnostics and tests).
    #[doc(hidden)]
    pub fn catalog(&self) -> &Arc<sr_catalog::Catalog> {
        &self.inner.cat
    }

    /// The store registry (diagnostics and tests).
    #[doc(hidden)]
    pub fn registry(&self) -> &Arc<StoreRegistry> {
        &self.inner.registry
    }

    fn sandbox(&self) -> Arc<SandboxFs> {
        self.inner
            .sandbox
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    // -------------------------------------------------------------------------------- status

    fn compute_status(&self) -> EngineStatus {
        let host = self.host_state();
        let default_group = self.inner.default_group;
        let r: Result<(Vec<FolderStatusSummary>, u64), Error> = self.inner.cat.read(|c| {
            let mut out = Vec::new();
            let group_of = sr_catalog::repo::groups::folder_groups(c)?;
            for f in folders::folders(c)? {
                let n = status::counts(c, f.id)?;
                out.push(FolderStatusSummary {
                    folder_id: f.id,
                    display_name: f.settings.display_name.clone(),
                    // Derived live (the cached column trails the writer by one refresh).
                    status: status::derive(&n),
                    files_indexed: n.indexed_files,
                    files_total: n.total_files(),
                    path: Some(f.root_path.to_string_lossy().into_owned()),
                    files_skipped: n.dead_files,
                    watch_note: sr_sync::watch_note(&f),
                    cloud_note: sr_common::api::online_only_note(
                        n.online_only_files,
                        f.settings.include_online_only,
                    ),
                    online_only_skipped: sr_common::api::online_only_skipped(
                        n.online_only_files,
                        f.settings.include_online_only,
                    ),
                    removing: false,
                    group_id: group_of.get(&f.id).copied().unwrap_or(default_group),
                    stopped: f.cancelled_at.is_some(),
                    was_files_only: f.was_files_only,
                });
            }
            out.extend(
                self.removing_folders()
                    .into_iter()
                    .map(|r| FolderStatusSummary {
                        folder_id: r.id,
                        display_name: r.name,
                        status: FolderStatus::Pending,
                        path: Some(r.path),
                        removing: true,
                        group_id: default_group,
                        ..FolderStatusSummary::default()
                    }),
            );
            let (q, l) = jobs::queue_depth(c)?;
            Ok((out, q + l))
        });
        let (folders, queued_jobs) = r.unwrap_or_default();
        let mut s = EngineStatus {
            host,
            folders,
            queued_jobs,
            summary: String::new(),
            catch_up: self.catch_up(),
            notices: self.notices(),
            tier: self.tier_summary(),
            waiting_for_fullscreen: self.waiting_for_fullscreen(),
            consent_needed: self.consent_needed(),
            index_move: None,
            index_missing: None,
            managed: self.inner.policy.managed_fields(),
            reduced_protection: self.reduced_protection(),
        };
        s.summary = s.plain_summary();
        s
    }

    /// Active / recommended tier and whether indexing runs on the GPU (M2-A R2). `None` until
    /// tier detection ran.
    fn tier_summary(&self) -> Option<sr_common::events::TierSummary> {
        let recommended = self
            .inner
            .detection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|d| d.1.tier)?;
        Some(sr_common::events::TierSummary {
            active: self.active_tier(),
            recommended,
            gpu: crate::provider_on_gpu(&self.inner.provider),
            multilingual: self.active_model_id().as_deref()
                == Some(crate::multilingual::MULTILINGUAL_MODEL_ID),
        })
    }

    /// S-04 (R40): the file reader runs with the low-integrity fallback.
    pub fn reduced_protection(&self) -> bool {
        self.inner.extract.get().is_some_and(|p| {
            p.stats().sandbox == Some(sr_extract_client::ActiveSandbox::LowIntegrity)
        })
    }

    /// R-UX32: GPU indexing waits behind a full-screen app right now.
    pub fn waiting_for_fullscreen(&self) -> bool {
        self.inner.fullscreen.waiting()
    }

    /// "Catching up after restart" progress while it runs (Amendment 4.24).
    pub fn catch_up(&self) -> Option<CatchUp> {
        self.inner
            .catch_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|c| c.progress)
    }

    /// Notices kept until the next start (R-UX19).
    pub fn notices(&self) -> Vec<Notice> {
        self.inner
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Status snapshot whose `seq` matches the state (taken under the bus lock).
    pub fn status(&self) -> Snapshot<EngineStatus> {
        self.inner.bus.snapshot(|| self.compute_status())
    }

    /// [`Self::status`] from async code (catalog reads on the blocking pool).
    pub async fn status_async(&self) -> Snapshot<EngineStatus> {
        let h = self.clone();
        match tokio::task::spawn_blocking(move || h.status()).await {
            Ok(s) => s,
            Err(_) => self.inner.bus.snapshot(|| EngineStatus {
                host: self.host_state(),
                summary: "Starting…".into(),
                catch_up: self.catch_up(),
                notices: self.notices(),
                managed: self.inner.policy.managed_fields(),
                ..EngineStatus::default()
            }),
        }
    }

    /// Plain multi-line status text (U25): the summary, then one line per folder.
    pub fn status_text(s: &EngineStatus) -> String {
        let mut t = s.summary.clone();
        for f in &s.folders {
            let what = match f.status {
                FolderStatus::Synced => "up to date".to_string(),
                FolderStatus::SyncedWithErrors => "up to date (some files couldn't be read)".into(),
                FolderStatus::Indexing | FolderStatus::Pending => {
                    format!("indexing {} of {} files", f.files_indexed, f.files_total)
                }
                FolderStatus::Scanning => "looking for changes".into(),
                FolderStatus::Partial => "partly indexed (some folders couldn't be opened)".into(),
                FolderStatus::Paused => "paused".into(),
                FolderStatus::Unavailable => {
                    "not available (drive unplugged or folder moved)".into()
                }
                FolderStatus::Error => "has a problem; open Selene Retrieval for details".into(),
            };
            t.push_str(&format!("\n- {}: {what}", f.display_name));
        }
        if s.catch_up.is_some_and(|c| c.done < c.total) {
            t.push_str("\nSearch works meanwhile; recent changes appear as they are applied.");
        }
        for n in &s.notices {
            // The one renderer's agent text: "Title. Body Ask the user to …".
            t.push_str("\nNote: ");
            t.push_str(&n.message.agent_text());
        }
        t
    }

    /// Re-derive folder statuses and publish what changed (`FolderStatusChanged`,
    /// `IndexProgress`, `QueueDepth`). `None` = every folder.
    pub async fn refresh_statuses(&self, only: Option<Vec<FolderId>>) {
        let now = self.inner.clock.now_ms();
        let res = self
            .inner
            .cat
            .write(move |c| {
                let ids = match only {
                    Some(v) => v,
                    None => folders::folder_ids(c)?,
                };
                let mut out = Vec::new();
                for id in ids {
                    // `changed` compares with the catalog's cached `folders.status`, which sr-sync's
                    // reconciler also updates when it publishes `scanning`/`indexing`. It is the
                    // shared "last published" state; this handle's own map alone missed the
                    // `indexing → synced` step when the writer's commit (T2) finished a folder
                    // (M1 tester fix: `sync_folder` with a progress token then never returned).
                    let (s, changed) = match status::refresh(c, id) {
                        Ok(v) => v,
                        Err(e) if e.code == ErrorCode::NotFound => continue,
                        Err(e) => return Err(e),
                    };
                    let mut finished = None;
                    if changed && matches!(s, FolderStatus::Synced | FolderStatus::SyncedWithErrors)
                    {
                        // Same rule as sr-sync's `Reconciler::refresh_status` (W2): success is
                        // recorded only when the derived status becomes synced.
                        let rec = folders::folder(c, id)?;
                        let trig = rec.last_run_trigger.unwrap_or(RunTrigger::Manual);
                        folders::record_run(c, id, trig, None, Some(now), now)?;
                        // M4 activity (page 30): a folder's first index, or a check the user (or
                        // an app) started, or a settings change. Not saved files, not the hourly
                        // schedule or the start-up check (they would flood the page).
                        let first = rec.last_success_at.is_none();
                        if first
                            || matches!(
                                trig,
                                RunTrigger::Manual | RunTrigger::Mcp | RunTrigger::SettingsChange
                            )
                        {
                            finished = Some(rec.display_name());
                        }
                    }
                    let n = status::counts(c, id)?;
                    let finished = finished.map(|name| (name, n.dead_files));
                    out.push((id, s, changed, n.indexed_files, n.total_files(), finished));
                }
                Ok(out)
            })
            .await;
        let Ok(rows) = res else { return };
        let mut lines = Vec::new();
        for (id, _, _, done, _, finished) in &rows {
            if let Some((name, dead)) = finished {
                use sr_catalog::repo::activity::NewActivity;
                use sr_common::api::ActivityKind;
                lines.push(
                    NewActivity::new(ActivityKind::FolderSynced)
                        .folder(*id, name.clone())
                        .count(*done),
                );
                if *dead > 0 {
                    lines.push(
                        NewActivity::new(ActivityKind::FilesSkipped)
                            .folder(*id, name.clone())
                            .count(*dead),
                    );
                }
            }
        }
        for a in lines {
            self.record_activity_bg(a);
        }
        let mut events = Vec::new();
        {
            let mut m = self
                .inner
                .maint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, s, changed, done, total, _) in rows {
                let prev = m.last_status.insert(id, (s, done, total));
                if changed || prev.map(|p| p.0) != Some(s) {
                    events.push(EngineEvent::FolderStatusChanged {
                        folder_id: id,
                        status: s,
                        reason: None,
                    });
                }
                if prev.map(|p| (p.1, p.2)) != Some((done, total))
                    && matches!(
                        s,
                        FolderStatus::Indexing
                            | FolderStatus::Pending
                            | FolderStatus::Synced
                            | FolderStatus::SyncedWithErrors
                    )
                {
                    events.push(EngineEvent::IndexProgress {
                        folder_id: id,
                        files_done: done,
                        files_total: total,
                        eta_s: None,
                    });
                }
            }
        }
        for e in events {
            self.inner.bus.publish(e);
        }
    }

    // ------------------------------------------------------------------------------- folders

    /// Rebuild the sandbox from the catalog and mark folders available / unavailable (startup
    /// re-validation and after folder changes). Returns how many are unavailable.
    pub(crate) async fn revalidate_roots(&self) -> Result<usize, Error> {
        let cat = self.inner.cat.clone();
        let root = self.inner.layout.root.clone();
        let (sb, failed, records) = tokio::task::spawn_blocking(move || {
            let roots = folder_roots(&cat)?;
            let (sb, failed) = SandboxFs::build(&roots, &root);
            let records = cat.read(folders::folders)?;
            Ok::<_, Error>((sb, failed, records))
        })
        .await
        .map_err(blocking_err)??;
        *self
            .inner
            .sandbox
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(sb);
        let bad: HashMap<FolderId, ErrorCode> = failed
            .iter()
            .map(|(id, e)| {
                // M5-E: a folder outside the policy's allowed roots stays listed (nothing is
                // deleted) but unavailable with its own reason.
                let code = match e.code {
                    ErrorCode::PathRefused | ErrorCode::ManagedByPolicy => e.code,
                    _ => ErrorCode::FolderUnavailable,
                };
                (*id, code)
            })
            .collect();
        let now = self.inner.clock.now_ms();
        let changes: Vec<(FolderId, bool, Option<ErrorCode>)> = records
            .iter()
            .filter_map(|r| {
                let reason = bad.get(&r.id).copied();
                let avail = reason.is_none();
                (avail != r.available).then_some((r.id, avail, reason))
            })
            .collect();
        if !changes.is_empty() {
            let ch = changes.clone();
            self.inner
                .cat
                .write(move |c| {
                    for (id, avail, reason) in &ch {
                        folders::set_available(c, *id, *avail, *reason, now)?;
                    }
                    Ok(())
                })
                .await?;
            for (id, avail, reason) in changes {
                if !avail {
                    self.inner.bus.publish(EngineEvent::FolderStatusChanged {
                        folder_id: id,
                        status: FolderStatus::Unavailable,
                        reason,
                    });
                } else {
                    self.inner.sched_nudge.notify_one();
                }
            }
        }
        self.refresh_statuses(None).await;
        // R34 (M5-E): the folder list a "Start fresh" can add back (skipped when unchanged).
        self.write_folder_backup().await;
        Ok(bad.len())
    }

    pub async fn list_folders(&self) -> Result<Vec<FolderInfo>, Error> {
        let default_group = self.inner.default_group;
        self.inner
            .cat
            .read_async(move |c| {
                let mut out = Vec::new();
                let group_of = sr_catalog::repo::groups::folder_groups(c)?;
                for f in folders::folders(c)? {
                    let n = status::counts(c, f.id)?;
                    out.push(FolderInfo {
                        folder_id: f.id,
                        name: f.settings.display_name.clone(),
                        path: f.root_path.to_string_lossy().into_owned(),
                        status: status::derive(&n),
                        available: f.available,
                        files_indexed: n.indexed_files,
                        files_total: n.total_files(),
                        files_skipped: n.dead_files,
                        last_success_at: f.last_success_at,
                        watch_note: sr_sync::watch_note(&f),
                        cloud_note: sr_common::api::online_only_note(
                            n.online_only_files,
                            f.settings.include_online_only,
                        ),
                        online_only_skipped: sr_common::api::online_only_skipped(
                            n.online_only_files,
                            f.settings.include_online_only,
                        ),
                        removing: false,
                        group_id: group_of.get(&f.id).copied().unwrap_or(default_group),
                        stopped: f.cancelled_at.is_some(),
                        was_files_only: f.was_files_only,
                    });
                }
                Ok(out)
            })
            .await
            .map(|mut out: Vec<FolderInfo>| {
                out.extend(self.removing_folders().into_iter().map(|r| FolderInfo {
                    folder_id: r.id,
                    name: r.name,
                    path: r.path,
                    status: FolderStatus::Pending,
                    available: true,
                    files_indexed: 0,
                    files_total: 0,
                    files_skipped: 0,
                    last_success_at: None,
                    watch_note: None,
                    cloud_note: None,
                    online_only_skipped: 0,
                    removing: true,
                    group_id: default_group,
                    stopped: false,
                    was_files_only: false,
                }));
                out
            })
    }

    /// Folders removed but not purged yet ("Removing…", OQ-18).
    fn removing_folders(&self) -> Vec<RemovingFolder> {
        let m = self
            .inner
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v: Vec<RemovingFolder> = m
            .removing
            .iter()
            .filter(|(id, _)| m.pending_purges.contains_key(*id))
            .map(|(id, (name, path))| RemovingFolder {
                id: *id,
                name: name.clone(),
                path: path.clone(),
            })
            .collect();
        v.sort_by_key(|r| r.id);
        v
    }

    async fn folder_info(&self, id: FolderId) -> Result<FolderInfo, Error> {
        self.list_folders()
            .await?
            .into_iter()
            .find(|f| f.folder_id == id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))
    }

    /// Add a folder (app UI / hidden admin CLI only; never an MCP tool by default). sr-sync
    /// validates it (canonical root, dangerous roots with "Choose a subfolder", the denylist,
    /// overlap within the collection) and queues the first scan.
    pub async fn add_folder(&self, path: &str, name: Option<String>) -> Result<FolderInfo, Error> {
        let raw = path.trim().to_string();
        if raw.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty path"));
        }
        let base = Path::new(raw.trim_end_matches(['\\', '/']))
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.clone());
        let display = name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| base.clone());
        let first = self
            .inner
            .cat
            .read_async(|c| Ok(folders::folder_ids(c)?.is_empty()))
            .await?;
        let mut req = sr_sync::AddFolder::new(self.inner.default_collection, PathBuf::from(&raw));
        req.is_primary = first;
        req.settings = FolderSettings {
            display_name: display,
            ..FolderSettings::default()
        };
        let id = match self.inner.sync.add_folder(req).await {
            Ok(id) => id,
            Err(e) => {
                return Err(match e.code {
                    // "Choose a subfolder" opens the picker inside the refused folder.
                    ErrorCode::PathRefused | ErrorCode::PathDenied => {
                        Error::new(ErrorCode::PathRefused).with_path(PathBuf::from(&raw))
                    }
                    ErrorCode::NotFound | ErrorCode::FolderUnavailable => {
                        Error::new(ErrorCode::FolderUnavailable).with_name(base)
                    }
                    // M5-E: outside the roots an administrator allows.
                    ErrorCode::ManagedByPolicy => {
                        Error::new(ErrorCode::ManagedByPolicy).with_name(base)
                    }
                    _ => e,
                });
            }
        };
        self.revalidate_roots().await?;
        tracing::info!(target: "sr_engine::folders", folder = %id, "folder added");
        let info = self.folder_info(id).await?;
        self.record_activity(
            sr_catalog::repo::activity::NewActivity::new(sr_common::api::ActivityKind::FolderAdded)
                .folder(id, info.name.clone()),
        )
        .await;
        Ok(info)
    }

    /// Remove a folder: unsearchable immediately (catalog rows and chunk text deleted in one
    /// transaction), physically removed from the stores by the next drain (SLA 15 min).
    pub async fn remove_folder(&self, id: FolderId) -> Result<(), Error> {
        let now = self.inner.clock.now_ms();
        // Kept for "Removing…" until the purge completes (OQ-18).
        let shown = self
            .inner
            .cat
            .read_async(move |c| folders::folder(c, id))
            .await
            .ok()
            .map(|f| {
                (
                    f.settings.display_name,
                    f.root_path.to_string_lossy().into_owned(),
                )
            });
        self.inner
            .cat
            .write(move |c| folders::remove_folder(c, id, now))
            .await?;
        self.inner
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_purges
            .insert(id, now);
        if let Some(s) = shown {
            self.record_activity_bg(
                sr_catalog::repo::activity::NewActivity::new(
                    sr_common::api::ActivityKind::FolderRemoved,
                )
                .folder(id, s.0.clone()),
            );
            self.inner
                .maint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .removing
                .insert(id, s);
        }
        self.revalidate_roots().await?;
        self.inner.writer_nudge.notify_one();
        // Removed outside sr-sync: unwatch now rather than at the watcher's next 2 s refresh.
        self.inner.sync.folders_changed();
        self.inner.sync.wake();
        tracing::info!(target: "sr_engine::folders", folder = %id, "folder removed; purge queued");
        Ok(())
    }

    /// Queue a re-scan of one folder (or all), as an MCP/app request (always allowed, also for
    /// manual folders). Returns the folders queued.
    pub async fn sync_folder(&self, id: Option<FolderId>) -> Result<Vec<FolderId>, Error> {
        let ids = match id {
            Some(id) => {
                self.inner
                    .cat
                    .read_async(move |c| folders::folder(c, id))
                    .await?;
                vec![id]
            }
            None => self.inner.cat.read_async(folders::folder_ids).await?,
        };
        let mut queued = Vec::new();
        for f in ids {
            if self.inner.sync.request_scan(f, RunTrigger::Mcp).await? {
                queued.push(f);
            }
        }
        Ok(queued)
    }

    pub(crate) async fn load_pending_purges(&self) -> Result<(), Error> {
        let rows: Vec<(FolderId, i64)> = self
            .inner
            .cat
            .read_async(|c| {
                let mut st = c
                    .prepare(
                        "SELECT folder_id, min(created_at) FROM index_ops
                         WHERE op = 'purge_folder' AND folder_id IS NOT NULL GROUP BY folder_id",
                    )
                    .map_err(sr_catalog::map_db_err)?;
                let v = st
                    .query_map([], |r| {
                        Ok((
                            FolderId::from_bytes(r.get::<_, [u8; 16]>(0)?),
                            r.get::<_, i64>(1)?,
                        ))
                    })
                    .map_err(sr_catalog::map_db_err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sr_catalog::map_db_err)?;
                Ok(v)
            })
            .await?;
        // The SLA restarts at this start ("within 15 min of the next start").
        let now = self.inner.clock.now_ms();
        let mut m = self
            .inner
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (f, t) in rows {
            m.pending_purges
                .insert(f, t.max(now - tasks::PURGE_FORCE_AFTER_MS));
        }
        Ok(())
    }

    /// Purges whose outbox ops are gone (both stores applied and pruned) → `PurgeCompleted`.
    pub(crate) async fn check_purges_completed(&self) -> Result<Vec<FolderId>, Error> {
        let pending: Vec<FolderId> = self
            .inner
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_purges
            .keys()
            .copied()
            .collect();
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let still: HashSet<FolderId> = self
            .inner
            .cat
            .read_async(|c| {
                let mut st = c
                    .prepare(
                        "SELECT DISTINCT folder_id FROM index_ops
                         WHERE op = 'purge_folder' AND folder_id IS NOT NULL",
                    )
                    .map_err(sr_catalog::map_db_err)?;
                let v = st
                    .query_map([], |r| Ok(FolderId::from_bytes(r.get::<_, [u8; 16]>(0)?)))
                    .map_err(sr_catalog::map_db_err)?
                    .collect::<rusqlite::Result<HashSet<_>>>()
                    .map_err(sr_catalog::map_db_err)?;
                Ok(v)
            })
            .await?;
        let done: Vec<FolderId> = pending.into_iter().filter(|f| !still.contains(f)).collect();
        if !done.is_empty() {
            let mut m = self
                .inner
                .maint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for f in &done {
                m.pending_purges.remove(f);
                m.last_status.remove(f);
                m.removing.remove(f);
            }
        }
        if !done.is_empty() {
            // R9: the purged rows' old images (file names, paths) live on in WAL frames until a
            // checkpoint; fold them into the database and truncate the WAL now.
            self.checkpoint_truncate().await;
        }
        for f in &done {
            self.inner
                .bus
                .publish(EngineEvent::PurgeCompleted { folder_id: *f });
        }
        Ok(done)
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)` (R9), retried briefly while a reader holds an old
    /// snapshot. Never fails the caller: a busy checkpoint is logged and SQLite's automatic
    /// checkpoints finish the job later.
    pub(crate) async fn checkpoint_truncate(&self) -> bool {
        for attempt in 0..20 {
            let r = self
                .inner
                .cat
                .write(|c| {
                    c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map_err(sr_catalog::map_db_err)
                })
                .await;
            match r {
                Ok(0) => return true,
                Ok(_) => {
                    // SQLITE_BUSY: a reader still uses an older snapshot.
                    tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
                }
                Err(e) => {
                    tracing::warn!(target: "sr_engine::maintenance", err = %e, "WAL checkpoint after purge failed");
                    return false;
                }
            }
        }
        tracing::warn!(target: "sr_engine::maintenance", "WAL checkpoint after purge stayed busy");
        false
    }

    // -------------------------------------------------------------------------- search, files

    /// Hybrid search over the user's folders. Never logs the query text above TRACE.
    pub async fn search(&self, args: SearchArgs) -> Result<SearchOutput, Error> {
        let t0 = Instant::now();
        if args.query.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty query"));
        }
        let folder_map: HashMap<FolderId, (PathBuf, String, sr_common::ids::CollectionId)> = self
            .inner
            .cat
            .read_async(|c| {
                Ok(folders::folders(c)?
                    .into_iter()
                    .map(|f| {
                        (
                            f.id,
                            (f.root_path, f.settings.display_name, f.collection_id),
                        )
                    })
                    .collect())
            })
            .await?;
        if folder_map.is_empty() {
            return Ok(SearchOutput {
                hits: Vec::new(),
                total_ms: t0.elapsed().as_secs_f64() * 1000.0,
                dropped_stale: 0,
                no_folders: true,
                no_good_match: false,
                top_dense: None,
                top_bm25: None,
                no_access: false,
                hydrate_capped: false,
            });
        }
        let registered: HashSet<_> = self
            .inner
            .registry
            .all()
            .into_iter()
            .map(|s| s.collection_id)
            .collect();
        let collections: Vec<_> = match &args.folder_ids {
            Some(ids) => {
                if ids.is_empty() {
                    return Err(
                        Error::new(ErrorCode::InvalidInput).with_detail("empty folder list")
                    );
                }
                let mut v = Vec::new();
                for id in ids {
                    let (_, _, coll) = folder_map.get(id).ok_or_else(|| {
                        Error::new(ErrorCode::NotFound).with_detail("unknown folder id")
                    })?;
                    if !v.contains(coll) {
                        v.push(*coll);
                    }
                }
                v
            }
            None => {
                let mut v: Vec<_> = folder_map.values().map(|f| f.2).collect();
                v.sort();
                v.dedup();
                v
            }
        };
        let collections: Vec<_> = collections
            .into_iter()
            .filter(|c| registered.contains(c))
            .collect();
        if collections.is_empty() {
            return Err(
                Error::new(ErrorCode::Corrupt).with_detail("no open index for these folders")
            );
        }
        let req = SearchRequest {
            query: args.query.clone(),
            collections,
            folders: args.folder_ids.clone(),
            max_results: args.max_results,
            min_score: args.min_score,
            mode: args.mode,
            // Keyword index rebuilding after an upgrade (catch-up running): don't answer "nothing
            // matched" just because keyword evidence isn't there yet.
            relax_no_match: self.catch_up().is_some(),
            collapse_duplicates: args.collapse_duplicates.unwrap_or(true),
        };
        let searcher = self
            .inner
            .searcher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let resp = run_blocking(async move { searcher.search(&req).await }).await?;
        let hits = resp
            .hits
            .into_iter()
            .map(|h| {
                let label = |id: &FolderId| {
                    folder_map
                        .get(id)
                        .map(|f| (f.0.clone(), f.1.clone()))
                        .unwrap_or_default()
                };
                let (root, name) = label(&h.folder_id);
                // No `\r` reaches a client or a model (a CRLF file wastes a token per line).
                let mut snippet = crate::files::normalize_newlines(&h.snippet).into_owned();
                if snippet.chars().count() > MAX_SNIPPET_CHARS {
                    snippet = snippet.chars().take(MAX_SNIPPET_CHARS).collect::<String>() + "…";
                }
                let also_at = h
                    .also_at
                    .into_iter()
                    .map(|a| {
                        let (r, n) = label(&a.folder_id);
                        AlsoAtOut {
                            folder_id: a.folder_id,
                            folder_name: n,
                            folder_root: r.to_string_lossy().into_owned(),
                            rel_path: a.path,
                            line_start: a.line_start,
                            line_end: a.line_end,
                        }
                    })
                    .collect();
                SearchHitOut {
                    path: root
                        .join(h.path.replace('/', "\\"))
                        .to_string_lossy()
                        .into_owned(),
                    rel_path: h.path,
                    folder_id: h.folder_id,
                    folder_name: name,
                    folder_root: root.to_string_lossy().into_owned(),
                    line_start: h.line_start,
                    line_end: h.line_end,
                    snippet,
                    score: h.scores.rrf,
                    scores: h.scores,
                    also_at,
                    text_hash: Some(h.text_hash),
                }
            })
            .collect::<Vec<_>>();
        tracing::debug!(target: "sr_engine::search", query_chars = args.query.chars().count(), hits = hits.len(), ms = resp.stats.total_ms, "search");
        Ok(SearchOutput {
            hits,
            total_ms: t0.elapsed().as_secs_f64() * 1000.0,
            dropped_stale: resp.stats.dropped_stale,
            no_folders: false,
            no_good_match: resp.stats.no_good_match,
            top_dense: resp.stats.top_dense,
            top_bm25: resp.stats.top_bm25,
            no_access: false,
            hydrate_capped: resp.stats.hydrate_capped,
        })
    }

    /// `read_file` (blocking work on the blocking pool).
    pub async fn read_file(&self, req: ReadFileRequest) -> Result<ReadFileResult, Error> {
        let sb = self.sandbox();
        tokio::task::spawn_blocking(move || crate::files::read_file(&sb, &req))
            .await
            .map_err(blocking_err)?
    }

    /// `grep` (blocking work on the blocking pool).
    pub async fn grep(&self, req: GrepRequest) -> Result<GrepResult, Error> {
        let sb = self.sandbox();
        tokio::task::spawn_blocking(move || crate::files::grep(&sb, &req))
            .await
            .map_err(blocking_err)?
    }

    // ------------------------------------------------------------------- writer, maintenance

    pub(crate) async fn outbox_has_work(&self) -> bool {
        let last = self
            .inner
            .last_drained_seq
            .load(std::sync::atomic::Ordering::SeqCst);
        match self.inner.cat.read_async(outbox::max_seq).await {
            Ok(max) => max > last,
            Err(_) => true,
        }
    }

    /// Apply every pending outbox op now, in bounded batches (Amendment 4.25), publishing status,
    /// progress and catch-up events after **each** batch. Stops early at the shutdown drain's
    /// batch boundary (`writer_stop`).
    pub async fn drain_writer_now(&self) -> Result<DrainReport, Error> {
        let stop = self.inner.writer_stop.clone();
        self.writer_batches(&stop).await
    }

    /// Bounded batches until the outbox is empty or `stop` fires.
    pub(crate) async fn writer_batches(
        &self,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Result<DrainReport, Error> {
        let mut total = DrainReport::default();
        loop {
            let (w, s) = (self.inner.writer.clone(), stop.clone());
            let b = run_blocking(async move { w.drain_batch(&s).await }).await?;
            self.after_batch(&b).await?;
            total.absorb(&b);
            if !b.more || b.stopped || b.batches == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Between batches: watermark of the last commit, folder status / progress, purges, and the
    /// restart catch-up phase.
    async fn after_batch(&self, b: &DrainReport) -> Result<(), Error> {
        if b.batches > 0 || !b.stopped {
            self.inner
                .last_drained_seq
                .store(b.last_seq, std::sync::atomic::Ordering::SeqCst);
        }
        if b.wrote() {
            self.refresh_statuses(None).await;
        }
        self.check_purges_completed().await?;
        let ev = {
            let mut g = self
                .inner
                .catch_up
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match g.as_mut() {
                Some(c) if b.batches > 0 || (!b.more && !b.stopped) => {
                    let p = &mut c.progress;
                    p.done = (p.done + b.ops_read as u64).min(p.total);
                    if b.last_seq >= c.until_seq || (!b.more && !b.stopped) {
                        p.done = p.total;
                    }
                    let ev = EngineEvent::CatchUpProgress {
                        done: p.done,
                        total: p.total,
                    };
                    if p.done >= p.total {
                        tracing::info!(target: "sr_engine::writer", ops = p.total, "caught up after restart");
                        *g = None;
                    }
                    Some(ev)
                }
                _ => None,
            }
        };
        if let Some(ev) = ev {
            // Also what makes MCP push `resources/updated` for `sr://status` (the summary changed).
            self.inner.bus.publish(ev);
        }
        Ok(())
    }

    /// One maintenance pass (the background task runs it every minute).
    pub async fn run_maintenance_once(&self) -> Result<MaintReport, Error> {
        tasks::maintenance_once(self).await
    }

    /// Lease and run every runnable job until none is left, then drain the writer (tests, the
    /// eval bench and the admin CLI's `--wait`). Uses the same code path as the claim loop.
    #[doc(hidden)]
    pub async fn run_pending_jobs(&self) -> Result<usize, Error> {
        let n = self.run_jobs_only().await?;
        self.drain_writer_now().await?;
        Ok(n)
    }

    /// [`Self::run_pending_jobs`] without the final drain (crash tests: T1 written, stores not).
    /// Without background tasks this runs sr-sync's claim loop until the queue is empty.
    #[doc(hidden)]
    pub async fn run_jobs_only(&self) -> Result<usize, Error> {
        let (q0, l0) = self.inner.cat.read_async(jobs::queue_depth).await?;
        if self.inner.cfg.background {
            self.wait_idle(Duration::from_secs(30 * 60)).await;
            return Ok(usize::try_from(q0 + l0).unwrap_or(usize::MAX));
        }
        let token = self.inner.shutdown.child_token();
        let (svc, t) = (self.inner.sync.clone(), token.clone());
        let run = tokio::spawn(async move { svc.run(t).await });
        self.inner.sync.wake();
        let deadline = Instant::now() + Duration::from_secs(30 * 60);
        loop {
            let (q, l) = self.inner.cat.read_async(jobs::queue_depth).await?;
            if q + l == 0 || Instant::now() >= deadline || self.inner.shutdown.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        token.cancel();
        match run.await {
            Ok(r) => r?,
            Err(e) => return Err(Error::new(ErrorCode::Internal).with_source(e)),
        }
        Ok(usize::try_from(q0 + l0).unwrap_or(usize::MAX))
    }

    /// Wait until no job is queued or running and the outbox is applied (or the timeout).
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let busy = self
                .inner
                .cat
                .read_async(|c| {
                    let (q, l) = jobs::queue_depth(c)?;
                    let ops: i64 = c
                        .query_row("SELECT count(*) FROM index_ops", [], |r| r.get(0))
                        .map_err(sr_catalog::map_db_err)?;
                    Ok(q + l > 0 || ops > 0)
                })
                .await
                .unwrap_or(true);
            if !busy
                && self.inner.jobs_in_flight.available_permits()
                    == self.inner.cfg.job_permits.max(1)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ----------------------------------------------------------------------- tier, extract

    pub(crate) async fn detect_tier(&self, choices: TierChoices) -> Result<(), Error> {
        let probe = self.inner.cfg.hw_probe.clone();
        let dir = self.inner.layout.root.clone();
        let (hw, d) = tokio::task::spawn_blocking(move || crate::hw::decide(&*probe, &dir))
            .await
            .map_err(blocking_err)?;
        let now = self.inner.clock.now_ms();
        let det = meta::HwDetection {
            hw: hw.clone(),
            decision: d.clone(),
            detected_at: now,
        };
        self.inner
            .cat
            .write(move |c| meta::set_hw_detection(c, &det))
            .await?;
        let active = self.inner.provider.profile().tier.unwrap_or(Tier::Minimum);
        self.inner.bus.publish(EngineEvent::TierDetected {
            recommended: d.tier,
            active,
            override_: choices.tier_override,
            ep: d.ep,
            gpu_class: d.gpu_class,
            reasons: d.reasons.clone(),
        });
        *self
            .inner
            .detection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((hw, d));
        Ok(())
    }

    pub(crate) async fn start_extract_pool(&self) {
        let Some(cfg) = self.inner.cfg.extract_config() else {
            return;
        };
        match tokio::task::spawn_blocking(move || sr_extract_client::ExtractPool::start(cfg)).await
        {
            Ok(Ok(pool)) => {
                let _ = self.inner.extract.set(Arc::new(pool));
            }
            Ok(Err(e)) => {
                tracing::error!(target: "sr_engine::extract", code = %e.code, err = %e, "file reader did not start");
                *self
                    .inner
                    .extract_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.to_string());
            }
            Err(e) => {
                tracing::error!(target: "sr_engine::extract", err = %e, "file reader start panicked")
            }
        }
    }

    /// Diagnostics (Details / "Copy diagnostics"): no query or file text, no secrets.
    pub fn diagnostics(&self) -> serde_json::Value {
        let i = &self.inner;
        let det = i
            .detection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let stats = i.extract.get().map(|p| p.stats());
        let p = i.provider.profile();
        serde_json::json!({
            "data_dir": i.layout.root,
            "lance_root": i.vectors.root(),
            "epoch": i.epoch,
            "launched_by": i.cfg.launched_by,
            "startup": self.startup_report(),
            "embedder": {
                "model": p.model, "dims": p.dims, "precision": p.precision,
                "variant_sha256": p.variant_sha256,
                "profile_fingerprint": blake3::Hash::from_bytes(i.profile_fp).to_hex().to_string(),
            },
            "hardware": det.as_ref().map(|d| &d.0),
            "tier_decision": det.as_ref().map(|d| &d.1),
            "extract": stats.map(|s| serde_json::json!({
                "sandbox": s.sandbox.map(|x| format!("{x:?}")),
                "fallback_reason": s.fallback_reason,
                "reduced_protection": (s.sandbox == Some(sr_extract_client::ActiveSandbox::LowIntegrity))
                    .then_some(sr_common::events::REDUCED_PROTECTION_NOTE),
                "pdf_engine_error": s.pdf_engine_error,
                "jobs_ok": s.jobs_ok, "jobs_failed": s.jobs_failed, "timeouts": s.timeouts,
                "crashes": s.crashes, "spawns": s.spawns,
            })),
            "embed_batcher": i.batcher.as_ref().map(|b| { let s = b.stats(); let c = b.config(); serde_json::json!({
                "batch_size": c.batch_size, "max_in_flight": c.max_in_flight,
                "batches": s.batches, "texts": s.texts, "embedded": s.embedded,
                "in_flight": s.in_flight, "queued_texts": s.queued_texts,
            })}),
            "catch_up": self.catch_up(),
            "extract_error": i.extract_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
            "collections": i.registry.all().iter().map(|s| serde_json::json!({
                "collection_id": s.collection_id, "table": s.vectors.name(),
            })).collect::<Vec<_>>(),
        })
    }

    /// Store-side facts for tests: Lance rows and tantivy docs of a collection, watermarks.
    #[doc(hidden)]
    pub async fn store_watermarks(&self) -> Result<Vec<(Store, i64)>, Error> {
        let w = self.inner.writer.clone();
        let v = w.watermarks().await?;
        Ok(v.into_iter()
            .map(|w| {
                let s = match w.store {
                    sr_common::events::StoreKind::Lance => Store::Lance,
                    sr_common::events::StoreKind::Tantivy => Store::Tantivy,
                };
                (s, w.seq)
            })
            .collect())
    }

    /// Default collection's active profile id (tests).
    #[doc(hidden)]
    pub async fn default_index(&self) -> Result<Option<profiles::VectorIndexRecord>, Error> {
        let c = self.inner.default_collection;
        Ok(self
            .inner
            .cat
            .read_async(move |cat| profiles::active_index(cat, c))
            .await?
            .map(|(i, _)| i))
    }

    /// Drain for shutdown (Amendment 4.25, target ≤ 10 s): stop claiming, give running jobs a
    /// few seconds, commit bounded batches until the commit budget ends, then stop the writer at
    /// its next batch boundary. Whatever is not committed stays in the outbox and is applied
    /// after the next start (4.24). Stops the extract workers. Idempotent.
    pub async fn drain(&self) {
        self.inner
            .drained
            .get_or_init(|| async {
                let t0 = Instant::now();
                *self
                    .inner
                    .host
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = HostState::Draining;
                self.inner.bus.publish(EngineEvent::HostState {
                    state: HostState::Draining,
                });
                self.inner.shutdown.cancel();
                // The writer stops at the next batch boundary once the commit budget is spent.
                let stop = self.inner.writer_stop.clone();
                let timer = tokio::spawn(async move {
                    tokio::time::sleep(tasks::DRAIN_COMMIT_BUDGET).await;
                    stop.cancel();
                });
                // sr-sync's claim loop lets in-flight jobs finish (its drain timeout), then returns.
                // Jobs only extract + chunk + write T1 (no embedding), so this is short; a job
                // still running when we stop waiting is fenced by the next start's epoch.
                let run = self
                    .inner
                    .sync_task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(run) = run {
                    let _ = tokio::time::timeout(tasks::DRAIN_JOBS_WAIT, run).await;
                }
                let left = tasks::DRAIN_JOBS_WAIT.saturating_sub(t0.elapsed());
                // The watcher stopped with the shutdown token; release its watches.
                let watcher = self
                    .inner
                    .watcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(w) = watcher {
                    let _ = tokio::time::timeout(Duration::from_secs(1), w.join()).await;
                }
                if !tasks::wait_jobs_idle(self, left.max(Duration::from_millis(100))).await {
                    tracing::warn!(target: "sr_engine::drain", "jobs still running; their leases are fenced at next start");
                }
                let stop = self.inner.writer_stop.clone();
                match self.writer_batches(&stop).await {
                    Ok(r) if r.more => {
                        tracing::info!(target: "sr_engine::drain", committed_ops = r.ops_read, "stopped at a batch boundary; the rest is applied after the next start");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(target: "sr_engine::drain", err = %e, "final commit failed; replay at next start");
                    }
                }
                self.inner.writer_stop.cancel();
                timer.abort();
                // The background writer loop: wait for its batch boundary (it saw the stop).
                let wt = self
                    .inner
                    .writer_task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(wt) = wt
                    && tokio::time::timeout(Duration::from_secs(3), wt).await.is_err()
                {
                    tracing::warn!(target: "sr_engine::drain", "writer did not reach a batch boundary in time");
                }
                let w = self.inner.writer.clone();
                let _ = tokio::time::timeout(Duration::from_secs(2), w.wait_batch_boundary()).await;
                tracing::info!(target: "sr_engine::drain", ms = t0.elapsed().as_millis() as u64, "writer stopped");
                if let Some(p) = self.inner.extract.get() {
                    let p = p.clone();
                    let _ = tokio::task::spawn_blocking(move || p.shutdown()).await;
                }
                let handles: Vec<_> = std::mem::take(
                    &mut *self
                        .inner
                        .tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                // Abort, then join: an aborted task is dropped asynchronously, and some hold an
                // EngineHandle (startup GC / model-change tasks), so the engine must not count as
                // drained until they are really gone (M2-R O2: tantivy LockBusy on a restart).
                for h in &handles {
                    h.abort();
                }
                for h in handles {
                    let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
                }
                self.inner.embed_cancel.cancel();
                tracing::info!(target: "sr_engine::drain", "drained");
            })
            .await;
    }

    /// Keep a background task's handle so [`Self::drain`] aborts and joins it. After a drain it
    /// is aborted at once.
    pub(crate) fn track_task(&self, task: tokio::task::JoinHandle<()>) {
        if self.inner.drained.initialized() {
            task.abort();
            return;
        }
        let mut v = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        v.retain(|t| !t.is_finished());
        v.push(task);
    }

    /// App-RPC dispatch (see [`crate::rpc`]).
    pub async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        crate::rpc::dispatch(self, method, params).await
    }

    /// App-RPC dispatch for a call made on pipe session `session` (M4: its `answer` /
    /// `enhance_prompt` calls stop when the session closes).
    pub async fn rpc_session(
        &self,
        method: &str,
        params: serde_json::Value,
        session: u64,
    ) -> Result<serde_json::Value, Error> {
        crate::rpc::dispatch_session(self, method, params, Some(session)).await
    }

    /// Re-index paths after renames (m1-e O3; sr-sync's watcher calls this).
    pub async fn refresh_paths(&self, ids: Vec<sr_common::ids::FileId>) -> Result<usize, Error> {
        self.inner.ingest.refresh_paths(ids).await
    }

    /// Test hook: the next `n` writer acknowledgements fail after both stores committed (the
    /// crash window between a store watermark and T2).
    #[doc(hidden)]
    pub fn inject_ack_failures(&self, n: u32) {
        self.inner
            .ack_faults
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Folder of a file (tests).
    #[doc(hidden)]
    pub async fn file_record(
        &self,
        id: sr_common::ids::FileId,
    ) -> Result<files::FileRecord, Error> {
        self.inner.cat.read_async(move |c| files::file(c, id)).await
    }
}
