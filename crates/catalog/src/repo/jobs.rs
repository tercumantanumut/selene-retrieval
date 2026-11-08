//! Durable job queue (architect D4, simplified per docs/00 §0.A M4).
//!
//! * **Dedupe:** `UNIQUE(dedupe_key) WHERE state IN ('queued','leased')`; enqueue is an
//!   `INSERT … ON CONFLICT DO UPDATE` that coalesces (lowest priority number and earliest run time
//!   win). Coalescing into a *leased* job sets `rerun`, so the change that arrived while it ran is
//!   not lost: on completion the job goes back to `queued` instead of `done`. Payloads are
//!   merged by [`merge_payload`] so a coalesced job keeps every need of both requests (a cheap
//!   `path_refresh` never swallows a content change; `force` is sticky).
//! * **Epoch fencing:** `lease_owner = daemon epoch`. At start every lease from an older epoch
//!   returns to `queued` ([`requeue_stale_leases`]); completing or failing a job checks the owner,
//!   so a stale holder can never finish someone else's lease. No heartbeat is required
//!   (`heartbeat_at` is informational).
//! * **Claim:** effective priority (0–3, with aging), then deficit round-robin across folders
//!   ([`Drr`]), limited by `permits` (downstream capacity = backpressure).
//! * **Failure:** retryable → `queued` with exponential backoff; out of attempts → `dead`
//!   (dead-letter; the file is marked `dead`); non-retryable non-file job → `failed`.
//! * `attempts` counts claims, so a job that crashes the daemon repeatedly ends in the dead-letter
//!   list instead of crash-looping.
//! * **Held folders (M2):** the claim skips every job of a **paused** folder, and the pipeline jobs
//!   (everything but `scan_folder`) of an **unavailable** one. They stay `queued` untouched, so a
//!   pause or an unplugged drive never consumes an attempt or drops an edit. A scan of an
//!   unavailable folder still runs: it re-validates the root and reconnects the folder.
//!   [`release`] returns a leased job to the queue without consuming its attempt (a job the
//!   service had to stop because its folder was paused meanwhile).

use std::collections::{HashMap, VecDeque};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::{FileId, FolderId};

use crate::db::{DbResultExt, atomic};
use crate::model::{Epoch, JobId, JobKind, JobState, Priority, RowExt};
use crate::repo::files;

/// `index_file` payload flag: re-index even if the content hash is unchanged.
pub const PAYLOAD_FORCE: &str = "force";
/// `index_file` payload flag: **only** the file's path changed (rename). The executor re-applies
/// the file at its current version (lexical `path` field refreshed, vectors copied, no embedding)
/// and never looks at the content.
pub const PAYLOAD_PATH_REFRESH: &str = "path_refresh";
/// `index_file` payload flag: the path changed **and** the content may have changed. The executor
/// checks the content as usual; if it turns out unchanged it still refreshes the path.
pub const PAYLOAD_RENAMED: &str = "renamed";

fn flag_of(m: &serde_json::Map<String, Value>, k: &str) -> bool {
    m.get(k).and_then(Value::as_bool) == Some(true)
}

/// Payload of a coalesced job: the union of both requests' needs.
///
/// * `force`: either side (sticky).
/// * `path_refresh` (path-only): only if **both** sides are path-only and neither forces; a
///   full `index_file` (no payload) merged with a path refresh becomes a full job flagged
///   `renamed`, so neither the content change nor the new path is lost.
/// * other keys: the newer request wins (the pre-CO-1 behaviour, e.g. a scan's `trigger`).
///
/// Both arguments are payloads of **live** requests for the same dedupe key; `None` = a plain
/// job with no flags (so `merge(None, path_refresh)` is a full job flagged `renamed`). Non-object
/// payloads keep the old rule (newer wins, else the older one).
pub fn merge_payload(old: Option<&Value>, new: Option<&Value>) -> Option<Value> {
    fn obj<'a>(
        v: Option<&'a Value>,
        empty: &'a serde_json::Map<String, Value>,
    ) -> Option<&'a serde_json::Map<String, Value>> {
        match v {
            None => Some(empty),
            Some(Value::Object(m)) => Some(m),
            Some(_) => None,
        }
    }
    let empty = serde_json::Map::new();
    let (Some(o), Some(n)) = (obj(old, &empty), obj(new, &empty)) else {
        return new.or(old).cloned();
    };
    let either = |k| flag_of(o, k) || flag_of(n, k);
    let force = either(PAYLOAD_FORCE);
    let path_only = flag_of(o, PAYLOAD_PATH_REFRESH) && flag_of(n, PAYLOAD_PATH_REFRESH) && !force;
    let renamed = !path_only && (either(PAYLOAD_PATH_REFRESH) || either(PAYLOAD_RENAMED));
    let mut m = o.clone();
    m.extend(n.iter().map(|(k, v)| (k.clone(), v.clone())));
    for k in [PAYLOAD_FORCE, PAYLOAD_PATH_REFRESH, PAYLOAD_RENAMED] {
        m.remove(k);
    }
    if force {
        m.insert(PAYLOAD_FORCE.into(), Value::Bool(true));
    }
    if path_only {
        m.insert(PAYLOAD_PATH_REFRESH.into(), Value::Bool(true));
    }
    if renamed {
        m.insert(PAYLOAD_RENAMED.into(), Value::Bool(true));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// Canonical dedupe key: one live job per (kind, target).
pub fn dedupe_key(kind: JobKind, folder_id: Option<FolderId>, file_id: Option<FileId>) -> String {
    match (file_id, folder_id) {
        (Some(f), _) => format!("{}:file:{f}", kind.as_str()),
        (None, Some(f)) => format!("{}:folder:{f}", kind.as_str()),
        (None, None) => format!("{}:global", kind.as_str()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    pub kind: JobKind,
    pub folder_id: Option<FolderId>,
    pub file_id: Option<FileId>,
    pub priority: Priority,
    pub dedupe_key: String,
    pub payload: Option<Value>,
    /// Earliest run time (ms); `None` = now.
    pub run_at: Option<i64>,
}

impl NewJob {
    /// Job with the canonical dedupe key.
    pub fn new(
        kind: JobKind,
        folder_id: Option<FolderId>,
        file_id: Option<FileId>,
        priority: Priority,
    ) -> Self {
        Self {
            kind,
            folder_id,
            file_id,
            priority,
            dedupe_key: dedupe_key(kind, folder_id, file_id),
            payload: None,
            run_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub id: JobId,
    /// Merged into an existing live job.
    pub coalesced: bool,
}

/// Enqueue with ON CONFLICT coalescing (see module docs).
pub fn enqueue(conn: &Connection, j: &NewJob, now: i64) -> Result<Enqueued> {
    if j.priority > 3 {
        return Err(Error::new(ErrorCode::InvalidInput).with_detail("priority must be 0..=3"));
    }
    // The catalog has one writer connection, so read-merge-write here cannot race another enqueue.
    let live: Option<Option<String>> = conn
        .prepare_cached(
            "SELECT payload FROM jobs WHERE dedupe_key = ?1 AND state IN ('queued','leased')",
        )
        .db()?
        .query_row([&j.dedupe_key], |r| r.get(0))
        .optional()
        .db()?;
    let existed = live.is_some();
    let payload = match live {
        None => j.payload.clone(),
        Some(old) => {
            let old: Option<Value> = old.and_then(|s| serde_json::from_str(&s).ok());
            merge_payload(old.as_ref(), j.payload.as_ref())
        }
    };
    let run_at = j.run_at.unwrap_or(now);
    let id: JobId = conn
        .prepare_cached(
            "INSERT INTO jobs(kind, folder_id, file_id, priority, dedupe_key, payload, state, next_run_at,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, ?8)
             ON CONFLICT(dedupe_key) WHERE state IN ('queued','leased') DO UPDATE SET
                 priority    = min(priority, excluded.priority),
                 next_run_at = CASE WHEN state = 'queued' THEN min(next_run_at, excluded.next_run_at)
                                    ELSE next_run_at END,
                 payload     = excluded.payload,
                 rerun       = CASE WHEN state = 'leased' THEN 1 ELSE rerun END,
                 updated_at  = excluded.updated_at
             RETURNING id",
        )
        .db()?
        .query_row(
            params![
                j.kind,
                j.folder_id.map(|f| f.to_bytes()),
                j.file_id.map(|f| f.to_bytes()),
                j.priority,
                j.dedupe_key,
                payload.as_ref().map(Value::to_string),
                run_at,
                now
            ],
            |r| r.get(0),
        )
        .db()?;
    Ok(Enqueued {
        id,
        coalesced: existed,
    })
}

/// Makes sure a live job exists for `j.dedupe_key`: enqueues `j` if there is none, otherwise
/// leaves the live job **untouched** (no payload merge, no `rerun`).
///
/// For callers that only repair a possibly lost job (sr-sync: a file still `pending_index` with
/// nothing changed). [`enqueue`] would flag a *running* job for a redundant second run.
pub fn ensure(conn: &Connection, j: &NewJob, now: i64) -> Result<Enqueued> {
    let live: Option<JobId> = conn
        .prepare_cached(
            "SELECT id FROM jobs WHERE dedupe_key = ?1 AND state IN ('queued','leased')",
        )
        .db()?
        .query_row([&j.dedupe_key], |r| r.get(0))
        .optional()
        .db()?;
    match live {
        Some(id) => Ok(Enqueued {
            id,
            coalesced: true,
        }),
        None => enqueue(conn, j, now),
    }
}

/// A leased job.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub folder_id: Option<FolderId>,
    pub file_id: Option<FileId>,
    pub priority: Priority,
    pub dedupe_key: String,
    pub payload: Option<Value>,
    pub state: JobState,
    /// Claims so far, including this one.
    pub attempts: u32,
    pub lease_owner: Option<Epoch>,
    pub next_run_at: i64,
    pub created_at: i64,
    pub last_error: Option<String>,
    pub error_code: Option<String>,
}

const JOB_COLS: &str =
    "id, kind, folder_id, file_id, priority, dedupe_key, payload, state, attempts,
     lease_owner, next_run_at, created_at, last_error, error_code";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        kind: r.get(1)?,
        folder_id: r.opt_folder_id(2)?,
        file_id: r.opt_file_id(3)?,
        priority: r.get(4)?,
        dedupe_key: r.get(5)?,
        payload: r
            .get::<_, Option<String>>(6)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        state: r.get(7)?,
        attempts: r.get(8)?,
        lease_owner: r.get(9)?,
        next_run_at: r.get(10)?,
        created_at: r.get(11)?,
        last_error: r.get(12)?,
        error_code: r.get(13)?,
    })
}

pub fn job(conn: &Connection, id: JobId) -> Result<Job> {
    conn.prepare_cached(&format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1"))
        .db()?
        .query_row([id], job_from_row)
        .db()
}

/// Live (queued or leased) jobs, optionally for one folder.
pub fn live_jobs(conn: &Connection, folder_id: Option<FolderId>) -> Result<Vec<Job>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {JOB_COLS} FROM jobs WHERE state IN ('queued','leased')
             AND (?1 IS NULL OR folder_id = ?1) ORDER BY id"
        ))
        .db()?;
    st.query_map([folder_id.map(|f| f.to_bytes())], job_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Epoch fencing at daemon start: every lease not held by `epoch` returns to `queued`.
pub fn requeue_stale_leases(conn: &Connection, epoch: Epoch, now: i64) -> Result<usize> {
    conn.execute(
        "UPDATE jobs SET state = 'queued', lease_owner = NULL, heartbeat_at = NULL, updated_at = ?2
         WHERE state = 'leased' AND (lease_owner IS NULL OR lease_owner <> ?1)",
        params![epoch, now],
    )
    .db()
}

// ------------------------------------------------------------------------------------- DRR

/// Fairness key: a folder, or "no folder" (global jobs).
pub type FairKey = Option<FolderId>;

/// One claimable candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub id: JobId,
    pub folder: FairKey,
    /// Effective priority after aging (0 = most urgent).
    pub priority: Priority,
    pub cost: u32,
}

/// Deficit round-robin across folders. Keeps deficits and the rotation point across claims, so
/// a folder with 100k queued files cannot starve one with 10 (efficiency §2.7, "fair").
#[derive(Debug, Clone)]
pub struct Drr {
    quantum: u32,
    deficit: HashMap<FairKey, u64>,
    /// Folder served last; the next round starts after it.
    last: Option<FairKey>,
}

impl Default for Drr {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Drr {
    pub fn new(quantum: u32) -> Self {
        Self {
            quantum: quantum.max(1),
            deficit: HashMap::new(),
            last: None,
        }
    }

    /// Picks up to `permits` candidates: strictly by effective priority level, and within one
    /// level by DRR across folders. Candidates must be ordered by (priority, run time) per folder.
    pub fn pick(&mut self, candidates: &[Candidate], permits: usize) -> Vec<JobId> {
        let mut out = Vec::with_capacity(permits.min(candidates.len()));
        let mut levels: Vec<Priority> = candidates.iter().map(|c| c.priority).collect();
        levels.sort_unstable();
        levels.dedup();
        for level in levels {
            if out.len() >= permits {
                break;
            }
            // Per-folder FIFO queues at this level, in a stable folder order.
            let mut order: Vec<FairKey> = Vec::new();
            let mut queues: HashMap<FairKey, VecDeque<Candidate>> = HashMap::new();
            for c in candidates.iter().filter(|c| c.priority == level) {
                queues
                    .entry(c.folder)
                    .or_insert_with(|| {
                        order.push(c.folder);
                        VecDeque::new()
                    })
                    .push_back(*c);
            }
            order.sort();
            // Rotate so the round starts after the folder served last.
            if let Some(last) = self.last
                && let Some(pos) = order.iter().position(|k| *k > last)
            {
                order.rotate_left(pos);
            }
            let mut active: VecDeque<FairKey> = order.into_iter().collect();
            while out.len() < permits {
                let Some(key) = active.pop_front() else { break };
                let q = queues.entry(key).or_default();
                let d = self.deficit.entry(key).or_insert(0);
                *d += u64::from(self.quantum);
                while out.len() < permits {
                    match q.front() {
                        Some(c) if u64::from(c.cost) <= *d => {
                            *d -= u64::from(c.cost);
                            out.push(c.id);
                            q.pop_front();
                        }
                        _ => break,
                    }
                }
                self.last = Some(key);
                if q.is_empty() {
                    self.deficit.remove(&key);
                } else {
                    active.push_back(key);
                }
            }
        }
        out
    }
}

/// Cost of a job for DRR (all equal for now; weights go here when measured).
pub const fn job_cost(kind: JobKind) -> u32 {
    match kind {
        JobKind::ScanFolder
        | JobKind::IndexFile
        | JobKind::DeleteFile
        | JobKind::RebuildProfile
        | JobKind::PurgeFolder
        | JobKind::Retention => 1,
    }
}

/// Claim parameters.
#[derive(Debug, Clone)]
pub struct ClaimRequest<'a> {
    pub epoch: Epoch,
    /// Downstream capacity (backpressure): at most this many jobs are leased.
    pub permits: usize,
    pub now: i64,
    /// Only these kinds (`None` = any).
    pub kinds: Option<&'a [JobKind]>,
    /// A queued job gains one priority level per `aging_ms` waited (0 = no aging).
    pub aging_ms: i64,
}

impl ClaimRequest<'_> {
    pub fn new(epoch: Epoch, permits: usize, now: i64) -> Self {
        Self {
            epoch,
            permits,
            now,
            kinds: None,
            aging_ms: DEFAULT_AGING_MS,
        }
    }
}

/// Default aging: one priority level per 10 minutes waited.
pub const DEFAULT_AGING_MS: i64 = 10 * 60 * 1000;

fn effective_priority(j: &Job, now: i64, aging_ms: i64) -> Priority {
    if aging_ms <= 0 {
        return j.priority;
    }
    let steps = ((now - j.created_at).max(0) / aging_ms).min(3);
    let steps = u8::try_from(steps).unwrap_or(3);
    j.priority.saturating_sub(steps)
}

/// Jobs the claim holds (see module docs): any job of a paused folder, pipeline jobs of an
/// unavailable one.
const HELD_FILTER: &str = "AND NOT EXISTS (SELECT 1 FROM folders fo WHERE fo.id = jobs.folder_id
     AND (fo.paused = 1 OR (fo.available = 0 AND jobs.kind <> 'scan_folder')))";

/// Leases up to `permits` runnable jobs for `epoch` (priority + aging + DRR across folders).
/// Jobs of paused folders (and pipeline jobs of unavailable ones) are held, not claimed.
pub fn claim(conn: &Connection, req: &ClaimRequest<'_>, drr: &mut Drr) -> Result<Vec<Job>> {
    if req.permits == 0 {
        return Ok(Vec::new());
    }
    atomic(conn, |c| {
        let kind_filter = match req.kinds {
            Some(ks) if !ks.is_empty() => format!(
                " AND kind IN ({})",
                ks.iter()
                    .map(|k| format!("'{}'", k.as_str()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Some(_) => return Ok(Vec::new()),
            None => String::new(),
        };
        let k = i64::try_from(req.permits).unwrap_or(i64::MAX);
        let mut cands: HashMap<JobId, Job> = HashMap::new();
        // Head of every folder's queue (index ix_jobs_folder), so no folder is invisible to DRR.
        let mut keys: Vec<Option<[u8; 16]>> = c
            .prepare_cached("SELECT id FROM folders WHERE paused = 0")
            .db()?
            .query_map([], |r| r.get::<_, [u8; 16]>(0).map(Some))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        keys.push(None);
        {
            let mut st = c
                .prepare_cached(&format!(
                    "SELECT {JOB_COLS} FROM jobs WHERE folder_id IS ?1 AND state = 'queued' AND next_run_at <= ?2
                     {kind_filter} {HELD_FILTER} ORDER BY priority, next_run_at, id LIMIT ?3"
                ))
                .db()?;
            for key in keys {
                for j in st.query_map(params![key, req.now, k], job_from_row).db()? {
                    let j = j.db()?;
                    cands.insert(j.id, j);
                }
            }
        }
        // Oldest waiting jobs regardless of folder, so aging can lift them (index ix_jobs_age).
        if req.aging_ms > 0 {
            let mut st = c
                .prepare_cached(&format!(
                    "SELECT {JOB_COLS} FROM jobs WHERE state = 'queued' AND next_run_at <= ?1 AND priority > 0
                     AND created_at <= ?2 {kind_filter} {HELD_FILTER} ORDER BY created_at, id LIMIT ?3"
                ))
                .db()?;
            for j in st
                .query_map(params![req.now, req.now - req.aging_ms, k], job_from_row)
                .db()?
            {
                let j = j.db()?;
                cands.insert(j.id, j);
            }
        }
        let mut list: Vec<&Job> = cands.values().collect();
        list.sort_by_key(|j| {
            (
                effective_priority(j, req.now, req.aging_ms),
                j.next_run_at,
                j.id,
            )
        });
        let picks: Vec<Candidate> = list
            .iter()
            .map(|j| Candidate {
                id: j.id,
                folder: j.folder_id,
                priority: effective_priority(j, req.now, req.aging_ms),
                cost: job_cost(j.kind),
            })
            .collect();
        let chosen = drr.pick(&picks, req.permits);
        let mut lease = c
            .prepare_cached(
                "UPDATE jobs SET state = 'leased', lease_owner = ?2, heartbeat_at = ?3, attempts = attempts + 1,
                     rerun = 0, updated_at = ?3 WHERE id = ?1 AND state = 'queued'",
            )
            .db()?;
        let mut out = Vec::with_capacity(chosen.len());
        for id in chosen {
            if lease.execute(params![id, req.epoch, req.now]).db()? == 1 {
                out.push(job(c, id)?);
            }
        }
        Ok(out)
    })
}

/// Informational heartbeat (epoch fencing is the correctness mechanism).
pub fn heartbeat(conn: &Connection, ids: &[JobId], epoch: Epoch, now: i64) -> Result<()> {
    let mut st = conn
        .prepare_cached(
            "UPDATE jobs SET heartbeat_at = ?3 WHERE id = ?1 AND state = 'leased' AND lease_owner = ?2",
        )
        .db()?;
    for id in ids {
        st.execute(params![id, epoch, now]).db()?;
    }
    Ok(())
}

fn stale_lease(id: JobId) -> Error {
    Error::new(ErrorCode::Conflict).with_detail(format!("job {id} is not leased by this epoch"))
}

fn to_history(conn: &Connection, id: JobId, state: JobState, now: i64) -> Result<()> {
    to_history_as(conn, id, state.as_str(), now)
}

/// `job_history.state` of jobs dropped because the user cancelled their folder's run.
pub const HISTORY_CANCELLED: &str = "cancelled";

fn to_history_as(conn: &Connection, id: JobId, state: &str, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO job_history(id, kind, folder_id, file_id, priority, dedupe_key, payload, state, attempts,
             lease_owner, last_error, error_code, created_at, finished_at)
         SELECT id, kind, folder_id, file_id, priority, dedupe_key, payload, ?2, attempts, lease_owner,
             last_error, error_code, created_at, ?3 FROM jobs WHERE id = ?1
         ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, folder_id = excluded.folder_id,
             file_id = excluded.file_id, priority = excluded.priority,
             dedupe_key = excluded.dedupe_key, payload = excluded.payload,
             state = excluded.state, attempts = excluded.attempts,
             lease_owner = excluded.lease_owner, last_error = excluded.last_error,
             error_code = excluded.error_code, created_at = excluded.created_at,
             finished_at = excluded.finished_at",
    )
    .db()?
    .execute(params![id, state, now])
    .db()?;
    conn.prepare_cached("DELETE FROM jobs WHERE id = ?1")
        .db()?
        .execute([id])
        .db()?;
    Ok(())
}

/// Completion outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completed {
    /// Moved to `job_history`.
    Done,
    /// A coalesced change arrived while it ran: queued again.
    Requeued,
}

/// Completes a job held by `epoch` (fenced).
pub fn complete(conn: &Connection, id: JobId, epoch: Epoch, now: i64) -> Result<Completed> {
    atomic(conn, |c| {
        let rerun: Option<bool> = c
            .prepare_cached(
                "SELECT rerun FROM jobs WHERE id = ?1 AND state = 'leased' AND lease_owner = ?2",
            )
            .db()?
            .query_row(params![id, epoch], |r| r.get(0))
            .optional()
            .db()?;
        match rerun {
            None => Err(stale_lease(id)),
            Some(true) => {
                c.execute(
                    "UPDATE jobs SET state = 'queued', lease_owner = NULL, rerun = 0, attempts = 0,
                         next_run_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )
                .db()?;
                Ok(Completed::Requeued)
            }
            Some(false) => {
                to_history(c, id, JobState::Done, now)?;
                Ok(Completed::Done)
            }
        }
    })
}

/// Returns a job leased by `epoch` to `queued` **without consuming the attempt** of its claim
/// (fenced). For work the service stopped for a reason that is not the job's fault (its folder
/// was paused). `run_at` = earliest next run.
pub fn release(conn: &Connection, id: JobId, epoch: Epoch, run_at: i64, now: i64) -> Result<()> {
    let n = conn
        .prepare_cached(
            "UPDATE jobs SET state = 'queued', lease_owner = NULL, heartbeat_at = NULL,
                 attempts = max(attempts - 1, 0), rerun = 0, next_run_at = ?3, updated_at = ?4
             WHERE id = ?1 AND state = 'leased' AND lease_owner = ?2",
        )
        .db()?
        .execute(params![id, epoch, run_at, now])
        .db()?;
    if n == 1 { Ok(()) } else { Err(stale_lease(id)) }
}

/// Kinds a folder cancel drops (its run: scan and file work). Purge, retention and rebuild jobs
/// are never dropped.
pub const CANCELLABLE_KINDS: &[JobKind] =
    &[JobKind::ScanFolder, JobKind::IndexFile, JobKind::DeleteFile];

/// What a cancel dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cancelled {
    pub jobs: usize,
    /// Files whose stored mtime was forgotten so the next scan re-checks them.
    pub files_invalidated: usize,
}

fn drop_cancelled(c: &Connection, j: &Job, now: i64, out: &mut Cancelled) -> Result<()> {
    if j.kind == JobKind::IndexFile
        && let Some(file) = j.file_id
    {
        // `observe` already stored the new size/mtime of a changed file; forget it so the next
        // scan hashes the file instead of trusting metadata that was never indexed.
        let pending_no_hash: Option<bool> = c
            .prepare_cached(
                "SELECT state = 'pending_index' AND content_hash IS NULL FROM files WHERE id = ?1",
            )
            .db()?
            .query_row([file.to_bytes()], |r| r.get(0))
            .optional()
            .db()?;
        if pending_no_hash == Some(false) {
            files::invalidate_metadata(c, file, now)?;
            out.files_invalidated += 1;
        }
    }
    to_history_as(c, j.id, HISTORY_CANCELLED, now)?;
    out.jobs += 1;
    Ok(())
}

/// User cancel of a folder's run: every **queued** scan/index/delete job of the folder goes to
/// history as `cancelled` (see [`CANCELLABLE_KINDS`]). Leased ones are stopped by the service and
/// dropped with [`cancel_leased`]. Files are never touched beyond [`files::invalidate_metadata`].
pub fn cancel_queued(conn: &Connection, folder_id: FolderId, now: i64) -> Result<Cancelled> {
    atomic(conn, |c| {
        let list: Vec<Job> = c
            .prepare_cached(&format!(
                "SELECT {JOB_COLS} FROM jobs WHERE folder_id = ?1 AND state = 'queued'
                 AND kind IN ('scan_folder','index_file','delete_file')"
            ))
            .db()?
            .query_map([folder_id.to_bytes()], job_from_row)
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        let mut out = Cancelled::default();
        for j in &list {
            drop_cancelled(c, j, now, &mut out)?;
        }
        Ok(out)
    })
}

/// Drops a job leased by `epoch` whose folder run was cancelled (fenced; no attempt is recorded
/// as a failure). A change coalesced into it while it ran is dropped with it (cancel = stop).
pub fn cancel_leased(conn: &Connection, id: JobId, epoch: Epoch, now: i64) -> Result<Cancelled> {
    atomic(conn, |c| {
        let j = job(c, id).map_err(|_| stale_lease(id))?;
        if j.state != JobState::Leased || j.lease_owner != Some(epoch) {
            return Err(stale_lease(id));
        }
        let mut out = Cancelled::default();
        drop_cancelled(c, &j, now, &mut out)?;
        Ok(out)
    })
}

/// Backoff and attempt limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_ms: i64,
    pub max_ms: i64,
}

impl Default for RetryPolicy {
    /// 5 attempts: 2 s, 4 s, 8 s, 16 s, then dead-letter (AV locks usually clear quickly, U20).
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_ms: 2_000,
            max_ms: 15 * 60 * 1000,
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt `attempts + 1`: `base · 2^(attempts−1)`, capped.
    pub fn backoff_ms(&self, attempts: u32) -> i64 {
        let shift = attempts.saturating_sub(1).min(30);
        self.base_ms.saturating_mul(1_i64 << shift).min(self.max_ms)
    }
}

/// Why a job failed.
#[derive(Debug, Clone, PartialEq)]
pub struct JobFailure {
    pub code: ErrorCode,
    /// Internal text for Details/diagnostics (never shown as the message).
    pub message: String,
    pub retryable: bool,
    /// Honour `Retry-After` etc.
    pub retry_after_ms: Option<i64>,
}

impl JobFailure {
    pub fn from_error(e: &Error) -> Self {
        Self {
            code: e.code,
            message: e.to_string(),
            retryable: e.retryable,
            retry_after_ms: e
                .retry_after
                .and_then(|d| i64::try_from(d.as_millis()).ok()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// Back to `queued`, runnable at `next_run_at`.
    Retrying { next_run_at: i64 },
    /// Terminal failure of a folder-level job (folder shows `error`).
    Failed,
    /// Dead-letter (file jobs mark the file `dead`).
    Dead,
}

/// Fails a job held by `epoch` (fenced). See module docs for the state rules.
pub fn fail(
    conn: &Connection,
    id: JobId,
    epoch: Epoch,
    f: &JobFailure,
    policy: &RetryPolicy,
    now: i64,
) -> Result<FailOutcome> {
    atomic(conn, |c| {
        let j = job(c, id).map_err(|_| stale_lease(id))?;
        if j.state != JobState::Leased || j.lease_owner != Some(epoch) {
            return Err(stale_lease(id));
        }
        if f.retryable && j.attempts < policy.max_attempts {
            let next = now
                + f.retry_after_ms
                    .unwrap_or_else(|| policy.backoff_ms(j.attempts));
            c.execute(
                "UPDATE jobs SET state = 'queued', lease_owner = NULL, next_run_at = ?2, last_error = ?3,
                     error_code = ?4, rerun = 0, updated_at = ?5 WHERE id = ?1",
                params![id, next, f.message, f.code.as_str(), now],
            )
            .db()?;
            return Ok(FailOutcome::Retrying { next_run_at: next });
        }
        let state = if j.file_id.is_some() || f.retryable {
            JobState::Dead
        } else {
            JobState::Failed
        };
        c.execute(
            "UPDATE jobs SET state = ?2, lease_owner = NULL, last_error = ?3, error_code = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, state, f.message, f.code.as_str(), now],
        )
        .db()?;
        if let (JobState::Dead, Some(file)) = (state, j.file_id) {
            match files::mark_dead(c, file, f.code, now) {
                Ok(()) => {}
                Err(e) if e.code == ErrorCode::NotFound => {} // file deleted meanwhile
                Err(e) => return Err(e),
            }
        }
        Ok(if state == JobState::Dead {
            FailOutcome::Dead
        } else {
            FailOutcome::Failed
        })
    })
}

/// Clears terminal (`failed`/`dead`) jobs of a folder into history, e.g. on "Retry" or a rescan.
pub fn clear_terminal(conn: &Connection, folder_id: FolderId, now: i64) -> Result<usize> {
    atomic(conn, |c| {
        let ids: Vec<JobId> = c
            .prepare("SELECT id FROM jobs WHERE folder_id = ?1 AND state IN ('failed','dead')")
            .db()?
            .query_map([folder_id.to_bytes()], |r| r.get(0))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        for id in &ids {
            let st: JobState = c
                .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .db()?;
            to_history(c, *id, st, now)?;
        }
        Ok(ids.len())
    })
}

/// Drops history older than `keep_ms` (7 days by default, §0.A M4).
pub fn prune_history(conn: &Connection, now: i64, keep_ms: i64) -> Result<usize> {
    conn.execute(
        "DELETE FROM job_history WHERE finished_at < ?1",
        [now - keep_ms],
    )
    .db()
}

pub const HISTORY_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// (`queued`, `leased`) counts for the `QueueDepth` event.
pub fn queue_depth(conn: &Connection) -> Result<(u64, u64)> {
    conn.query_row(
        "SELECT coalesce(sum(state = 'queued'), 0), coalesce(sum(state = 'leased'), 0) FROM jobs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .db()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: JobId, folder: u8, priority: Priority) -> Candidate {
        let mut b = [0u8; 16];
        b[15] = folder;
        Candidate {
            id,
            folder: Some(FolderId::from_bytes(b)),
            priority,
            cost: 1,
        }
    }

    #[test]
    fn drr_shares_permits_across_folders() {
        // Folder 1 has 1000 queued jobs, folder 2 has 10.
        let mut c: Vec<Candidate> = (0..1000).map(|i| cand(i, 1, 2)).collect();
        c.extend((1000..1010).map(|i| cand(i, 2, 2)));
        let mut drr = Drr::default();
        let got = drr.pick(&c, 10);
        let from2 = got.iter().filter(|id| **id >= 1000).count();
        assert_eq!(got.len(), 10);
        assert_eq!(from2, 5, "{got:?}");
    }

    #[test]
    fn drr_priority_levels_are_strict() {
        let c = vec![cand(1, 1, 3), cand(2, 2, 0), cand(3, 1, 1), cand(4, 2, 3)];
        let mut drr = Drr::default();
        assert_eq!(drr.pick(&c, 2), vec![2, 3]);
    }

    #[test]
    fn drr_rotates_across_calls() {
        let c: Vec<Candidate> = (0..6)
            .map(|i| cand(i, u8::try_from(i % 3).unwrap_or(0), 1))
            .collect();
        let mut drr = Drr::default();
        let a = drr.pick(&c, 1);
        let rest: Vec<Candidate> = c.iter().copied().filter(|x| !a.contains(&x.id)).collect();
        let b = drr.pick(&rest, 1);
        let fa = c.iter().find(|x| x.id == a[0]).map(|x| x.folder);
        let fb = c.iter().find(|x| x.id == b[0]).map(|x| x.folder);
        assert_ne!(fa, fb, "next claim starts at the next folder");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff_ms(1), 2_000);
        assert_eq!(p.backoff_ms(2), 4_000);
        assert_eq!(p.backoff_ms(4), 16_000);
        assert_eq!(p.backoff_ms(40), p.max_ms);
    }
}
