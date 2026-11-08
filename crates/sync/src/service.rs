//! [`SyncService`]: the claim loop, startup recovery, the stale-folder scheduler, the live
//! watcher, pause/resume/cancel and `add_folder` approvals, over one [`Reconciler`] and the
//! engine's [`JobExecutor`].
//!
//! # Claim loop (D4)
//! * **Two lanes.** `scan_folder` jobs run in the reconciler under their own small limit
//!   ([`SyncConfig::max_concurrent_scans`]); every other kind needs a **pipeline permit**
//!   ([`SyncConfig::pipeline_permits`], or a semaphore shared with the engine via
//!   [`SyncService::with_pipeline`]). The loop claims only as many jobs as permits are free at
//!   that moment, so backpressure lives at the claim (no channel, no prefetch): a leased job
//!   always has a permit.
//! * **Order.** The catalog claims by effective priority (aging: one level per
//!   [`SyncConfig::aging_ms`]) and, within a level, by deficit round-robin across folders. Each
//!   lane owns one persistent [`Drr`], so a 100k-file folder cannot starve a 10-file one.
//! * **Fencing.** Every claim, completion and failure carries this run's epoch
//!   (`Catalog::start_epoch`); leases of a crashed run are re-queued at the next start and a stale
//!   holder can never finish them.
//! * **Outcomes.** `Ok(Done)` → complete (re-queued if a change was coalesced meanwhile);
//!   `Ok(Retry)` / retryable `Err` → back to `queued` with exponential backoff; `Ok(Dead)` /
//!   non-retryable `Err` / out of attempts → dead-letter (file `dead`, folder
//!   `synced_with_errors`) or `failed` for folder jobs (folder `error`). A panicking executor
//!   counts as a retryable failure, so a crash-looping file ends in the dead-letter list.
//! * **Internal kinds.** `scan_folder` and `delete_file` are executed here (a delete re-checks the
//!   root and the file first: [`Reconciler::delete_still_wanted`]); `index_file` for an access
//!   grant or a still-too-large file completes as a no-op. Everything else goes to the
//!   [`JobExecutor`].
//! * **Held work (M2).** The catalog never claims jobs of a paused folder, nor pipeline jobs of
//!   an unavailable one. A job whose folder was paused or went away *after* its claim is
//!   **released** (`jobs::release`): queued again, attempt not consumed, nothing dropped.
//! * **Shutdown.** Cancelling the token stops claiming; in-flight jobs get
//!   [`SyncConfig::drain_timeout`] to finish, then their cancel token fires. A job that returns
//!   `Cancelled` during shutdown keeps its lease, so the next epoch re-queues it (no attempt is
//!   wasted on a retry, nothing is dead-lettered).
//!
//! # Pause, resume, cancel (M2)
//! * [`SyncService::pause`]: the folder's running jobs are stopped and released, queued ones are
//!   held, the watcher stops, status `paused`. Nothing is lost.
//! * [`SyncService::resume`]: unpauses, the watcher restarts and a catch-up scan picks up what
//!   changed meanwhile (if the folder's sync mode reconciles at all). A folder paused by catalog
//!   migration V5 (the removed Files-only mode, Amendment 17) is read in full instead.
//! * [`SyncService::cancel`]: stops the **current run**: queued and running scan/index/delete jobs
//!   of the folder are dropped (to history as `cancelled`). Files it never reached stay pending
//!   (status `partial` until the next full scan); an edit whose job was dropped is re-checked by
//!   the next scan (`files::invalidate_metadata`). Live sync continues afterwards.
//!
//! Pause and cancel are serialised with the claim (one async gate), so a job can't slip through
//! between "claimed" and "registered as running".

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sr_catalog::repo::approvals::{self, AddFolderRequest, ApprovalRecord};
use sr_catalog::repo::files;
use sr_catalog::repo::folders;
use sr_catalog::repo::jobs::{self, FailOutcome, JobFailure, RetryPolicy};
use sr_catalog::repo::meta;
use sr_catalog::repo::status;
use sr_catalog::{
    Catalog, ClaimRequest, Drr, Epoch, FileState, FolderKind, Job, JobId, JobKind, RunTrigger,
};
use sr_common::config::FolderSettings;
use sr_common::error::{ClientKind, ErrCtx, Error, ErrorCode, Result};
use sr_common::events::EngineEvent;
use sr_common::ids::{ApprovalId, CollectionId, FolderId};
use sr_common::sandbox::DangerousRoots;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::events::{EventSink, Throttle};
use crate::reconcile::{
    AddFolder, DeleteCheck, EventBatch, EventOutcome, PathEvent, Reconciler, RootCheck,
    SKIP_FILES_ONLY, SKIP_TOO_LARGE, ScanOutcome, behavior_of,
};
use crate::watcher::{self, WatchConfig, WatcherHandle};
use crate::{JobExecutor, JobOutcome};

/// Kinds that need a pipeline permit.
pub const PIPELINE_KINDS: &[JobKind] = &[
    JobKind::IndexFile,
    JobKind::DeleteFile,
    JobKind::RebuildProfile,
    JobKind::PurgeFolder,
    JobKind::Retention,
];
const SCAN_KINDS: &[JobKind] = &[JobKind::ScanFolder];

/// Tunables (defaults suit the daemon; tests shrink the timers).
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Downstream capacity: jobs leased at once outside scans (ignored with `with_pipeline`).
    pub pipeline_permits: usize,
    /// Folder scans running at once.
    pub max_concurrent_scans: usize,
    /// Upper bound of one claim.
    pub claim_batch: usize,
    pub retry: RetryPolicy,
    /// One priority level per this many ms waited (0 = no aging).
    pub aging_ms: i64,
    /// Idle re-check (backoff timers); wake-ups are pushed via [`SyncService::wake`].
    pub idle_wait: Duration,
    /// Progress event rate limit per folder.
    pub progress_every: Duration,
    /// Time in-flight jobs get to finish after shutdown before their cancel token fires.
    pub drain_timeout: Duration,
    /// Roots that may never be folders.
    pub dangerous: DangerousRoots,
    /// MCP clients may ask to add folders (`settings.toml` `[integrations]
    /// mcp_add_folder_enabled`). **Off by default**; each request still needs the user's approval.
    /// Changeable at runtime with [`SyncService::set_mcp_add_folder_enabled`].
    pub mcp_add_folder_enabled: bool,
    /// How long an `add_folder` request waits for the user (R-UX12: 10 minutes).
    pub approval_ttl: Duration,
    /// Live watcher tunables ([`SyncService::start_watcher`]).
    pub watch: WatchConfig,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            pipeline_permits: 4,
            max_concurrent_scans: 2,
            claim_batch: 64,
            retry: RetryPolicy::default(),
            aging_ms: jobs::DEFAULT_AGING_MS,
            idle_wait: Duration::from_millis(500),
            progress_every: Duration::from_millis(250),
            drain_timeout: Duration::from_secs(10),
            dangerous: DangerousRoots::from_env(None),
            mcp_add_folder_enabled: false,
            approval_ttl: Duration::from_millis(
                u64::try_from(approvals::DEFAULT_TTL_MS).unwrap_or(600_000),
            ),
            watch: WatchConfig::default(),
        }
    }
}

/// What `startup` found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    pub folders: usize,
    pub unavailable: Vec<FolderId>,
    pub scans_enqueued: usize,
}

/// What one scheduler tick did ([`SyncService::scheduler_tick`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerTick {
    /// Folders whose cadence elapsed: a `schedule` scan was enqueued.
    pub due: Vec<FolderId>,
    /// Unavailable folders whose root is back: `available` again, `reconnect` scan enqueued
    /// where the policy allows it.
    pub reconnected: Vec<FolderId>,
    /// `add_folder` requests that timed out.
    pub approvals_expired: usize,
}

/// What [`SyncService::cancel`] stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CancelReport {
    /// Queued jobs dropped.
    pub dropped: usize,
    /// Running jobs told to stop (dropped when they return).
    pub stopped: usize,
}

/// Plug-in point for a folder watcher: the service tells it which roots to watch
/// ([`SyncService::reconcile_watchers`]); the watcher reports changes back through
/// [`SyncService::on_path_events`]. The built-in notify watcher ([`SyncService::start_watcher`])
/// manages its watch set by itself and doesn't need this.
pub trait FolderWatcher: Send + Sync {
    fn watch(&self, folder: FolderId, root: &std::path::Path) -> Result<()>;
    fn unwatch(&self, folder: FolderId);
}

/// An MCP client's request to add a folder ([`SyncService::request_add_folder`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAddFolder {
    /// Handshake `client_id` of the connection.
    pub client_id: String,
    pub client_kind: ClientKind,
    /// Display name when the client is not a known kind.
    pub client_name: Option<String>,
    /// The folder, as the client sent it (absolute).
    pub path: PathBuf,
    /// Collection to add it to (the engine passes the client's / default collection).
    pub collection_id: CollectionId,
}

/// A recorded `add_folder` request waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalTicket {
    pub id: ApprovalId,
    /// Canonical folder the user is asked about.
    pub root_path: PathBuf,
    pub expires_at: i64,
    /// `false` = the client had already asked; this is the same pending request.
    pub created: bool,
    pub client_kind: ClientKind,
    pub client_name: Option<String>,
}

impl ApprovalTicket {
    /// The error the MCP tool returns while the request waits: `ApprovalRequired`
    /// ("Waiting for your approval — {client} wants to add "{path}"", action ReviewApproval).
    pub fn to_error(&self) -> Error {
        Error::new(ErrorCode::ApprovalRequired).with_ctx(ErrCtx {
            path: Some(self.root_path.clone()),
            approval_id: Some(self.id),
            client: Some(self.client_kind),
            client_name: self.client_name.clone(),
            ..ErrCtx::default()
        })
    }
}

const STOP_NONE: u8 = 0;
const STOP_PAUSE: u8 = 1;
const STOP_CANCEL: u8 = 2;

/// A job between claim and result: its cancel token and why it was told to stop.
struct Inflight {
    folder: Option<FolderId>,
    token: CancellationToken,
    stop: Arc<AtomicU8>,
}

struct Inner<E> {
    catalog: Arc<Catalog>,
    executor: Arc<E>,
    events: Arc<dyn EventSink>,
    reconciler: Arc<Reconciler>,
    epoch: Epoch,
    cfg: SyncConfig,
    pipeline: Arc<Semaphore>,
    scans: Arc<Semaphore>,
    wake: Arc<Notify>,
    /// Folder set or folder state changed: the watcher re-reads folders now.
    watch_dirty: Arc<Notify>,
    dirty: Mutex<HashSet<FolderId>>,
    progress: Throttle,
    queue_depth: Throttle,
    inflight: Mutex<HashMap<JobId, Inflight>>,
    /// Serialises claim+register with pause/cancel (see module docs).
    gate: tokio::sync::Mutex<()>,
    mcp_add_folder: AtomicBool,
}

/// Sync service (cheap to clone).
pub struct SyncService<E> {
    inner: Arc<Inner<E>>,
}

impl<E> Clone for SyncService<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn trigger_of(job: &Job) -> RunTrigger {
    job.payload
        .as_ref()
        .and_then(|p| p.get("trigger"))
        .and_then(|t| t.as_str())
        .and_then(RunTrigger::parse)
        .unwrap_or(RunTrigger::Manual)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?
}

/// How a claimed job is disposed of.
enum Disposition {
    Ran(Result<JobOutcome>),
    /// Not the job's fault (folder paused or unavailable after the claim): release it.
    Hold,
}

/// Whether an `index_file` job should reach the executor.
enum Wanted {
    Run,
    Noop,
    Hold,
}

impl<E: JobExecutor + 'static> SyncService<E> {
    /// `epoch` must come from `Catalog::start_epoch()` of this daemon run.
    pub fn new(
        catalog: Arc<Catalog>,
        executor: Arc<E>,
        events: Arc<dyn EventSink>,
        epoch: Epoch,
        cfg: SyncConfig,
    ) -> Self {
        let pipeline = Arc::new(Semaphore::new(cfg.pipeline_permits.max(1)));
        Self::with_pipeline(catalog, executor, events, epoch, cfg, pipeline)
    }

    /// Like [`Self::new`] but sharing the engine's pipeline semaphore (one permit per job the
    /// downstream ingest pipeline can take).
    pub fn with_pipeline(
        catalog: Arc<Catalog>,
        executor: Arc<E>,
        events: Arc<dyn EventSink>,
        epoch: Epoch,
        cfg: SyncConfig,
        pipeline: Arc<Semaphore>,
    ) -> Self {
        let reconciler = Arc::new(Reconciler::new(
            Arc::clone(&catalog),
            Arc::clone(&events),
            cfg.dangerous.clone(),
            cfg.progress_every,
        ));
        let scans = Arc::new(Semaphore::new(cfg.max_concurrent_scans.max(1)));
        let progress = Throttle::new(cfg.progress_every);
        let queue_depth = Throttle::new(cfg.progress_every);
        let mcp = cfg.mcp_add_folder_enabled;
        Self {
            inner: Arc::new(Inner {
                catalog,
                executor,
                events,
                reconciler,
                epoch,
                cfg,
                pipeline,
                scans,
                wake: Arc::new(Notify::new()),
                watch_dirty: Arc::new(Notify::new()),
                dirty: Mutex::new(HashSet::new()),
                progress,
                queue_depth,
                inflight: Mutex::new(HashMap::new()),
                gate: tokio::sync::Mutex::new(()),
                mcp_add_folder: AtomicBool::new(mcp),
            }),
        }
    }

    pub fn reconciler(&self) -> &Arc<Reconciler> {
        &self.inner.reconciler
    }

    pub fn epoch(&self) -> Epoch {
        self.inner.epoch
    }

    /// New work may be claimable (enqueue from outside sr-sync, settings change, ...).
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// The folder set or a folder's state changed outside sr-sync (e.g. the engine removed a
    /// folder): the live watcher re-reads folders now instead of at its next probe (≤ 2 s).
    pub fn folders_changed(&self) {
        self.inner.watch_dirty.notify_one();
    }

    fn changed(&self) {
        self.wake();
        self.folders_changed();
    }

    /// Daemon start (after `start_epoch`, replay and `finalize_applied`): re-validate every root
    /// (junction retarget, dangerous roots, volume swap → `unavailable`, never deleted), refresh
    /// and push every folder status (re-queued work shows `indexing`, never `synced`: W2), and
    /// enqueue a catch-up scan where the policy allows it.
    pub async fn startup(&self) -> Result<StartupReport> {
        let rec = Arc::clone(&self.inner.reconciler);
        let report = blocking(move || {
            let all = rec.catalog().read(folders::folders)?;
            let mut r = StartupReport {
                folders: all.len(),
                ..StartupReport::default()
            };
            // docs/06 change 5: new built-in ignore defaults apply to every folder once (a
            // `settings_change` scan sweeps newly ignored files and re-checks JSON data files).
            let seen_defaults: u32 = rec
                .catalog()
                .read(|c| meta::get(c, crate::filter::META_IGNORE_DEFAULTS_VERSION))?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let defaults_changed = seen_defaults < crate::filter::IGNORE_DEFAULTS_VERSION;
            for f in all {
                let was_unavailable = !f.available;
                match rec.check_root(&f)? {
                    RootCheck::Unavailable(_) => {
                        r.unavailable.push(f.id);
                        continue;
                    }
                    RootCheck::Ok { .. } => {}
                }
                rec.refresh_status(f.id)?;
                let trigger = if defaults_changed {
                    RunTrigger::SettingsChange
                } else if was_unavailable {
                    RunTrigger::Reconnect
                } else {
                    RunTrigger::Startup
                };
                if !f.paused && behavior_of(&f).should_run(trigger) {
                    rec.enqueue_scan(f.id, trigger)?;
                    r.scans_enqueued += 1;
                }
            }
            if defaults_changed {
                let v = crate::filter::IGNORE_DEFAULTS_VERSION.to_string();
                rec.catalog().write_blocking(move |c| {
                    meta::set(c, crate::filter::META_IGNORE_DEFAULTS_VERSION, &v)
                })?;
            }
            Ok(r)
        })
        .await?;
        self.emit_queue_depth(true).await;
        self.changed();
        Ok(report)
    }

    /// Adds a folder (sandbox-validated, canonical, overlap-checked) and, if its sync mode allows
    /// automatic add sync (Selene `allowsAutomaticAddSync`), enqueues the first scan.
    pub async fn add_folder(&self, req: AddFolder) -> Result<FolderId> {
        let rec = Arc::clone(&self.inner.reconciler);
        let id = blocking(move || {
            let id = rec.add_folder(&req)?;
            let f = rec.catalog().read(|c| folders::folder(c, id))?;
            if behavior_of(&f).should_sync_on_add() {
                rec.enqueue_scan(id, RunTrigger::Manual)?;
            }
            Ok(id)
        })
        .await?;
        self.changed();
        Ok(id)
    }

    /// Removes a folder: its running jobs are stopped, the watcher drops it, and the catalog
    /// removal writes the `purge_folder` outbox op (purge SLA; `folders::remove_folder`).
    pub async fn remove_folder(&self, folder: FolderId) -> Result<()> {
        {
            let _g = self.inner.gate.lock().await;
            self.stop_folder(folder, STOP_CANCEL);
            self.inner
                .catalog
                .write(move |c| folders::remove_folder(c, folder, sr_catalog::now_ms()))
                .await?;
        }
        self.inner.reconciler.invalidate_root(folder);
        self.changed();
        Ok(())
    }

    /// Enqueues a scan if the folder's policy allows `trigger` (manual and MCP always may).
    /// Returns whether a job is now live. A **paused** folder refuses every trigger (`false`):
    /// its jobs would be held anyway; the user resumes it first.
    pub async fn request_scan(&self, folder: FolderId, trigger: RunTrigger) -> Result<bool> {
        let rec = Arc::clone(&self.inner.reconciler);
        let ok = blocking(move || {
            let f = rec.catalog().read(|c| folders::folder(c, folder))?;
            if f.paused || !behavior_of(&f).should_run(trigger) {
                return Ok(false);
            }
            rec.enqueue_scan(folder, trigger)?;
            Ok(true)
        })
        .await?;
        if ok {
            self.wake();
        }
        Ok(ok)
    }

    /// Applies new settings (see `Reconciler::update_settings`) and enqueues a `settings_change`
    /// scan when indexing output changed.
    pub async fn update_settings(&self, folder: FolderId, s: FolderSettings) -> Result<bool> {
        let rec = Arc::clone(&self.inner.reconciler);
        let affects = blocking(move || {
            let affects = rec.update_settings(folder, &s)?;
            if affects {
                rec.enqueue_scan(folder, RunTrigger::SettingsChange)?;
            }
            Ok(affects)
        })
        .await?;
        self.changed();
        Ok(affects)
    }

    /// Watcher hook: one path change. Prefer [`Self::on_path_events`] for batches.
    pub async fn on_path_event(&self, folder: FolderId, ev: PathEvent) -> Result<EventOutcome> {
        let rec = Arc::clone(&self.inner.reconciler);
        let out = blocking(move || rec.on_path_event(folder, &ev)).await?;
        if matches!(out, EventOutcome::Enqueued { .. }) {
            self.wake();
        }
        Ok(out)
    }

    /// Watcher hook: a debounced batch of changes for one folder (one folder read, cached root,
    /// batched writes; see `Reconciler::on_path_events`).
    pub async fn on_path_events(
        &self,
        folder: FolderId,
        evs: Vec<PathEvent>,
    ) -> Result<EventBatch> {
        let rec = Arc::clone(&self.inner.reconciler);
        let out = blocking(move || rec.on_path_events(folder, &evs)).await?;
        if out.scan_requested || out.index_enqueued + out.index_coalesced + out.deletes > 0 {
            self.wake();
        }
        Ok(out)
    }

    /// Starts the built-in live watcher (notify / ReadDirectoryChangesW, debounced, coalesced into
    /// jobs). It watches exactly the folders whose behaviour has `watch` and that are available
    /// and not paused, follows folder changes by itself, turns overflow and event storms into a
    /// folder rescan, and marks a folder whose root disappears `unavailable` (never deleting).
    /// Call once, inside the runtime; stop it with `cancel` and await [`WatcherHandle::join`].
    pub fn start_watcher(&self, cancel: CancellationToken) -> WatcherHandle {
        watcher::spawn(
            Arc::clone(&self.inner.reconciler),
            Arc::clone(&self.inner.wake),
            Arc::clone(&self.inner.watch_dirty),
            self.inner.cfg.watch.clone(),
            cancel,
        )
    }

    /// Starts/stops watchers of an external [`FolderWatcher`] so exactly the folders whose
    /// behaviour has `watch` (and are available and not paused) are watched.
    pub async fn reconcile_watchers(&self, w: &dyn FolderWatcher) -> Result<()> {
        let all = self.inner.catalog.read_async(folders::folders).await?;
        for f in all {
            if behavior_of(&f).watch && f.available && !f.paused {
                w.watch(f.id, &f.root_path)?;
            } else {
                w.unwatch(f.id);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ pause / resume / cancel

    /// Tells running jobs of `folder` to stop, recording why.
    fn stop_folder(&self, folder: FolderId, why: u8) -> usize {
        let g = lock(&self.inner.inflight);
        let mut n = 0;
        for i in g.values().filter(|i| i.folder == Some(folder)) {
            i.stop.store(why, Ordering::SeqCst);
            i.token.cancel();
            n += 1;
        }
        n
    }

    /// Pauses a folder (see module docs). Idempotent.
    pub async fn pause(&self, folder: FolderId) -> Result<()> {
        {
            let _g = self.inner.gate.lock().await;
            self.inner
                .catalog
                .write(move |c| folders::set_paused(c, folder, true, sr_catalog::now_ms()))
                .await?;
            self.stop_folder(folder, STOP_PAUSE);
        }
        self.refresh(folder).await;
        self.changed();
        Ok(())
    }

    /// Resumes a paused folder: held jobs run again, the watcher restarts, and a catch-up scan
    /// (`reconnect`) is enqueued when the folder's sync mode reconciles. Returns whether a scan
    /// was enqueued.
    ///
    /// Amendment 17: resuming a folder that was in the removed Files-only mode
    /// (`FolderRecord::was_files_only`) is the user's choice to search its contents. The mark is
    /// cleared with the pause, and a `settings_change` scan reads its listed files whatever its
    /// sync mode.
    pub async fn resume(&self, folder: FolderId) -> Result<bool> {
        let rec = Arc::clone(&self.inner.reconciler);
        let scanned = blocking(move || {
            let was_files_only = rec.catalog().write_blocking(move |c| {
                sr_catalog::atomic(c, |c| {
                    let now = sr_catalog::now_ms();
                    folders::set_paused(c, folder, false, now)?;
                    folders::clear_files_only_mark(c, folder, now)
                })
            })?;
            let f = rec.catalog().read(|c| folders::folder(c, folder))?;
            rec.refresh_status(folder)?;
            let trigger = if was_files_only {
                RunTrigger::SettingsChange
            } else {
                RunTrigger::Reconnect
            };
            if behavior_of(&f).should_run(trigger) {
                rec.enqueue_scan(folder, trigger)?;
                return Ok(true);
            }
            Ok(false)
        })
        .await?;
        self.changed();
        Ok(scanned)
    }

    /// Stops the folder's current run (see module docs). Live sync continues afterwards.
    pub async fn cancel(&self, folder: FolderId) -> Result<CancelReport> {
        let report = {
            let _g = self.inner.gate.lock().await;
            let dropped = self
                .inner
                .catalog
                .write(move |c| {
                    sr_catalog::atomic(c, |c| {
                        let now = sr_catalog::now_ms();
                        folders::set_cancelled(c, folder, Some(now), now)?;
                        jobs::cancel_queued(c, folder, now)
                    })
                })
                .await?;
            CancelReport {
                dropped: dropped.jobs,
                stopped: self.stop_folder(folder, STOP_CANCEL),
            }
        };
        self.refresh(folder).await;
        self.emit_queue_depth(true).await;
        self.wake();
        Ok(report)
    }

    // ------------------------------------------------------------------------------- approvals

    /// Turns the MCP `add_folder` tool on or off at runtime (`mcp_add_folder_enabled`, no
    /// restart). Turning it off expires every pending request.
    pub async fn set_mcp_add_folder_enabled(&self, on: bool) -> Result<()> {
        self.inner.mcp_add_folder.store(on, Ordering::SeqCst);
        if !on {
            self.inner
                .catalog
                .write(|c| approvals::expire(c, i64::MAX).map(|_| ()))
                .await?;
        }
        Ok(())
    }

    pub fn mcp_add_folder_enabled(&self) -> bool {
        self.inner.mcp_add_folder.load(Ordering::SeqCst)
    }

    /// An MCP client asks to add a folder. Off by default: then `ApprovalDenied`. When on, the
    /// folder is validated **first** (dangerous roots → `PathRefused`, missing → `FolderUnavailable`,
    /// overlap → `FolderOverlap`), so the user is only asked about folders that can be added; the
    /// request is recorded as **pending** (same request if the client asks again) and
    /// `ApprovalRequested` is pushed for the app. The tool then answers with
    /// [`ApprovalTicket::to_error`] (`ApprovalRequired`).
    pub async fn request_add_folder(&self, req: McpAddFolder) -> Result<ApprovalTicket> {
        if !self.mcp_add_folder_enabled() {
            return Err(Error::new(ErrorCode::ApprovalDenied)
                .with_detail("adding folders from connected apps is turned off")
                .with_ctx(ErrCtx {
                    client: Some(req.client_kind),
                    client_name: req.client_name.clone(),
                    ..ErrCtx::default()
                }));
        }
        let rec = Arc::clone(&self.inner.reconciler);
        let ttl = i64::try_from(self.inner.cfg.approval_ttl.as_millis()).unwrap_or(i64::MAX);
        let (a, created) = blocking(move || {
            let root = rec.validate_new_root(&req.path)?;
            let key = folders::root_key(&root);
            let collection = req.collection_id;
            let check_key = key.clone();
            rec.catalog()
                .read(move |c| folders::check_overlap_key(c, collection, &check_key))?;
            let r = AddFolderRequest {
                client_id: req.client_id,
                client_kind: req.client_kind,
                client_name: req.client_name,
                collection_id: collection,
                root_path: root,
                root_key: key,
            };
            rec.catalog().write_blocking(move |c| {
                approvals::request_add_folder(c, &r, sr_catalog::now_ms(), ttl)
            })
        })
        .await?;
        if created {
            self.inner.events.emit(EngineEvent::ApprovalRequested {
                approval_id: a.id,
                client: a.client_kind,
            });
        }
        Ok(ApprovalTicket {
            id: a.id,
            root_path: a.root_path,
            expires_at: a.expires_at,
            created,
            client_kind: a.client_kind,
            client_name: a.client_name,
        })
    }

    /// Requests waiting for the user (the app's approval list).
    pub async fn pending_approvals(&self) -> Result<Vec<ApprovalRecord>> {
        self.inner
            .catalog
            .read_async(|c| approvals::pending(c, sr_catalog::now_ms()))
            .await
    }

    /// One request (any state): the MCP side polls it while waiting.
    pub async fn approval(&self, id: ApprovalId) -> Result<ApprovalRecord> {
        self.inner
            .catalog
            .read_async(move |c| approvals::approval(c, id))
            .await
    }

    /// The user approves: the folder is added exactly as from the app (validated again, since
    /// things may have changed while the request waited) and its first scan follows the sync
    /// mode. Expired → `ApprovalDenied`; already decided → `Conflict`; the add itself may fail
    /// (`FolderOverlap`, `FolderUnavailable`, ...), which is recorded on the request.
    pub async fn approve(&self, id: ApprovalId) -> Result<FolderId> {
        let a = self
            .inner
            .catalog
            .write(move |c| approvals::decide(c, id, true, sr_catalog::now_ms()))
            .await?;
        let mut req = AddFolder::new(
            a.collection_id.unwrap_or_else(CollectionId::new),
            a.root_path.clone(),
        );
        req.kind = FolderKind::Indexed;
        let res = match a.collection_id {
            Some(_) => self.add_folder(req).await,
            None => Err(Error::new(ErrorCode::NotFound).with_detail("approval without collection")),
        };
        let (folder, code) = match &res {
            Ok(f) => (Some(*f), None),
            Err(e) => (None, Some(e.code)),
        };
        self.inner
            .catalog
            .write(move |c| approvals::record_result(c, id, folder, code))
            .await?;
        res
    }

    /// The user declines. Expired → `ApprovalDenied`; already decided → `Conflict`.
    pub async fn deny(&self, id: ApprovalId) -> Result<()> {
        self.inner
            .catalog
            .write(move |c| approvals::decide(c, id, false, sr_catalog::now_ms()).map(|_| ()))
            .await
    }

    // ------------------------------------------------------------------------------- scheduler

    /// Scheduler tick: enqueue a `schedule` scan for every folder whose cadence elapsed since its
    /// last full scan (Selene's background sync, cadence honoured below 1 h).
    pub async fn schedule_due(&self, now: i64) -> Result<Vec<FolderId>> {
        let rec = Arc::clone(&self.inner.reconciler);
        let due = blocking(move || {
            let mut due = Vec::new();
            for f in rec.catalog().read(folders::folders)? {
                let Some(min) = behavior_of(&f).reconcile_every_min else {
                    continue;
                };
                if f.paused || !f.available {
                    continue;
                }
                let every = i64::from(min) * 60_000;
                if f.last_full_scan_at.is_none_or(|t| now - t >= every) {
                    rec.enqueue_scan(f.id, RunTrigger::Schedule)?;
                    due.push(f.id);
                }
            }
            Ok(due)
        })
        .await?;
        if !due.is_empty() {
            self.wake();
        }
        Ok(due)
    }

    /// One full scheduler tick: [`Self::schedule_due`], reconnect probe of unavailable folders
    /// (root back → `available`, `reconnect` scan if the policy allows; the watcher covers its
    /// own folders faster), and approval expiry.
    pub async fn scheduler_tick(&self, now: i64) -> Result<SchedulerTick> {
        let due = self.schedule_due(now).await?;
        let rec = Arc::clone(&self.inner.reconciler);
        let reconnected = blocking(move || {
            let mut back = Vec::new();
            for f in rec.catalog().read(folders::folders)? {
                if f.available || f.paused {
                    continue;
                }
                if let RootCheck::Ok { .. } = rec.check_root(&f)? {
                    if behavior_of(&f).should_run(RunTrigger::Reconnect) {
                        rec.enqueue_scan(f.id, RunTrigger::Reconnect)?;
                    }
                    back.push(f.id);
                }
            }
            Ok(back)
        })
        .await?;
        let approvals_expired = self
            .inner
            .catalog
            .write(move |c| approvals::expire(c, now).map(|v| v.len()))
            .await?;
        if !reconnected.is_empty() {
            self.changed();
        }
        Ok(SchedulerTick {
            due,
            reconnected,
            approvals_expired,
        })
    }

    /// Stale-folder scheduler: [`Self::scheduler_tick`] every `tick` (the engine uses 60 s).
    pub async fn run_scheduler(&self, cancel: CancellationToken, tick: Duration) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(tick) => {}
            }
            if let Err(e) = self.scheduler_tick(sr_catalog::now_ms()).await {
                tracing::warn!(error = %e, "scheduler tick failed");
            }
        }
    }

    async fn emit_queue_depth(&self, force: bool) {
        if !force && !self.inner.queue_depth.ready(None) {
            return;
        }
        if let Ok((queued, leased)) = self.inner.catalog.read_async(jobs::queue_depth).await {
            self.inner
                .events
                .emit(EngineEvent::QueueDepth { queued, leased });
        }
    }

    // ------------------------------------------------------------------------------ claim loop

    /// Runs the claim loop until `cancel` fires, then drains. Returns when every task finished.
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let job_cancel = CancellationToken::new();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut drr_scan = Drr::default();
        let mut drr_pipe = Drr::default();
        loop {
            while let Some(r) = tasks.try_join_next() {
                if let Err(e) = r {
                    tracing::error!(error = %e, "job task failed");
                }
            }
            if cancel.is_cancelled() {
                break;
            }
            let mut claimed = 0;
            for (lane_sem, kinds, drr) in [
                (&self.inner.scans, SCAN_KINDS, &mut drr_scan),
                (&self.inner.pipeline, PIPELINE_KINDS, &mut drr_pipe),
            ] {
                let free = lane_sem.available_permits().min(self.inner.cfg.claim_batch);
                if free == 0 {
                    continue;
                }
                let Ok(permit) =
                    Arc::clone(lane_sem).try_acquire_many_owned(u32::try_from(free).unwrap_or(1))
                else {
                    continue;
                };
                match self
                    .claim(kinds, permit, drr, &mut tasks, &job_cancel, &cancel)
                    .await
                {
                    Ok(n) => claimed += n,
                    Err(e) => tracing::warn!(error = %e, "claim failed"),
                }
            }
            if claimed > 0 {
                self.emit_queue_depth(false).await;
                continue;
            }
            self.flush_dirty().await;
            tokio::select! {
                () = cancel.cancelled() => break,
                () = self.inner.wake.notified() => {}
                () = tokio::time::sleep(self.inner.cfg.idle_wait) => {}
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        // Drain: let in-flight jobs finish, then cancel them; never leave tasks behind.
        let drained = tokio::time::timeout(self.inner.cfg.drain_timeout, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_ok();
        if !drained {
            job_cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(5), async {
                while tasks.join_next().await.is_some() {}
            })
            .await
            .is_err()
            {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            }
        }
        lock(&self.inner.inflight).clear();
        self.flush_dirty().await;
        self.emit_queue_depth(true).await;
        Ok(())
    }

    async fn claim(
        &self,
        kinds: &'static [JobKind],
        mut permit: OwnedSemaphorePermit,
        drr: &mut Drr,
        tasks: &mut JoinSet<()>,
        job_cancel: &CancellationToken,
        shutdown: &CancellationToken,
    ) -> Result<usize> {
        let req = ClaimRequest {
            epoch: self.inner.epoch,
            permits: permit.num_permits(),
            now: sr_catalog::now_ms(),
            kinds: Some(kinds),
            aging_ms: self.inner.cfg.aging_ms,
        };
        let state = std::mem::take(drr);
        // Claim and register under the gate: a pause/cancel either happens before the claim (the
        // catalog then holds/drops the jobs) or after registration (it stops them).
        let gate = self.inner.gate.lock().await;
        let (claimed, state) = self
            .inner
            .catalog
            .write(move |c| {
                let mut d = state;
                let j = jobs::claim(c, &req, &mut d)?;
                Ok((j, d))
            })
            .await?;
        *drr = state;
        let n = claimed.len();
        let mut scan_folders = Vec::new();
        let mut spawn = Vec::with_capacity(n);
        {
            let mut inflight = lock(&self.inner.inflight);
            for job in claimed {
                let token = job_cancel.child_token();
                let stop = Arc::new(AtomicU8::new(STOP_NONE));
                inflight.insert(
                    job.id,
                    Inflight {
                        folder: job.folder_id,
                        token: token.clone(),
                        stop: Arc::clone(&stop),
                    },
                );
                spawn.push((job, token, stop));
            }
        }
        drop(gate);
        for (job, token, stop) in spawn {
            let Some(one) = permit.split(1) else {
                // Unreachable (claims ≤ permits); keep the lease for the next epoch.
                tracing::error!(job = job.id, "claimed more jobs than permits");
                lock(&self.inner.inflight).remove(&job.id);
                break;
            };
            if job.kind == JobKind::ScanFolder
                && let Some(f) = job.folder_id
            {
                scan_folders.push(f);
            }
            let svc = self.clone();
            let sd = shutdown.clone();
            tasks.spawn(async move {
                let res = svc.dispatch(&job, &token).await;
                svc.finish(&job, res, &stop, &sd).await;
                drop(one);
                svc.inner.wake.notify_one();
            });
        }
        drop(permit); // unused remainder back to the lane
        for f in scan_folders {
            self.refresh(f).await;
        }
        Ok(n)
    }

    async fn folder_paused(&self, folder: FolderId) -> Result<bool> {
        match self
            .inner
            .catalog
            .read_async(move |c| folders::folder(c, folder))
            .await
        {
            Ok(f) => Ok(f.paused),
            Err(e) if e.code == ErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn dispatch(&self, job: &Job, cancel: &CancellationToken) -> Disposition {
        match self.dispatch_inner(job, cancel).await {
            Ok(d) => d,
            Err(e) => Disposition::Ran(Err(e)),
        }
    }

    async fn dispatch_inner(&self, job: &Job, cancel: &CancellationToken) -> Result<Disposition> {
        Ok(match job.kind {
            JobKind::ScanFolder => {
                let Some(folder) = job.folder_id else {
                    return Ok(Disposition::Ran(Ok(JobOutcome::Done)));
                };
                if self.folder_paused(folder).await? {
                    return Ok(Disposition::Hold);
                }
                let rec = Arc::clone(&self.inner.reconciler);
                let c = cancel.clone();
                let trig = trigger_of(job);
                Disposition::Ran(
                    match blocking(move || rec.scan_blocking(folder, trig, &c)).await {
                        Ok(ScanOutcome::Completed(_) | ScanOutcome::Unavailable(_)) => {
                            Ok(JobOutcome::Done)
                        }
                        Err(e) if e.code == ErrorCode::NotFound => Ok(JobOutcome::Done), // removed
                        Err(e) => Err(e),
                    },
                )
            }
            JobKind::DeleteFile => {
                let Some(file) = job.file_id else {
                    return Ok(Disposition::Ran(Ok(JobOutcome::Done)));
                };
                let rec = Arc::clone(&self.inner.reconciler);
                match blocking(move || rec.delete_still_wanted(file)).await? {
                    DeleteCheck::Delete => {
                        self.inner
                            .catalog
                            .write(move |c| files::delete_file(c, file, sr_catalog::now_ms()))
                            .await?;
                        Disposition::Ran(Ok(JobOutcome::Done))
                    }
                    DeleteCheck::RootUnavailable => Disposition::Hold,
                    DeleteCheck::FileExists | DeleteCheck::AlreadyGone => {
                        Disposition::Ran(Ok(JobOutcome::Done))
                    }
                }
            }
            JobKind::IndexFile => match self.index_still_wanted(job).await? {
                Wanted::Noop => Disposition::Ran(Ok(JobOutcome::Done)),
                Wanted::Hold => Disposition::Hold,
                Wanted::Run => Disposition::Ran(self.execute(job, cancel).await),
            },
            _ => Disposition::Ran(self.execute(job, cancel).await),
        })
    }

    async fn execute(&self, job: &Job, cancel: &CancellationToken) -> Result<JobOutcome> {
        let ex = Arc::clone(&self.inner.executor);
        let c = cancel.clone();
        let j = job.clone();
        match tokio::spawn(async move { ex.execute(j, &c).await }).await {
            Ok(r) => r,
            Err(je) if je.is_panic() => {
                let mut e = Error::new(ErrorCode::Internal).with_detail("executor panicked");
                e.retryable = true; // attempts accumulate → dead-letter, no crash loop
                Err(e)
            }
            Err(_) => Err(Error::new(ErrorCode::Cancelled)),
        }
    }

    /// `index_file` reaches the executor unless: the folder or file is gone, the folder is an
    /// access grant (the file is listed instead), or the file is still too large (no-op); or the
    /// folder was paused / went unavailable after the claim (held: released, nothing dropped).
    async fn index_still_wanted(&self, job: &Job) -> Result<Wanted> {
        let (Some(folder), Some(file)) = (job.folder_id, job.file_id) else {
            return Ok(Wanted::Run);
        };
        self.inner
            .catalog
            .write(move |c| {
                let f = match folders::folder(c, folder) {
                    Ok(f) => f,
                    Err(e) if e.code == ErrorCode::NotFound => return Ok(Wanted::Noop),
                    Err(e) => return Err(e),
                };
                let rec = match files::file(c, file) {
                    Ok(r) => r,
                    Err(e) if e.code == ErrorCode::NotFound => return Ok(Wanted::Noop),
                    Err(e) => return Err(e),
                };
                if !f.available || f.paused {
                    return Ok(Wanted::Hold);
                }
                if !behavior_of(&f).embed {
                    if rec.state != FileState::Skipped {
                        files::mark_skipped(c, file, SKIP_FILES_ONLY, None, sr_catalog::now_ms())?;
                    }
                    return Ok(Wanted::Noop);
                }
                // A skip that still applies (the scan re-enqueues a file once it no longer does).
                let still_too_large = rec.skip_reason.as_deref() == Some(SKIP_TOO_LARGE)
                    && rec.size > f.settings.max_file_bytes;
                Ok(if rec.state == FileState::Skipped && still_too_large {
                    Wanted::Noop
                } else {
                    Wanted::Run
                })
            })
            .await
    }

    async fn finish(
        &self,
        job: &Job,
        res: Disposition,
        stop: &AtomicU8,
        shutdown: &CancellationToken,
    ) {
        lock(&self.inner.inflight).remove(&job.id);
        let (id, epoch, policy) = (job.id, self.inner.epoch, self.inner.cfg.retry);
        let why = stop.load(Ordering::SeqCst);
        enum Act {
            Complete,
            Fail(JobFailure),
            Release,
            Drop,
        }
        let act = match res {
            Disposition::Hold => Act::Release,
            Disposition::Ran(Ok(JobOutcome::Done)) => Act::Complete,
            Disposition::Ran(Err(e)) if e.code == ErrorCode::Cancelled && why == STOP_PAUSE => {
                Act::Release
            }
            Disposition::Ran(Err(e)) if e.code == ErrorCode::Cancelled && why == STOP_CANCEL => {
                Act::Drop
            }
            Disposition::Ran(Err(e))
                if e.code == ErrorCode::Cancelled && shutdown.is_cancelled() =>
            {
                // Keep the lease: the next epoch re-queues it without burning an attempt here.
                return;
            }
            Disposition::Ran(Ok(JobOutcome::Retry)) => Act::Fail(JobFailure {
                code: ErrorCode::Busy,
                message: "executor asked to retry".into(),
                retryable: true,
                retry_after_ms: None,
            }),
            Disposition::Ran(Ok(JobOutcome::Dead)) => Act::Fail(JobFailure {
                code: ErrorCode::FileUnreadable,
                message: "executor gave up".into(),
                retryable: false,
                retry_after_ms: None,
            }),
            Disposition::Ran(Err(e)) if e.code == ErrorCode::Cancelled => Act::Fail(JobFailure {
                code: ErrorCode::Cancelled,
                message: e.to_string(),
                retryable: true,
                retry_after_ms: Some(0),
            }),
            Disposition::Ran(Err(e)) => Act::Fail(JobFailure::from_error(&e)),
        };
        let code = match &act {
            Act::Fail(f) => Some(f.code),
            _ => None,
        };
        let r = self
            .inner
            .catalog
            .write(move |c| {
                let now = sr_catalog::now_ms();
                match act {
                    Act::Complete => jobs::complete(c, id, epoch, now).map(|_| None),
                    Act::Fail(f) => jobs::fail(c, id, epoch, &f, &policy, now).map(Some),
                    Act::Release => jobs::release(c, id, epoch, now, now).map(|()| None),
                    Act::Drop => jobs::cancel_leased(c, id, epoch, now).map(|_| None),
                }
            })
            .await;
        match r {
            Ok(Some(FailOutcome::Dead)) => {
                if let (Some(folder_id), Some(file_id), Some(code)) =
                    (job.folder_id, job.file_id, code)
                {
                    self.inner.events.emit(EngineEvent::FileFailed {
                        folder_id,
                        file_id,
                        code,
                    });
                }
            }
            Ok(_) => {}
            Err(e) if e.code == ErrorCode::Conflict => {
                tracing::info!(
                    job = id,
                    "lease fenced or job dropped meanwhile; result dropped"
                );
            }
            Err(e) => tracing::warn!(job = id, error = %e, "could not record job result"),
        }
        if let Some(f) = job.folder_id {
            if self.inner.progress.ready(Some(f)) {
                self.refresh(f).await;
            } else {
                lock(&self.inner.dirty).insert(f);
            }
        }
    }

    /// Status + progress push for one folder.
    async fn refresh(&self, folder: FolderId) {
        let rec = Arc::clone(&self.inner.reconciler);
        let r = blocking(move || {
            rec.refresh_status(folder)?;
            rec.catalog().read(|c| status::counts(c, folder))
        })
        .await;
        match r {
            Ok(n) => self.inner.events.emit(EngineEvent::IndexProgress {
                folder_id: folder,
                files_done: n.indexed_files + n.skipped_files + n.listed_files + n.dead_files,
                files_total: n.total_files(),
                eta_s: None,
            }),
            Err(e) if e.code == ErrorCode::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "status refresh failed"),
        }
    }

    async fn flush_dirty(&self) {
        let dirty: Vec<FolderId> = lock(&self.inner.dirty).drain().collect();
        for f in dirty {
            self.refresh(f).await;
        }
    }
}
