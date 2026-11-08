//! Live watcher (M2): notify (ReadDirectoryChangesW on Windows) → per-folder debounce → batched
//! [`PathEvent`]s → `Reconciler::on_path_events` → coalesced jobs.
//!
//! # Pipeline
//! 1. One notify watcher per watched folder (recursive unless the folder is not). Its callback
//!    turns each event into root-relative paths and `try_send`s it to one bounded queue
//!    ([`WatchConfig::queue_capacity`]). A full queue never blocks the OS thread: the folder is
//!    flagged **overflowed** and gets a rescan instead.
//! 2. The debouncer keeps one pending batch per folder: Changed/Removed paths deduplicated,
//!    renames paired (`From` + `To` → `Renamed`), and flushes a batch once the folder has been
//!    quiet for [`WatchConfig::debounce`] or its first event is [`WatchConfig::max_delay`] old
//!    (**≤ 500 ms** by default), one batch in flight per folder (order kept).
//! 3. Each batch is applied with `Reconciler::on_path_events` (one folder read, cached root,
//!    batched writes; jobs coalesce on the catalog `dedupe_key`), then the claim loop is woken.
//!
//! # Overflow, storms, dead watches (notify 8.2.0)
//! notify 8.2.0's Windows backend does **not** report a ReadDirectoryChangesW buffer overflow
//! (a 0-byte completion is parsed as an empty buffer), and on an unknown completion error or a
//! deleted root it unwatches silently. notify 9 fixes both (`Flag::Rescan`); until the workspace
//! moves to it, this module defends itself:
//! * `Flag::Rescan` (notify 9, other backends) and our own queue overflow → folder rescan;
//! * an **event storm** (≥ [`WatchConfig::storm_events`] raw events in one batch window, i.e. a
//!   burst that could have filled the 16 KiB OS buffer) → the batch becomes one folder rescan
//!   and the watch is **re-armed** (a new watch is created before the old one is dropped);
//! * every watch is re-armed every [`WatchConfig::rearm_every`], so a silently dead watch
//!   recovers; the rescan catches what it missed;
//! * a **root probe** every [`WatchConfig::probe_every`]: a root that is gone or renamed goes
//!   through `check_root` → folder `unavailable` (nothing deleted) and is unwatched; when it
//!   comes back the folder is reconnected (`reconnect` scan) and watched again.
//!
//! # Which folders
//! Exactly those whose behaviour has `watch` (sync mode `auto`/`triggered`, not a network drive,
//! watcher supported), that are available and not paused. If a watch can't be started on a file
//! system the folder is recorded as `watch_support = unsupported` and becomes reconcile-only
//! (scheduled checks, plain note via `watch_note`); the next daemon start tries again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use sr_catalog::repo::folders::{self, FolderRecord};
use sr_catalog::{RunTrigger, WatchSupport};
use sr_common::error::Result;
use sr_common::ids::FolderId;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::reconcile::{EventBatch, PathEvent, Reconciler, RootCheck, behavior_of};

/// Quiet time after a storm before its watch is re-armed.
const STORM_QUIET: Duration = Duration::from_secs(1);

/// Watcher tunables.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Quiet time after the last event of a folder before its batch is applied.
    pub debounce: Duration,
    /// Upper bound from a folder's first pending event to its flush (continuous churn).
    pub max_delay: Duration,
    /// Raw events buffered between the OS threads and the debouncer.
    pub queue_capacity: usize,
    /// Raw events in one batch window that count as a storm (→ rescan + re-arm).
    pub storm_events: usize,
    /// Root probe and watch-set reconcile period.
    pub probe_every: Duration,
    /// Every watch is re-created this often (recovers a silently dead watch).
    pub rearm_every: Duration,
    /// Debouncer tick.
    pub tick: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(200),
            max_delay: Duration::from_millis(500),
            queue_capacity: 65_536,
            storm_events: 256,
            probe_every: Duration::from_secs(2),
            rearm_every: Duration::from_secs(10 * 60),
            tick: Duration::from_millis(50),
        }
    }
}

/// Watcher counters (diagnostics and tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchCounters {
    /// Raw notify events received.
    pub raw_events: u64,
    /// Batches applied.
    pub batches: u64,
    /// Path events applied (after dedupe and rename pairing).
    pub events_applied: u64,
    /// Batches turned into a folder rescan (overflow, storm, notify `Rescan`, errors).
    pub rescans: u64,
    pub storms: u64,
    /// Queue overflows (raw events dropped, folder rescanned).
    pub overflows: u64,
    pub rearms: u64,
    /// Watches that could not be started.
    pub watch_errors: u64,
    /// Folders found unavailable by the root probe.
    pub roots_lost: u64,
    /// Folders reconnected by the root probe.
    pub roots_back: u64,
}

#[derive(Debug, Default)]
struct Counters {
    raw_events: AtomicU64,
    batches: AtomicU64,
    events_applied: AtomicU64,
    rescans: AtomicU64,
    storms: AtomicU64,
    overflows: AtomicU64,
    rearms: AtomicU64,
    watch_errors: AtomicU64,
    roots_lost: AtomicU64,
    roots_back: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> WatchCounters {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        WatchCounters {
            raw_events: l(&self.raw_events),
            batches: l(&self.batches),
            events_applied: l(&self.events_applied),
            rescans: l(&self.rescans),
            storms: l(&self.storms),
            overflows: l(&self.overflows),
            rearms: l(&self.rearms),
            watch_errors: l(&self.watch_errors),
            roots_lost: l(&self.roots_lost),
            roots_back: l(&self.roots_back),
        }
    }
}

fn bump(a: &AtomicU64) {
    a.fetch_add(1, Ordering::Relaxed);
}

/// What the notify callback hands to the debouncer.
#[derive(Debug)]
enum FsEv {
    Changed(Vec<String>),
    Removed(Vec<String>),
    RenameFrom(String),
    RenameTo(String),
    Renamed(String, String),
    /// notify asked for a rescan, or a path could not be mapped.
    Rescan,
    /// Watch error or an event about the root itself: probe the root now and rescan.
    Trouble,
}

#[derive(Debug)]
enum Raw {
    Fs {
        folder: FolderId,
        generation: u64,
        ev: FsEv,
    },
    Inject {
        folder: FolderId,
        ev: PathEvent,
    },
}

/// Root-relative `/` path of `p` under `root`; `Some("")` for the root itself.
fn rel_of(root: &Path, p: &Path) -> Option<String> {
    let rest = p.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rest.components() {
        parts.push(c.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

fn classify(root: &Path, ev: &notify::Event) -> Option<FsEv> {
    if ev.need_rescan() {
        return Some(FsEv::Rescan);
    }
    if matches!(ev.kind, EventKind::Access(_)) {
        return None;
    }
    let mut rels = Vec::with_capacity(ev.paths.len());
    for p in &ev.paths {
        match rel_of(root, p) {
            Some(r) if r.is_empty() => return Some(FsEv::Trouble),
            Some(r) => rels.push(r),
            None => return Some(FsEv::Rescan),
        }
    }
    if rels.is_empty() {
        return None;
    }
    Some(match ev.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => FsEv::RenameFrom(rels.remove(0)),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => FsEv::RenameTo(rels.remove(0)),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if rels.len() == 2 => {
            let to = rels.pop().unwrap_or_default();
            FsEv::Renamed(rels.remove(0), to)
        }
        EventKind::Remove(_) => FsEv::Removed(rels),
        _ => FsEv::Changed(rels),
    })
}

/// One folder's batch being collected.
#[derive(Debug)]
struct Pending {
    first: Instant,
    last: Instant,
    events: Vec<PathEvent>,
    seen: HashSet<String>,
    rename_from: Option<String>,
    rescan: bool,
    raw: usize,
}

impl Pending {
    fn new(now: Instant) -> Self {
        Self {
            first: now,
            last: now,
            events: Vec::new(),
            seen: HashSet::new(),
            rename_from: None,
            rescan: false,
            raw: 0,
        }
    }

    fn touch(&mut self, ev: PathEvent) {
        let key = match &ev {
            PathEvent::Changed(r) | PathEvent::Removed(r) => r.to_lowercase(),
            _ => String::new(),
        };
        if key.is_empty() || self.seen.insert(key) {
            self.events.push(ev);
        }
    }

    fn unpaired_from(&mut self) {
        if let Some(from) = self.rename_from.take() {
            // Moved out of the watched tree (or the `To` half never came): the path is gone.
            self.touch(PathEvent::Removed(from));
        }
    }

    fn add(&mut self, ev: FsEv) {
        if !matches!(ev, FsEv::RenameTo(_)) && !matches!(ev, FsEv::RenameFrom(_)) {
            self.unpaired_from();
        }
        match ev {
            FsEv::Changed(v) => v
                .into_iter()
                .for_each(|r| self.touch(PathEvent::Changed(r))),
            FsEv::Removed(v) => v
                .into_iter()
                .for_each(|r| self.touch(PathEvent::Removed(r))),
            FsEv::RenameFrom(r) => {
                self.unpaired_from();
                self.rename_from = Some(r);
            }
            FsEv::RenameTo(to) => match self.rename_from.take() {
                Some(from) => self.events.push(PathEvent::Renamed { from, to }),
                None => self.touch(PathEvent::Changed(to)),
            },
            FsEv::Renamed(from, to) => self.events.push(PathEvent::Renamed { from, to }),
            FsEv::Rescan | FsEv::Trouble => self.rescan = true,
        }
    }

    fn take(mut self) -> Vec<PathEvent> {
        self.unpaired_from();
        self.events
    }
}

/// One live notify watch.
struct Watch {
    root: PathBuf,
    recursive: bool,
    generation: u64,
    armed_at: Instant,
    _watcher: notify::RecommendedWatcher,
}

struct Shared {
    rec: Arc<Reconciler>,
    wake: Arc<Notify>,
    dirty: Arc<Notify>,
    cfg: WatchConfig,
    tx: mpsc::Sender<Raw>,
    watches: Mutex<HashMap<FolderId, Watch>>,
    overflowed: Arc<Mutex<HashSet<FolderId>>>,
    next_gen: AtomicU64,
    counters: Arc<Counters>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Handle of the running watcher (see [`crate::SyncService::start_watcher`]).
pub struct WatcherHandle {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for WatcherHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatcherHandle")
            .field("watched", &self.watched().len())
            .finish_non_exhaustive()
    }
}

impl WatcherHandle {
    /// Folders currently watched.
    pub fn watched(&self) -> Vec<FolderId> {
        let mut v: Vec<FolderId> = lock(&self.shared.watches).keys().copied().collect();
        v.sort();
        v
    }

    pub fn is_watching(&self, folder: FolderId) -> bool {
        lock(&self.shared.watches).contains_key(&folder)
    }

    /// Re-read folders now (the service does this itself after its own changes).
    pub fn refresh(&self) {
        self.shared.dirty.notify_one();
    }

    /// Feeds a path event through the debouncer as if the OS had reported it (tools, tests).
    /// Returns `false` if the queue was full (the folder is then rescanned).
    pub fn inject(&self, folder: FolderId, ev: PathEvent) -> bool {
        match self.shared.tx.try_send(Raw::Inject { folder, ev }) {
            Ok(()) => true,
            Err(_) => {
                lock(&self.shared.overflowed).insert(folder);
                false
            }
        }
    }

    /// Marks a folder as if its OS event buffer had overflowed (diagnostics, tests): it is
    /// rescanned at the next tick.
    pub fn simulate_overflow(&self, folder: FolderId) {
        lock(&self.shared.overflowed).insert(folder);
    }

    pub fn counters(&self) -> WatchCounters {
        self.shared.counters.snapshot()
    }

    /// Waits for the watcher to stop (after its cancel token fired). All watches are dropped.
    pub async fn join(self) {
        let _ = self.task.await;
        lock(&self.shared.watches).clear();
    }
}

/// Starts the watcher task. See [`crate::SyncService::start_watcher`].
pub(crate) fn spawn(
    rec: Arc<Reconciler>,
    wake: Arc<Notify>,
    dirty: Arc<Notify>,
    cfg: WatchConfig,
    cancel: CancellationToken,
) -> WatcherHandle {
    let (tx, rx) = mpsc::channel(cfg.queue_capacity.max(16));
    let shared = Arc::new(Shared {
        rec,
        wake,
        dirty,
        cfg,
        tx,
        watches: Mutex::new(HashMap::new()),
        overflowed: Arc::new(Mutex::new(HashSet::new())),
        next_gen: AtomicU64::new(1),
        counters: Arc::new(Counters::default()),
    });
    let task = tokio::spawn(Arc::clone(&shared).run(rx, cancel));
    WatcherHandle { shared, task }
}

/// Per-run state of the debouncer loop.
#[derive(Default)]
struct Loop {
    pending: HashMap<FolderId, Pending>,
    busy: HashSet<FolderId>,
    /// Folders to re-arm at the next reconcile (after a watch error).
    rearm: HashSet<FolderId>,
    /// Last storm window per folder: the watch is re-armed once the storm is over
    /// ([`STORM_QUIET`]), not once per window.
    storm_at: HashMap<FolderId, Instant>,
    /// Folders whose root must be probed at the next reconcile.
    probe: HashSet<FolderId>,
    /// `unsupported` folders already retried this run.
    retried: HashSet<FolderId>,
}

impl Shared {
    async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<Raw>, cancel: CancellationToken) {
        let mut st = Loop::default();
        let mut tasks: JoinSet<(FolderId, Result<EventBatch>)> = JoinSet::new();
        let mut tick = tokio::time::interval(self.cfg.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_reconcile = Instant::now();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                Some(raw) = rx.recv() => {
                    self.ingest(&mut st, raw);
                    // Drain what is already queued before looking at timers.
                    while let Ok(raw) = rx.try_recv() {
                        self.ingest(&mut st, raw);
                    }
                }
                _ = tick.tick() => {}
                () = self.dirty.notified() => next_reconcile = Instant::now(),
                Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Ok((folder, res)) = done {
                        st.busy.remove(&folder);
                        match res {
                            Ok(b) => {
                                if b.scan_requested
                                    || b.index_enqueued + b.index_coalesced + b.deletes > 0
                                {
                                    self.wake.notify_one();
                                }
                            }
                            Err(e) => {
                                tracing::warn!(folder = %folder, error = %e, "watcher batch failed; rescanning");
                                st.pending.entry(folder).or_insert_with(|| Pending::new(Instant::now())).rescan = true;
                            }
                        }
                    }
                }
            }
            let overflowed: Vec<FolderId> = lock(&self.overflowed).drain().collect();
            for f in overflowed {
                bump(&self.counters.overflows);
                st.pending
                    .entry(f)
                    .or_insert_with(|| Pending::new(Instant::now()))
                    .rescan = true;
            }
            self.flush_ready(&mut st, &mut tasks);
            if Instant::now() >= next_reconcile {
                self.reconcile(&mut st).await;
                next_reconcile = Instant::now() + self.cfg.probe_every;
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        lock(&self.watches).clear();
    }

    fn ingest(&self, st: &mut Loop, raw: Raw) {
        let now = Instant::now();
        match raw {
            Raw::Fs {
                folder,
                generation,
                ev,
            } => {
                let current = lock(&self.watches).get(&folder).map(|w| w.generation);
                if current != Some(generation) {
                    return; // a watch we dropped meanwhile
                }
                if matches!(ev, FsEv::Trouble) {
                    st.probe.insert(folder);
                    st.rearm.insert(folder);
                }
                let p = st
                    .pending
                    .entry(folder)
                    .or_insert_with(|| Pending::new(now));
                p.last = now;
                p.raw += 1;
                p.add(ev);
            }
            Raw::Inject { folder, ev } => {
                let p = st
                    .pending
                    .entry(folder)
                    .or_insert_with(|| Pending::new(now));
                p.last = now;
                p.raw += 1;
                match ev {
                    PathEvent::Rescan => p.rescan = true,
                    PathEvent::Renamed { from, to } => p.add(FsEv::Renamed(from, to)),
                    other => p.touch(other),
                }
            }
        }
    }

    fn flush_ready(
        self: &Arc<Self>,
        st: &mut Loop,
        tasks: &mut JoinSet<(FolderId, Result<EventBatch>)>,
    ) {
        let now = Instant::now();
        let ready: Vec<FolderId> = st
            .pending
            .iter()
            .filter(|(f, p)| {
                !st.busy.contains(f)
                    && (now.duration_since(p.last) >= self.cfg.debounce
                        || now.duration_since(p.first) >= self.cfg.max_delay)
            })
            .map(|(f, _)| *f)
            .collect();
        for folder in ready {
            let Some(mut p) = st.pending.remove(&folder) else {
                continue;
            };
            if p.raw >= self.cfg.storm_events.max(1) {
                // A burst big enough to overflow the OS buffer (notify 8.2 would not tell us):
                // one rescan covers it, and the watch is re-armed after the storm in case it died.
                bump(&self.counters.storms);
                p.rescan = true;
                st.storm_at.insert(folder, now);
            }
            let events = if p.rescan {
                bump(&self.counters.rescans);
                vec![PathEvent::Rescan]
            } else {
                p.take()
            };
            if events.is_empty() {
                continue;
            }
            bump(&self.counters.batches);
            self.counters
                .events_applied
                .fetch_add(events.len() as u64, Ordering::Relaxed);
            st.busy.insert(folder);
            let rec = Arc::clone(&self.rec);
            tasks.spawn(async move {
                let r = tokio::task::spawn_blocking(move || rec.on_path_events(folder, &events))
                    .await
                    .map_err(|e| {
                        sr_common::error::Error::new(sr_common::error::ErrorCode::Internal)
                            .with_source(e)
                    })
                    .and_then(|r| r);
                (folder, r)
            });
        }
    }

    /// Creates a notify watch for one folder.
    fn arm(&self, f: &FolderRecord, generation: u64) -> notify::Result<Watch> {
        let tx = self.tx.clone();
        let root = f.root_path.clone();
        let folder = f.id;
        let overflowed = Arc::clone(&self.overflowed);
        let counters = Arc::clone(&self.counters);
        let cb_root = root.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            counters.raw_events.fetch_add(1, Ordering::Relaxed);
            let ev = match res {
                Ok(e) => match classify(&cb_root, &e) {
                    Some(ev) => ev,
                    None => return,
                },
                Err(_) => FsEv::Trouble,
            };
            if tx
                .try_send(Raw::Fs {
                    folder,
                    generation,
                    ev,
                })
                .is_err()
            {
                // Never block the OS thread: drop the event and rescan the folder instead.
                lock(&overflowed).insert(folder);
            }
        })?;
        let mode = if f.settings.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        w.watch(&root, mode)?;
        Ok(Watch {
            root,
            recursive: f.settings.recursive,
            generation,
            armed_at: Instant::now(),
            _watcher: w,
        })
    }

    /// Brings the watch set in line with the catalog, re-arms where needed, probes roots and
    /// reconnects folders whose root is back.
    async fn reconcile(&self, st: &mut Loop) {
        let all = match self.rec.catalog().read_async(folders::folders).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "watcher could not read folders");
                return;
            }
        };
        let ids: HashSet<FolderId> = all.iter().map(|f| f.id).collect();
        // Folders gone from the catalog.
        lock(&self.watches).retain(|id, _| ids.contains(id));
        for f in &all {
            let b = behavior_of(f);
            // An `unsupported` folder gets one more try per daemon run (another file system
            // driver, a transient error). Network drives never get a watcher.
            let retry_unsupported = f.watch_support == WatchSupport::Unsupported
                && b.allows_watcher_events
                && !crate::reconcile::is_network_share(&f.root_path)
                && st.retried.insert(f.id);
            let eligible = (b.watch || retry_unsupported) && !f.paused;
            if !eligible {
                lock(&self.watches).remove(&f.id);
                continue;
            }
            if !f.available {
                lock(&self.watches).remove(&f.id);
                self.try_reconnect(f).await;
                continue;
            }
            let (have, stale, due) = match lock(&self.watches).get(&f.id) {
                Some(w) => (
                    true,
                    w.root != f.root_path || w.recursive != f.settings.recursive,
                    w.armed_at.elapsed() >= self.cfg.rearm_every,
                ),
                None => (false, false, false),
            };
            let probe = st.probe.remove(&f.id);
            if have && (probe || !f.root_path.is_dir()) {
                // Root gone, renamed or replaced: validate it properly.
                if !self.root_ok(f).await {
                    bump(&self.counters.roots_lost);
                    lock(&self.watches).remove(&f.id);
                    continue;
                }
            }
            let storm_over = st
                .storm_at
                .get(&f.id)
                .is_some_and(|t| t.elapsed() >= STORM_QUIET);
            if storm_over {
                st.storm_at.remove(&f.id);
            }
            let rearm = st.rearm.remove(&f.id) || storm_over || due;
            if have && !stale && !rearm {
                continue;
            }
            let generation = match lock(&self.watches).get(&f.id) {
                Some(w) if !stale => w.generation,
                _ => self.next_gen.fetch_add(1, Ordering::Relaxed),
            };
            // New watch first, then drop the old one (no gap while re-arming).
            match self.arm(f, generation) {
                Ok(w) => {
                    if have {
                        bump(&self.counters.rearms);
                    }
                    lock(&self.watches).insert(f.id, w);
                    if f.watch_support == WatchSupport::Unsupported {
                        self.set_support(f.id, WatchSupport::Native).await;
                    }
                    if !have && !retry_unsupported {
                        tracing::debug!(folder = %f.id, "watching");
                    }
                }
                Err(e) => {
                    bump(&self.counters.watch_errors);
                    lock(&self.watches).remove(&f.id);
                    if !f.root_path.is_dir() {
                        // Not a file-system limitation: the root went away.
                        let _ = self.root_ok(f).await;
                        continue;
                    }
                    tracing::warn!(folder = %f.id, error = %e, "live updates unavailable for this folder; using scheduled checks");
                    if f.watch_support == WatchSupport::Native {
                        self.set_support(f.id, WatchSupport::Unsupported).await;
                    }
                }
            }
        }
    }

    async fn set_support(&self, folder: FolderId, ws: WatchSupport) {
        let r = self
            .rec
            .catalog()
            .write(move |c| folders::set_watch_support(c, folder, ws, sr_catalog::now_ms()))
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "could not record watch support");
        }
    }

    /// Full root validation (blocking pool). `false` = the folder is now `unavailable`.
    async fn root_ok(&self, f: &FolderRecord) -> bool {
        let rec = Arc::clone(&self.rec);
        let f = f.clone();
        matches!(
            tokio::task::spawn_blocking(move || rec.check_root(&f)).await,
            Ok(Ok(RootCheck::Ok { .. }))
        )
    }

    /// An unavailable, watch-eligible folder: if its root exists again and validates, it becomes
    /// available, a `reconnect` scan catches up, and the next reconcile watches it.
    async fn try_reconnect(&self, f: &FolderRecord) {
        if !f.root_path.is_dir() {
            return;
        }
        let rec = Arc::clone(&self.rec);
        let rec2 = Arc::clone(&self.rec);
        let f2 = f.clone();
        let back = tokio::task::spawn_blocking(move || -> Result<bool> {
            match rec2.check_root(&f2)? {
                RootCheck::Ok { .. } => {
                    rec2.enqueue_scan(f2.id, RunTrigger::Reconnect)?;
                    Ok(true)
                }
                RootCheck::Unavailable(_) => Ok(false),
            }
        })
        .await;
        drop(rec);
        if let Ok(Ok(true)) = back {
            bump(&self.counters.roots_back);
            self.wake.notify_one();
            self.dirty.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn classify_maps_kinds_and_paths() {
        let root = Path::new(r"C:\r");
        let c = |k, p: &[&str]| classify(root, &ev(k, p));
        assert!(matches!(
            c(EventKind::Create(notify::event::CreateKind::Any), &[r"C:\r\a\b.md"]),
            Some(FsEv::Changed(v)) if v == ["a/b.md"]
        ));
        assert!(matches!(
            c(EventKind::Remove(notify::event::RemoveKind::Any), &[r"C:\r\x.md"]),
            Some(FsEv::Removed(v)) if v == ["x.md"]
        ));
        assert!(matches!(
            c(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &[r"C:\r\o.md"]),
            Some(FsEv::RenameFrom(r)) if r == "o.md"
        ));
        assert!(matches!(
            c(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &[r"C:\r\o.md", r"C:\r\n.md"]),
            Some(FsEv::Renamed(a, b)) if a == "o.md" && b == "n.md"
        ));
        assert!(
            c(
                EventKind::Access(notify::event::AccessKind::Any),
                &[r"C:\r\a"]
            )
            .is_none()
        );
        assert!(matches!(c(EventKind::Any, &[r"C:\r"]), Some(FsEv::Trouble)));
        assert!(matches!(
            c(EventKind::Any, &[r"D:\elsewhere"]),
            Some(FsEv::Rescan)
        ));
        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(matches!(classify(root, &rescan), Some(FsEv::Rescan)));
    }

    #[test]
    fn pending_pairs_renames_and_dedupes() {
        let mut p = Pending::new(Instant::now());
        p.add(FsEv::Changed(vec!["a.md".into()]));
        p.add(FsEv::Changed(vec!["A.md".into()]));
        p.add(FsEv::RenameFrom("a.md".into()));
        p.add(FsEv::RenameTo("b.md".into()));
        p.add(FsEv::RenameFrom("gone.md".into()));
        p.add(FsEv::Changed(vec!["c.md".into()]));
        p.add(FsEv::RenameTo("orphan.md".into()));
        p.add(FsEv::RenameFrom("last.md".into()));
        assert_eq!(
            p.take(),
            vec![
                PathEvent::Changed("a.md".into()),
                PathEvent::Renamed {
                    from: "a.md".into(),
                    to: "b.md".into()
                },
                PathEvent::Removed("gone.md".into()),
                PathEvent::Changed("c.md".into()),
                PathEvent::Changed("orphan.md".into()),
                PathEvent::Removed("last.md".into()),
            ]
        );
    }
}
