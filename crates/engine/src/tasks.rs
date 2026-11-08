//! Background tasks: group-commit writer, claim loop, maintenance.
//!
//! * **Writer** (§2.7, Amendments 4.24/4.25): wakes every `group_commit` (2 s) while there is
//!   work, `commit_debounce` (300 ms) after a T1 nudge, and at once after start when the last
//!   run left outbox work ("Catching up after restart"). It then commits bounded batches (2 s /
//!   512 embedded chunks / 1 000 ops) until the outbox is empty, pushing progress after each;
//!   each batch runs on a blocking-pool thread (tantivy commits and the synchronous catalog
//!   traits block), never on a request worker.
//! * **Claim loop + scheduler**: sr-sync's `SyncService` (scans, deletes, DRR claims with the
//!   engine's pipeline semaphore as permits); index jobs run on [`Ingest`](crate::ingest::Ingest).
//! * **Maintenance** (every minute; schedule uses the injectable clock): `Catalog::optimize` every
//!   6 h; Lance version retention (`retention_hours`, default 1 h) and 7-day job history; the
//!   purge SLA: a removed folder is unsearchable at once (catalog rows gone), and physically
//!   removed from Lance and tantivy by the next drain — forced at 5 min, alarm at 15 min.

use std::collections::HashMap;
use std::time::Duration;

use sr_catalog::repo::jobs;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::FolderStatus;
use sr_common::ids::FolderId;

use crate::EngineHandle;

/// Purge: force a drain + compaction once a removal has waited this long.
pub const PURGE_FORCE_AFTER_MS: i64 = 5 * 60 * 1000;
/// Purge SLA (docs/00 Q7).
pub const PURGE_SLA_MS: i64 = 15 * 60 * 1000;
/// `Catalog::optimize` interval.
pub const OPTIMIZE_EVERY_MS: i64 = 6 * 60 * 60 * 1000;
/// Maintenance tick (real time).
pub const MAINT_TICK: Duration = Duration::from_secs(60);
/// Shutdown drain (Amendment 4.25, target ≤ 10 s): how long running jobs may finish ...
pub const DRAIN_JOBS_WAIT: Duration = Duration::from_secs(3);
/// ... and when the writer is told to stop at its next batch boundary (from the drain's start).
/// An in-flight embedding window is abandoned at once; a commit in progress finishes.
pub const DRAIN_COMMIT_BUDGET: Duration = Duration::from_secs(7);

/// Maintenance bookkeeping.
#[derive(Debug, Default)]
pub struct MaintState {
    pub last_optimize_ms: i64,
    pub last_retention_ms: i64,
    /// Removed folders waiting for physical removal → time of removal.
    pub pending_purges: HashMap<FolderId, i64>,
    pub last_status: HashMap<FolderId, (FolderStatus, u64, u64)>,
    /// Removed folders' display name and path, shown as "Removing…" until purged (OQ-18).
    pub removing: HashMap<FolderId, (String, String)>,
}

/// What one maintenance pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MaintReport {
    pub optimized: bool,
    pub retention: bool,
    pub purges_forced: usize,
    pub purges_completed: Vec<FolderId>,
    pub purge_sla_missed: usize,
}

pub(crate) fn spawn_background(h: &EngineHandle) {
    let i = h.inner();
    let mut v = Vec::new();
    *i.writer_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(tokio::spawn(writer_loop(h.clone())));
    v.push(tokio::spawn(maintenance_loop(h.clone())));
    // sr-sync: the scheduler (reconcile, reconnect probe, approval expiry) and the claim loop. The claim loop
    // drains in-flight jobs when the shutdown token fires; `drain` awaits it.
    let (s, c) = (i.sync.clone(), i.shutdown.child_token());
    v.push(tokio::spawn(async move {
        s.run_scheduler(c, Duration::from_secs(60)).await;
    }));
    let (s, c) = (i.sync.clone(), i.shutdown.clone());
    let run = tokio::spawn(async move {
        if let Err(e) = s.run(c).await {
            tracing::error!(target: "sr_engine::jobs", err = %e, "claim loop stopped");
        }
    });
    *i.sync_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(run);
    // Live sync (M2-W): notify watcher behind sr-sync's hook; reads the folder set itself.
    *i.watcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(i.sync.start_watcher(i.shutdown.child_token()));
    i.tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(v);
}

async fn writer_loop(h: EngineHandle) {
    let i = h.inner();
    let mut backoff = Duration::from_millis(500);
    loop {
        let nudged = tokio::select! {
            () = i.shutdown.cancelled() => break,
            () = i.writer_nudge.notified() => true,
            () = tokio::time::sleep(i.cfg.group_commit) => false,
        };
        if nudged {
            // Group commit: let more T1s arrive, but stay within the save-to-searchable budget.
            tokio::select! {
                () = i.shutdown.cancelled() => break,
                () = tokio::time::sleep(i.cfg.commit_debounce) => {}
            }
        } else if !h.outbox_has_work().await {
            continue;
        }
        // Bounded batches (4.25) until the outbox is empty; status and progress are pushed after
        // each one. At shutdown `writer_stop` ends it at the next batch boundary.
        match h.writer_batches(&i.writer_stop).await {
            Ok(_) => backoff = Duration::from_millis(500),
            Err(e) => {
                tracing::error!(target: "sr_engine::writer", code = %e.code, err = %e, "index commit failed; retrying");
                tokio::select! {
                    () = i.shutdown.cancelled() => break,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn maintenance_loop(h: EngineHandle) {
    let i = h.inner();
    loop {
        tokio::select! {
            () = i.shutdown.cancelled() => break,
            () = tokio::time::sleep(MAINT_TICK) => {}
        }
        if let Err(e) = h.run_maintenance_once().await {
            tracing::error!(target: "sr_engine::maintenance", err = %e, "maintenance pass failed");
        }
    }
}

/// One maintenance pass (also called directly by tests with a manual clock).
pub(crate) async fn maintenance_once(h: &EngineHandle) -> Result<MaintReport, Error> {
    let i = h.inner();
    let now = i.clock.now_ms();
    let mut rep = MaintReport::default();

    // Purge SLA.
    let (overdue, oldest, before) = {
        let m = i
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let overdue = m
            .pending_purges
            .values()
            .filter(|t| now - **t >= PURGE_FORCE_AFTER_MS)
            .count();
        let before: Vec<FolderId> = m.pending_purges.keys().copied().collect();
        (overdue, m.pending_purges.values().copied().min(), before)
    };
    if overdue > 0 {
        rep.purges_forced = overdue;
        h.drain_writer_now().await?;
        // Belt and braces: compact + prune every table so no old version keeps the bytes.
        for s in i.registry.all() {
            s.vectors.compact_and_purge_old_versions().await?;
            let lex = s.lexical.clone();
            tokio::task::spawn_blocking(move || lex.merge_and_gc())
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        }
    }
    h.check_purges_completed().await?;
    // M4 (page 30): activity is kept for 7 days.
    h.prune_activity().await;
    {
        // Completed during this pass (by the forced drain or this check).
        let m = i
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rep.purges_completed = before
            .into_iter()
            .filter(|f| !m.pending_purges.contains_key(f))
            .collect();
    }
    if let Some(t) = oldest
        && now - t >= PURGE_SLA_MS
    {
        let still = i
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_purges
            .len();
        if still > 0 {
            rep.purge_sla_missed = still;
            tracing::error!(target: "sr_engine::maintenance", pending = still, "purge SLA missed");
        }
    }

    // Catalog optimize every 6 h (the first one 6 h after start; `open` already ran one).
    let do_optimize = {
        let mut m = i
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if m.last_optimize_ms == 0 {
            m.last_optimize_ms = now;
        }
        now - m.last_optimize_ms >= OPTIMIZE_EVERY_MS
    };
    if do_optimize {
        let cat = i.cat.clone();
        tokio::task::spawn_blocking(move || cat.optimize())
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        i.maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_optimize_ms = now;
        rep.optimized = true;
    }

    // Retention: Lance versions older than `retention_hours`; job history older than 7 days.
    let hours = i
        .settings
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .system
        .retention_hours
        .max(1);
    let every = i64::from(hours) * 60 * 60 * 1000;
    let do_retention = {
        let mut m = i
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if m.last_retention_ms == 0 {
            m.last_retention_ms = now;
        }
        now - m.last_retention_ms >= every
    };
    if do_retention {
        let keep = lancedb::table::optimize::Duration::try_hours(i64::from(hours))
            .unwrap_or_else(lancedb::table::optimize::Duration::zero);
        for s in i.registry.all() {
            s.vectors.prune_versions(keep).await?;
        }
        i.cat
            .write(move |c| jobs::prune_history(c, now, jobs::HISTORY_KEEP_MS))
            .await?;
        i.maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_retention_ms = now;
        rep.retention = true;
    }
    Ok(rep)
}

/// Wait for every executor slot (in-flight jobs finished) or the timeout.
pub(crate) async fn wait_jobs_idle(h: &EngineHandle, timeout: Duration) -> bool {
    let sem = h.inner().jobs_in_flight.clone();
    let n = u32::try_from(h.inner().cfg.job_permits.max(1)).unwrap_or(u32::MAX);
    match tokio::time::timeout(timeout, sem.acquire_many_owned(n)).await {
        Ok(Ok(p)) => {
            drop(p);
            true
        }
        _ => false,
    }
}
