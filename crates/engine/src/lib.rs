//! sr-engine: the composition root (docs/02 §2.1, Amendments 4.2, 4.4).
//!
//! [`Engine::start`] runs the required start order (Amendments 4.2, 4.11, **4.24**):
//!
//! ```text
//! data dir rule → settings.toml (zero-config: created with defaults) → catalog open
//!   → start_epoch (fences old leases) → embedder (bundled model, sha256-checked) + profile
//!   → open stores per collection (Lance table of the ACTIVE index + tantivy) → StoreRegistry
//!   → copy each store's own watermark into the catalog cache → finalize_applied
//!   → re-validate folder roots (unavailable, never deleted) → tier detection (silent; meta)
//!   → background: group-commit writer, claim loop, maintenance → HostState::Ready
//! ```
//! Every step is cheap. The daemon then opens the pipe (open pipe == ready). Outbox work left by
//! the previous run is **not** replayed before ready (4.24): the background writer applies it as
//! its first bounded batches (4.25) while search serves the last committed state, and the status
//! shows "Catching up after restart (n%)" ([`sr_common::events::CatchUp`]). `HostLock` and the
//! packaged-context check happen in `sr-daemon` before this.
//!
//! Everything a front end needs goes through the cheap, clonable [`EngineHandle`]: search,
//! read_file, grep, folders, status snapshot + event bus, app-RPC dispatch and drain.

/// Activity lines and egress pages (M4-E, page 30).
pub mod activity;
pub mod adapters;
/// `answer` and `enhance_prompt` over the user's folders (M3-Q).
pub mod answers;
/// App-facing operations: estimate, pause/resume/cancel, approvals, notices (M2 tester).
pub mod appops;
pub mod bus;
/// "Claude (sign in)" / "Codex (sign in)": the user's own CLI login (M5-C, Amendments 10.2 / 10.3).
#[cfg(feature = "remote")]
pub mod cli_signin;
/// Connected apps and per-client scopes (M4-E, page 29).
pub mod clients;
pub mod clock;
pub mod datadir;
pub mod files;
/// Folder detail: skipped files, retry, settings (M4-E, page 25).
pub mod folderdetail;
/// R-UX32: "Waiting while a full-screen app is open" and the 5-minute processor fallback (M3-Q).
pub mod fullscreen;
/// GPU on by default: detection with GPUs, fault ledger, probe cache (M2-G).
pub mod gpu;
/// Folder groups (M4-E, page 31).
pub mod groups;
mod handle;
/// Opt-in streamable HTTP: tokens, port, listener control (M4-E, page 29).
pub mod httpctl;
pub mod hw;
/// `index_location.*` app-RPC (M4-E, page 32).
pub mod indexloc;
/// Moving the search index to another drive (M4-E, page 32).
pub mod indexmove;
pub mod ingest;
/// The AI provider: consent gate, chat provider slot, providers / consent / answers app-RPC (M3-Q).
pub mod llm;
/// The multilingual search model profile and its pins (M3-Q).
pub mod multilingual;
/// Daemon pipe server: handshake, version skew, MCP byte stream, app-RPC, event resume (M1-C).
pub mod pipe;
pub mod policy;
pub mod recover;
/// Background re-embed, atomic index swap, search-model change operations (M2-G).
pub mod reembed;
/// In-flight app requests: cancel by id or with the session (M4-E, pages 27-28).
pub mod requests;
/// The local cross-encoder reranker: pinned download on consent, CPU, Advanced only (M3-Q).
pub mod rerank;
pub mod rpc;
pub mod sandboxfs;
mod tasks;
pub mod win;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use sr_catalog::Catalog;
use sr_catalog::model::{ProfilePrecision, ProfileTier};
use sr_catalog::repo::profiles::{self, ProfileSpec};
use sr_catalog::repo::{chunks, folders};
use sr_common::config::{LoadReport, Settings, load_or_create};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{CatchUp, EngineEvent, HostState, Notice};
use sr_common::ids::{CollectionId, FolderId};
use sr_common::tier::{Tier, TierDecision};
use sr_core::index::{
    AnnConfig, CollectionStores, IndexWriter, LexConfig, LexicalIndex, StoreRegistry, VectorStore,
    WriterConfig,
};
use sr_core::ingest::Chunker;
use sr_core::query::{QueryConfig, Searcher};
use sr_extract_client::{ExtractConfig, ExtractPool};
use sr_providers::{BatcherConfig, EmbedBatcher, EmbeddingProfile, EmbeddingProvider};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub use bus::EventBus;
pub use clients::Scope;
pub use handle::{AlsoAtOut, EngineHandle, SearchArgs, SearchHitOut, SearchOutput};
pub use sr_common::api::FolderInfo;
pub use sr_core::query::{Scores, SearchMode};

use crate::adapters::{CatalogChunks, CatalogOutbox, ProviderEmbedder};
use crate::clock::SharedClock;
use crate::datadir::DataLayout;
use crate::hw::{HwProbe, TierChoices};
use crate::sandboxfs::{SandboxFs, SharedSandbox};

/// Where the embedding provider comes from.
#[derive(Clone)]
pub enum EmbedderSource {
    /// The bundled Minimum model in `<app_dir>\models\bge-small-en-v1.5` (sha256-checked).
    #[cfg(feature = "local")]
    Bundled,
    /// Any provider (tests: `sr_providers::testing::HashEmbedder`; the eval bench: other variants).
    Provided(Arc<dyn EmbeddingProvider>),
}

impl std::fmt::Debug for EmbedderSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "local")]
            Self::Bundled => f.write_str("Bundled"),
            Self::Provided(p) => write!(f, "Provided({})", p.model_id()),
        }
    }
}

/// How the extract worker pool is configured.
#[derive(Debug, Clone)]
pub enum ExtractSetup {
    /// `sr-extract.exe` + `pdfium.dll` next to the daemon (the install layout).
    InstallDir,
    /// Explicit configuration (tests, the eval bench).
    Config(Box<ExtractConfig>),
    /// No worker pool: index jobs fail retryably (search, read_file and grep still work).
    Disabled,
}

/// R32 (M4-E): how long the start waits for the configured AI provider to connect before Ready.
pub const PROVIDER_START_WAIT: Duration = Duration::from_secs(2);

/// Engine start options. [`EngineConfig::new`] gives the production defaults.
#[derive(Clone)]
pub struct EngineConfig {
    /// `%LOCALAPPDATA%\SeleneRetrieval` unless moved in Advanced → System.
    pub data_dir: PathBuf,
    /// Install directory: `models\`, `sr-extract.exe`, `pdfium.dll`.
    pub app_dir: PathBuf,
    pub embedder: EmbedderSource,
    pub extract: ExtractSetup,
    pub clock: SharedClock,
    pub hw_probe: Arc<dyn HwProbe>,
    /// Start the writer timer, claim loop and maintenance task (tests drive them by hand).
    pub background: bool,
    /// Group commit: drain at least this often while the outbox has work (§2.7: 2 s).
    pub group_commit: Duration,
    /// After a T1, wait this long for more before draining (keeps save-to-searchable ≤ 3 s).
    pub commit_debounce: Duration,
    /// Concurrent jobs (claim permits; backpressure).
    pub job_permits: usize,
    pub writer: WriterConfig,
    /// ANN knobs; default = `AnnConfig::from(&sr_common::tier::ANN_DEFAULT)` (single source).
    pub ann: AnnConfig,
    /// Ranking overrides (otherwise from `settings.toml` → Advanced: ranking).
    pub query: Option<QueryConfig>,
    /// Chunker tokenizer; default: the bundled bge-small `tokenizer.json` (pinned sha256).
    pub tokenizer_json: Option<PathBuf>,
    /// Diagnostics only (`--launched-by`).
    pub launched_by: String,
    /// The event bus to publish on. `sr-daemon` creates it before the engine so the pipe can open
    /// (handshake, status, event subscriptions) while the model loads (M2 tester O1). `None` =
    /// the engine creates its own.
    pub bus: Option<Arc<EventBus>>,
    /// API keys (M3). `None` = Windows Credential Manager (memory if it is unavailable).
    pub secrets: Option<Arc<dyn sr_providers::SecretStore>>,
    /// A chat provider to use instead of connecting the configured remote one (tests, bench).
    /// Used when `[provider] provider` names its id.
    pub llm: Option<llm::LlmOverride>,
    /// M5-E: the machine policy to run under. `None` = read it from `%ProgramData%` and the
    /// registry ([`policy::load_machine`]); tests pass their own.
    pub policy: Option<sr_common::config::policy::Policy>,
    /// M5-E (R-UX36): tokens the cross-encoder reads per pair (`None` = [`rerank::RERANK_MAX_TOKENS`]);
    /// the eval bench measures the speed / quality trade-off with it.
    pub rerank_max_tokens: Option<usize>,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("data_dir", &self.data_dir)
            .field("app_dir", &self.app_dir)
            .field("embedder", &self.embedder)
            .field("background", &self.background)
            .finish_non_exhaustive()
    }
}

impl EngineConfig {
    /// Production defaults: bundled model, worker pool from the install dir, background on.
    #[cfg(feature = "local")]
    pub fn new(data_dir: PathBuf, app_dir: PathBuf) -> Self {
        Self::with_embedder(data_dir, app_dir, EmbedderSource::Bundled)
    }

    pub fn with_embedder(data_dir: PathBuf, app_dir: PathBuf, embedder: EmbedderSource) -> Self {
        Self {
            data_dir,
            app_dir,
            embedder,
            extract: ExtractSetup::InstallDir,
            clock: clock::system(),
            hw_probe: Arc::new(crate::gpu::GpuSystemProbe), // M2-G: DXGI GPUs included
            background: true,
            group_commit: Duration::from_secs(2),
            commit_debounce: Duration::from_millis(300),
            job_permits: 4,
            // Amendment 4.25: 2 s / 512 embedded chunks / 1 000 ops per batch. `embed_batch: 0`
            // = one window per EmbedBatcher round (provider batch × calls in flight).
            writer: WriterConfig {
                embed_batch: 0,
                ..WriterConfig::default()
            },
            // sr_common::tier::ANN_DEFAULT (= the Minimum tier, the only bundled one in M1).
            ann: AnnConfig::default(),
            query: None,
            tokenizer_json: None,
            launched_by: "direct".into(),
            bus: None,
            secrets: None,
            llm: None,
            policy: None,
            rerank_max_tokens: None,
        }
    }

    fn tokenizer_path(&self) -> PathBuf {
        self.tokenizer_json.clone().unwrap_or_else(|| {
            sr_common::install::resources_dir(&self.app_dir)
                .join("models")
                .join(sr_providers::models::BUNDLED_MODEL_DIR)
                .join("tokenizer.json")
        })
    }
}

/// Ranking knobs from `settings.toml` (Advanced: ranking). Reranking is switched on by
/// `EngineHandle::apply_ranking` once the reranker is loaded (M3-Q); never by the tier.
pub fn query_config(s: &Settings) -> QueryConfig {
    let r = &s.ranking;
    QueryConfig {
        rrf_k: r.rrf_k,
        default_max_results: r.default_max_results,
        default_min_score: r.min_score,
        mmr_lambda: r.mmr_lambda,
        mode: if r.dense_only {
            SearchMode::DenseOnly
        } else {
            SearchMode::Hybrid
        },
        rerank_enabled: false,
        rerank_top_n: r.rerank_top_n as usize,
        ..QueryConfig::default()
    }
}

/// Catalog profile for a provider. Local tier models use the catalog's own constructor so the
/// fingerprint matches `ProfileSpec::local` exactly.
pub fn profile_spec(p: &EmbeddingProfile) -> ProfileSpec {
    // M3-Q: the multilingual model is its own profile, not a tier.
    let multilingual = p.provider == "local" && p.model == multilingual::MULTILINGUAL_MODEL_ID;
    if p.provider == "local"
        && !multilingual
        && let Some(t) = p.tier
    {
        let tp = sr_common::tier::TierProfile::for_tier(t);
        for f in [tp.gpu_file, tp.cpu_file] {
            if f.sha256 == p.variant_sha256 {
                return ProfileSpec::local(tp, &f);
            }
        }
    }
    ProfileSpec {
        provider: p.provider.clone(),
        model: p.model.clone(),
        model_revision: p.model_revision.clone(),
        dims: p.dims,
        pooling: p.pooling.clone(),
        query_prefix: p.query_prefix.clone(),
        doc_prefix: p.doc_prefix.clone(),
        variant_path: format!("{}@{}", p.model, p.model_revision),
        variant_sha256: p.variant_sha256.clone(),
        precision: match p.precision.as_str() {
            "fp16" => ProfilePrecision::Fp16,
            "int8" => ProfilePrecision::Int8,
            "remote" => ProfilePrecision::Remote,
            _ => ProfilePrecision::Fp32,
        },
        tier: match p.tier {
            _ if multilingual => ProfileTier::Custom,
            Some(Tier::Minimum) => ProfileTier::Minimum,
            Some(Tier::Standard) => ProfileTier::Standard,
            Some(Tier::High) => ProfileTier::High,
            None => ProfileTier::Custom,
        },
        is_remote: p.is_remote,
    }
}

fn hex(b: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*b).to_hex().to_string()
}

/// Run a future to completion on a blocking-pool thread (tantivy commits and the synchronous
/// catalog traits block; they must not hold a runtime worker that serves requests).
pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T, Error>
where
    F: std::future::Future<Output = Result<T, Error>> + Send + 'static,
    T: Send + 'static,
{
    let h = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || h.block_on(f))
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?
}

/// Shared engine state behind [`EngineHandle`].
pub(crate) struct Inner {
    pub(crate) cfg: EngineConfig,
    pub(crate) layout: DataLayout,
    pub(crate) clock: SharedClock,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) cat: Arc<Catalog>,
    pub(crate) settings: RwLock<Settings>,
    /// M5-E: the machine policy (also installed process-wide for the sandbox root rules).
    pub(crate) policy: Arc<sr_common::config::policy::Policy>,
    pub(crate) epoch: i64,
    pub(crate) provider: Arc<dyn EmbeddingProvider>,
    pub(crate) profile_fp: [u8; 32],
    pub(crate) default_collection: CollectionId,
    pub(crate) vectors: VectorStore,
    pub(crate) registry: Arc<StoreRegistry>,
    pub(crate) writer: Arc<IndexWriter>,
    pub(crate) searcher: RwLock<Arc<Searcher>>,
    pub(crate) ingest: Arc<ingest::Ingest>,
    pub(crate) sandbox: SharedSandbox,
    pub(crate) extract: Arc<OnceLock<Arc<ExtractPool>>>,
    pub(crate) extract_error: Mutex<Option<String>>,
    pub(crate) host: RwLock<HostState>,
    pub(crate) writer_nudge: Arc<Notify>,
    pub(crate) sched_nudge: Arc<Notify>,
    /// Cancels the claim loop and maintenance (drain).
    pub(crate) shutdown: CancellationToken,
    /// Cancels in-flight embedding calls (only at final shutdown).
    pub(crate) embed_cancel: CancellationToken,
    pub(crate) tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub(crate) detection: RwLock<Option<(sr_common::tier::HwInfo, TierDecision)>>,
    pub(crate) maint: Mutex<tasks::MaintState>,
    /// The ingest pipeline's capacity, shared with sr-sync's claim loop (backpressure).
    pub(crate) jobs_in_flight: Arc<tokio::sync::Semaphore>,
    /// Scans, deletes, claim loop and scheduler (sr-sync); the engine's executor runs index jobs.
    pub(crate) sync: sr_sync::SyncService<ingest::Ingest>,
    pub(crate) sync_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The live folder watcher (M2-W, `SyncService::start_watcher`); background mode only.
    pub(crate) watcher: Mutex<Option<sr_sync::WatcherHandle>>,
    pub(crate) drained: tokio::sync::OnceCell<()>,
    pub(crate) last_drained_seq: std::sync::atomic::AtomicI64,
    pub(crate) startup: Mutex<StartupReport>,
    pub(crate) ack_faults: Arc<std::sync::atomic::AtomicU32>,
    /// Document embedding for the writer (Amendment 4.25); `None` only if it could not start.
    pub(crate) batcher: Option<Arc<EmbedBatcher>>,
    /// Stops the writer at its next batch boundary (shutdown drain budget, 4.25).
    pub(crate) writer_stop: CancellationToken,
    /// The background writer loop (drain waits for its batch boundary).
    pub(crate) writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// "Catching up after restart" (4.24) while outbox work from the last run is applied.
    pub(crate) catch_up: Mutex<Option<CatchUpState>>,
    /// Notices kept in the status snapshot until the next start (R-UX19).
    pub(crate) notices: Mutex<Vec<Notice>>,
    /// The AI provider, its consent gate and the key store (M3-Q).
    pub(crate) llm: llm::LlmState,
    /// R-UX32: the current wait behind a full-screen app.
    pub(crate) fullscreen: Arc<fullscreen::FullscreenWait>,
    /// The optional reranker (Advanced; off by default).
    pub(crate) reranker: rerank::RerankerState,
    /// Serialises read-modify-write of `settings.toml` (app-RPC calls can overlap).
    pub(crate) settings_write: tokio::sync::Mutex<()>,
    /// M4-E: the default folder group ("My folders", page 31).
    pub(crate) default_group: sr_common::ids::GroupId,
    /// M4-E: connected apps' live sessions and the last-seen throttle (page 29).
    pub(crate) clients: Mutex<clients::ClientsState>,
    /// M4-E: in-flight `answer` / `enhance_prompt` calls by request id (pages 27-28).
    pub(crate) requests: requests::Requests,
    /// M4-E: opt-in HTTP policy, tokens and listener state (page 29).
    pub(crate) http: httpctl::HttpControl,
    /// M5-C: CLI sign-in status cache and open sign-in windows.
    #[cfg(feature = "remote")]
    pub(crate) cli: cli_signin::CliSignin,
    /// M4-E: set to `true` to make the daemon drain and exit (index move, page 32).
    pub(crate) restart: Arc<tokio::sync::watch::Sender<bool>>,
}

/// Restart catch-up bookkeeping (Amendment 4.24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatchUpState {
    /// Highest outbox seq pending at start; the phase ends once a batch reaches it.
    pub(crate) until_seq: i64,
    pub(crate) progress: CatchUp,
}

/// What happened during start (diagnostics, tests).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StartupReport {
    pub epoch: i64,
    pub requeued_leases: usize,
    /// Outbox ops above the lowest store watermark at start: applied by the background writer
    /// after ready ("Catching up after restart", Amendment 4.24), not before.
    pub replay_ops: usize,
    pub finalized_files: usize,
    pub unavailable_folders: usize,
    pub settings_created: bool,
    pub settings_reset_fields: usize,
    /// Startup steps in the order they ran (asserted by the start-order test).
    pub steps: Vec<String>,
}

/// The engine.
pub struct Engine;

impl Engine {
    /// Start the engine (see the module docs for the order).
    pub async fn start(cfg: EngineConfig) -> Result<EngineHandle, Error> {
        let mut report = StartupReport::default();
        let step = |r: &mut StartupReport, s: &str| {
            tracing::info!(target: "sr_engine::start", step = s);
            r.steps.push(s.to_string());
        };
        let layout = datadir::prepare(&cfg.data_dir)?;
        // M5-E: the machine policy first, so every later check (settings, folder roots, providers,
        // downloads) runs under it.
        let machine_policy = match cfg.policy.clone() {
            Some(p) => p,
            None => tokio::task::spawn_blocking(policy::load_machine)
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?,
        };
        policy::log(&machine_policy);
        sr_common::config::policy::install(machine_policy.clone());
        let machine_policy = Arc::new(machine_policy);
        let clock = cfg.clock.clone();
        let bus = cfg.bus.clone().unwrap_or_default();
        bus.publish(EngineEvent::HostState {
            state: HostState::Starting,
        });

        // Settings: zero-config (created with defaults), lenient per field.
        let settings_path = layout.settings.clone();
        let LoadReport {
            mut settings,
            issues,
            created,
            ..
        } = tokio::task::spawn_blocking(move || load_or_create(&settings_path))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        report.settings_created = created;
        report.settings_reset_fields = issues.len();
        // Policy wins over settings.toml (in memory; the file follows at the next settings write).
        let locked = machine_policy.apply(&mut settings);
        if !locked.is_empty() {
            tracing::info!(target: "sr_engine::policy", fields = ?locked, "settings locked by your organisation's policy");
        }
        let mut notices = Vec::new();
        if !issues.is_empty() {
            tracing::warn!(target: "sr_engine::start", fields = issues.len(), "some settings were reset to defaults");
            bus.publish(EngineEvent::SettingsChanged);
            // R-UX19: the plain notice reaches clients (snapshot `notices` + `Notice` event).
            let notice = settings_reset_notice(&issues);
            bus.publish(EngineEvent::Notice {
                notice: notice.clone(),
            });
            notices.push(notice);
        }
        step(&mut report, "settings");

        // 1. Catalog open (+ migrations).
        let cat_path = layout.catalog.clone();
        let cat = Arc::new(
            tokio::task::spawn_blocking(move || Catalog::open(&cat_path))
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??,
        );
        step(&mut report, "catalog_open");

        // 2. Epoch (fences leases of the previous run).
        let c2 = cat.clone();
        let ep = tokio::task::spawn_blocking(move || c2.start_epoch())
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        report.epoch = ep.epoch;
        report.requeued_leases = ep.requeued;
        step(&mut report, "start_epoch");

        // 3. Embedder (the active index's model) and the catalog profile + default collection.
        let choices = TierChoices::from_settings(&settings);
        // M2-G: detect GPUs now (DXGI, ~0.2-1 s) so even the first index runs on the GPU.
        let gpu = crate::gpu::GpuContext::detect(
            cfg.hw_probe.clone(),
            &layout.root,
            cat.clone(),
            clock.clone(),
            choices,
        )
        .await?;
        let fullscreen = fullscreen::FullscreenWait::new(bus.clone());
        let provider = load_embedder(&cfg, &cat, &bus, &gpu, &fullscreen).await?;
        let spec = profile_spec(provider.profile());
        let profile_fp = spec.fingerprint();
        let now = clock.now_ms();
        let spec2 = spec.clone();
        let (default_collection, default_group) = cat
            .write(move |c| {
                let pid = profiles::ensure_profile(c, &spec2, now)?;
                let coll = profiles::ensure_default_collection(c, pid, now)?;
                // M4-E: the default folder group ("My folders") always exists.
                let group = sr_catalog::repo::groups::ensure_default(c, now)?;
                Ok((coll, group))
            })
            .await?;
        step(&mut report, "embedder");

        // Chunker: always the bundled bge-small tokenizer (chunk ids never depend on the tier).
        let tok_path = cfg.tokenizer_path();
        let chunker = Arc::new(
            tokio::task::spawn_blocking(move || load_chunker(&tok_path))
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??,
        );

        // Document embedding streams through one EmbedBatcher per profile (Amendment 4.25).
        let bcfg = batcher_config(&provider);
        let batcher = Some(Arc::new(EmbedBatcher::spawn(
            provider.clone(),
            bcfg,
            Arc::new(BusSink(bus.clone())),
        )));

        // 4. Stores of every collection's ACTIVE index.
        let vectors = VectorStore::open(&layout.lance).await?;
        let registry = StoreRegistry::new();
        let embed_cancel = CancellationToken::new();
        let collections = cat.read_async(profiles::collections).await?;
        for coll in &collections {
            if let Some(stores) = open_collection_with(
                &cat,
                &vectors,
                &layout,
                coll.id,
                &provider,
                profile_fp,
                &cfg.ann,
                &embed_cancel,
                batcher.clone(),
            )
            .await?
            {
                registry.register(stores);
            }
        }
        step(&mut report, "open_stores");

        let chunk_src = Arc::new(CatalogChunks::new(cat.clone()));
        let ack_faults = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let outbox = Arc::new(
            CatalogOutbox::new(cat.clone(), clock.clone(), Some(bus.clone()))
                .with_ack_faults(ack_faults.clone()),
        );
        let mut wcfg = cfg.writer.clone();
        if wcfg.embed_batch == 0 {
            // One window = what the batcher runs at once (provider batch × calls in flight).
            wcfg.embed_batch = (bcfg.batch_size * bcfg.max_in_flight).max(1);
        }
        let writer = Arc::new(IndexWriter::new(
            outbox,
            chunk_src.clone(),
            registry.clone(),
            wcfg,
        ));

        // 5. The stores hold the truth; copy each store's own watermark into the catalog cache,
        // so a crash between a store commit and its acknowledgement cannot leave files pending
        // forever (4.11). No replay here: pending outbox work is applied after ready (4.24).
        let wms = writer.watermarks().await?;
        let min_wm = wms.iter().map(|w| w.seq).min();
        cat.write(move |c| adapters::sync_watermark_cache(c, &wms))
            .await?;
        step(&mut report, "sync_watermarks");

        // 6. finalize_applied: T2 for anything a store applied before a crash (Amendment 4.2).
        let now = clock.now_ms();
        let ids: Vec<CollectionId> = collections.iter().map(|c| c.id).collect();
        report.finalized_files = cat
            .write(move |c| {
                let mut n = 0;
                for id in &ids {
                    n += chunks::finalize_applied(c, *id, now)?;
                }
                Ok(n)
            })
            .await?;
        step(&mut report, "finalize_applied");

        // What the background writer must catch up on (cheap count; the work runs after ready).
        let (pending_ops, until_seq) = match min_wm {
            Some(m) => {
                cat.read_async(move |c| {
                    c.query_row(
                        "SELECT count(*), coalesce(max(seq), 0) FROM index_ops WHERE seq > ?1",
                        [m],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .map_err(sr_catalog::map_db_err)
                })
                .await?
            }
            None => (0, 0),
        };
        report.replay_ops = usize::try_from(pending_ops).unwrap_or(0);
        let catch_up = (pending_ops > 0).then(|| CatchUpState {
            until_seq,
            progress: CatchUp {
                done: 0,
                total: u64::try_from(pending_ops).unwrap_or(0),
            },
        });

        // 7. Re-validate folder roots (unavailable, never deleted).
        let sandbox: SharedSandbox =
            Arc::new(RwLock::new(Arc::new(SandboxFs::empty(&layout.root))));
        let writer_nudge = Arc::new(Notify::new());
        let extract = Arc::new(OnceLock::new());
        let ingest = Arc::new(ingest::Ingest {
            cat: cat.clone(),
            clock: clock.clone(),
            bus: bus.clone(),
            sandbox: sandbox.clone(),
            chunker,
            extract: extract.clone(),
            writer_nudge: writer_nudge.clone(),
        });
        // R-UX18: the "no good match" floors follow each collection's embedder (NoMatchMode::Model).
        let qc = cfg.query.clone().unwrap_or_else(|| query_config(&settings));
        let searcher = Arc::new(Searcher::new(registry.clone(), chunk_src.clone(), None, qc));
        let job_permits = cfg.job_permits.max(1);
        let jobs_in_flight = Arc::new(tokio::sync::Semaphore::new(job_permits));
        let sync = sr_sync::SyncService::with_pipeline(
            cat.clone(),
            ingest.clone(),
            bus.clone() as Arc<dyn sr_sync::EventSink>,
            ep.epoch,
            sr_sync::SyncConfig {
                pipeline_permits: job_permits,
                mcp_add_folder_enabled: settings.integrations.mcp_add_folder_enabled,
                // The index root and (M4-E: after a move they differ) the anchor with settings
                // and logs can never be added as folders.
                dangerous: sr_common::sandbox::DangerousRoots::from_env(Some(&layout.root))
                    .with_never_inside(layout.anchor.clone()),
                ..sr_sync::SyncConfig::default()
            },
            jobs_in_flight.clone(),
        );
        let (cat_for_gate, bus_for_gate) = (cat.clone(), bus.clone());
        let http = httpctl::HttpControl::new(
            settings.integrations.http_server_enabled,
            settings.integrations.http_port,
        );
        let inner = Arc::new(Inner {
            layout,
            clock,
            bus,
            cat,
            settings: RwLock::new(settings),
            policy: machine_policy,
            epoch: ep.epoch,
            provider,
            profile_fp,
            default_collection,
            vectors,
            registry,
            writer,
            searcher: RwLock::new(searcher),
            ingest,
            sandbox,
            extract,
            extract_error: Mutex::new(None),
            host: RwLock::new(HostState::Starting),
            writer_nudge,
            sched_nudge: Arc::new(Notify::new()),
            shutdown: CancellationToken::new(),
            embed_cancel,
            tasks: Mutex::new(Vec::new()),
            detection: RwLock::new(None),
            maint: Mutex::new(tasks::MaintState::default()),
            jobs_in_flight,
            sync,
            sync_task: Mutex::new(None),
            watcher: Mutex::new(None),
            drained: tokio::sync::OnceCell::new(),
            last_drained_seq: std::sync::atomic::AtomicI64::new(min_wm.unwrap_or(0)),
            startup: Mutex::new(StartupReport::default()),
            ack_faults,
            batcher,
            writer_stop: CancellationToken::new(),
            writer_task: Mutex::new(None),
            catch_up: Mutex::new(catch_up),
            notices: Mutex::new(notices),
            llm: llm::LlmState {
                slot: RwLock::new(None),
                secrets: cfg.secrets.clone().unwrap_or_else(llm::default_secrets),
                gate: Arc::new(llm::CatalogGate::new(cat_for_gate, bus_for_gate)),
                override_: cfg.llm.clone(),
            },
            fullscreen,
            reranker: rerank::RerankerState::new(),
            settings_write: tokio::sync::Mutex::new(()),
            default_group,
            clients: Mutex::new(clients::ClientsState::default()),
            requests: requests::Requests::default(),
            http,
            #[cfg(feature = "remote")]
            cli: cli_signin::CliSignin::default(),
            restart: Arc::new(tokio::sync::watch::channel(false).0),
            cfg,
        });
        let h = EngineHandle::from_inner(inner);
        report.unavailable_folders = h.revalidate_roots().await?;
        // sr-sync: re-validate roots (canonical), push statuses, enqueue catch-up scans.
        let sr = h.inner().sync.startup().await?;
        report.unavailable_folders = report.unavailable_folders.max(sr.unavailable.len());
        // R34 (M5-E): after "Start fresh", add the remembered folders back (once).
        let restored = h.restore_folders_if_requested().await;
        if restored > 0 {
            tracing::info!(target: "sr_engine::start", restored, "folders added back to the fresh index");
        }
        step(&mut report, "revalidate_roots");

        // 8. Tier detection: silent, recorded in meta, published as `tier.detected`.
        h.detect_tier(choices).await?;
        step(&mut report, "tier_detection");

        // Pending purges from a previous run keep their deadline (SLA within 15 min of start).
        h.load_pending_purges().await?;

        // Extract workers (sandboxed); failure is non-fatal: search, read_file, grep still work.
        h.start_extract_pool().await;

        if h.inner().cfg.background {
            tasks::spawn_background(&h);
            // M4-E: `index.swapped` → "Search upgraded" (page 30).
            h.spawn_activity_listener();
        }
        // M4-E: "Updated to version …" (page 30) and the HTTP tools' token hashes (page 29).
        h.note_build().await;
        // Always (also with HTTP off): `http.status.tools` counts the tools, and turning HTTP on
        // later needs no reload. Only HTTP tool rows touch Credential Manager.
        h.load_http_tokens().await;
        // M3-Q: the egress audit writer, then the configured AI provider (no network call here;
        // a failure only leaves the provider tools hidden). Before Ready, so the MCP tool list
        // computed at Ready already includes `context_answer` (U17).
        let audit = h.inner().llm.gate.start_audit_writer();
        h.track_task(audit);
        // R32 (M4-E): proxy auto-detection (WPAD) on a slow network must not delay Ready. Wait at
        // most PROVIDER_START_WAIT; a connect still running then finishes in the background and
        // pushes `SettingsChanged`, so MCP sessions get `tools/list_changed` when it lands.
        // M5-C: an access token left by the removed OAuth design (M5 test builds) is deleted.
        #[cfg(feature = "remote")]
        h.drop_old_signin_token();
        let h2 = h.clone();
        let mut load = tokio::spawn(async move { h2.reload_provider().await });
        match tokio::time::timeout(PROVIDER_START_WAIT, &mut load).await {
            Ok(Ok(Err(e))) => {
                tracing::warn!(target: "sr_engine::start", code = %e.code, "AI provider not loaded");
            }
            Ok(_) => {}
            Err(_) => {
                tracing::info!(target: "sr_engine::start", "AI provider still connecting; continuing the start");
                h.track_task(tokio::spawn(async move {
                    if let Ok(Err(e)) = load.await {
                        tracing::warn!(target: "sr_engine::start", code = %e.code, "AI provider not loaded");
                    }
                }));
            }
        }
        *h.inner()
            .host
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = HostState::Ready;
        h.bus().publish(EngineEvent::HostState {
            state: HostState::Ready,
        });
        // M2-G: GC retired vector tables; resume an unfinished search-model change.
        crate::reembed::on_engine_ready(&h);
        // The reranker, only if the user turned it on (it loads or resumes its download).
        h.sync_reranker(false);
        if let Some(c) = catch_up {
            tracing::info!(target: "sr_engine::start", ops = c.progress.total, "catching up after restart in the background");
            h.bus().publish(EngineEvent::CatchUpProgress {
                done: 0,
                total: c.progress.total,
            });
            // Start at once instead of after the first group-commit tick.
            h.inner().writer_nudge.notify_one();
        }
        step(&mut report, "ready");
        *h.inner()
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
        Ok(h)
    }
}

fn load_chunker(path: &Path) -> Result<Chunker, Error> {
    let bytes = std::fs::read(path).map_err(|e| {
        let mut err = Error::from(e).with_detail("chunker tokenizer");
        err.ctx.model = Some(sr_common::tier::MINIMUM.model_id.into());
        err
    })?;
    let want = sr_providers::models::BGE_SMALL_TOKENIZER
        .tokenizer_json
        .sha256;
    if sr_providers::models::sha256_hex(&bytes) != want {
        let mut e =
            Error::new(ErrorCode::ModelIntegrity).with_detail("tokenizer.json sha256 mismatch");
        e.ctx.model = Some(sr_common::tier::MINIMUM.model_id.into());
        return Err(e);
    }
    Chunker::from_tokenizer_json(&bytes)
}

/// Bridges provider events to the bus (`ep.changed`: diagnostics only, never a toast).
struct BusSink(Arc<EventBus>);

impl sr_providers::EventSink for BusSink {
    fn emit(&self, ev: sr_providers::ProviderEvent) {
        use sr_common::events::EpChangeReason as R;
        use sr_providers::EpReason as P;
        match ev {
            sr_providers::ProviderEvent::EpChanged {
                from: Some(from),
                to,
                reason_code,
                ..
            } => {
                let reason = match reason_code {
                    P::DisabledByUser => R::UserSwitch,
                    P::CanaryNonFinite | P::CanaryMismatch => R::CanaryFailed,
                    P::RuntimeFault => R::DeviceRemoved,
                    P::DllMissing
                    | P::DllUntrusted
                    | P::DllLoadFailed
                    | P::RegistrationFailed
                    | P::Unsupported => R::RegistrationFailed,
                    P::Planned | P::CpuPlan | P::CpuPinnedVariant | P::ProbeTooSlow => {
                        R::ProbeResult
                    }
                };
                self.0.publish(EngineEvent::EpChanged {
                    from,
                    to,
                    reason_code: reason,
                });
            }
            sr_providers::ProviderEvent::EpChanged {
                to, reason_code, ..
            } => {
                tracing::info!(target: "sr_engine::embed", ?to, ?reason_code, "embedding session ready");
            }
            sr_providers::ProviderEvent::EmbedBatch {
                items,
                unique,
                duration_ms,
                ok,
                ..
            } => {
                tracing::debug!(target: "sr_engine::embed", items, unique, duration_ms, ok, "embed batch");
            }
        }
    }
}

async fn load_embedder(
    cfg: &EngineConfig,
    cat: &Arc<Catalog>,
    bus: &Arc<EventBus>,
    gpu: &crate::gpu::GpuContext,
    wait: &Arc<fullscreen::FullscreenWait>,
) -> Result<Arc<dyn EmbeddingProvider>, Error> {
    let _ = (cat, bus, gpu, wait);
    match &cfg.embedder {
        EmbedderSource::Provided(p) => Ok(p.clone()),
        // M2-G: the active index's model (sticky variant), GPU plan + fault ledger applied;
        // M3-Q: wrapped with the full-screen processor fallback (R-UX32).
        #[cfg(feature = "local")]
        EmbedderSource::Bundled => {
            crate::reembed::load_active_local(cfg, cat, bus, gpu, wait).await
        }
    }
}

/// The "Some settings were reset" notice (R-UX19): the one renderer's text and action
/// ("Review settings" opens the first reset field's section), the reset paths under Details.
pub(crate) fn settings_reset_notice(issues: &[sr_common::config::FieldIssue]) -> Notice {
    let mut e = Error::new(ErrorCode::SettingsInvalid);
    if let Some(i) = issues.first() {
        e = e.with_section(i.section);
    }
    Notice {
        code: ErrorCode::SettingsInvalid,
        message: e.user_message(),
        fields: issues.iter().map(|i| i.path.clone()).collect(),
    }
}

/// Whether the provider's document session runs on the GPU: a batch above the tier's CPU batch
/// means DirectML (the same rule as [`batcher_config`]).
pub(crate) fn provider_on_gpu(provider: &Arc<dyn EmbeddingProvider>) -> bool {
    provider.profile().tier.is_some_and(|t| {
        provider.max_batch() as u32
            > sr_common::tier::TierProfile::for_tier(t).batch(sr_common::tier::EpActive::Cpu)
    })
}

/// EmbedBatcher sizing from the provider: its current batch (TierProfile batch for the active
/// EP, VRAM-capped) and the TierProfile's calls in flight for that EP (GPU 2, CPU 1).
pub(crate) fn batcher_config(provider: &Arc<dyn EmbeddingProvider>) -> BatcherConfig {
    let batch = provider.max_batch().max(1);
    let in_flight = match provider.profile().tier {
        Some(t) => {
            let tp = sr_common::tier::TierProfile::for_tier(t);
            // A batch above the CPU batch means the session runs on the GPU.
            let ep = if batch as u32 > tp.batch(sr_common::tier::EpActive::Cpu) {
                sr_common::tier::EpActive::DirectMl
            } else {
                sr_common::tier::EpActive::Cpu
            };
            tp.concurrency(ep).max(1) as usize
        }
        None => 1,
    };
    BatcherConfig {
        batch_size: batch,
        max_in_flight: in_flight,
        queue_items: batch * (in_flight + 2),
        max_wait: Duration::from_millis(50),
    }
}

/// [`open_collection_with`] without an EmbedBatcher (document embedding calls the provider
/// directly). Kept for callers that open a collection later (M2 index swap); pass
/// `Inner::batcher` to `open_collection_with` instead to keep the 4.25 streaming.
#[allow(clippy::too_many_arguments, dead_code)]
pub(crate) async fn open_collection(
    cat: &Arc<Catalog>,
    vectors: &VectorStore,
    layout: &DataLayout,
    coll: CollectionId,
    provider: &Arc<dyn EmbeddingProvider>,
    profile_fp: [u8; 32],
    ann: &AnnConfig,
    cancel: &CancellationToken,
) -> Result<Option<CollectionStores>, Error> {
    open_collection_with(
        cat, vectors, layout, coll, provider, profile_fp, ann, cancel, None,
    )
    .await
}

/// Open a collection's ACTIVE index (Lance table + tantivy) with its embedder.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_collection_with(
    cat: &Arc<Catalog>,
    vectors: &VectorStore,
    layout: &DataLayout,
    coll: CollectionId,
    provider: &Arc<dyn EmbeddingProvider>,
    profile_fp: [u8; 32],
    ann: &AnnConfig,
    cancel: &CancellationToken,
    batcher: Option<Arc<EmbedBatcher>>,
) -> Result<Option<CollectionStores>, Error> {
    let Some((idx, prof)) = cat
        .read_async(move |c| profiles::active_index(c, coll))
        .await?
    else {
        tracing::warn!(target: "sr_engine::start", collection = %coll, "collection has no active index");
        return Ok(None);
    };
    if prof.fingerprint != profile_fp {
        // Another embedding space (a tier switch in progress, M2): never mix spaces.
        tracing::warn!(target: "sr_engine::start", collection = %coll, "active index uses another model; not opened");
        return Ok(None);
    }
    let table = vectors
        .open_table(&idx.lance_table, idx.dims as usize, ann.clone())
        .await?;
    let root = layout.tantivy.clone();
    let lex =
        tokio::task::spawn_blocking(move || LexicalIndex::open(&root, coll, LexConfig::default()))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
    // R-UX33: a keyword index from an older build is recreated empty (new `exact` field). The
    // same self-heal covers a keyword index lost from disk: an empty index next to indexed files
    // with chunks re-queues those files (vectors are copied by text_hash, nothing is re-embedded;
    // the writer applies them after ready as "Catching up").
    let pending = if lex.recreated() || lex.num_docs() > 0 {
        0
    } else {
        // An empty index with ops still pending is a rebuild (or first index) in progress:
        // don't queue every file again on each restart.
        cat.read_async(move |c| {
            c.query_row(
                "SELECT count(*) FROM index_ops WHERE collection_id = ?1",
                [coll.to_bytes()],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .await?
    };
    if lex.recreated() || (lex.num_docs() == 0 && pending == 0) {
        let n = requeue_for_lexical_rebuild(cat, coll).await?;
        if n > 0 {
            tracing::info!(target: "sr_engine::start", collection = %coll, files = n, recreated = lex.recreated(), "rebuilding the keyword index in the background");
        }
    }
    Ok(Some(CollectionStores {
        collection_id: coll,
        vectors: Arc::new(table),
        lexical: Arc::new(lex),
        embedder: Arc::new(
            ProviderEmbedder::new(provider.clone(), hex(&prof.fingerprint), cancel.clone())
                .with_batcher(batcher),
        ),
    }))
}

/// Queue an `upsert` for every indexed file of `coll` that has chunks at its current version, so
/// an empty (recreated) keyword index is filled again from the catalog. Returns how many.
pub(crate) async fn requeue_for_lexical_rebuild(
    cat: &Arc<Catalog>,
    coll: CollectionId,
) -> Result<usize, Error> {
    let now = i64::try_from(sr_common::events::now_ms()).unwrap_or(i64::MAX);
    cat.write(move |c| {
        sr_catalog::atomic(c, |c| {
            let mut st = c
                .prepare(
                    "SELECT f.id, f.version FROM files f JOIN folders d ON d.id = f.folder_id
                     WHERE d.collection_id = ?1 AND f.state = 'indexed' AND f.version > 0
                       AND EXISTS (SELECT 1 FROM chunks k WHERE k.file_id = f.id AND k.file_version = f.version)",
                )
                .map_err(sr_catalog::map_db_err)?;
            let rows: Vec<(Vec<u8>, i64)> = st
                .query_map([coll.to_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sr_catalog::map_db_err)?
                .collect::<Result<_, _>>()
                .map_err(sr_catalog::map_db_err)?;
            drop(st);
            let mut n = 0usize;
            for (id, version) in rows {
                let Ok(b) = <[u8; 16]>::try_from(id.as_slice()) else {
                    continue;
                };
                sr_catalog::repo::outbox::append(
                    c,
                    coll,
                    None,
                    None,
                    Some(sr_common::ids::FileId::from_bytes(b)),
                    sr_catalog::model::IndexOpKind::Upsert,
                    version,
                    now,
                )?;
                n += 1;
            }
            Ok(n)
        })
    })
    .await
}

/// Folders' `(id, root)` from the catalog (blocking helper for sandbox rebuilds).
pub(crate) fn folder_roots(cat: &Catalog) -> Result<Vec<(FolderId, PathBuf)>, Error> {
    cat.read(|c| {
        Ok(folders::folders(c)?
            .into_iter()
            .map(|f| (f.id, f.root_path))
            .collect())
    })
}

impl EngineConfig {
    /// Build the extract pool configuration.
    pub(crate) fn extract_config(&self) -> Option<ExtractConfig> {
        match &self.extract {
            ExtractSetup::InstallDir => {
                let mut c = sr_core::ingest::extract_client::config_for_install_dir(&self.app_dir);
                // Debug builds only (ignored in release, like every dev hook): tests that run the
                // real daemon use their own sandbox profile and delete it, instead of creating
                // (and leaking) the production `SeleneRetrieval.Extract` profile (M2-I finding).
                if let Some(n) = sr_client::naming::dev_env("SR_APP_CONTAINER_NAME") {
                    c.app_container_name = n.to_string_lossy().into_owned();
                }
                Some(c)
            }
            ExtractSetup::Config(c) => Some(c.as_ref().clone()),
            ExtractSetup::Disabled => None,
        }
    }
}
