//! Reconciler: one scan of a folder → catalog observations, change detection, jobs, sweep.
//!
//! # Tiered change detection
//! 1. **New path** → identity (`FILE_ID_INFO`); the catalog matches it against unseen rows of the
//!    folder: same identity = **rename** (path updated, `file_id` and chunk IDs kept). A pure
//!    rename enqueues `index_file {"path_refresh": true}`: the engine re-applies the file at its
//!    current version so the lexical `path` field shows the new name, without re-embedding. If
//!    the content metadata also changed, the job is a normal `index_file` flagged `renamed` (the
//!    path is refreshed even if the content turns out unchanged). Otherwise a new file →
//!    `index_file`.
//! 2. **Known path**, size and mtime equal and not racy → nothing (no read).
//! 3. Size or mtime differ, or the file is **racy**, or the scan is a deep verify → blake3 of the
//!    bytes; re-index only if the hash differs from the catalog's `content_hash`. Racy (racy-git
//!    rule): `mtime ≥ min(scan_start, last_full_scan_at) − 2 s`, i.e. modified within the
//!    timestamp window of when the catalog recorded its size/mtime, or while this scan runs.
//! 4. `reindex_policy = always` → every included file is re-indexed (`payload.force = true`).
//!
//! Jobs are coalesced by the catalog's `dedupe_key` (one live `index_file` per file), so a burst
//! of watcher events for one file leaves a single job; `jobs::merge_payload` keeps the needs of
//! both requests (a path refresh never swallows a content change).
//!
//! # Policy skips (no job)
//! Access grants are listed only: files are marked `skipped` with reason `files_only`, which also
//! deletes any chunks and writes the outbox `Delete` op. Files over `max_file_bytes` →
//! `too_large` (`FileTooLarge`); online-only placeholders → `online_only` (`FileOnlineOnly`) unless
//! `include_online_only`. These are re-evaluated on every scan, so a listed file left by the
//! removed Files-only mode (Amendment 17) is read by the first scan after the user resumes its
//! folder.
//!
//! # Safety rules
//! * The root is re-validated through the sandbox at the start of every scan (dangerous roots,
//!   denylist, existence). If its canonical form no longer equals the stored root (a junction or
//!   folder along the path was retargeted) or the volume serial changed (another drive mounted on
//!   the same letter), the folder becomes `unavailable` and **nothing is deleted**.
//! * Deletion is mark-and-sweep, only below directories listed completely by this scan (W4).
//! * A cancelled scan never sweeps.
//!
//! # Watcher events (M2)
//! [`Reconciler::on_path_events`] applies a debounced batch for one folder with **one** folder read,
//! a **cached** root validation ([`ROOT_CACHE_TTL`]; scans always re-validate) and batched catalog
//! writes (512 files per transaction). Renames are applied first, in order; every other path is
//! "touched" and its current state on disk decides (exists → change detection, gone → delete, a
//! directory → a scan only if it brings files the catalog doesn't know). A removal is never applied
//! blindly: the root is re-validated (uncached) first, the `delete_file` job waits
//! [`DELETE_GRACE_MS`], and the job re-checks the root and the file when it runs, so deleting or
//! unplugging the root makes the folder `unavailable` instead of emptying it. A batch that edits an
//! ignore file, or adds or removes a `Cargo.toml` the cached rules looked at, asks for one scan
//! instead: the folder's rules changed.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sr_catalog::model::{PRIORITY_BACKGROUND, PRIORITY_INTERACTIVE, PRIORITY_LIVE};
use sr_catalog::repo::files::{self, FileObservation, FileRecord, Observed};
use sr_catalog::repo::folders::{self, FolderRecord, NewFolder};
use sr_catalog::repo::jobs::{self, Enqueued};
use sr_catalog::repo::status;
use sr_catalog::{
    Catalog, FileState, FolderKind, JobKind, NewJob, Priority, RunTrigger, ScanId, WatchSupport,
};
use sr_common::config::FolderSettings;
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::events::{EngineEvent, FolderStatus};
use sr_common::ids::{CollectionId, ContentHash, FileId, FolderId};
use sr_common::sandbox::{DangerousRoots, Sandbox, SandboxError, parse_relative, path_key};
use tokio_util::sync::CancellationToken;

use crate::events::{EventSink, Throttle};
use crate::filter::{Excluded, PathFilter, extension_of};
use crate::fsid;
use crate::policy::{FolderBehavior, ScanDepth};
use crate::rules::{FolderRules, cargo_manifest_dir, is_ignore_file};
use crate::walker::{self, WalkSpec, WalkedFile};

/// The watcher's view of a folder's rules: the filter plus its ignore files, as in the walk.
struct Checks<'a> {
    filter: &'a PathFilter,
    rules: &'a FolderRules,
}

impl Checks<'_> {
    fn dir(&self, rel: &str) -> std::result::Result<(), Excluded> {
        self.filter.check_dir_with(rel, Some(self.rules))?;
        if self.rules.ignored(rel, true) {
            return Err(Excluded::Ignored);
        }
        Ok(())
    }

    fn file(&self, rel: &str) -> std::result::Result<(), Excluded> {
        self.filter.check_file_with(rel, Some(self.rules))?;
        if self.rules.ignored(rel, false) {
            return Err(Excluded::Ignored);
        }
        Ok(())
    }
}

/// Racy-git window: files modified this close to (or after) the time their metadata was
/// recorded, or the scan start, are re-hashed.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

/// Skip reasons written by the reconciler (re-evaluated on every scan). `SKIP_FILES_ONLY`: listed,
/// not read (access grants).
pub const SKIP_FILES_ONLY: &str = sr_catalog::SKIP_REASON_FILES_ONLY;
pub const SKIP_TOO_LARGE: &str = "too_large";
pub const SKIP_ONLINE_ONLY: &str = "online_only";
/// Written by the ingest step: a `.json` / `.geojson` data dump (docs/06 change 5,
/// [`crate::filter::is_data_dump`]). Checked again by a `settings_change` scan only.
pub const SKIP_DATA_FILE: &str = "data_file";
const POLICY_SKIPS: [&str; 3] = [SKIP_FILES_ONLY, SKIP_TOO_LARGE, SKIP_ONLINE_ONLY];

/// `stats_json` key of the last smart deep verify (ms).
pub const STAT_SMART_VERIFY_AT: &str = "smart_verify_at";

/// blake3 of a file's bytes (`ContentHash`), streamed.
pub fn hash_file(path: &Path) -> std::io::Result<ContentHash> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(ContentHash::from_bytes(*h.finalize().as_bytes()))
}

/// Whether a root is a network share (UNC); those use a scheduled reconcile, not a watcher.
/// Mapped network drive letters are recognised by [`fsid::drive_kind`] when the root is validated
/// and recorded as `folders.watch_support = network`.
pub fn is_network_share(root: &Path) -> bool {
    fsid::is_unc(root)
}

/// Behaviour of a folder record (kind-aware: access grants are never embedded). A folder the
/// watcher can't serve (network drive, or a file system where the watcher failed) is
/// reconcile-only, exactly like an SMB share.
pub fn behavior_of(f: &FolderRecord) -> FolderBehavior {
    let no_watch = is_network_share(&f.root_path) || f.watch_support != WatchSupport::Native;
    let mut b = FolderBehavior::resolve(&f.settings, no_watch);
    if f.kind != FolderKind::Indexed {
        b.embed = false;
    }
    b
}

/// Result of a root re-validation.
#[derive(Debug)]
pub enum RootCheck {
    Ok {
        sandbox: Arc<Sandbox>,
        canonical: PathBuf,
    },
    Unavailable(ErrorCode),
}

/// What one scan did (stored in `folders.stats_json`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files_seen: u64,
    pub new: u64,
    pub renamed: u64,
    pub changed: u64,
    pub unchanged: u64,
    pub hashed: u64,
    pub enqueued: u64,
    pub coalesced: u64,
    /// Pure renames that got an `index_file {"path_refresh": true}` job (also in
    /// `enqueued`/`coalesced`).
    pub path_refreshes: u64,
    pub skipped: u64,
    pub deleted: u64,
    pub kept_unseen: u64,
    pub excluded: u64,
    pub rejected: u64,
    pub errors: u64,
    pub partial: bool,
}

impl ScanStats {
    fn add(&mut self, o: &BatchCounts) {
        self.files_seen += o.seen;
        self.new += o.new;
        self.renamed += o.renamed;
        self.changed += o.changed;
        self.unchanged += o.unchanged;
        self.hashed += o.hashed;
        self.enqueued += o.enqueued;
        self.coalesced += o.coalesced;
        self.path_refreshes += o.path_refreshes;
        self.skipped += o.skipped;
    }

    fn to_json(&self) -> Value {
        json!({
            "files_seen": self.files_seen, "new": self.new, "renamed": self.renamed,
            "changed": self.changed, "unchanged": self.unchanged, "hashed": self.hashed,
            "enqueued": self.enqueued, "path_refreshes": self.path_refreshes,
            "skipped": self.skipped, "deleted": self.deleted,
            "kept_unseen": self.kept_unseen, "excluded": self.excluded, "rejected": self.rejected,
            "errors": self.errors, "partial": self.partial,
        })
    }
}

/// Outcome of [`Reconciler::scan_blocking`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    Completed(ScanStats),
    /// Root failed re-validation; the folder is now `unavailable` and nothing was deleted.
    Unavailable(ErrorCode),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BatchCounts {
    seen: u64,
    new: u64,
    renamed: u64,
    changed: u64,
    unchanged: u64,
    hashed: u64,
    enqueued: u64,
    coalesced: u64,
    path_refreshes: u64,
    skipped: u64,
}

/// What to do with one observed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Nothing,
    /// Pure rename: refresh the path tokens only (no embedding).
    PathRefresh,
    /// `renamed`: the path changed too (refreshed even if the content is unchanged).
    Index {
        force: bool,
        renamed: bool,
    },
    /// `pending_index` with nothing changed: enqueue only if no job is live (`jobs::ensure`).
    EnsureJob,
    Skip(&'static str, Option<ErrorCode>),
}

/// Per-file inputs prepared outside the writer (identity, hash).
#[derive(Debug)]
struct Prepared {
    obs: FileObservation,
    known: Option<FileRecord>,
    hash: Option<std::io::Result<ContentHash>>,
    too_large: bool,
    online_only: bool,
    racy: bool,
}

/// Per-scan context shared by every batch.
#[derive(Debug, Clone, Copy)]
struct BatchCtx {
    folder_id: FolderId,
    scan_id: ScanId,
    scan_start_ms: i64,
    /// Metadata recorded at or after this time can't be trusted at mtime granularity: the
    /// earlier of this scan's start and the last full scan (when the catalog recorded it).
    trusted_before_ms: i64,
    depth: ScanDepth,
    embed: bool,
    priority: Priority,
    max_file_bytes: u64,
    include_online_only: bool,
    /// A `settings_change` scan (settings, an ignore file or the built-in defaults changed):
    /// JSON files are handed to the ingest step again so the data-file rule is re-applied in both
    /// directions (docs/06 change 5). Unchanged files are only hashed there.
    recheck_data_files: bool,
}

fn decide(p: &Prepared, o: &Observed, ctx: &BatchCtx) -> Action {
    let same_skip = |reason: &str| {
        o.state == FileState::Skipped
            && p.known
                .as_ref()
                .and_then(|k| k.skip_reason.as_deref())
                .is_some_and(|r| r == reason)
    };
    if !ctx.embed {
        return if same_skip(SKIP_FILES_ONLY) {
            Action::Nothing
        } else {
            Action::Skip(SKIP_FILES_ONLY, None)
        };
    }
    if p.too_large {
        return if same_skip(SKIP_TOO_LARGE) {
            Action::Nothing
        } else {
            Action::Skip(SKIP_TOO_LARGE, Some(ErrorCode::FileTooLarge))
        };
    }
    if p.online_only {
        return if same_skip(SKIP_ONLINE_ONLY) {
            Action::Nothing
        } else {
            Action::Skip(SKIP_ONLINE_ONLY, Some(ErrorCode::FileOnlineOnly))
        };
    }
    let renamed = o.renamed_from.is_some();
    if o.new {
        return Action::Index {
            force: false,
            renamed: false,
        };
    }
    if ctx.depth == ScanDepth::ForceAll {
        return Action::Index {
            force: true,
            renamed,
        };
    }
    if o.state == FileState::PendingIndex && o.content_hash.is_none() {
        // Never got through T1 (job lost, queued or running): make sure a job exists. With a
        // hash, T1 already ran and only the store apply / T2 is outstanding (D3), so the file is
        // compared like an indexed one below instead of being re-indexed.
        if !renamed && !o.metadata_changed {
            // Nothing new to tell the job: don't flag a running one for a second run (CO-1).
            return Action::EnsureJob;
        }
        return Action::Index {
            force: false,
            renamed,
        };
    }
    let policy_skip = o.state == FileState::Skipped
        && p.known
            .as_ref()
            .and_then(|k| k.skip_reason.as_deref())
            .is_some_and(|r| POLICY_SKIPS.contains(&r));
    if policy_skip {
        return Action::Index {
            force: false,
            renamed,
        };
    }
    if ctx.recheck_data_files
        && crate::filter::DATA_FILE_EXTS.contains(&p.obs.ext.as_str())
        && (o.state == FileState::Indexed || same_skip(SKIP_DATA_FILE))
    {
        return Action::Index {
            force: false,
            renamed,
        };
    }
    if renamed && !o.metadata_changed && !p.racy {
        // The catalog row already moved (same file_id and chunk IDs); only the store-side path
        // tokens are stale (m1-e O3).
        return Action::PathRefresh;
    }
    let suspect = o.metadata_changed || p.racy || ctx.depth == ScanDepth::VerifyAll;
    if !suspect {
        return Action::Nothing;
    }
    match (&p.hash, &o.content_hash) {
        (Some(Ok(h)), Some(have)) if h == have && renamed => Action::PathRefresh,
        (Some(Ok(h)), Some(have)) if h == have => Action::Nothing,
        _ => Action::Index {
            force: false,
            renamed,
        },
    }
}

/// `index_file` payload for a content (re-)index.
fn index_payload(force: bool, renamed: bool) -> Option<Value> {
    let mut m = serde_json::Map::new();
    if force {
        m.insert(jobs::PAYLOAD_FORCE.into(), Value::Bool(true));
    }
    if renamed {
        m.insert(jobs::PAYLOAD_RENAMED.into(), Value::Bool(true));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

fn record_skip_reason(stats: &mut ScanStats, w: &walker::WalkStats) {
    stats.excluded = w.excluded_default + w.excluded_user + w.not_included + w.secret;
    stats.rejected = w.rejected + w.links_skipped;
    stats.errors = w.errors;
}

/// Priority of the jobs a run creates.
pub fn priority_for(trigger: RunTrigger) -> Priority {
    match trigger {
        RunTrigger::Manual | RunTrigger::Mcp | RunTrigger::Watcher => PRIORITY_LIVE,
        RunTrigger::Startup
        | RunTrigger::Schedule
        | RunTrigger::SettingsChange
        | RunTrigger::Reconnect => PRIORITY_BACKGROUND,
    }
}

/// Priority of the scan job itself for a trigger.
pub fn scan_priority(trigger: RunTrigger) -> Priority {
    match trigger {
        RunTrigger::Manual | RunTrigger::Mcp => PRIORITY_INTERACTIVE,
        _ => PRIORITY_LIVE,
    }
}

/// Watcher-style change of one path (M2's notify watcher calls this; also usable by tools).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathEvent {
    /// Created or modified (root-relative path, any separator).
    Changed(String),
    Removed(String),
    Renamed {
        from: String,
        to: String,
    },
    /// Watcher overflow or "something changed below here": rescan the folder.
    Rescan,
}

/// What [`Reconciler::on_path_event`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventOutcome {
    /// The folder's sync mode does not take watcher events, or it is paused/unavailable.
    Ignored,
    /// Refused or excluded by the sandbox/filter.
    Excluded(Excluded),
    /// A job was enqueued (`coalesced` = merged into an existing live job).
    Enqueued { kind: JobKind, coalesced: bool },
    /// Rename: identity and chunk IDs kept; an `index_file {"path_refresh": true}` job refreshes
    /// the lexical path tokens (no re-embedding).
    Renamed,
    /// Nothing to do (content unchanged, already gone, ...).
    Unchanged,
}

/// Totals of one [`Reconciler::on_path_events`] batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventBatch {
    /// Events in the batch.
    pub events: u64,
    /// Nothing done: the folder doesn't take watcher events, is paused or unavailable.
    pub ignored: bool,
    /// A folder scan was enqueued (overflow, a directory brought unknown files, never scanned).
    pub scan_requested: bool,
    pub index_enqueued: u64,
    pub index_coalesced: u64,
    /// Catalog renames (identity and chunk IDs kept, path refresh queued).
    pub renamed: u64,
    pub deletes: u64,
    pub excluded: u64,
    pub unchanged: u64,
}

/// Touched paths of a batch, deduplicated (case-insensitively, any separator), in first-seen order.
#[derive(Debug, Default)]
struct Touched {
    items: Vec<(String, Vec<usize>)>,
    index: HashMap<String, usize>,
}

impl Touched {
    fn push(&mut self, rel: &str, slot: Option<usize>) {
        let key = rel.replace('\\', "/").to_lowercase();
        let i = *self.index.entry(key).or_insert_with(|| {
            self.items.push((rel.to_string(), Vec::new()));
            self.items.len() - 1
        });
        if let Some(s) = slot {
            self.items[i].1.push(s);
        }
    }
}

enum RenameStep {
    Done,
    Excluded(Excluded),
    /// Not a clean move: treat both sides as touched paths.
    Touch,
}

/// Verdict of [`Reconciler::delete_still_wanted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCheck {
    /// Root valid and the file is gone: delete it.
    Delete,
    /// The file exists (re-created, or the removal was part of a save): keep it.
    FileExists,
    /// The root no longer validates (folder now `unavailable`): hold the job, delete nothing.
    RootUnavailable,
    /// The catalog row is already gone.
    AlreadyGone,
}

fn set_all(out: &mut [Option<EventOutcome>], slots: &[usize], o: &EventOutcome) {
    for &s in slots {
        if let Some(x) = out.get_mut(s) {
            *x = Some(o.clone());
        }
    }
}

fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Request to add a folder (validated and canonicalised by [`Reconciler::add_folder`]).
#[derive(Debug, Clone, PartialEq)]
pub struct AddFolder {
    pub collection_id: CollectionId,
    pub root: PathBuf,
    pub kind: FolderKind,
    pub is_primary: bool,
    pub settings: FolderSettings,
}

impl AddFolder {
    pub fn new(collection_id: CollectionId, root: impl Into<PathBuf>) -> Self {
        Self {
            collection_id,
            root: root.into(),
            kind: FolderKind::Indexed,
            is_primary: false,
            settings: FolderSettings::default(),
        }
    }
}

/// Scans folders and applies path events. Blocking API: run it on a blocking thread.
pub struct Reconciler {
    catalog: Arc<Catalog>,
    events: Arc<dyn EventSink>,
    dangerous: DangerousRoots,
    progress: Throttle,
    batch: usize,
    hash_threads: usize,
    /// Per-folder validated roots for watcher events ([`ROOT_CACHE_TTL`]).
    roots: Mutex<HashMap<FolderId, CachedRoot>>,
    /// Per-folder ignore files, rebuilt by every scan, used by watcher events (docs/06 change 5).
    rules: Mutex<HashMap<FolderId, Arc<FolderRules>>>,
}

#[derive(Debug, Clone)]
struct CachedRoot {
    at: Instant,
    root_key: String,
    sandbox: Arc<Sandbox>,
    canonical: PathBuf,
}

impl std::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

/// How long a validated root (its sandbox) is reused for watcher events before it is checked
/// again. Scans, removals and deletes always re-validate.
pub const ROOT_CACHE_TTL: Duration = Duration::from_secs(30);

/// Earliest run of a watcher `delete_file` job after the removal was seen. Deleting a folder
/// root reports its files' removals first; by the time the job may run the root is gone, the
/// folder is `unavailable` and the claim holds the job instead of emptying the folder.
pub const DELETE_GRACE_MS: i64 = 2_000;

/// Plain-language note for a folder the live watcher can't serve (shown under the folder in the
/// app). `None` when changes are picked up right away, or the folder only syncs on request.
pub fn watch_note(f: &FolderRecord) -> Option<String> {
    let b = behavior_of(f);
    if b.watch || !b.allows_watcher_events {
        return None;
    }
    let every = b.reconcile_every_min?;
    let when = if every % 60 == 0 {
        match every / 60 {
            1 => "every hour".to_string(),
            h => format!("every {h} hours"),
        }
    } else {
        format!("every {every} minutes")
    };
    Some(if f.watch_support == WatchSupport::Unsupported {
        format!("Live updates aren't available for this drive, so changes are picked up {when}.")
    } else {
        format!("This folder is on a network drive, so changes are picked up {when}.")
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Reconciler {
    pub fn new(
        catalog: Arc<Catalog>,
        events: Arc<dyn EventSink>,
        dangerous: DangerousRoots,
        progress_every: std::time::Duration,
    ) -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(1, 4);
        Self {
            catalog,
            events,
            dangerous,
            progress: Throttle::new(progress_every),
            batch: 512,
            hash_threads: threads,
            roots: Mutex::new(HashMap::new()),
            rules: Mutex::new(HashMap::new()),
        }
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    fn sandbox_for(&self, root: &Path) -> std::result::Result<(Sandbox, PathBuf), SandboxError> {
        // M5-E (R6): a hung network share answers "unavailable" within seconds instead of
        // blocking the start, a scan or an add (never deleted; checked again hourly).
        if !crate::netprobe::reachable(root) {
            return Err(sr_common::sandbox::Violation::BadRoot.into());
        }
        let mut sb = Sandbox::with_policy(self.dangerous.clone());
        let i = sb.add_root(root)?;
        let canonical = sb.roots()[i].canonical.clone();
        Ok((sb, canonical))
    }

    /// Validates a candidate root the way [`Self::add_folder`] will (sandbox rules, dangerous
    /// roots, existence) and returns its canonical, simplified form. Used to refuse an impossible
    /// MCP `add_folder` request before the user is asked to approve it.
    pub fn validate_new_root(&self, root: &Path) -> Result<PathBuf> {
        let (_, canonical) = self.sandbox_for(root).map_err(Error::from)?;
        Ok(dunce::simplified(&canonical).to_path_buf())
    }

    /// Pre-scan for the wizard (M2-A R1): the same root validation (dangerous roots, sandbox),
    /// filters (defaults, ignore files, secret denylist, file-type preset, size limit) and walker
    /// as the folder's first scan. Read-only: names and sizes only, no file is opened and nothing
    /// is written. Stops at `max_files` or when `cancel` fires (time budget); `capped` then says
    /// the counts are lower bounds.
    pub fn estimate(
        &self,
        root: &Path,
        settings: &FolderSettings,
        max_files: u64,
        cancel: &CancellationToken,
    ) -> Result<FolderEstimate> {
        let (sandbox, canonical) = self.sandbox_for(root).map_err(Error::from)?;
        let spec = WalkSpec {
            rules: Arc::new(FolderRules::new(&canonical)),
            root: canonical,
            sandbox: Arc::new(sandbox),
            filter: Arc::new(PathFilter::new(settings)),
            recursive: settings.recursive,
            follow_symlinks: settings.follow_symlinks,
            batch: 256,
        };
        let mut est = FolderEstimate::default();
        let r = walker::walk(&spec, cancel, |batch| {
            for f in batch {
                if (f.online_only && !settings.include_online_only)
                    || f.size > settings.max_file_bytes
                {
                    continue; // skipped by the scan too
                }
                est.files += 1;
                est.text_bytes += estimated_text_bytes(&f.ext, f.size);
                if est.files >= max_files {
                    return Err(Error::new(ErrorCode::Cancelled));
                }
            }
            Ok(())
        });
        match r {
            Ok(_) => Ok(est),
            Err(e) if e.code == ErrorCode::Cancelled => {
                est.capped = true;
                Ok(est)
            }
            Err(e) => Err(e),
        }
    }

    /// Drops the cached root of a folder (settings change, removal, root event).
    pub fn invalidate_root(&self, folder: FolderId) {
        lock(&self.roots).remove(&folder);
        lock(&self.rules).remove(&folder);
    }

    /// The folder's ignore rules for watcher events (the last scan's, or fresh).
    fn rules_for(&self, folder: FolderId, root: &Path) -> Arc<FolderRules> {
        Arc::clone(
            lock(&self.rules)
                .entry(folder)
                .or_insert_with(|| Arc::new(FolderRules::new(root))),
        )
    }

    /// The folder's validated root, reused for [`ROOT_CACHE_TTL`] (watcher events).
    pub fn cached_root(&self, f: &FolderRecord) -> Result<RootCheck> {
        if let Some(c) = lock(&self.roots).get(&f.id)
            && c.root_key == f.root_key
            && c.at.elapsed() < ROOT_CACHE_TTL
        {
            return Ok(RootCheck::Ok {
                sandbox: Arc::clone(&c.sandbox),
                canonical: c.canonical.clone(),
            });
        }
        self.check_root(f)
    }

    /// Re-validates a folder root (startup, every scan, removals). Updates `available` and emits
    /// a status change when it flips. Also records whether the root is on a network drive
    /// (`watch_support`) and upgrades a pre-V3 32-bit volume serial to the full 64 bits.
    pub fn check_root(&self, f: &FolderRecord) -> Result<RootCheck> {
        let verdict = match self.sandbox_for(&f.root_path) {
            Err(SandboxError::Io(_)) => RootCheck::Unavailable(ErrorCode::FolderUnavailable),
            Err(SandboxError::Violation(v)) => RootCheck::Unavailable(v.code()),
            Ok((sb, canonical)) => {
                let simplified = dunce::simplified(&canonical).to_path_buf();
                let same_place = folders::root_key(&simplified) == f.root_key;
                let serial = fsid::identity(&canonical).ok().map(|i| i.volume_serial);
                let low = |v: u64| v & 0xFFFF_FFFF;
                let (same_volume, legacy) = match (f.volume_serial, serial) {
                    (None, _) => (true, false),
                    (Some(_), None) => (false, false),
                    (Some(have), Some(now)) if have == now => (true, false),
                    // Catalog written before V3: only the low half was stored.
                    (Some(have), Some(now)) if have == low(have) && have == low(now) => {
                        (true, true)
                    }
                    _ => (false, false),
                };
                if same_place && same_volume {
                    let id = f.id;
                    let net = is_network_share(&simplified)
                        || fsid::drive_kind(&canonical) == fsid::DriveKind::Network;
                    let ws = match (net, f.watch_support) {
                        (true, _) => WatchSupport::Network,
                        (false, WatchSupport::Network) => WatchSupport::Native,
                        (false, other) => other,
                    };
                    if legacy || ws != f.watch_support {
                        let full = serial.filter(|_| legacy);
                        self.catalog.write_blocking(move |c| {
                            let now = sr_catalog::now_ms();
                            if let Some(full) = full {
                                folders::upgrade_volume_serial(c, id, full, now)?;
                            }
                            folders::set_watch_support(c, id, ws, now)?;
                            Ok(())
                        })?;
                    }
                    let sandbox = Arc::new(sb);
                    lock(&self.roots).insert(
                        f.id,
                        CachedRoot {
                            at: Instant::now(),
                            root_key: f.root_key.clone(),
                            sandbox: Arc::clone(&sandbox),
                            canonical: canonical.clone(),
                        },
                    );
                    RootCheck::Ok { sandbox, canonical }
                } else {
                    tracing::warn!(
                        folder = %f.id,
                        same_place,
                        same_volume,
                        "folder root now resolves elsewhere; marked unavailable"
                    );
                    RootCheck::Unavailable(ErrorCode::FolderUnavailable)
                }
            }
        };
        let (available, reason) = match &verdict {
            RootCheck::Ok { .. } => (true, None),
            RootCheck::Unavailable(c) => {
                self.invalidate_root(f.id);
                (false, Some(*c))
            }
        };
        if available != f.available
            || (!available && f.status_reason_code.as_deref() != reason.map(ErrorCode::as_str))
        {
            let id = f.id;
            self.catalog.write_blocking(move |c| {
                folders::set_available(c, id, available, reason, sr_catalog::now_ms())
            })?;
            self.refresh_status(id)?;
        }
        Ok(verdict)
    }

    /// Re-derives and caches the folder status; emits `FolderStatusChanged` if it changed. Sets
    /// `last_success_at` only when the derived status is `synced`/`synced_with_errors` (never on
    /// recovery of interrupted work: W2).
    pub fn refresh_status(&self, folder: FolderId) -> Result<FolderStatus> {
        let (s, changed, reason) = self.catalog.write_blocking(move |c| {
            let (s, changed) = status::refresh(c, folder)?;
            let f = folders::folder(c, folder)?;
            if changed && matches!(s, FolderStatus::Synced | FolderStatus::SyncedWithErrors) {
                let now = sr_catalog::now_ms();
                let trig = f.last_run_trigger.unwrap_or(RunTrigger::Manual);
                folders::record_run(c, folder, trig, None, Some(now), now)?;
            }
            Ok((s, changed, f.status_reason_code))
        })?;
        if changed {
            let reason = reason
                .and_then(|r| serde_json::from_value::<ErrorCode>(Value::String(r)).ok())
                .filter(|_| matches!(s, FolderStatus::Unavailable | FolderStatus::Error));
            self.events.emit(EngineEvent::FolderStatusChanged {
                folder_id: folder,
                status: s,
                reason,
            });
        }
        Ok(s)
    }

    /// Validates, canonicalises and inserts a folder. Overlap with an indexed folder of the same
    /// collection (also through 8.3 or junction aliases, since the canonical path is compared) is
    /// refused with `FolderOverlap`. Does not enqueue (the service does, per policy).
    pub fn add_folder(&self, req: &AddFolder) -> Result<FolderId> {
        let (_, canonical) = self.sandbox_for(&req.root).map_err(Error::from)?;
        let root_path = dunce::simplified(&canonical).to_path_buf();
        let volume_serial = fsid::identity(&canonical).ok().map(|i| i.volume_serial);
        let mut settings = req.settings.clone();
        settings.sanitize();
        let nf = NewFolder {
            collection_id: req.collection_id,
            kind: req.kind,
            root_path,
            volume_serial,
            is_primary: req.is_primary,
            settings,
        };
        // Network shares and mapped network drives: reconcile-only from the start (plain note via
        // `watch_note`), never refused: a scheduled check works there, a watcher does not.
        let network = is_network_share(&nf.root_path)
            || fsid::drive_kind(&canonical) == fsid::DriveKind::Network;
        let id = self.catalog.write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                let now = sr_catalog::now_ms();
                let id = folders::insert_folder(c, &nf, now)?;
                if network {
                    folders::set_watch_support(c, id, WatchSupport::Network, now)?;
                }
                // The first folder is scanned with today's ignore defaults: no catch-up rescan
                // is owed (an upgraded catalog with older folders has no stamp yet).
                if folders::folders(c)?.len() == 1 {
                    sr_catalog::repo::meta::set(
                        c,
                        crate::filter::META_IGNORE_DEFAULTS_VERSION,
                        &crate::filter::IGNORE_DEFAULTS_VERSION.to_string(),
                    )?;
                }
                Ok(id)
            })
        })?;
        self.refresh_status(id)?;
        Ok(id)
    }

    /// Enqueues a scan job (no policy check; see `SyncService::request_scan`).
    pub fn enqueue_scan(&self, folder: FolderId, trigger: RunTrigger) -> Result<Enqueued> {
        let mut j = NewJob::new(
            JobKind::ScanFolder,
            Some(folder),
            None,
            scan_priority(trigger),
        );
        j.payload = Some(json!({ "trigger": trigger.as_str() }));
        let e = self
            .catalog
            .write_blocking(move |c| jobs::enqueue(c, &j, sr_catalog::now_ms()))?;
        self.refresh_status(folder)?;
        Ok(e)
    }

    /// Prepares identities and hashes for a batch (read pool + blocking I/O, no writer).
    fn prepare(
        &self,
        batch: Vec<WalkedFile>,
        ctx: &BatchCtx,
    ) -> Result<(Vec<Prepared>, Vec<PathBuf>)> {
        let fid = ctx.folder_id;
        let keys: Vec<String> = batch.iter().map(|w| w.path_key.clone()).collect();
        let known: Vec<Option<FileRecord>> = self.catalog.read(|c| {
            keys.iter()
                .map(|k| files::file_by_path(c, fid, k))
                .collect::<Result<Vec<_>>>()
        })?;
        let racy_from = ctx.trusted_before_ms.saturating_mul(1_000_000) - RACY_WINDOW_NS;
        let identities: Vec<Option<fsid::FileIdentity>> = batch
            .iter()
            .zip(&known)
            .map(|(w, k)| {
                if k.is_none() {
                    fsid::identity(&w.abs).ok()
                } else {
                    None
                }
            })
            .collect();
        // Rename candidates: unknown at this path but the folder has a row with the same NTFS
        // identity. Their old size/mtime decide whether to hash, as for a known file, so a racy
        // or touched rename with unchanged content stays a cheap path refresh (CO-1).
        let candidates: Vec<Option<(u64, i64)>> = self.catalog.read(|c| {
            identities
                .iter()
                .map(|i| match i {
                    Some(i) => Ok(files::file_by_identity(c, fid, i.volume_serial, i.file_id)?
                        .map(|r| (r.size, r.mtime_ns))),
                    None => Ok(None),
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut prepared = Vec::with_capacity(batch.len());
        let mut paths = Vec::with_capacity(batch.len());
        let mut was: Vec<Option<(u64, i64)>> = Vec::with_capacity(batch.len());
        for (((w, k), identity), cand) in
            batch.into_iter().zip(known).zip(identities).zip(candidates)
        {
            was.push(k.as_ref().map(|k| (k.size, k.mtime_ns)).or(cand));
            let too_large = w.size > ctx.max_file_bytes;
            let online_only = w.online_only && !ctx.include_online_only;
            let racy = w.mtime_ns >= racy_from;
            prepared.push(Prepared {
                obs: FileObservation {
                    rel_path: w.rel,
                    path_key: w.path_key,
                    ext: w.ext,
                    volume_serial: identity.map(|i| i.volume_serial),
                    ntfs_file_id: identity.map(|i| i.file_id),
                    size: w.size,
                    mtime_ns: w.mtime_ns,
                },
                known: k,
                hash: None,
                too_large,
                online_only,
                racy,
            });
            paths.push(w.abs);
        }
        // Hash only known files (or rename candidates) whose metadata changed, racy ones, or all
        // on a deep verify.
        let need: Vec<usize> = prepared
            .iter()
            .zip(&was)
            .enumerate()
            .filter(|(_, (p, was))| {
                ctx.embed
                    && !p.too_large
                    && !p.online_only
                    && ctx.depth != ScanDepth::ForceAll
                    && was.is_some_and(|(size, mtime_ns)| {
                        ctx.depth == ScanDepth::VerifyAll
                            || p.racy
                            || size != p.obs.size
                            || mtime_ns != p.obs.mtime_ns
                    })
            })
            .map(|(i, _)| i)
            .collect();
        if need.len() <= 2 {
            // Watcher events and tiny batches: no thread hand-off.
            for i in need {
                prepared[i].hash = Some(hash_file(&paths[i]));
            }
        } else {
            let chunk = need.len().div_ceil(self.hash_threads.max(1));
            let results: Vec<(usize, std::io::Result<ContentHash>)> = std::thread::scope(|s| {
                let handles: Vec<_> = need
                    .chunks(chunk.max(1))
                    .map(|idxs| {
                        let paths = &paths;
                        s.spawn(move || {
                            idxs.iter()
                                .map(|&i| (i, hash_file(&paths[i])))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            for (i, r) in results {
                prepared[i].hash = Some(r);
            }
        }
        Ok((prepared, paths))
    }

    /// Writes one prepared batch in one writer transaction. Also returns one outcome per file
    /// (watcher events report it).
    fn apply(
        &self,
        prepared: Vec<Prepared>,
        ctx: BatchCtx,
    ) -> Result<(BatchCounts, Vec<EventOutcome>)> {
        self.catalog.write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                let now = sr_catalog::now_ms();
                let mut n = BatchCounts::default();
                let mut per_file = Vec::with_capacity(prepared.len());
                for p in &prepared {
                    n.seen += 1;
                    if p.hash.is_some() {
                        n.hashed += 1;
                    }
                    let o = files::observe(c, ctx.folder_id, ctx.scan_id, &p.obs, now)?;
                    if o.new {
                        n.new += 1;
                    } else if o.renamed_from.is_some() {
                        n.renamed += 1;
                    }
                    match decide(p, &o, &ctx) {
                        Action::Nothing => {
                            n.unchanged += 1;
                            per_file.push(if o.renamed_from.is_some() {
                                EventOutcome::Renamed
                            } else {
                                EventOutcome::Unchanged
                            });
                        }
                        Action::Skip(reason, code) => {
                            files::mark_skipped(c, o.file_id, reason, code, now)?;
                            n.skipped += 1;
                            per_file.push(EventOutcome::Unchanged);
                        }
                        a @ (Action::Index { .. } | Action::PathRefresh | Action::EnsureJob) => {
                            let payload = match a {
                                Action::Index { force, renamed } => {
                                    if !o.new {
                                        n.changed += 1;
                                    }
                                    index_payload(force, renamed)
                                }
                                Action::EnsureJob => None,
                                _ => {
                                    n.path_refreshes += 1;
                                    Some(json!({ jobs::PAYLOAD_PATH_REFRESH: true }))
                                }
                            };
                            let mut j = NewJob::new(
                                JobKind::IndexFile,
                                Some(ctx.folder_id),
                                Some(o.file_id),
                                ctx.priority,
                            );
                            // Newest files first within a priority (§2.7 "fast first results").
                            let mtime_s = p.obs.mtime_ns.max(0) / 1_000_000_000;
                            j.run_at = Some(ctx.scan_start_ms - mtime_s);
                            j.payload = payload;
                            let e = if a == Action::EnsureJob {
                                jobs::ensure(c, &j, now)?
                            } else {
                                jobs::enqueue(c, &j, now)?
                            };
                            if e.coalesced {
                                n.coalesced += 1;
                            } else {
                                n.enqueued += 1;
                            }
                            per_file.push(EventOutcome::Enqueued {
                                kind: JobKind::IndexFile,
                                coalesced: e.coalesced,
                            });
                        }
                    }
                }
                Ok((n, per_file))
            })
        })
    }

    fn process_batch(
        &self,
        batch: Vec<WalkedFile>,
        ctx: BatchCtx,
    ) -> Result<(BatchCounts, Vec<EventOutcome>)> {
        let (prepared, _) = self.prepare(batch, &ctx)?;
        self.apply(prepared, ctx)
    }

    /// Runs one scan of `folder` (blocking). `Err(Cancelled)` = interrupted: no sweep happened,
    /// the folder keeps its previous scan state and will not show `synced`.
    pub fn scan_blocking(
        &self,
        folder_id: FolderId,
        trigger: RunTrigger,
        cancel: &CancellationToken,
    ) -> Result<ScanOutcome> {
        let f = self.catalog.read(|c| folders::folder(c, folder_id))?;
        let (sandbox, canonical) = match self.check_root(&f)? {
            RootCheck::Ok { sandbox, canonical } => (sandbox, canonical),
            RootCheck::Unavailable(code) => return Ok(ScanOutcome::Unavailable(code)),
        };
        let behavior = behavior_of(&f);
        let start = sr_catalog::now_ms();
        let scan_id = self.catalog.write_blocking(move |c| {
            folders::record_run(c, folder_id, trigger, None, None, start)?;
            files::begin_scan(c, folder_id, start)
        })?;
        let last_verify = f.stats.get(STAT_SMART_VERIFY_AT).and_then(Value::as_i64);
        let depth = behavior.scan_depth(trigger, last_verify, start);
        let ctx = BatchCtx {
            folder_id,
            scan_id,
            scan_start_ms: start,
            trusted_before_ms: f.last_full_scan_at.map_or(start, |t| t.min(start)),
            depth,
            embed: behavior.embed,
            priority: priority_for(trigger),
            max_file_bytes: f.settings.max_file_bytes,
            include_online_only: f.settings.include_online_only,
            recheck_data_files: trigger == RunTrigger::SettingsChange,
        };
        // Fresh ignore rules for this scan; watcher events use them until the next one.
        let rules = Arc::new(FolderRules::new(&canonical));
        lock(&self.rules).insert(folder_id, Arc::clone(&rules));
        let spec = WalkSpec {
            rules,
            root: canonical,
            sandbox,
            filter: Arc::new(PathFilter::new(&f.settings)),
            recursive: f.settings.recursive,
            follow_symlinks: f.settings.follow_symlinks,
            batch: self.batch,
        };
        self.refresh_status(folder_id)?;
        let mut stats = ScanStats::default();
        let walked = walker::walk(&spec, cancel, |batch| {
            let (n, _) = self.process_batch(batch, ctx)?;
            stats.add(&n);
            if self.progress.ready(Some(folder_id)) {
                self.events.emit(EngineEvent::ScanProgress {
                    folder_id,
                    done: stats.files_seen,
                    total_est: None,
                });
            }
            Ok(())
        })?;
        record_skip_reason(&mut stats, &walked.stats);
        let dirs = walked.dirs;
        let sweep = self.catalog.write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                for (key, complete, code) in &dirs {
                    files::mark_dir(c, scan_id, folder_id, key, *complete, *code)?;
                }
                files::finish_scan(c, folder_id, scan_id, sr_catalog::now_ms())
            })
        })?;
        stats.deleted = sweep.deleted.len() as u64;
        stats.kept_unseen = sweep.kept_unseen as u64;
        stats.partial = sweep.partial;
        let mut stats_json = stats.to_json();
        let verified_at = if depth == ScanDepth::VerifyAll {
            Some(start)
        } else {
            last_verify
        };
        if let (Some(t), Some(m)) = (verified_at, stats_json.as_object_mut()) {
            m.insert(STAT_SMART_VERIFY_AT.into(), json!(t));
        }
        self.catalog.write_blocking(move |c| {
            folders::record_run(
                c,
                folder_id,
                trigger,
                Some(&stats_json),
                None,
                sr_catalog::now_ms(),
            )
        })?;
        self.events.emit(EngineEvent::ScanProgress {
            folder_id,
            done: stats.files_seen,
            total_est: Some(stats.files_seen),
        });
        Ok(ScanOutcome::Completed(stats))
    }

    /// Applies one watcher-style event (blocking). Same rules as [`Self::on_path_events`].
    pub fn on_path_event(&self, folder_id: FolderId, ev: &PathEvent) -> Result<EventOutcome> {
        let (_, mut per_event) = self.apply_events(folder_id, std::slice::from_ref(ev))?;
        Ok(per_event.pop().unwrap_or(EventOutcome::Unchanged))
    }

    /// Applies a debounced batch of watcher events for one folder (blocking; see the module
    /// docs). Gated by `allows_watcher_events`; paused and unavailable folders ignore events.
    pub fn on_path_events(&self, folder_id: FolderId, events: &[PathEvent]) -> Result<EventBatch> {
        self.apply_events(folder_id, events).map(|(b, _)| b)
    }

    fn apply_events(
        &self,
        folder_id: FolderId,
        events: &[PathEvent],
    ) -> Result<(EventBatch, Vec<EventOutcome>)> {
        let n = events.len();
        let mut b = EventBatch {
            events: n as u64,
            ..EventBatch::default()
        };
        let f = self.catalog.read(|c| folders::folder(c, folder_id))?;
        let behavior = behavior_of(&f);
        if !behavior.allows_watcher_events || f.paused || !f.available {
            b.ignored = true;
            return Ok((b, vec![EventOutcome::Ignored; n]));
        }
        // docs/06 change 5: an edited `.seleneignore` / `.gitignore` / `.ignore` changes what
        // the folder holds: one `settings_change` scan sweeps newly ignored files and adds
        // re-included ones (it also re-reads the rules).
        let rules_changed = events.iter().any(|e| match e {
            PathEvent::Changed(r) | PathEvent::Removed(r) => is_ignore_file(r),
            PathEvent::Renamed { from, to } => is_ignore_file(from) || is_ignore_file(to),
            PathEvent::Rescan => false,
        });
        if rules_changed {
            lock(&self.rules).remove(&folder_id);
            let e = self.enqueue_scan(folder_id, RunTrigger::SettingsChange)?;
            b.scan_requested = true;
            let o = EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                coalesced: e.coalesced,
            };
            return Ok((b, vec![o; n]));
        }
        if events.iter().any(|e| matches!(e, PathEvent::Rescan)) {
            // Overflow or "something changed below here": one scan covers every event.
            let e = self.enqueue_scan(folder_id, RunTrigger::Watcher)?;
            b.scan_requested = true;
            let o = EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                coalesced: e.coalesced,
            };
            return Ok((b, vec![o; n]));
        }
        let (sandbox, canonical) = match self.cached_root(&f)? {
            RootCheck::Ok { sandbox, canonical } => (sandbox, canonical),
            RootCheck::Unavailable(_) => {
                b.ignored = true;
                return Ok((b, vec![EventOutcome::Ignored; n]));
            }
        };
        let filter = PathFilter::new(&f.settings);
        // docs/06 §2.4: events honour the same ignore files as the walk.
        let rules = self.rules_for(folder_id, &canonical);
        // A `Cargo.toml` that appeared or went away changes which directories are Cargo sources
        // (`FolderRules::cargo_source`): one scan re-applies the defaults with fresh rules.
        let manifest_changed =
            |r: &str| cargo_manifest_dir(r).is_some_and(|d| rules.cargo_changed(&d));
        if events.iter().any(|e| match e {
            PathEvent::Changed(r) | PathEvent::Removed(r) => manifest_changed(r),
            PathEvent::Renamed { from, to } => manifest_changed(from) || manifest_changed(to),
            PathEvent::Rescan => false,
        }) {
            lock(&self.rules).remove(&folder_id);
            let e = self.enqueue_scan(folder_id, RunTrigger::Watcher)?;
            b.scan_requested = true;
            let o = EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                coalesced: e.coalesced,
            };
            return Ok((b, vec![o; n]));
        }
        let filter = Checks {
            filter: &filter,
            rules: &rules,
        };
        let mut out: Vec<Option<EventOutcome>> = vec![None; n];
        let mut touched = Touched::default();
        for (i, ev) in events.iter().enumerate() {
            match ev {
                PathEvent::Changed(rel) | PathEvent::Removed(rel) => touched.push(rel, Some(i)),
                PathEvent::Renamed { from, to } => {
                    match self.apply_rename(&f, &behavior, &sandbox, &filter, from, to)? {
                        RenameStep::Done => {
                            b.renamed += 1;
                            out[i] = Some(EventOutcome::Renamed);
                        }
                        RenameStep::Excluded(x) => {
                            b.excluded += 1;
                            out[i] = Some(EventOutcome::Excluded(x));
                        }
                        RenameStep::Touch => {
                            touched.push(from, None);
                            touched.push(to, Some(i));
                        }
                    }
                }
                PathEvent::Rescan => {}
            }
        }

        // Classify every touched path by its state on disk now.
        let mut walked: Vec<WalkedFile> = Vec::new();
        let mut walked_slots: Vec<Vec<usize>> = Vec::new();
        let mut gone: Vec<(String, Vec<usize>)> = Vec::new();
        let mut dirs: Vec<(String, PathBuf, Vec<usize>)> = Vec::new();
        for (rel, slots) in touched.items {
            let (abs, long) = match walker::resolve_event_path(&sandbox, &rel) {
                Ok(x) => x,
                Err(v) => {
                    b.excluded += 1;
                    set_all(&mut out, &slots, &EventOutcome::Excluded(excluded_of(v)));
                    continue;
                }
            };
            let md = match std::fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    gone.push((long, slots));
                    continue;
                }
                Err(e) => {
                    // Locked or unreadable right now: a later event or the next scan sees it.
                    tracing::debug!(error = %e, "watcher: could not stat a changed path");
                    b.unchanged += 1;
                    set_all(&mut out, &slots, &EventOutcome::Unchanged);
                    continue;
                }
            };
            if md.is_dir() {
                match filter.dir(&long) {
                    Ok(()) => dirs.push((long, abs, slots)),
                    Err(e) => {
                        b.excluded += 1;
                        set_all(&mut out, &slots, &EventOutcome::Excluded(e));
                    }
                }
                continue;
            }
            if md.file_type().is_symlink() && !f.settings.follow_symlinks {
                b.excluded += 1;
                let o = EventOutcome::Excluded(Excluded::Rejected(
                    sr_common::sandbox::Violation::OutsideRoot,
                ));
                set_all(&mut out, &slots, &o);
                continue;
            }
            if let Err(e) = filter.file(&long) {
                b.excluded += 1;
                set_all(&mut out, &slots, &EventOutcome::Excluded(e));
                continue;
            }
            let Ok(md) = std::fs::metadata(&abs) else {
                b.unchanged += 1;
                set_all(&mut out, &slots, &EventOutcome::Unchanged);
                continue;
            };
            let name = long.rsplit('/').next().unwrap_or(&long).to_string();
            walked.push(WalkedFile {
                abs,
                path_key: path_key(&long),
                ext: extension_of(&name),
                size: md.len(),
                mtime_ns: mtime_of(&md),
                online_only: fsid::is_online_only(&md),
                rel: long,
            });
            walked_slots.push(slots);
        }

        let fid = f.id;
        let mut scan_slots: Vec<usize> = Vec::new();
        // Before removals are looked up, so a moved file's old path finds nothing to delete.
        let moved = self.apply_moves(&f, &behavior, &sandbox, &walked)?;
        b.renamed += moved.iter().filter(|m| **m).count() as u64;
        // Directories: their files report their own events (the watch is recursive). Only a
        // directory that brings files the catalog has never seen below it (moved or copied in)
        // needs a scan; an empty new directory or a modified known one needs nothing.
        if !dirs.is_empty() {
            let keys: Vec<String> = dirs.iter().map(|(l, _, _)| path_key(l)).collect();
            let known: Vec<bool> = self.catalog.read(|c| {
                keys.iter()
                    .map(|k| files::any_under(c, fid, k))
                    .collect::<Result<Vec<_>>>()
            })?;
            for ((_, abs, slots), known) in dirs.into_iter().zip(known) {
                let has_entries = std::fs::read_dir(&abs)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(false);
                if !known && has_entries {
                    scan_slots.extend(slots);
                } else {
                    b.unchanged += 1;
                    set_all(&mut out, &slots, &EventOutcome::Unchanged);
                }
            }
        }

        // Removals: cataloged files get a (verified, delayed) delete job; an unknown path with
        // cataloged files below it was a directory and needs a sweep; anything else is noise
        // (temp files that came and went between two flushes).
        if !gone.is_empty() {
            let keys: Vec<String> = gone.iter().map(|(l, _)| path_key(l)).collect();
            let found: Vec<(Option<FileId>, bool)> = self.catalog.read(|c| {
                keys.iter()
                    .map(|k| {
                        Ok(match files::file_by_path(c, fid, k)? {
                            Some(r) => (Some(r.id), false),
                            None => (None, files::any_under(c, fid, k)?),
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            let mut deletes: Vec<(FileId, Vec<usize>)> = Vec::new();
            for ((_, slots), (file, dir_below)) in gone.into_iter().zip(found) {
                match file {
                    Some(id) => deletes.push((id, slots)),
                    None if dir_below => scan_slots.extend(slots),
                    None => {
                        b.unchanged += 1;
                        set_all(&mut out, &slots, &EventOutcome::Unchanged);
                    }
                }
            }
            if !deletes.is_empty() || !scan_slots.is_empty() {
                // Never act on removals with a cached root: a deleted, renamed or unplugged root
                // reports exactly these events. Re-validate now; unavailable → nothing is deleted.
                let fresh = self.catalog.read(|c| folders::folder(c, fid))?;
                if let RootCheck::Unavailable(_) = self.check_root(&fresh)? {
                    b = EventBatch {
                        events: n as u64,
                        ignored: true,
                        ..EventBatch::default()
                    };
                    return Ok((b, vec![EventOutcome::Ignored; n]));
                }
            }
            if !deletes.is_empty() {
                let ids: Vec<FileId> = deletes.iter().map(|(id, _)| *id).collect();
                let res = self.catalog.write_blocking(move |c| {
                    sr_catalog::atomic(c, |c| {
                        let now = sr_catalog::now_ms();
                        ids.iter()
                            .map(|id| {
                                let mut j = NewJob::new(
                                    JobKind::DeleteFile,
                                    Some(fid),
                                    Some(*id),
                                    PRIORITY_LIVE,
                                );
                                j.run_at = Some(now + DELETE_GRACE_MS);
                                jobs::enqueue(c, &j, now)
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                })?;
                for ((_, slots), e) in deletes.iter().zip(res) {
                    b.deletes += 1;
                    let o = EventOutcome::Enqueued {
                        kind: JobKind::DeleteFile,
                        coalesced: e.coalesced,
                    };
                    set_all(&mut out, slots, &o);
                }
            }
        }

        // Changed files: tiered change detection in batches, joined to the latest mark pass.
        if !walked.is_empty() {
            match self.catalog.read(|c| files::latest_scan_id(c, fid))? {
                None => {
                    // Never scanned: the initial scan will catalog them.
                    for s in walked_slots {
                        scan_slots.extend(s);
                    }
                }
                Some(scan_id) => {
                    let now = sr_catalog::now_ms();
                    let ctx = BatchCtx {
                        folder_id: fid,
                        scan_id,
                        scan_start_ms: now,
                        trusted_before_ms: f.last_full_scan_at.map_or(now, |t| t.min(now)),
                        depth: ScanDepth::Tiered,
                        embed: behavior.embed,
                        priority: priority_for(RunTrigger::Watcher),
                        max_file_bytes: f.settings.max_file_bytes,
                        include_online_only: f.settings.include_online_only,
                        recheck_data_files: false,
                    };
                    let mut slots_iter = walked_slots.into_iter();
                    let mut moved_iter = moved.into_iter();
                    let mut rest = walked;
                    while !rest.is_empty() {
                        let tail = rest.split_off(rest.len().min(self.batch.max(1)));
                        let chunk = std::mem::replace(&mut rest, tail);
                        let (counts, per_file) = self.process_batch(chunk, ctx)?;
                        b.index_enqueued += counts.enqueued;
                        b.index_coalesced += counts.coalesced;
                        b.renamed += counts.renamed;
                        for o in per_file {
                            let o = match (o, moved_iter.next()) {
                                (EventOutcome::Unchanged, Some(true)) => EventOutcome::Renamed,
                                (o, _) => o,
                            };
                            if o == EventOutcome::Unchanged {
                                b.unchanged += 1;
                            }
                            if let Some(slots) = slots_iter.next() {
                                set_all(&mut out, &slots, &o);
                            }
                        }
                    }
                }
            }
        }

        if !scan_slots.is_empty() {
            let e = self.enqueue_scan(fid, RunTrigger::Watcher)?;
            b.scan_requested = true;
            let o = EventOutcome::Enqueued {
                kind: JobKind::ScanFolder,
                coalesced: e.coalesced,
            };
            set_all(&mut out, &scan_slots, &o);
        } else {
            self.refresh_status(fid)?;
        }
        let per_event = out
            .into_iter()
            .map(|o| o.unwrap_or(EventOutcome::Unchanged))
            .collect();
        Ok((b, per_event))
    }

    /// One watcher rename `from → to` (CO-1 / Amendment 4.17): the catalog row moves (same
    /// `file_id`, same chunk IDs) and an `index_file {"path_refresh": true}` job refreshes the
    /// lexical path, in one transaction. Anything that is not a clean move of a cataloged file is
    /// handed back as two touched paths, so the state on disk decides:
    /// * `from` exists again (atomic-save pattern: `x → x~`, `x.tmp → x`);
    /// * the target is excluded, gone, not a file, or another cataloged file (renamed over it);
    /// * `from` is unknown (a directory, or a file the catalog never saw).
    fn apply_rename(
        &self,
        f: &FolderRecord,
        behavior: &FolderBehavior,
        sandbox: &Sandbox,
        filter: &Checks<'_>,
        from: &str,
        to: &str,
    ) -> Result<RenameStep> {
        let from_key = match parse_relative(from) {
            Ok(c) => path_key(&c.join("/")),
            Err(v) => return Ok(RenameStep::Excluded(Excluded::Rejected(v))),
        };
        let fid = f.id;
        let Some(old) = self
            .catalog
            .read(|c| files::file_by_path(c, fid, &from_key))?
        else {
            return Ok(RenameStep::Touch);
        };
        if walker::resolve_event_path(sandbox, from).is_ok_and(|(abs, _)| abs.is_file()) {
            return Ok(RenameStep::Touch);
        }
        let to_rel = match walker::resolve_event_path(sandbox, to) {
            Ok((abs, long)) if filter.file(&long).is_ok() && abs.is_file() => long,
            _ => return Ok(RenameStep::Touch),
        };
        let key = path_key(&to_rel);
        if key != old.path_key
            && self
                .catalog
                .read(|c| files::file_by_path(c, fid, &key))?
                .is_some()
        {
            return Ok(RenameStep::Touch);
        }
        self.move_row(fid, behavior, &old, to_rel, key)?;
        Ok(RenameStep::Done)
    }

    /// Moves a catalog row to a new path (same `file_id`, same chunk IDs) and queues the lexical
    /// path refresh (Amendment 4.17), in one transaction.
    fn move_row(
        &self,
        folder: FolderId,
        behavior: &FolderBehavior,
        old: &FileRecord,
        to_rel: String,
        key: String,
    ) -> Result<()> {
        let id = old.id;
        let refresh =
            behavior.embed && matches!(old.state, FileState::Indexed | FileState::PendingIndex);
        self.catalog.write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                let now = sr_catalog::now_ms();
                files::rename_file(c, id, &to_rel, &key, now)?;
                if refresh {
                    let mut j = NewJob::new(
                        JobKind::IndexFile,
                        Some(folder),
                        Some(id),
                        priority_for(RunTrigger::Watcher),
                    );
                    j.payload = Some(json!({ jobs::PAYLOAD_PATH_REFRESH: true }));
                    jobs::enqueue(c, &j, now)?;
                }
                Ok(())
            })
        })
    }

    /// Cross-directory moves. ReadDirectoryChangesW reports `RENAMED_OLD/NEW_NAME` only for a
    /// rename inside one directory; a move into another directory arrives as REMOVED + ADDED.
    /// An added path the catalog doesn't know, whose file identity (volume serial + 128-bit id)
    /// matches a row of this folder whose old path is gone, **is** that row: it moves (identity
    /// and chunk IDs kept, path refresh queued) instead of becoming delete + new. A row whose old
    /// path still exists is a hard link and stays. Returns, per walked file, whether it moved.
    fn apply_moves(
        &self,
        f: &FolderRecord,
        behavior: &FolderBehavior,
        sandbox: &Sandbox,
        walked: &[WalkedFile],
    ) -> Result<Vec<bool>> {
        let mut moved = vec![false; walked.len()];
        if walked.is_empty() {
            return Ok(moved);
        }
        let fid = f.id;
        let keys: Vec<String> = walked.iter().map(|w| w.path_key.clone()).collect();
        let known: Vec<bool> = self.catalog.read(|c| {
            keys.iter()
                .map(|k| files::file_by_path(c, fid, k).map(|r| r.is_some()))
                .collect::<Result<Vec<_>>>()
        })?;
        let ids: Vec<Option<fsid::FileIdentity>> = walked
            .iter()
            .zip(&known)
            .map(|(w, k)| {
                if *k {
                    None
                } else {
                    fsid::identity(&w.abs).ok()
                }
            })
            .collect();
        if ids.iter().all(Option::is_none) {
            return Ok(moved);
        }
        let candidates: Vec<Option<FileRecord>> = self.catalog.read(|c| {
            ids.iter()
                .map(|i| match i {
                    Some(i) => files::file_by_identity(c, fid, i.volume_serial, i.file_id),
                    None => Ok(None),
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut taken: HashSet<FileId> = HashSet::new();
        for (i, (w, cand)) in walked.iter().zip(candidates).enumerate() {
            let Some(old) = cand else { continue };
            if old.path_key == w.path_key || !taken.insert(old.id) {
                continue;
            }
            let old_still_there = walker::resolve_event_path(sandbox, &old.rel_path)
                .is_ok_and(|(abs, _)| abs.exists());
            if old_still_there {
                continue;
            }
            self.move_row(fid, behavior, &old, w.rel.clone(), w.path_key.clone())?;
            moved[i] = true;
        }
        Ok(moved)
    }

    /// `delete_file` safety check at execution time: the folder root still validates and the
    /// file is really gone. `false` = don't delete (the file is back, or the root went away).
    pub fn delete_still_wanted(&self, file: FileId) -> Result<DeleteCheck> {
        let rec = match self.catalog.read(|c| files::file(c, file)) {
            Ok(r) => r,
            Err(e) if e.code == ErrorCode::NotFound => return Ok(DeleteCheck::AlreadyGone),
            Err(e) => return Err(e),
        };
        let f = self.catalog.read(|c| folders::folder(c, rec.folder_id))?;
        let RootCheck::Ok { sandbox, .. } = self.check_root(&f)? else {
            return Ok(DeleteCheck::RootUnavailable);
        };
        Ok(match walker::resolve_event_path(&sandbox, &rec.rel_path) {
            Ok((abs, _)) if abs.is_file() => DeleteCheck::FileExists,
            _ => DeleteCheck::Delete,
        })
    }

    /// Applies new folder settings. Returns whether indexing output changed (the caller then
    /// enqueues a `SettingsChange` scan).
    pub fn update_settings(&self, folder: FolderId, new: &FolderSettings) -> Result<bool> {
        let mut s = new.clone();
        s.sanitize();
        let affects = self.catalog.write_blocking(move |c| {
            folders::update_settings(c, folder, &s, sr_catalog::now_ms())
        })?;
        self.invalidate_root(folder);
        self.refresh_status(folder)?;
        Ok(affects)
    }
}

fn excluded_of(v: sr_common::sandbox::Violation) -> Excluded {
    use sr_common::sandbox::Violation as V;
    match v {
        V::SecretDenied => Excluded::Secret,
        other => Excluded::Rejected(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(state: FileState, new: bool, changed: bool, hash: Option<ContentHash>) -> Observed {
        Observed {
            file_id: FileId::new(),
            new,
            renamed_from: None,
            metadata_changed: changed,
            state,
            content_hash: hash,
        }
    }

    fn prep(hash: Option<ContentHash>, racy: bool) -> Prepared {
        Prepared {
            obs: FileObservation {
                rel_path: "a.md".into(),
                path_key: "a.md".into(),
                ext: "md".into(),
                volume_serial: None,
                ntfs_file_id: None,
                size: 1,
                mtime_ns: 1,
            },
            known: None,
            hash: hash.map(Ok),
            too_large: false,
            online_only: false,
            racy,
        }
    }

    fn ctx(depth: ScanDepth, embed: bool) -> BatchCtx {
        BatchCtx {
            folder_id: FolderId::new(),
            scan_id: 1,
            scan_start_ms: 0,
            trusted_before_ms: 0,
            depth,
            embed,
            priority: 2,
            max_file_bytes: 10,
            include_online_only: false,
            recheck_data_files: false,
        }
    }

    /// docs/06 change 5: a `settings_change` scan hands unchanged JSON files to the ingest step
    /// again (the data-file rule is checked there); other scans and other files don't.
    #[test]
    fn settings_change_rechecks_json_data_files() {
        let h1 = ContentHash::of_bytes(b"1");
        let mut json = prep(Some(h1), false);
        json.obs.ext = "json".into();
        let unchanged = obs(FileState::Indexed, false, false, Some(h1));
        let idx = Action::Index {
            force: false,
            renamed: false,
        };
        let mut t = ctx(ScanDepth::Tiered, true);
        assert_eq!(decide(&json, &unchanged, &t), Action::Nothing);
        t.recheck_data_files = true;
        assert_eq!(decide(&json, &unchanged, &t), idx);
        assert_eq!(
            decide(&prep(Some(h1), false), &unchanged, &t),
            Action::Nothing,
            "not JSON"
        );
        // A file skipped as a data dump is checked again too (a rule may re-include it).
        let mut skipped = json;
        skipped.known = Some(FileRecord {
            id: FileId::new(),
            folder_id: FolderId::new(),
            rel_path: "a.json".into(),
            path_key: "a.json".into(),
            ext: "json".into(),
            size: 1,
            mtime_ns: 1,
            content_hash: Some(h1),
            version: 1,
            extractor_version: None,
            chunk_cfg_fp: None,
            scan_id: 1,
            state: FileState::Skipped,
            skip_reason: Some(SKIP_DATA_FILE.into()),
            error_code: None,
            attempts: 0,
        });
        let o = obs(FileState::Skipped, false, false, Some(h1));
        assert_eq!(decide(&skipped, &o, &t), idx);
        t.recheck_data_files = false;
        assert_eq!(decide(&skipped, &o, &t), Action::Nothing);
    }

    #[test]
    fn tiered_decisions() {
        let h1 = ContentHash::of_bytes(b"1");
        let h2 = ContentHash::of_bytes(b"2");
        let t = ctx(ScanDepth::Tiered, true);
        let idx = Action::Index {
            force: false,
            renamed: false,
        };
        // New file → index; unchanged → nothing (no hash needed).
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::Indexed, true, true, None),
                &t
            ),
            idx
        );
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::Indexed, false, false, Some(h1)),
                &t
            ),
            Action::Nothing
        );
        // Metadata changed but same bytes → nothing; different bytes → index.
        assert_eq!(
            decide(
                &prep(Some(h1), false),
                &obs(FileState::Indexed, false, true, Some(h1)),
                &t
            ),
            Action::Nothing
        );
        assert_eq!(
            decide(
                &prep(Some(h2), false),
                &obs(FileState::Indexed, false, true, Some(h1)),
                &t
            ),
            idx
        );
        // Racy with unchanged metadata is hashed and compared.
        assert_eq!(
            decide(
                &prep(Some(h2), true),
                &obs(FileState::Indexed, false, false, Some(h1)),
                &t
            ),
            idx
        );
        // Pending without T1 and unchanged only needs *a* job (never a rerun of a running one,
        // CO-1); changed, it gets a coalescing job; pending after T1 (only the store apply is
        // outstanding, D3) is compared instead of re-indexed; access grants (`embed` off) skip.
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::PendingIndex, false, false, None),
                &t
            ),
            Action::EnsureJob
        );
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::PendingIndex, false, true, None),
                &t
            ),
            idx
        );
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::PendingIndex, false, false, Some(h1)),
                &t
            ),
            Action::Nothing
        );
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::Indexed, true, true, None),
                &ctx(ScanDepth::Tiered, false)
            ),
            Action::Skip(SKIP_FILES_ONLY, None)
        );
        assert_eq!(
            decide(
                &prep(None, false),
                &obs(FileState::Indexed, false, false, Some(h1)),
                &ctx(ScanDepth::ForceAll, true)
            ),
            Action::Index {
                force: true,
                renamed: false
            }
        );
        let mut big = prep(None, false);
        big.too_large = true;
        assert_eq!(
            decide(&big, &obs(FileState::Indexed, true, true, None), &t),
            Action::Skip(SKIP_TOO_LARGE, Some(ErrorCode::FileTooLarge))
        );
    }

    /// CO-1: renames refresh the lexical path (m1-e O3) without re-embedding.
    #[test]
    fn rename_decisions() {
        let h1 = ContentHash::of_bytes(b"1");
        let t = ctx(ScanDepth::Tiered, true);
        let ren = |changed, hash| Observed {
            renamed_from: Some("old.md".into()),
            ..obs(FileState::Indexed, false, changed, hash)
        };
        // Pure rename → path-only refresh.
        assert_eq!(
            decide(&prep(None, false), &ren(false, Some(h1)), &t),
            Action::PathRefresh
        );
        // Rename + metadata change (content unknown) → full index flagged `renamed`.
        assert_eq!(
            decide(&prep(None, false), &ren(true, Some(h1)), &t),
            Action::Index {
                force: false,
                renamed: true
            }
        );
        // Rename + touched but same bytes → still a path refresh.
        assert_eq!(
            decide(&prep(Some(h1), false), &ren(true, Some(h1)), &t),
            Action::PathRefresh
        );
        // Access grants (`embed` off) never get a job.
        assert_eq!(
            decide(
                &prep(None, false),
                &Observed {
                    state: FileState::Skipped,
                    ..ren(false, Some(h1))
                },
                &ctx(ScanDepth::Tiered, false)
            ),
            Action::Skip(SKIP_FILES_ONLY, None)
        );
        assert_eq!(
            index_payload(false, true),
            Some(json!({ jobs::PAYLOAD_RENAMED: true }))
        );
        assert_eq!(index_payload(false, false), None);
    }

    #[test]
    fn network_share_detection() {
        assert!(is_network_share(Path::new(r"\\server\share\x")));
        assert!(is_network_share(Path::new(r"\\?\UNC\server\share")));
        assert!(!is_network_share(Path::new(r"\\?\C:\x")));
        assert!(!is_network_share(Path::new(r"C:\x")));
    }
}

/// Pre-scan counts ([`Reconciler::estimate`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderEstimate {
    /// Files the first scan would index.
    pub files: u64,
    /// Estimated bytes of extractable text (documents count a fraction of their size).
    pub text_bytes: u64,
    /// The walk stopped early (file cap or time budget): both counts are lower bounds.
    pub capped: bool,
}

/// Containers whose text is a fraction of their size (PDF, Office, e-books).
fn estimated_text_bytes(ext: &str, size: u64) -> u64 {
    const DOCS: &[&str] = &["pdf", "docx", "pptx", "xlsx", "xls", "odt", "rtf", "epub"];
    if DOCS.contains(&ext) {
        (size / 10).min(1024 * 1024)
    } else {
        size
    }
}
