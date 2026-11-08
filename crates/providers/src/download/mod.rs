//! Model downloader (Amendment 1; docs/02-architecture.md §2.8 "Model download").
//!
//! * **Only pinned files.** Standard / High ONNX files come from `sr_common::tier::files` (HF repo,
//!   revision, path, size, sha256) and their tokenizer side files from
//!   [`crate::models::tokenizer_pins_for`]. The URL is
//!   `https://huggingface.co/<repo>/resolve/<pinned revision>/<path>`; nothing else is fetched.
//! * **Consent required.** [`Downloader::download_model`] takes a [`DownloadConsent`], made from
//!   the "Recommended…" line the user clicked ([Use it]) or an Advanced tier choice. It names the
//!   exact file and the size the user saw. Nothing downloads on its own.
//! * **Resumable.** Bytes go to `<models>\<sha256>.part`; a retry or a later call sends
//!   `Range: bytes=<have>-` and hashes the bytes already on disk first. A server that ignores the
//!   range (200) restarts the file from zero; `416` with a complete part goes straight to the check.
//! * **Verified, then atomic.** At the end the sha256 of the whole file must equal the pin; then
//!   `<sha256>.part` is fsynced and renamed to `<sha256>.onnx` (same directory → atomic on NTFS).
//!   A mismatch deletes the part (`ModelIntegrity`, not resumable).
//! * **Metered connections.** Checked before the first byte and every few seconds; on a metered
//!   connection the download pauses ([`DownloadOutcome::PausedMetered`], part kept) unless the user
//!   chose "Download anyway" ([`DownloadConsent::allow_metered`]).
//! * **Network.** reqwest + rustls with the **ring** provider ([`ensure_crypto_provider`],
//!   Amendment 3.13), the Windows trust store (platform verifier: corporate TLS inspection works),
//!   the system proxy (WinINet settings; `system-proxy`), redirects followed only to `https://` (or
//!   loopback in tests). Network drops are retried with backoff inside one call, then reported as
//!   resumable `ModelDownloadFailed`.
//! * **Never touches search.** The downloader writes only below `<data dir>\models`. A failed,
//!   corrupt or paused download leaves the active index, its model and every query untouched.
//!
//! Events (`sr_common::events`): `model.download.progress` (≤ every 250 ms),
//! `model.download.completed` after the rename, `model.download.failed{code, resumable}`.

pub mod metered;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::tier::{GIB, MIB, ModelFile, Recommendation, Tier, TierProfile};
use tokio_util::sync::CancellationToken;

use crate::models::{ModelPaths, TokenizerPins, ensure_file_belongs, tokenizer_pins_for};

/// Hugging Face, the only production origin.
pub const HF_BASE: &str = "https://huggingface.co";

/// Install the ring rustls provider for this process (Amendment 3.13: exactly one provider,
/// ring). Idempotent; a provider installed earlier (sr-mcp-server's `main`) is kept.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ------------------------------------------------------------------------------------- layout

/// Downloaded models under `<data dir>\models` (gitignored in dev; the bundled Minimum model lives
/// in the install dir, not here).
///
/// ```text
/// models\<sha256>.onnx         verified model file (name = its content hash)
/// models\<sha256>.part         download in progress (resume point = its length)
/// models\tok-<repo>@<rev12>\   tokenizer.json, config.json, tokenizer_config.json, special_tokens_map.json
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStore {
    root: PathBuf,
}

/// What is on disk for one model file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// ONNX at its pinned size + all tokenizer files (sha256 is re-checked at load).
    Present,
    /// A `.part` with this many bytes.
    Partial {
        have: u64,
    },
    Missing,
}

impl ModelStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<data_dir>\models`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("models"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn onnx_path(&self, f: &ModelFile) -> PathBuf {
        self.root.join(format!("{}.onnx", f.sha256))
    }

    pub fn part_path(&self, f: &ModelFile) -> PathBuf {
        self.root.join(format!("{}.part", f.sha256))
    }

    pub fn tokenizer_dir(&self, pins: &TokenizerPins) -> PathBuf {
        let rev = pins.hf_revision.get(..12).unwrap_or(pins.hf_revision);
        self.root
            .join(format!("tok-{}@{rev}", pins.hf_repo.replace('/', "--")))
    }

    /// Where [`crate::models::load_verified`] finds this model.
    pub fn paths(&self, tier: &TierProfile, f: &ModelFile) -> ModelPaths {
        ModelPaths {
            onnx: self.onnx_path(f),
            tokenizer_dir: self.tokenizer_dir(tokenizer_pins_for(tier)),
        }
    }

    pub fn state(&self, tier: &TierProfile, f: &ModelFile) -> FileState {
        let tok = self.tokenizer_dir(tokenizer_pins_for(tier));
        let aux_ok = tokenizer_pins_for(tier)
            .files()
            .iter()
            .all(|a| tok.join(a.name).is_file());
        match std::fs::metadata(self.onnx_path(f)) {
            Ok(m) if m.len() == f.bytes && aux_ok => return FileState::Present,
            _ => {}
        }
        match std::fs::metadata(self.part_path(f)) {
            Ok(m) => FileState::Partial { have: m.len() },
            Err(_) => FileState::Missing,
        }
    }

    /// ONNX bytes still to fetch (the tokenizer files add < 1 MB).
    pub fn missing_bytes(&self, tier: &TierProfile, f: &ModelFile) -> u64 {
        match self.state(tier, f) {
            FileState::Present => 0,
            FileState::Partial { have } => f.bytes.saturating_sub(have),
            FileState::Missing => f.bytes,
        }
    }
}

// ------------------------------------------------------------------------------------ consent

/// Proof that the user asked for this download after seeing its size: built from the
/// "Recommended for your computer" line they clicked, or from an Advanced tier choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadConsent {
    pub tier: Tier,
    pub sha256: &'static str,
    /// The size the UI showed (diagnostics).
    pub shown_bytes: u64,
    /// "Download anyway" on a metered connection.
    pub allow_metered: bool,
}

impl DownloadConsent {
    /// [Use it] on the Basic "Recommended…" line.
    pub fn from_recommendation(r: &Recommendation) -> Self {
        Self {
            tier: r.tier,
            sha256: r.file.sha256,
            shown_bytes: r.download_bytes,
            allow_metered: false,
        }
    }

    /// An explicit tier choice (Advanced → search model) for exactly this file.
    pub fn for_file(tier: &TierProfile, file: &ModelFile) -> Self {
        Self {
            tier: tier.tier,
            sha256: file.sha256,
            shown_bytes: file.bytes,
            allow_metered: false,
        }
    }

    /// "Download anyway" (metered connection).
    pub fn allow_metered(mut self) -> Self {
        self.allow_metered = true;
        self
    }
}

// ------------------------------------------------------------------------------------- config

pub type MeteredProbe = Arc<dyn Fn() -> bool + Send + Sync>;
pub type FreeDiskProbe = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;
/// Where download events go (the engine forwards them to its bus).
pub type DownloadSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

#[derive(Clone)]
pub struct DownloadConfig {
    /// `https://huggingface.co` in production; tests use `http://127.0.0.1:<port>`.
    pub base_url: String,
    pub connect_timeout: Duration,
    /// No bytes for this long = a dropped connection (retried with Range).
    pub read_timeout: Duration,
    /// Tries per call (network drops, 5xx, 429), each resuming from the bytes on disk.
    pub attempts: u32,
    /// Backoff before retry n: `backoff × 2ⁿ`, capped at 30 s.
    pub backoff: Duration,
    pub progress_every: Duration,
    pub metered_check_every: Duration,
    pub is_metered: MeteredProbe,
    pub free_disk: FreeDiskProbe,
    /// Space needed besides the model file (the shadow index during the re-embed, §2.8 pre-flight:
    /// `rows × dims × 4 × 1.3 + 2 GiB`; see `sr_common::tier::upgrade_disk_needed`).
    pub extra_disk_needed: u64,
    pub user_agent: String,
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadConfig")
            .field("base_url", &self.base_url)
            .field("attempts", &self.attempts)
            .field("extra_disk_needed", &self.extra_disk_needed)
            .finish_non_exhaustive()
    }
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            base_url: HF_BASE.into(),
            connect_timeout: Duration::from_secs(20),
            read_timeout: Duration::from_secs(60),
            attempts: 4,
            backoff: Duration::from_secs(1),
            progress_every: Duration::from_millis(250),
            metered_check_every: Duration::from_secs(5),
            is_metered: Arc::new(metered::is_metered),
            free_disk: Arc::new(crate::sys::free_disk),
            extra_disk_needed: 2 * GIB,
            user_agent: format!("SeleneRetrieval/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

// ------------------------------------------------------------------------------------ outcomes

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// Fetched (fully or the rest of a part) and verified.
    Completed { sha256: String, fetched: u64 },
    /// Already on disk and verified; nothing fetched.
    AlreadyPresent,
    /// Stopped on a metered connection; the part is kept. Retry with
    /// [`DownloadConsent::allow_metered`] or when the connection changes.
    PausedMetered { have: u64, total: u64 },
}

/// One file to fetch (public for tests and diagnostics; production goes through
/// [`Downloader::download_model`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Model id for errors and events.
    pub model: String,
    pub url: String,
    pub sha256: String,
    /// Pinned size (ONNX); `None` for the small tokenizer files.
    pub bytes: Option<u64>,
    pub dest: PathBuf,
    pub part: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Done { fetched: u64 },
    AlreadyPresent,
    PausedMetered { have: u64 },
}

enum Attempt {
    /// Worth another try (drop, timeout, 5xx, 429); the part is kept.
    Retry(Error),
    Fatal(Error),
}

// --------------------------------------------------------------------------------- downloader

/// One download per file at a time in this process.
fn active() -> &'static Mutex<HashSet<String>> {
    static A: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashSet::new()))
}

struct ActiveGuard(String);

impl ActiveGuard {
    fn take(sha: &str) -> Result<Self> {
        let mut g = active().lock().unwrap_or_else(|p| p.into_inner());
        if !g.insert(sha.to_owned()) {
            return Err(
                Error::new(ErrorCode::Busy).with_detail("this model is already downloading")
            );
        }
        Ok(Self(sha.to_owned()))
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        active()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

fn is_loopback(u: &reqwest::Url) -> bool {
    let Some(h) = u.host_str() else {
        return false;
    };
    let h = h.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `https://`, or `http://` to loopback (the test server). Anything else is refused.
fn url_allowed(u: &reqwest::Url) -> bool {
    u.scheme() == "https" || (u.scheme() == "http" && is_loopback(u))
}

fn model_err(code: ErrorCode, model: &str, host: Option<&str>, detail: impl Into<String>) -> Error {
    let mut e = Error::new(code).with_detail(detail);
    e.ctx.model = Some(model.to_owned());
    e.ctx.host = host.map(str::to_owned);
    e
}

fn chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// Network error → user-facing code: TLS / certificate trouble = `NetworkBlocked` (proxy or
/// firewall in the way), no connection = `Offline`, anything mid-transfer = `ModelDownloadFailed`.
fn classify_net(e: &reqwest::Error, model: &str, host: &str) -> Attempt {
    let text = chain_text(e);
    let low = text.to_lowercase();
    let tls = [
        "certificate",
        "unknownissuer",
        "handshake",
        "tls",
        "invalid peer",
    ]
    .iter()
    .any(|k| low.contains(k));
    if tls {
        return Attempt::Fatal(model_err(
            ErrorCode::NetworkBlocked,
            model,
            Some(host),
            text,
        ));
    }
    if e.is_connect() {
        // Retried: a flaky Wi-Fi reconnects; still Offline if it never comes back.
        return Attempt::Retry(model_err(ErrorCode::Offline, model, Some(host), text));
    }
    Attempt::Retry(model_err(
        ErrorCode::ModelDownloadFailed,
        model,
        Some(host),
        text,
    ))
}

fn io_err(e: std::io::Error, model: &str, what: &str) -> Error {
    // ERROR_DISK_FULL (112) / ERROR_HANDLE_DISK_FULL (39) map to StorageFull on recent Rust;
    // match the raw codes too.
    let code = if matches!(e.raw_os_error(), Some(112 | 39))
        || matches!(
            e.kind(),
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
        ) {
        ErrorCode::DiskFull
    } else {
        ErrorCode::ModelDownloadFailed
    };
    let mut err = Error::new(code).with_detail(format!("{what}: {e}"));
    err.ctx.model = Some(model.to_owned());
    err
}

fn hex(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0'));
    }
    s
}

/// sha256 of the first `len` bytes of `p` (blocking; resume needs the hash of what is on disk).
fn hash_prefix(p: &Path, len: u64) -> std::io::Result<Sha256> {
    let mut h = Sha256::new();
    if len == 0 {
        return Ok(h);
    }
    let mut f = std::fs::File::open(p)?.take(len);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h)
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    let len = std::fs::metadata(p)?.len();
    Ok(hex(&hash_prefix(p, len)?.finalize()))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(std::io::Error::other)?
}

/// `Content-Range: bytes <start>-<end>/<total>` → start.
fn content_range_start(h: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = h.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

fn drive_of(p: &Path) -> Option<String> {
    match p.components().next() {
        Some(std::path::Component::Prefix(pre)) => Some(
            pre.as_os_str()
                .to_string_lossy()
                .trim_end_matches(':')
                .to_owned()
                + ":",
        ),
        _ => None,
    }
}

/// Plain size for `ErrCtx::need` ("1.2 GB").
fn fmt_size(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MB", bytes.div_ceil(MIB))
    }
}

/// The downloader. Build once ([`Downloader::new`]) and share.
pub struct Downloader {
    client: reqwest::Client,
    cfg: DownloadConfig,
}

impl std::fmt::Debug for Downloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloader")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl Downloader {
    pub fn new(cfg: DownloadConfig) -> Result<Self> {
        ensure_crypto_provider();
        let base = reqwest::Url::parse(&cfg.base_url).map_err(|e| {
            Error::new(ErrorCode::InvalidInput).with_detail(format!("download base url: {e}"))
        })?;
        if !url_allowed(&base) {
            return Err(Error::new(ErrorCode::InvalidInput)
                .with_detail("downloads use https (http only to loopback, for tests)"));
        }
        let policy = reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= 10 {
                a.error("too many redirects")
            } else if url_allowed(a.url()) {
                a.follow()
            } else {
                a.stop()
            }
        });
        // System proxy (WinINet / registry) is picked up by reqwest's `system-proxy` feature.
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .connect_timeout(cfg.connect_timeout)
            .read_timeout(cfg.read_timeout)
            .redirect(policy)
            .tls_backend_rustls()
            .build()
            .map_err(|e| {
                Error::new(ErrorCode::Internal).with_detail(format!("http client: {e}"))
            })?;
        Ok(Self { client, cfg })
    }

    pub fn config(&self) -> &DownloadConfig {
        &self.cfg
    }

    /// `<base>/<repo>/resolve/<revision>/<path>`.
    pub fn url(&self, repo: &str, revision: &str, path: &str) -> String {
        format!(
            "{}/{repo}/resolve/{revision}/{path}",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    /// Download (or resume, or verify) the model file `file` of `tier` and its tokenizer files
    /// into `store`. Emits `model.download.*` on `sink`. `Cancelled` (shutdown, user stop) keeps
    /// the part and emits nothing.
    pub async fn download_model(
        &self,
        store: &ModelStore,
        tier: &'static TierProfile,
        file: &ModelFile,
        consent: &DownloadConsent,
        sink: &DownloadSink,
        cancel: &CancellationToken,
    ) -> Result<DownloadOutcome> {
        ensure_file_belongs(tier, file)?;
        if consent.tier != tier.tier || consent.sha256 != file.sha256 {
            return Err(Error::new(ErrorCode::InvalidInput)
                .with_detail("consent was given for a different model file"));
        }
        if tier.bundled {
            return Ok(DownloadOutcome::AlreadyPresent);
        }
        let model = tier.model_id;
        let _guard = ActiveGuard::take(file.sha256)?;
        let fail = |e: Error| -> Error {
            if e.code != ErrorCode::Cancelled {
                sink(EngineEvent::ModelDownloadFailed {
                    model_id: model.to_owned(),
                    code: e.code,
                    resumable: e.code != ErrorCode::ModelIntegrity,
                });
            }
            e
        };

        if !consent.allow_metered && (self.cfg.is_metered)() {
            let have = store.state(tier, file);
            let have = match have {
                FileState::Partial { have } => have,
                FileState::Present => file.bytes,
                FileState::Missing => 0,
            };
            tracing::info!(target: "sr_providers::download", model, "metered connection: download paused");
            return Ok(DownloadOutcome::PausedMetered {
                have,
                total: file.bytes,
            });
        }

        // Disk pre-flight (§2.8): missing model bytes + the shadow index + margin.
        let missing = store.missing_bytes(tier, file);
        let need = missing.saturating_add(self.cfg.extra_disk_needed);
        if let Some(free) = (self.cfg.free_disk)(store.root())
            && free < need
        {
            let mut e = model_err(
                ErrorCode::DiskFull,
                model,
                None,
                format!("free {free} B < needed {need} B"),
            );
            e.ctx.need = Some(fmt_size(need - free));
            e.ctx.drive = drive_of(store.root());
            return Err(fail(e));
        }
        std::fs::create_dir_all(store.root()).map_err(|e| fail(io_err(e, model, "models dir")))?;

        // Tokenizer side files (small, pinned, same repo/revision as the tier's GPU file).
        let pins = tokenizer_pins_for(tier);
        let tok = store.tokenizer_dir(pins);
        std::fs::create_dir_all(&tok).map_err(|e| fail(io_err(e, model, "tokenizer dir")))?;
        for a in pins.files() {
            let spec = FileSpec {
                model: model.to_owned(),
                url: self.url(pins.hf_repo, pins.hf_revision, a.name),
                sha256: a.sha256.to_owned(),
                bytes: None,
                dest: tok.join(a.name),
                part: tok.join(format!("{}.part", a.name)),
            };
            match self.fetch(&spec, true, &mut |_, _| {}, cancel).await {
                Ok(_) => {}
                Err(e) => return Err(fail(e)),
            }
        }

        // The model file, with throttled progress.
        let spec = FileSpec {
            model: model.to_owned(),
            url: self.url(file.hf_repo, file.hf_revision, file.path),
            sha256: file.sha256.to_owned(),
            bytes: Some(file.bytes),
            dest: store.onnx_path(file),
            part: store.part_path(file),
        };
        let s2 = sink.clone();
        let mut last = Instant::now()
            .checked_sub(self.cfg.progress_every)
            .unwrap_or_else(Instant::now);
        let every = self.cfg.progress_every;
        let mut progress = |bytes: u64, total: u64| {
            if last.elapsed() >= every || bytes == total {
                last = Instant::now();
                s2(EngineEvent::ModelDownloadProgress {
                    model_id: model.to_owned(),
                    bytes,
                    total,
                });
            }
        };
        match self
            .fetch(&spec, consent.allow_metered, &mut progress, cancel)
            .await
        {
            Ok(FetchOutcome::AlreadyPresent) => Ok(DownloadOutcome::AlreadyPresent),
            Ok(FetchOutcome::Done { fetched }) => {
                sink(EngineEvent::ModelDownloadCompleted {
                    model_id: model.to_owned(),
                    sha256: file.sha256.to_owned(),
                });
                tracing::info!(target: "sr_providers::download", model, fetched, "model downloaded and verified");
                Ok(DownloadOutcome::Completed {
                    sha256: file.sha256.to_owned(),
                    fetched,
                })
            }
            Ok(FetchOutcome::PausedMetered { have }) => Ok(DownloadOutcome::PausedMetered {
                have,
                total: file.bytes,
            }),
            Err(e) => Err(fail(e)),
        }
    }

    /// Fetch one pinned file: verify an existing `dest`, else resume `part` with retries.
    pub async fn fetch(
        &self,
        spec: &FileSpec,
        allow_metered: bool,
        progress: &mut (dyn FnMut(u64, u64) + Send),
        cancel: &CancellationToken,
    ) -> Result<FetchOutcome> {
        let host = reqwest::Url::parse(&spec.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        if spec.dest.is_file() {
            let d = spec.dest.clone();
            let ok_len = match (spec.bytes, std::fs::metadata(&d)) {
                (Some(want), Ok(m)) => m.len() == want,
                (None, Ok(_)) => true,
                _ => false,
            };
            if ok_len {
                let got = blocking(move || sha256_file(&d))
                    .await
                    .map_err(|e| io_err(e, &spec.model, "verify existing"))?;
                if got == spec.sha256 {
                    return Ok(FetchOutcome::AlreadyPresent);
                }
            }
            // A wrong file is never used: delete it and fetch again.
            tracing::warn!(target: "sr_providers::download", file = %spec.dest.display(), "existing model file failed its check; re-fetching");
            std::fs::remove_file(&spec.dest)
                .map_err(|e| io_err(e, &spec.model, "remove bad file"))?;
        }
        // M5-E: an administrator's policy can turn optional model downloads off. Checked here,
        // before any request, so every download path (search model, multilingual, reranker,
        // resumes after a restart) obeys it; files already on disk keep working.
        if !sr_common::config::policy::current().model_downloads_allowed() {
            let mut e = Error::new(ErrorCode::ManagedByPolicy)
                .with_detail("Managed by your organisation: model downloads are turned off");
            e.ctx.model = Some(spec.model.clone());
            return Err(e);
        }
        let mut n = 0u32;
        let mut transferred = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            match self
                .attempt(
                    spec,
                    &host,
                    allow_metered,
                    progress,
                    &mut transferred,
                    cancel,
                )
                .await
            {
                Ok(o) => return Ok(o),
                Err(Attempt::Retry(e)) if n + 1 < self.cfg.attempts.max(1) => {
                    let wait = self
                        .cfg
                        .backoff
                        .saturating_mul(1 << n.min(5))
                        .min(Duration::from_secs(30));
                    tracing::info!(target: "sr_providers::download", attempt = n + 1, error = %e, wait_ms = wait.as_millis() as u64, "download interrupted; retrying with Range");
                    n += 1;
                    tokio::select! {
                        () = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)),
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                Err(Attempt::Retry(e) | Attempt::Fatal(e)) => return Err(e),
            }
        }
    }

    async fn attempt(
        &self,
        spec: &FileSpec,
        host: &str,
        allow_metered: bool,
        progress: &mut (dyn FnMut(u64, u64) + Send),
        transferred: &mut u64,
        cancel: &CancellationToken,
    ) -> std::result::Result<FetchOutcome, Attempt> {
        let m = spec.model.as_str();
        let fatal_io = |e: std::io::Error, what: &str| Attempt::Fatal(io_err(e, m, what));
        let mut have = std::fs::metadata(&spec.part).map_or(0, |md| md.len());
        if spec.bytes.is_some_and(|t| have > t) {
            std::fs::remove_file(&spec.part).map_err(|e| fatal_io(e, "reset part"))?;
            have = 0;
        }
        let p = spec.part.clone();
        let mut hasher = blocking(move || hash_prefix(&p, have))
            .await
            .map_err(|e| fatal_io(e, "hash part"))?;
        if have > 0 && spec.bytes == Some(have) {
            return self
                .finish(spec, hasher, *transferred, progress)
                .map_err(Attempt::Fatal);
        }

        let mut req = self.client.get(&spec.url);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let resp = tokio::select! {
            () = cancel.cancelled() => return Err(Attempt::Fatal(Error::new(ErrorCode::Cancelled))),
            r = req.send() => r.map_err(|e| classify_net(&e, m, host))?,
        };
        let status = resp.status().as_u16();
        if !url_allowed(resp.url()) {
            return Err(Attempt::Fatal(model_err(
                ErrorCode::NetworkBlocked,
                m,
                Some(host),
                "redirected to a non-https location",
            )));
        }
        match status {
            206 => {
                if content_range_start(resp.headers()) != Some(have) {
                    // Not the range we asked for: start over (never splice mismatched bytes).
                    std::fs::remove_file(&spec.part).map_err(|e| fatal_io(e, "reset part"))?;
                    return Err(Attempt::Retry(model_err(
                        ErrorCode::ModelDownloadFailed,
                        m,
                        Some(host),
                        "unexpected Content-Range",
                    )));
                }
            }
            200 => {
                if have > 0 {
                    // Server ignored Range: the body is the whole file.
                    tracing::info!(target: "sr_providers::download", "server ignored Range; restarting from 0");
                    have = 0;
                    hasher = Sha256::new();
                    std::fs::File::create(&spec.part).map_err(|e| fatal_io(e, "truncate part"))?;
                }
            }
            416 => {
                // Range starts at/after the end: either the part is complete (checked above when
                // the size is pinned) or the part is wrong. Start over.
                std::fs::remove_file(&spec.part).map_err(|e| fatal_io(e, "reset part"))?;
                return Err(Attempt::Retry(model_err(
                    ErrorCode::ModelDownloadFailed,
                    m,
                    Some(host),
                    "416 range not satisfiable",
                )));
            }
            407 => {
                return Err(Attempt::Fatal(model_err(
                    ErrorCode::NetworkBlocked,
                    m,
                    Some(host),
                    "proxy authentication required (407)",
                )));
            }
            429 | 500..=599 => {
                return Err(Attempt::Retry(model_err(
                    ErrorCode::ModelDownloadFailed,
                    m,
                    Some(host),
                    format!("HTTP {status}"),
                )));
            }
            _ => {
                return Err(Attempt::Fatal(model_err(
                    ErrorCode::ModelDownloadFailed,
                    m,
                    Some(host),
                    format!("HTTP {status}"),
                )));
            }
        }

        let total = spec.bytes;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.part)
            .map_err(|e| fatal_io(e, "open part"))?;
        let mut body = resp.bytes_stream();
        let mut last_metered = Instant::now();
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => {
                    let _ = f.flush();
                    return Err(Attempt::Fatal(Error::new(ErrorCode::Cancelled)));
                }
                n = body.next() => n,
            };
            let Some(chunk) = next else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let _ = f.flush();
                    return Err(classify_net(&e, m, host));
                }
            };
            let n = chunk.len() as u64;
            if total.is_some_and(|t| have + n > t) {
                drop(f);
                let _ = std::fs::remove_file(&spec.part);
                return Err(Attempt::Fatal(model_err(
                    ErrorCode::ModelIntegrity,
                    m,
                    Some(host),
                    "server sent more bytes than the pinned size",
                )));
            }
            f.write_all(&chunk).map_err(|e| fatal_io(e, "write part"))?;
            hasher.update(&chunk);
            have += n;
            *transferred += n;
            if let Some(t) = total {
                progress(have, t);
            }
            if !allow_metered && last_metered.elapsed() >= self.cfg.metered_check_every {
                last_metered = Instant::now();
                if (self.cfg.is_metered)() {
                    f.flush().map_err(|e| fatal_io(e, "flush part"))?;
                    tracing::info!(target: "sr_providers::download", have, "connection became metered: download paused");
                    return Ok(FetchOutcome::PausedMetered { have });
                }
            }
        }
        f.flush().map_err(|e| fatal_io(e, "flush part"))?;
        drop(f);
        if total.is_some_and(|t| have < t) {
            return Err(Attempt::Retry(model_err(
                ErrorCode::ModelDownloadFailed,
                m,
                Some(host),
                format!("connection closed at {have} bytes"),
            )));
        }
        self.finish(spec, hasher, *transferred, progress)
            .map_err(Attempt::Fatal)
    }

    /// Verify the whole part, fsync, rename atomically.
    fn finish(
        &self,
        spec: &FileSpec,
        hasher: Sha256,
        fetched: u64,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<FetchOutcome> {
        let m = spec.model.as_str();
        let got = hex(&hasher.finalize());
        if got != spec.sha256 {
            let _ = std::fs::remove_file(&spec.part);
            return Err(model_err(
                ErrorCode::ModelIntegrity,
                m,
                None,
                format!("sha256 {got}, pinned {}", spec.sha256),
            ));
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&spec.part)
            .map_err(|e| io_err(e, m, "reopen part"))?;
        f.sync_all().map_err(|e| io_err(e, m, "fsync part"))?;
        drop(f);
        std::fs::rename(&spec.part, &spec.dest).map_err(|e| io_err(e, m, "rename part"))?;
        if let Some(t) = spec.bytes {
            progress(t, t);
        }
        Ok(FetchOutcome::Done { fetched })
    }
}

/// Whether a file at `p` (any size) is a leftover `.part` (for GC and diagnostics).
pub fn is_part_file(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("part"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn only_https_or_loopback_http() {
        assert!(url_allowed(&url("https://huggingface.co/x")));
        assert!(url_allowed(&url("https://cas-bridge.xethub.hf.co/x")));
        assert!(url_allowed(&url("http://127.0.0.1:8080/x")));
        assert!(url_allowed(&url("http://localhost/x")));
        assert!(url_allowed(&url("http://[::1]:1/x")));
        assert!(!url_allowed(&url("http://huggingface.co/x")));
        assert!(!url_allowed(&url("http://10.0.0.1/x")));
        assert!(!url_allowed(&url("ftp://127.0.0.1/x")));
        assert!(!url_allowed(&url("file:///C:/x")));
    }

    #[test]
    fn content_range_and_sizes() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 4000-9999/10000".parse().unwrap(),
        );
        assert_eq!(content_range_start(&h), Some(4000));
        h.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes */10000".parse().unwrap(),
        );
        assert_eq!(content_range_start(&h), None);
        assert_eq!(fmt_size(3 * GIB / 2), "1.5 GB");
        assert_eq!(fmt_size(208 * MIB + 1), "209 MB");
        assert_eq!(
            drive_of(Path::new(r"D:\data\models")).as_deref(),
            Some("D:")
        );
        assert!(is_part_file(Path::new("a.PART")) && !is_part_file(Path::new("a.onnx")));
    }

    #[test]
    fn store_layout_is_content_addressed() {
        use sr_common::tier::STANDARD;
        let s = ModelStore::new(r"C:\d\models");
        let f = STANDARD.gpu_file;
        assert_eq!(
            s.onnx_path(&f),
            Path::new(r"C:\d\models").join(format!("{}.onnx", f.sha256))
        );
        assert_eq!(
            s.part_path(&f).extension().and_then(|e| e.to_str()),
            Some("part")
        );
        let p = s.paths(&STANDARD, &f);
        assert!(
            p.tokenizer_dir
                .ends_with("tok-Snowflake--snowflake-arctic-embed-m-v1.5@e58a8f756156")
        );
    }

    #[test]
    fn consent_names_the_file_shown() {
        use sr_common::tier::{STANDARD, recommendation};
        let r = recommendation(Tier::Standard, Tier::Minimum, true).unwrap();
        let c = DownloadConsent::from_recommendation(&r);
        assert_eq!(
            (c.tier, c.sha256),
            (Tier::Standard, STANDARD.gpu_file.sha256)
        );
        assert_eq!(c.shown_bytes, 218_155_826);
        assert!(!c.allow_metered && c.clone().allow_metered().allow_metered);
    }
}
