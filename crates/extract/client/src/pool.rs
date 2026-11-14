//! The worker pool: bounded permits (backpressure), one OS thread per worker slot, the
//! AppContainer → low-IL fallback (Windows; macOS has one mode, Seatbelt, and no fallback),
//! recycling and the outcome → `ExtractError` mapping.

use std::fs::File;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sr_common::ErrorCode;
use sr_common::error::{ErrCtx, Error};
use sr_common::protocol::extract::DocKind;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::sandbox::{AppContainer, JobKiller, TokenKind, WorkerId};
use crate::worker::{NotReady, Outcome, Watchdog, Worker, WorkerSpec};
use crate::{
    ActiveSandbox, Disposition, ExtractConfig, ExtractError, ExtractSource, Extraction,
    FailureCause, PoolStats, SandboxMode, format_bytes,
};

type Reply = Result<Extraction, ExtractError>;

struct JobMsg {
    id: u64,
    source: ExtractSource,
    kind: DocKind,
    reply: oneshot::Sender<Reply>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct Counters {
    jobs_ok: AtomicU64,
    jobs_failed: AtomicU64,
    timeouts: AtomicU64,
    cpu_limit_kills: AtomicU64,
    memory_kills: AtomicU64,
    crashes: AtomicU64,
    protocol_violations: AtomicU64,
    spawns: AtomicU64,
    spawn_failures: AtomicU64,
    recycles: AtomicU64,
    bytes_in: AtomicU64,
    text_bytes_out: AtomicU64,
}

struct ModeState {
    token: TokenKind,
    app_container: Option<Arc<AppContainer>>,
    fallback_reason: Option<String>,
}

struct Shared {
    cfg: ExtractConfig,
    permits: Arc<Semaphore>,
    tx: Mutex<Option<SyncSender<JobMsg>>>,
    shutdown: AtomicBool,
    next_id: AtomicU64,
    counters: Counters,
    mode: Mutex<ModeState>,
    /// Killers of the live worker per slot (shutdown kills them all at once).
    live: Mutex<Vec<Option<(WorkerId, JobKiller)>>>,
    /// Every worker pid ever started (leak checks).
    pids: Mutex<Vec<WorkerId>>,
    pdf_engine_error: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Pool of sandboxed extract workers. Cheap to share behind an `Arc`; dropping it (or
/// [`ExtractPool::shutdown`]) kills every worker.
pub struct ExtractPool {
    shared: Arc<Shared>,
    slots: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for ExtractPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractPool")
            .field("workers", &self.shared.cfg.workers)
            .field("stats", &self.stats())
            .finish()
    }
}

impl ExtractPool {
    /// Validates the configuration, prepares the sandbox (AppContainer profile, install-dir ACL)
    /// and starts the worker slots (each warms up one worker in the background).
    pub fn start(mut cfg: ExtractConfig) -> Result<ExtractPool, Error> {
        // Fail closed: no pool (and so no worker) where the platform has no sandbox yet.
        crate::sandbox::platform_supported()?;
        cfg.workers = cfg.workers.clamp(1, 64);
        cfg.max_file_bytes = cfg
            .max_file_bytes
            .min(sr_common::protocol::extract::MAX_PAYLOAD_BYTES);
        cfg.max_text_bytes = cfg
            .max_text_bytes
            .min(sr_common::protocol::extract::MAX_TEXT_BYTES);
        if !cfg.worker_exe.is_absolute() || !cfg.worker_exe.is_file() {
            return Err(Error::new(ErrorCode::Internal)
                .with_detail("extract worker executable missing or not an absolute path"));
        }
        if cfg.pdfium_dll.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err(Error::new(ErrorCode::Internal).with_detail("pdfium path must be absolute"));
        }
        let mode = prepare_sandbox(&cfg)?;
        if let Some(r) = &mode.fallback_reason {
            tracing::warn!(reason = %r, "extract sandbox: using the low-integrity fallback");
        }
        let n = cfg.workers;
        let (tx, rx) = sync_channel::<JobMsg>(n + cfg.queue);
        let shared = Arc::new(Shared {
            permits: Arc::new(Semaphore::new(n + cfg.queue)),
            cfg,
            tx: Mutex::new(Some(tx)),
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            counters: Counters::default(),
            mode: Mutex::new(mode),
            live: Mutex::new(vec![None; n]),
            pids: Mutex::new(Vec::new()),
            pdf_engine_error: Mutex::new(None),
        });
        let rx = Arc::new(Mutex::new(rx));
        let mut slots = Vec::with_capacity(n);
        for i in 0..n {
            let sh = Arc::clone(&shared);
            let rx = Arc::clone(&rx);
            let h = std::thread::Builder::new()
                .name(format!("sr-extract-slot-{i}"))
                .spawn(move || slot_main(&sh, &rx, i))
                .map_err(|e| {
                    Error::new(ErrorCode::Internal)
                        .with_detail("could not start an extract slot thread")
                        .with_source(e)
                })?;
            slots.push(h);
        }
        Ok(ExtractPool {
            shared,
            slots: Mutex::new(slots),
        })
    }

    /// Extracts the text of `source`. Waits for a permit first (backpressure: at most
    /// `workers + queue` jobs are admitted at once).
    pub async fn extract(&self, source: ExtractSource, kind: DocKind) -> Reply {
        let permit = Arc::clone(&self.shared.permits)
            .acquire_owned()
            .await
            .map_err(|_| shut_down())?;
        self.submit(source, kind, permit)?
            .await
            .map_err(|_| shut_down())?
    }

    /// Like [`ExtractPool::extract`] but fails with `Busy` instead of waiting for a permit.
    pub async fn try_extract(&self, source: ExtractSource, kind: DocKind) -> Reply {
        let permit = Arc::clone(&self.shared.permits)
            .try_acquire_owned()
            .map_err(|e| match e {
                tokio::sync::TryAcquireError::Closed => shut_down(),
                tokio::sync::TryAcquireError::NoPermits => ExtractError::new(
                    Error::new(ErrorCode::Busy).with_detail("extract queue full"),
                    Disposition::Retry,
                    FailureCause::Busy,
                ),
            })?;
        self.submit(source, kind, permit)?
            .await
            .map_err(|_| shut_down())?
    }

    fn submit(
        &self,
        source: ExtractSource,
        kind: DocKind,
        permit: OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<Reply>, ExtractError> {
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(shut_down());
        }
        let (tx, rx) = oneshot::channel();
        let msg = JobMsg {
            id: self.shared.next_id.fetch_add(1, Ordering::Relaxed),
            source,
            kind,
            reply: tx,
            _permit: permit,
        };
        let guard = lock(&self.shared.tx);
        let Some(sender) = guard.as_ref() else {
            return Err(shut_down());
        };
        // Never blocks: the channel holds as many jobs as there are permits.
        match sender.try_send(msg) {
            Ok(()) => Ok(rx),
            Err(TrySendError::Disconnected(_)) => Err(shut_down()),
            Err(TrySendError::Full(_)) => Err(ExtractError::new(
                Error::new(ErrorCode::Busy).with_detail("extract queue full"),
                Disposition::Retry,
                FailureCause::Busy,
            )),
        }
    }

    /// Permits currently free (diagnostics).
    pub fn available_permits(&self) -> usize {
        self.shared.permits.available_permits()
    }

    pub fn stats(&self) -> PoolStats {
        let c = &self.shared.counters;
        let m = lock(&self.shared.mode);
        PoolStats {
            jobs_ok: c.jobs_ok.load(Ordering::Relaxed),
            jobs_failed: c.jobs_failed.load(Ordering::Relaxed),
            timeouts: c.timeouts.load(Ordering::Relaxed),
            cpu_limit_kills: c.cpu_limit_kills.load(Ordering::Relaxed),
            memory_kills: c.memory_kills.load(Ordering::Relaxed),
            crashes: c.crashes.load(Ordering::Relaxed),
            protocol_violations: c.protocol_violations.load(Ordering::Relaxed),
            spawns: c.spawns.load(Ordering::Relaxed),
            spawn_failures: c.spawn_failures.load(Ordering::Relaxed),
            recycles: c.recycles.load(Ordering::Relaxed),
            bytes_in: c.bytes_in.load(Ordering::Relaxed),
            text_bytes_out: c.text_bytes_out.load(Ordering::Relaxed),
            sandbox: Some(active(m.token)),
            fallback_reason: m.fallback_reason.clone(),
            pdf_engine_error: lock(&self.shared.pdf_engine_error).clone(),
        }
    }

    /// Every worker pid this pool has started (leak checks in tests and diagnostics).
    pub fn spawned_pids(&self) -> Vec<u32> {
        lock(&self.shared.pids).iter().map(|w| w.pid).collect()
    }

    /// Every worker this pool has started, as pid-reuse-proof identities.
    pub fn spawned_workers(&self) -> Vec<WorkerId> {
        lock(&self.shared.pids).clone()
    }

    /// Workers this pool started that are still running (after [`ExtractPool::shutdown`] this
    /// must be empty).
    pub fn leaked_workers(&self) -> Vec<WorkerId> {
        lock(&self.shared.pids)
            .iter()
            .copied()
            .filter(WorkerId::is_alive)
            .collect()
    }

    /// Pids of the workers alive right now.
    pub fn live_pids(&self) -> Vec<u32> {
        lock(&self.shared.live)
            .iter()
            .flatten()
            .map(|(id, _)| id.pid)
            .collect()
    }

    /// Identities of the workers alive right now.
    pub fn live_workers(&self) -> Vec<WorkerId> {
        lock(&self.shared.live)
            .iter()
            .flatten()
            .map(|(id, _)| *id)
            .collect()
    }

    /// Kills every worker, fails queued jobs with `Cancelled` and joins the slot threads.
    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.permits.close();
        lock(&self.shared.tx).take();
        for (_, k) in lock(&self.shared.live).iter().flatten() {
            k.kill();
        }
        let slots = std::mem::take(&mut *lock(&self.slots));
        for h in slots {
            let _ = h.join();
        }
    }
}

impl Drop for ExtractPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn active(t: TokenKind) -> ActiveSandbox {
    match t {
        TokenKind::AppContainer => ActiveSandbox::AppContainer,
        TokenKind::LowIntegrity => ActiveSandbox::LowIntegrity,
        #[cfg(not(windows))]
        TokenKind::Seatbelt => ActiveSandbox::Seatbelt,
    }
}

fn shut_down() -> ExtractError {
    ExtractError::new(
        Error::new(ErrorCode::Cancelled).with_detail("extract pool is shutting down"),
        Disposition::Retry,
        FailureCause::ShutDown,
    )
}

/// macOS (and the fail-closed stub elsewhere): one mode, whatever `cfg.sandbox` says. No
/// fallback exists; a worker that can't be sandboxed never starts.
#[cfg(not(windows))]
fn prepare_sandbox(_cfg: &ExtractConfig) -> Result<ModeState, Error> {
    Ok(ModeState {
        token: TokenKind::Seatbelt,
        app_container: None,
        fallback_reason: None,
    })
}

#[cfg(windows)]
fn prepare_sandbox(cfg: &ExtractConfig) -> Result<ModeState, Error> {
    let low = |reason: Option<String>| ModeState {
        token: TokenKind::LowIntegrity,
        app_container: None,
        fallback_reason: reason,
    };
    // The install-dir layout matters for both tokens: if any specific package SID ends up in the
    // DACL (other software, an old per-file grant), Windows checks a low-IL token like an
    // AppContainer and only the ALL APPLICATION PACKAGES ACE grants it (m1-b-extract.md §ACL).
    if cfg.ensure_install_acl {
        let mut dirs: Vec<&std::path::Path> = Vec::new();
        dirs.extend(cfg.worker_exe.parent());
        if let Some(p) = &cfg.pdfium_dll {
            dirs.extend(p.parent());
        }
        dirs.dedup();
        for d in dirs {
            match crate::sandbox::ensure_app_packages_read(d) {
                Ok(true) => {
                    tracing::info!("extract sandbox: granted app packages read on the install dir")
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "extract sandbox: could not check the install dir ACL")
                }
            }
        }
    }
    if cfg.sandbox == SandboxMode::LowIntegrity {
        return Ok(low(None));
    }
    match AppContainer::ensure(&cfg.app_container_name, "Selene Retrieval file reader") {
        Ok(ac) => Ok(ModeState {
            token: TokenKind::AppContainer,
            app_container: Some(Arc::new(ac)),
            fallback_reason: None,
        }),
        // S-04: only a classified "AppContainer unsupported" error leaves the container. Anything
        // else keeps AppContainer mode with the profile still to be created: every worker spawn
        // tries again (with backoff) and never falls back for it.
        Err(e) if cfg.sandbox == SandboxMode::Auto => match classify_ac_error(&e) {
            AcFailure::Unsupported => Ok(low(Some(format!(
                "AppContainer unsupported on this PC (profile): {e}"
            )))),
            AcFailure::Transient => {
                tracing::warn!(error = %e, "extract sandbox: AppContainer profile not ready yet; will retry");
                Ok(ModeState {
                    token: TokenKind::AppContainer,
                    app_container: None,
                    fallback_reason: None,
                })
            }
        },
        Err(e) => Err(Error::new(ErrorCode::Internal)
            .with_detail("could not create the extractor's AppContainer profile")
            .with_source(e)),
    }
}

/// S-04: an AppContainer start failure is either "AppContainer unsupported" (deterministic on
/// this PC: retrying can't help, so an `Auto` pool may fall back) or transient (retry, never
/// fall back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcFailure {
    Unsupported,
    Transient,
}

/// `STATUS_ACCESS_DENIED`, `STATUS_DLL_NOT_FOUND`, `STATUS_DLL_INIT_FAILED`: the loader refused
/// the worker inside the container (ACLs, missing capability). Deterministic.
const AC_LOADER_REFUSALS: [u32; 3] = [0xC000_0022, 0xC000_0135, 0xC000_0142];

/// Classify an AppContainer profile / spawn / ready failure. Timeouts and anything unknown are
/// transient; only errors that say the container itself can't work here are "unsupported".
pub(crate) fn classify_ac_error(e: &std::io::Error) -> AcFailure {
    if let Some(nr) = e.get_ref().and_then(|i| i.downcast_ref::<NotReady>()) {
        return if !nr.timed_out && nr.exit.is_some_and(|c| AC_LOADER_REFUSALS.contains(&c)) {
            AcFailure::Unsupported
        } else {
            AcFailure::Transient
        };
    }
    // ERROR_ACCESS_DENIED (5), ERROR_NOT_SUPPORTED (50), ERROR_INVALID_PARAMETER (87),
    // ERROR_CALL_NOT_IMPLEMENTED (120): matched by code, because std leaves ERROR_NOT_SUPPORTED
    // uncategorised (M5 tester: it was classified transient, so such a PC never fell back).
    if matches!(e.raw_os_error(), Some(5 | 50 | 87 | 120)) {
        return AcFailure::Unsupported;
    }
    match e.kind() {
        // The same failures from APIs that report them by kind (an OS without the AppContainer
        // process attribute).
        std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::Unsupported
        | std::io::ErrorKind::InvalidInput => AcFailure::Unsupported,
        _ => AcFailure::Transient,
    }
}

/// Backoff between AppContainer attempts within one spawn (then the spawn reports failure and
/// the next one starts over).
pub(crate) const AC_BACKOFF: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcStep {
    Retry(Duration),
    /// "Unsupported" twice in a row: an `Auto` pool moves to low integrity.
    FallBack,
    GiveUp,
}

/// What to do after the AppContainer failures of one spawn so far (oldest first). Pure.
pub(crate) fn ac_step(history: &[AcFailure]) -> AcStep {
    let n = history.len();
    if n >= 2
        && history[n - 2..]
            .iter()
            .all(|f| *f == AcFailure::Unsupported)
    {
        return AcStep::FallBack;
    }
    match n.checked_sub(1).and_then(|i| AC_BACKOFF.get(i)) {
        Some(d) => AcStep::Retry(*d),
        None => AcStep::GiveUp,
    }
}

impl Shared {
    fn spec(&self, token: TokenKind, ac: Option<Arc<AppContainer>>) -> WorkerSpec {
        WorkerSpec {
            exe: self.cfg.worker_exe.clone(),
            pdfium: self.cfg.pdfium_dll.clone(),
            token,
            app_container: ac,
            memory_limit: self.cfg.memory_limit,
            spawn_timeout: self.cfg.spawn_timeout,
        }
    }

    fn fall_back(&self, reason: String) {
        let mut m = lock(&self.mode);
        if m.token == TokenKind::AppContainer {
            tracing::warn!(reason = %reason, "extract sandbox: falling back to low integrity");
            m.token = TokenKind::LowIntegrity;
            m.fallback_reason = Some(reason);
        }
    }

    /// Starts a worker with the current token (S-04 policy, [`ac_step`]):
    /// * a transient AppContainer failure (spawn timeout, a worker that exited for another
    ///   reason, a profile not created yet) is retried with backoff, then reported; the next
    ///   spawn tries the AppContainer again. It never falls back;
    /// * only "AppContainer unsupported" ([`classify_ac_error`]: access denied, not supported,
    ///   the loader refused the container, pdfium can't load in it), seen twice in a row, moves
    ///   an `Auto` pool to low integrity, shown in diagnostics and status
    ///   ([`PoolStats::fallback_reason`]).
    fn spawn_worker(&self, wd: &Watchdog) -> Result<Worker, String> {
        let res = self.spawn_with_policy(wd);
        match res {
            Ok(w) => {
                self.counters.spawns.fetch_add(1, Ordering::Relaxed);
                lock(&self.pids).push(w.id());
                lock(&self.pdf_engine_error).clone_from(&w.ready.pdf_engine_error);
                if !w.ready.pdf_engine && self.cfg.pdfium_dll.is_some() {
                    tracing::warn!(
                        error = ?w.ready.pdf_engine_error,
                        "extract worker started without pdfium"
                    );
                }
                tracing::debug!(
                    pid = w.pid(),
                    token = ?w.token,
                    integrity = %w.ready.integrity,
                    app_container = w.ready.app_container,
                    ms = w.spawn_time.as_millis() as u64,
                    "extract worker ready"
                );
                Ok(w)
            }
            Err(e) => {
                self.counters.spawn_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, "extract worker failed to start");
                Err(e.to_string())
            }
        }
    }

    fn spawn_with_policy(&self, wd: &Watchdog) -> std::io::Result<Worker> {
        let auto = self.cfg.sandbox == SandboxMode::Auto;
        let mut history: Vec<AcFailure> = Vec::new();
        loop {
            let (token, ac) = {
                let m = lock(&self.mode);
                (m.token, m.app_container.clone())
            };
            if token != TokenKind::AppContainer {
                return Worker::spawn(&self.spec(token, None), wd);
            }
            let attempt = match ac {
                Some(ac) => Ok(ac),
                None => AppContainer::ensure(
                    &self.cfg.app_container_name,
                    "Selene Retrieval file reader",
                )
                .map(|ac| {
                    let ac = Arc::new(ac);
                    lock(&self.mode).app_container = Some(Arc::clone(&ac));
                    ac
                }),
            }
            .and_then(|ac| Worker::spawn(&self.spec(token, Some(ac)), wd));
            let (failure, err) = match attempt {
                Ok(w) if auto && self.cfg.pdfium_dll.is_some() && !w.ready.pdf_engine => {
                    let why = w.ready.pdf_engine_error.clone().unwrap_or_default();
                    drop(w);
                    (
                        AcFailure::Unsupported,
                        std::io::Error::other(format!(
                            "pdfium did not load in the AppContainer: {why}"
                        )),
                    )
                }
                Ok(w) => return Ok(w),
                Err(e) => {
                    self.counters.spawn_failures.fetch_add(1, Ordering::Relaxed);
                    (classify_ac_error(&e), e)
                }
            };
            history.push(failure);
            tracing::warn!(error = %err, ?failure, attempt = history.len(), "extract worker did not start in the AppContainer");
            match ac_step(&history) {
                AcStep::FallBack if auto => {
                    self.fall_back(format!("AppContainer unsupported on this PC: {err}"));
                }
                AcStep::Retry(d) if !self.shutdown.load(Ordering::Acquire) => {
                    std::thread::sleep(d);
                }
                _ => return Err(err),
            }
        }
    }

    fn set_live(&self, slot: usize, w: Option<&Worker>) {
        if let Some(e) = lock(&self.live).get_mut(slot) {
            *e = w.map(|w| (w.id(), w.killer()));
        }
    }
}

fn slot_main(shared: &Arc<Shared>, rx: &Arc<Mutex<Receiver<JobMsg>>>, slot: usize) {
    let wd = match Watchdog::new() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "extract slot: no watchdog thread; slot disabled");
            return;
        }
    };
    let mut worker: Option<Worker> = None;
    if !shared.shutdown.load(Ordering::Acquire) {
        worker = shared.spawn_worker(&wd).ok();
        shared.set_live(slot, worker.as_ref());
    }
    loop {
        let msg = {
            let r = lock(rx);
            match r.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        let reply = if shared.shutdown.load(Ordering::Acquire) {
            Err(shut_down())
        } else {
            run_job(shared, &wd, &mut worker, slot, msg.id, msg.source, msg.kind)
        };
        let _ = msg.reply.send(reply);
    }
    shared.set_live(slot, None);
    drop(worker);
}

fn open_source(src: ExtractSource) -> Result<(Box<dyn Read>, u64), std::io::Error> {
    Ok(match src {
        ExtractSource::Path(p) => {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            (Box::new(f), len)
        }
        ExtractSource::File(mut f) => {
            use std::io::Seek;
            let pos = f.stream_position().unwrap_or(0);
            let len = f.metadata()?.len().saturating_sub(pos);
            (Box::new(f), len)
        }
        ExtractSource::Bytes(b) => {
            let len = b.len() as u64;
            (Box::new(Cursor::new(b)), len)
        }
    })
}

fn run_job(
    shared: &Shared,
    wd: &Watchdog,
    worker: &mut Option<Worker>,
    slot: usize,
    id: u64,
    source: ExtractSource,
    kind: DocKind,
) -> Reply {
    let cfg = &shared.cfg;
    let c = &shared.counters;
    let t0 = Instant::now();
    let fail = |e: ExtractError| {
        c.jobs_failed.fetch_add(1, Ordering::Relaxed);
        Err(e)
    };
    let (mut reader, len) = match open_source(source) {
        Ok(x) => x,
        Err(e) => {
            return fail(ExtractError::new(
                Error::from(e),
                Disposition::Retry,
                FailureCause::Source,
            ));
        }
    };
    if len > cfg.max_file_bytes {
        return fail(ExtractError::new(
            Error::new(ErrorCode::FileTooLarge).with_ctx(ErrCtx {
                limit: Some(format_bytes(cfg.max_file_bytes)),
                detail: Some(format!("{len} bytes")),
                ..ErrCtx::default()
            }),
            Disposition::DeadLetter,
            FailureCause::TooLarge,
        ));
    }
    if worker.is_none() {
        match shared.spawn_worker(wd) {
            Ok(w) => *worker = Some(w),
            Err(e) => {
                return fail(ExtractError::new(
                    Error::new(ErrorCode::Internal)
                        .with_detail(format!("extract worker could not start: {e}")),
                    Disposition::Retry,
                    FailureCause::SpawnFailed,
                ));
            }
        }
        shared.set_live(slot, worker.as_ref());
    }
    let Some(w) = worker.as_mut() else {
        return fail(ExtractError::new(
            Error::new(ErrorCode::Internal).with_detail("no extract worker"),
            Disposition::Retry,
            FailureCause::SpawnFailed,
        ));
    };
    let timeout = cfg.timeout_for(len);
    let outcome = w.request(
        wd,
        id,
        kind,
        &mut reader,
        len,
        cfg.max_text_bytes,
        timeout,
        timeout.saturating_mul(2),
    );
    drop(reader);
    let sandbox = active(w.token);
    let peak = w.peak_memory();
    let recycle = !outcome.worker_unusable()
        && (peak > cfg.recycle_peak_bytes
            || w.jobs_done >= cfg.recycle_after_jobs
            || w.needs_recycle());
    if outcome.worker_unusable() || recycle {
        if recycle {
            c.recycles.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                peak_mib = peak >> 20,
                jobs = w.jobs_done,
                "recycling extract worker"
            );
        }
        shared.set_live(slot, None);
        *worker = None; // Drop kills the job.
    }
    c.bytes_in.fetch_add(len, Ordering::Relaxed);
    let dead = |code: ErrorCode, cause: FailureCause, detail: String| {
        ExtractError::new(
            Error::new(code).with_detail(detail),
            Disposition::DeadLetter,
            cause,
        )
    };
    match outcome {
        Outcome::Ok {
            text,
            detected,
            encoding,
            pages,
            truncated,
        } => {
            c.jobs_ok.fetch_add(1, Ordering::Relaxed);
            c.text_bytes_out
                .fetch_add(text.len() as u64, Ordering::Relaxed);
            Ok(Extraction {
                text,
                detected,
                encoding,
                pages,
                truncated,
                bytes_in: len,
                elapsed: t0.elapsed(),
                sandbox,
            })
        }
        Outcome::Failed { code, detail } => fail(ExtractError::new(
            Error::new(code.error_code()).with_detail(detail),
            if code.is_file_fault() {
                Disposition::DeadLetter
            } else {
                Disposition::Retry
            },
            FailureCause::Worker(code),
        )),
        Outcome::Timeout { memory_capped } => {
            c.timeouts.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                job = id,
                secs = timeout.as_secs(),
                memory_capped,
                "extract timeout; worker killed"
            );
            fail(dead(
                ErrorCode::ExtractTimeout,
                FailureCause::Timeout { memory_capped },
                format!("no result after {} s", timeout.as_secs()),
            ))
        }
        Outcome::CpuLimit => {
            c.cpu_limit_kills.fetch_add(1, Ordering::Relaxed);
            fail(dead(
                ErrorCode::ExtractTimeout,
                FailureCause::CpuLimit,
                "CPU budget exhausted".into(),
            ))
        }
        Outcome::MemoryLimit => {
            c.memory_kills.fetch_add(1, Ordering::Relaxed);
            fail(dead(
                ErrorCode::FileUnreadable,
                FailureCause::MemoryLimit,
                format!("memory limit ({}) reached", format_bytes(cfg.memory_limit)),
            ))
        }
        Outcome::Crashed { exit_code } => {
            c.crashes.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(job = id, exit = ?exit_code, "extract worker crashed");
            fail(dead(
                ErrorCode::FileUnreadable,
                FailureCause::Crashed { exit_code },
                format!(
                    "reader stopped unexpectedly (exit {})",
                    exit_code.map_or("unknown".into(), |c| format!("0x{c:X}"))
                ),
            ))
        }
        Outcome::Protocol(why) => {
            c.protocol_violations.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(job = id, why = %why, "extract worker broke the protocol; killed");
            fail(dead(
                ErrorCode::FileUnreadable,
                FailureCause::ProtocolViolation,
                "reader returned an invalid result".into(),
            ))
        }
        Outcome::Source(e) => {
            let err = if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::new(ErrorCode::FileLocked).with_detail(e.to_string())
            } else {
                Error::from(e)
            };
            fail(ExtractError::new(
                err,
                Disposition::Retry,
                FailureCause::Source,
            ))
        }
    }
}

#[cfg(test)]
mod s04_tests {
    use super::*;
    use std::io;

    fn not_ready(timed_out: bool, exit: Option<u32>) -> io::Error {
        io::Error::other(NotReady {
            timed_out,
            exit,
            msg: "x".into(),
        })
    }

    #[test]
    fn only_deterministic_container_failures_are_unsupported() {
        use AcFailure::{Transient as T, Unsupported as U};
        // A slow start is never "unsupported", whatever the exit code.
        assert_eq!(classify_ac_error(&not_ready(true, None)), T);
        assert_eq!(classify_ac_error(&not_ready(true, Some(0xC000_0022))), T);
        // The loader refused the container.
        assert_eq!(classify_ac_error(&not_ready(false, Some(0xC000_0022))), U);
        assert_eq!(classify_ac_error(&not_ready(false, Some(0xC000_0135))), U);
        // A crash or a closed pipe for another reason: transient.
        assert_eq!(classify_ac_error(&not_ready(false, Some(0xC000_0005))), T);
        assert_eq!(classify_ac_error(&not_ready(false, None)), T);
        assert_eq!(classify_ac_error(&io::Error::from_raw_os_error(5)), U);
        assert_eq!(classify_ac_error(&io::Error::from_raw_os_error(50)), U);
        assert_eq!(classify_ac_error(&io::Error::from_raw_os_error(87)), U);
        assert_eq!(
            classify_ac_error(&io::Error::new(
                io::ErrorKind::PermissionDenied,
                "CreateProcess"
            )),
            U
        );
        assert_eq!(classify_ac_error(&io::Error::from_raw_os_error(1455)), T); // commit limit
        assert_eq!(classify_ac_error(&io::Error::other("bad ready frame")), T);
        assert_eq!(classify_ac_error(&io::ErrorKind::TimedOut.into()), T);
    }

    #[test]
    fn transient_failures_back_off_and_never_fall_back() {
        use AcFailure::{Transient as T, Unsupported as U};
        assert_eq!(ac_step(&[T]), AcStep::Retry(AC_BACKOFF[0]));
        assert_eq!(ac_step(&[T, T]), AcStep::Retry(AC_BACKOFF[1]));
        assert_eq!(ac_step(&[T, T, T]), AcStep::GiveUp);
        // One "unsupported" alone is retried; twice in a row falls back.
        assert_eq!(ac_step(&[U]), AcStep::Retry(AC_BACKOFF[0]));
        assert_eq!(ac_step(&[U, U]), AcStep::FallBack);
        assert_eq!(ac_step(&[T, U, U]), AcStep::FallBack);
        assert_eq!(ac_step(&[U, T]), AcStep::Retry(AC_BACKOFF[1]));
        assert_eq!(ac_step(&[U, T, U]), AcStep::GiveUp);
        assert_eq!(ac_step(&[]), AcStep::GiveUp);
    }
}
