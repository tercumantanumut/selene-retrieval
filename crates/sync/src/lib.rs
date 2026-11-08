//! sr-sync: walker, reconciler, folder policy, scheduler, claim loop and the [`JobExecutor`]
//! trait (the engine wires sr-core's executor in). Depends on sr-common and sr-catalog only.
//!
//! # Modules
//! * [`policy`]: [`FolderBehavior`], the port of Selene's `resolveFolderSyncBehavior` /
//!   `shouldRunForTrigger`, plus reindex-policy scan depth.
//! * [`filter`]: Selene's default ignore patterns, user globs, file-type presets and the
//!   always-on sandbox component/denylist checks.
//! * [`rules`]: a folder's ignore files outside the walk (`.seleneignore` re-includes of built-in
//!   defaults, watcher events; docs/06 change 5).
//! * [`walker`]: the `ignore`-crate walk with gitignore support and directory-completeness
//!   tracking for mark-and-sweep (W4).
//! * [`reconcile`]: tiered change detection (size + mtime → blake3, racy-git), rename by 128-bit
//!   file id, policy skips, the sweep, root re-validation, watcher-style path events, settings
//!   changes.
//! * [`service`]: [`SyncService`]: the claim loop (two lanes, DRR, permits = backpressure, epoch
//!   fencing, retry/dead-letter, drain), startup recovery, scheduler and the [`FolderWatcher`]
//!   hook for M2.
//! * [`watcher`]: the M2 live watcher (notify / ReadDirectoryChangesW, debounce ≤ 500 ms,
//!   coalescing, overflow and storm rescans, root probe → `unavailable`), started with
//!   [`SyncService::start_watcher`].
//! * [`events`]: [`EventSink`] (push-only progress; the sink assigns `seq`).
//! * [`fsid`]: `FILE_ID_INFO` identity and online-only attributes (reviewed-unsafe module).
//!
//! # Engine wiring (start order, Amendment 4.2)
//! `Catalog::open` → `start_epoch` (→ `epoch`) → store replay → `finalize_applied` →
//! `SyncService::new(catalog, executor, bus, epoch, cfg)` → `startup()` → spawn `run(cancel)` and
//! `run_scheduler(cancel, 60 s)` → `start_watcher(cancel)` → pipe opens. Shutdown: cancel the token,
//! await `run`, then `WatcherHandle::join`.
//!
//! # M2 additions
//! Pause / resume / cancel per folder, held (not dropped) work for paused and unavailable folders,
//! MCP `add_folder` approvals (off by default), network / mapped drives reconcile-only
//! (`watch_note`), 64-bit volume serials. API summary: `docs/research/m2-w-progress.md`.

pub mod events;
pub mod filter;
pub mod fsid;
pub mod netprobe;
pub mod policy;
pub mod reconcile;
pub mod rules;
pub mod service;
pub mod walker;
pub mod watcher;

use std::future::Future;

pub use events::{EventSink, NullSink};
pub use policy::{FolderBehavior, ScanDepth};
pub use reconcile::{
    AddFolder, DeleteCheck, EventBatch, EventOutcome, FolderEstimate, PathEvent, Reconciler,
    ScanOutcome, ScanStats, watch_note,
};
pub use service::{
    ApprovalTicket, CancelReport, FolderWatcher, McpAddFolder, SchedulerTick, StartupReport,
    SyncConfig, SyncService,
};
use sr_catalog::Job;
use sr_common::Result;
use tokio_util::sync::CancellationToken;
pub use watcher::{WatchConfig, WatchCounters, WatcherHandle};

/// Outcome of executing one job.
///
/// Prefer returning `Err(e)` with a specific `ErrorCode` (its `retryable` flag decides between
/// backoff and dead-letter, `retry_after` is honoured); the plain variants map to `Busy`
/// (retryable) and `FileUnreadable` (not retryable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    Done,
    /// Retry later (backoff by attempts).
    Retry,
    /// Give up; the file/job is marked dead with its error code.
    Dead,
}

/// Executes claimed jobs (implemented by sr-core's ingest pipeline, wired in by sr-engine).
///
/// Receives `index_file`, `rebuild_profile`, `purge_folder` and `retention`. `index_file`
/// payload flags (`sr_catalog::repo::jobs::PAYLOAD_*`, merged on coalescing by
/// `jobs::merge_payload`):
/// * `{"force": true}`: `reindex_policy = always` asks to re-index unchanged content;
/// * `{"path_refresh": true}`: a pure rename; re-apply the file at its current version so the
///   lexical `path` field is current, **without** reading or re-embedding the content;
/// * `{"renamed": true}`: a rename that may also have changed the content; index as usual, and
///   if the content is unchanged still refresh the path as for `path_refresh`.
///
/// `scan_folder`
/// and `delete_file` are executed by sr-sync itself. The job holds one pipeline permit while it
/// runs. Return `Err(Cancelled)` promptly when `cancel` fires (daemon shutdown).
pub trait JobExecutor: Send + Sync {
    fn execute(
        &self,
        job: Job,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<JobOutcome>> + Send;
}
