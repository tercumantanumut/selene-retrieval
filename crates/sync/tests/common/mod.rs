//! Shared fixtures: a real sr-catalog on a temp dir, a fake `JobExecutor`, helpers to run the
//! claim loop until the queue is empty.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sr_catalog::repo::chunks::{self, FileChunks, NewChunk};
use sr_catalog::repo::jobs::RetryPolicy;
use sr_catalog::repo::{files, folders, jobs, profiles};
use sr_catalog::{Catalog, Job, JobKind};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::events::{EngineEvent, EventLog, Replay, Seq};
use sr_common::ids::{ChunkCfgFp, CollectionId, ContentHash, FolderId};
use sr_common::sandbox::DangerousRoots;
use sr_common::tier::{MINIMUM, TierProfile};
use sr_sync::{AddFolder, JobExecutor, JobOutcome, SyncConfig, SyncService};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub struct Fx {
    pub cat: Arc<Catalog>,
    pub collection: CollectionId,
    pub events: Arc<EventLog<EngineEvent>>,
    pub db_path: PathBuf,
    /// Folders live here.
    pub data: tempfile::TempDir,
    pub db_dir: tempfile::TempDir,
}

pub fn setup() -> Fx {
    let db_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("catalog.db");
    let cat = Arc::new(Catalog::open(&db_path).unwrap());
    let collection = cat
        .write_blocking(|c| {
            let p: &TierProfile = &MINIMUM;
            let spec = profiles::ProfileSpec::local(p, &p.cpu_file);
            let pid = profiles::ensure_profile(c, &spec, sr_catalog::now_ms())?;
            profiles::ensure_default_collection(c, pid, sr_catalog::now_ms())
        })
        .unwrap();
    Fx {
        cat,
        collection,
        events: Arc::new(EventLog::new(4096)),
        db_path,
        data,
        db_dir,
    }
}

impl Fx {
    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.data.path().join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn epoch(&self) -> i64 {
        self.cat.start_epoch().unwrap().epoch
    }

    pub fn service<E: JobExecutor + 'static>(
        &self,
        exec: Arc<E>,
        epoch: i64,
        cfg: SyncConfig,
    ) -> SyncService<E> {
        let sink: Arc<dyn sr_sync::EventSink> = self.events.clone();
        SyncService::new(Arc::clone(&self.cat), exec, sink, epoch, cfg)
    }

    pub fn events(&self) -> Vec<Seq<EngineEvent>> {
        let c = sr_common::events::Cursor {
            stream: self.events.stream(),
            seq: 0,
        };
        match self.events.replay_since(c) {
            Replay::Events(v) => v,
            Replay::Resnapshot => Vec::new(),
        }
    }

    pub fn folder(&self, id: FolderId) -> folders::FolderRecord {
        self.cat.read(|c| folders::folder(c, id)).unwrap()
    }

    pub fn file_paths(&self, id: FolderId) -> Vec<String> {
        let mut v: Vec<String> = self
            .cat
            .read(|c| {
                let mut out = Vec::new();
                for st in sr_catalog::FileState::ALL {
                    out.extend(files::files_in_state(c, id, *st)?);
                }
                Ok(out)
            })
            .unwrap()
            .into_iter()
            .map(|r| r.rel_path)
            .collect();
        v.sort();
        v
    }

    pub fn live_jobs(&self, folder: Option<FolderId>) -> Vec<Job> {
        self.cat.read(|c| jobs::live_jobs(c, folder)).unwrap()
    }

    pub fn queue_depth(&self) -> (u64, u64) {
        self.cat.read(jobs::queue_depth).unwrap()
    }
}

/// Fast timers, no protected locations except drive roots.
pub fn fast_cfg() -> SyncConfig {
    SyncConfig {
        pipeline_permits: 4,
        max_concurrent_scans: 2,
        claim_batch: 64,
        retry: RetryPolicy {
            max_attempts: 3,
            base_ms: 5,
            max_ms: 20,
        },
        aging_ms: 0,
        idle_wait: Duration::from_millis(10),
        progress_every: Duration::from_millis(0),
        drain_timeout: Duration::from_secs(2),
        dangerous: DangerousRoots::none(),
        mcp_add_folder_enabled: false,
        approval_ttl: Duration::from_secs(600),
        // Production debounce (≤ 500 ms); faster probes so root loss shows up quickly in tests.
        watch: sr_sync::WatchConfig {
            probe_every: Duration::from_millis(300),
            ..sr_sync::WatchConfig::default()
        },
    }
}

/// Live index_file jobs per file (a file must never have more than one).
pub fn live_index_jobs_by_file(
    fx: &Fx,
    folder: FolderId,
) -> std::collections::HashMap<sr_common::ids::FileId, usize> {
    let mut m = std::collections::HashMap::new();
    for j in fx.live_jobs(Some(folder)) {
        if j.kind == JobKind::IndexFile
            && let Some(f) = j.file_id
        {
            *m.entry(f).or_insert(0) += 1;
        }
    }
    m
}

/// Marks every pending file of `folder` indexed at its current content in one transaction and
/// clears the queue (a fast baseline for big tests; same writes as `FakeExec::index`).
pub fn index_all_now(fx: &Fx, folder: FolderId) {
    let cat = Arc::clone(&fx.cat);
    let root = fx.folder(folder).root_path;
    let pending = cat
        .read(move |c| files::files_in_state(c, folder, sr_catalog::FileState::PendingIndex))
        .unwrap();
    let batch: Vec<FileChunks> = pending
        .iter()
        .map(|r| {
            let bytes = std::fs::read(root.join(&r.rel_path)).unwrap();
            FileChunks {
                file_id: r.id,
                content_hash: ContentHash::of_bytes(&bytes),
                chunk_cfg_fp: ChunkCfgFp::compute("t", "{}", "tok", "1"),
                extractor_version: "1".into(),
                chunks: vec![NewChunk {
                    ord: 0,
                    byte_start: 0,
                    byte_end: bytes.len() as u64,
                    line_start: 1,
                    line_end: 1,
                    tokens: 1,
                    text: String::from_utf8_lossy(&bytes).into_owned(),
                }],
            }
        })
        .collect();
    fx.cat
        .write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                let now = sr_catalog::now_ms();
                for fc in &batch {
                    let id = fc.file_id;
                    let t1 = chunks::write_file_chunks(c, fc, now)?;
                    chunks::finalize_file(c, id, t1.file_version, now)?;
                }
                c.execute("DELETE FROM jobs WHERE folder_id = ?1", [folder.to_bytes()])
                    .map_err(sr_catalog::map_db_err)?;
                Ok(())
            })
        })
        .unwrap();
}

pub fn add(svc_rec: &sr_sync::Reconciler, fx: &Fx, root: &Path) -> FolderId {
    svc_rec
        .add_folder(&AddFolder::new(fx.collection, root))
        .unwrap()
}

pub fn write(root: &Path, rel: &str, content: &str) -> PathBuf {
    let p = root.join(rel);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(&p, content).unwrap();
    p
}

pub fn set_mtime(p: &Path, t: SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

pub fn hour_ago() -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600)
}

/// Behaviour of the fake executor per job.
pub type Behave = dyn Fn(&Job, &Path) -> Result<JobOutcome> + Send + Sync;

/// Fake `JobExecutor`: indexes for real (T1 + T2 into the catalog) unless `behave` says
/// otherwise; tracks concurrency and order; optionally blocks on a gate.
pub struct FakeExec {
    pub cat: Arc<Catalog>,
    pub behave: Mutex<Option<Arc<Behave>>>,
    pub gate: Option<Arc<Semaphore>>,
    pub delay: Duration,
    pub running: AtomicUsize,
    pub max_running: AtomicUsize,
    pub max_leased: AtomicUsize,
    pub order: Mutex<Vec<(JobKind, Option<FolderId>, String)>>,
}

impl FakeExec {
    pub fn new(cat: &Arc<Catalog>) -> Self {
        Self {
            cat: Arc::clone(cat),
            behave: Mutex::new(None),
            gate: None,
            delay: Duration::ZERO,
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
            max_leased: AtomicUsize::new(0),
            order: Mutex::new(Vec::new()),
        }
    }

    pub fn with_behave(
        self,
        f: impl Fn(&Job, &Path) -> Result<JobOutcome> + Send + Sync + 'static,
    ) -> Self {
        *self.behave.lock().unwrap() = Some(Arc::new(f));
        self
    }

    pub fn order(&self) -> Vec<(JobKind, Option<FolderId>, String)> {
        self.order.lock().unwrap().clone()
    }

    fn index(&self, job: &Job) -> Result<JobOutcome> {
        let Some(file) = job.file_id else {
            return Ok(JobOutcome::Done);
        };
        let rec = match self.cat.read(|c| files::file(c, file)) {
            Ok(r) => r,
            Err(e) if e.code == ErrorCode::NotFound => return Ok(JobOutcome::Done),
            Err(e) => return Err(e),
        };
        let root = self
            .cat
            .read(|c| folders::folder(c, rec.folder_id))?
            .root_path;
        let bytes = std::fs::read(root.join(&rec.rel_path))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let fc = FileChunks {
            file_id: file,
            content_hash: ContentHash::of_bytes(&bytes),
            chunk_cfg_fp: ChunkCfgFp::compute("t", "{}", "tok", "1"),
            extractor_version: "1".into(),
            chunks: vec![NewChunk {
                ord: 0,
                byte_start: 0,
                byte_end: bytes.len() as u64,
                line_start: 1,
                line_end: 1,
                tokens: 1,
                text: if text.is_empty() { " ".into() } else { text },
            }],
        };
        self.cat.write_blocking(move |c| {
            let now = sr_catalog::now_ms();
            let t1 = chunks::write_file_chunks(c, &fc, now)?;
            chunks::finalize_file(c, file, t1.file_version, now)?;
            Ok(())
        })?;
        Ok(JobOutcome::Done)
    }
}

impl JobExecutor for FakeExec {
    async fn execute(&self, job: Job, cancel: &CancellationToken) -> Result<JobOutcome> {
        let now_running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(now_running, Ordering::SeqCst);
        let leased = self.cat.read(jobs::queue_depth).map(|d| d.1).unwrap_or(0);
        self.max_leased
            .fetch_max(usize::try_from(leased).unwrap_or(0), Ordering::SeqCst);
        let rel = job
            .file_id
            .and_then(|f| self.cat.read(|c| files::file(c, f)).ok())
            .map(|r| r.rel_path)
            .unwrap_or_default();
        self.order
            .lock()
            .unwrap()
            .push((job.kind, job.folder_id, rel.clone()));
        let res = async {
            if let Some(g) = &self.gate {
                tokio::select! {
                    p = g.acquire() => { drop(p); }
                    () = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)),
                }
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let behave = self.behave.lock().unwrap().clone();
            if let Some(b) = behave {
                return b(&job, Path::new(&rel));
            }
            self.index(&job)
        }
        .await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        res
    }
}

/// Runs the claim loop until the queue is empty (or `timeout`), then shuts it down.
pub async fn run_until_idle<E: JobExecutor + 'static>(
    fx: &Fx,
    svc: &SyncService<E>,
    timeout: Duration,
) {
    let cancel = CancellationToken::new();
    let s = svc.clone();
    let c = cancel.clone();
    let h = tokio::spawn(async move { s.run(c).await });
    let start = Instant::now();
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if fx.queue_depth() == (0, 0) {
            let t = *idle_since.get_or_insert_with(Instant::now);
            if t.elapsed() > Duration::from_millis(60) {
                break;
            }
        } else {
            idle_since = None;
        }
        assert!(
            start.elapsed() < timeout,
            "queue never drained: {:?}",
            fx.live_jobs(None)
        );
    }
    cancel.cancel();
    h.await.unwrap().unwrap();
}

/// Waits until `f` is true.
pub async fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < timeout, "condition not reached");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
