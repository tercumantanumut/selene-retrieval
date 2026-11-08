//! The local cross-encoder reranker (M3-Q; docs/03 `v2/reranker.ts` row; §2.5 Advanced: ranking
//! "rerank_enabled (false → model download)"). Off by default.
//!
//! * **Model:** `cross-encoder/ms-marco-MiniLM-L-6-v2` (Apache-2.0), the ONNX export from
//!   `Xenova/ms-marco-MiniLM-L-6-v2` at a pinned revision, fp32 (≈ 91 MB). Every file (the ONNX file
//!   and the four tokenizer files) is sha256-pinned here and checked after download **and** at
//!   load (bytes are read once, checked, then handed to ONNX Runtime: no re-open in between).
//! * **Consent:** turning "Re-rank the top results with a local model (downloads the model)" on in
//!   Advanced is the click; nothing downloads before it. The download uses M2's resumable,
//!   verifying downloader (`sr_providers::download::Downloader::fetch`, Range resume, metered
//!   pause with "Download anyway"), into `<data>\models\` like the search models, and pushes the
//!   same `model.download.*` events (model id = [`RERANKER_MODEL_ID`]).
//! * **Running:** queries only, on the CPU (the top `rerank_top_n` hits, ≤ 512 tokens each),
//!   scores are the model's logit through a sigmoid (0..1), so `minScore` keeps Selene's meaning
//!   ("applies to the rerank score when reranking is on", §2.5). A failure degrades to the RRF
//!   order for that query (sr-core).
//! * Lives in the engine (M3-P: "the cross-encoder reranker is engine work") behind the
//!   `sr_providers::Reranker` trait; moving it into `sr-providers::rerank::local` later changes no
//!   caller.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio_util::sync::CancellationToken;

use crate::EngineHandle;

/// Canonical model id (events, diagnostics).
pub const RERANKER_MODEL_ID: &str = "cross-encoder/ms-marco-MiniLM-L-6-v2";
/// The ONNX export's repository and pinned revision.
pub const RERANKER_REPO: &str = "Xenova/ms-marco-MiniLM-L-6-v2";
pub const RERANKER_REVISION: &str = "a09144355adeed5f58c8ed011d209bf8ee5a1fec";

/// One pinned file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    pub path: &'static str,
    /// `None` for the small side files (size not pinned; the sha256 is).
    pub bytes: Option<u64>,
    pub sha256: &'static str,
}

/// `onnx/model.onnx` (fp32). HF LFS oid = sha256, verified 2026-10-19 via the HF API.
pub const RERANKER_ONNX: PinnedFile = PinnedFile {
    path: "onnx/model.onnx",
    bytes: Some(90_992_115),
    sha256: "c623d0bcb99f4622beb413eaef00cfbe5db20df9f1dd982da4b4f26022881870",
};

/// Tokenizer side files (downloaded at the pinned revision and hashed, 2026-10-19).
/// `tokenizer.json` is byte-identical to the bundled bge-small one (bert-base-uncased vocab).
pub const RERANKER_TOKENIZER: [PinnedFile; 4] = [
    PinnedFile {
        path: "tokenizer.json",
        bytes: None,
        sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    },
    PinnedFile {
        path: "config.json",
        bytes: None,
        sha256: "d827779a72d27ae68cf878a6fc2e954542663fe21ca515d9f4783fc96be2d37e",
    },
    PinnedFile {
        path: "tokenizer_config.json",
        bytes: None,
        sha256: "0b29c7bfc889e53b36d9dd3e686dd4300f6525110eaa98c76a5dafceb2029f53",
    },
    PinnedFile {
        path: "special_tokens_map.json",
        bytes: None,
        sha256: "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    },
];

/// Shown before the click ("downloads the model, about 91 MB").
pub fn download_mb() -> u64 {
    RERANKER_ONNX.bytes.unwrap_or(0).div_ceil(1_000_000)
}

/// Longest passage + query the cross-encoder reads (tokens).
pub const RERANK_MAX_TOKENS: usize = 512;
/// Pairs per ONNX run.
const RERANK_BATCH: usize = 16;

/// Where the reranker files live under `<data>\models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankerPaths {
    pub onnx: PathBuf,
    pub tokenizer_dir: PathBuf,
}

impl RerankerPaths {
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let models = data_dir.join("models");
        let rev = &RERANKER_REVISION[..12];
        Self {
            onnx: models.join(format!("{}.onnx", RERANKER_ONNX.sha256)),
            tokenizer_dir: models.join(format!("tok-{}@{rev}", RERANKER_REPO.replace('/', "--"))),
        }
    }

    fn file(&self, f: &PinnedFile) -> PathBuf {
        if f.path == RERANKER_ONNX.path {
            self.onnx.clone()
        } else {
            self.tokenizer_dir.join(f.path)
        }
    }

    /// Every file is on disk (sizes only; the sha256 is checked at load).
    pub fn present(&self) -> bool {
        let onnx_ok =
            std::fs::metadata(&self.onnx).is_ok_and(|m| Some(m.len()) == RERANKER_ONNX.bytes);
        onnx_ok
            && RERANKER_TOKENIZER
                .iter()
                .all(|f| self.tokenizer_dir.join(f.path).is_file())
    }
}

/// Where the reranker is (Advanced → ranking; pushed progress uses `model.download.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum RerankerPhase {
    /// Turned off (default).
    Off,
    Downloading {
        bytes: u64,
        total: u64,
    },
    /// Metered connection: waiting for "Download anyway" or another connection.
    WaitingForUnmeteredNetwork {
        have: u64,
        total: u64,
    },
    Loading,
    /// Re-ranking every search.
    Ready,
    Failed {
        code: ErrorCode,
        resumable: bool,
    },
}

/// `reranker.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RerankerStatus {
    pub enabled: bool,
    pub model_id: &'static str,
    pub download_mb: u64,
    pub downloaded: bool,
    #[serde(flatten)]
    pub phase: RerankerPhase,
}

/// Engine-side reranker state (in `Inner`).
pub(crate) struct RerankerState {
    pub(crate) phase: Mutex<RerankerPhase>,
    pub(crate) loaded: Mutex<Option<Arc<dyn sr_providers::Reranker>>>,
    pub(crate) cancel: Mutex<Option<CancellationToken>>,
}

impl RerankerState {
    pub(crate) fn new() -> Self {
        Self {
            phase: Mutex::new(RerankerPhase::Off),
            loaded: Mutex::new(None),
            cancel: Mutex::new(None),
        }
    }

    fn set(&self, p: RerankerPhase) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
    }

    pub(crate) fn loaded(&self) -> Option<Arc<dyn sr_providers::Reranker>> {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Logit → 0..1.
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Read `path`, check its size and sha256 against `f`.
fn read_pinned(path: &Path, f: &PinnedFile) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|e| {
        let mut err = Error::from(e).with_detail(format!("reranker file {}", f.path));
        err.ctx.model = Some(RERANKER_MODEL_ID.into());
        err
    })?;
    let bad_len = f.bytes.is_some_and(|b| b != bytes.len() as u64);
    if bad_len || sr_providers::models::sha256_hex(&bytes) != f.sha256 {
        let mut e = Error::new(ErrorCode::ModelIntegrity)
            .with_detail(format!("reranker file {} failed its check", f.path));
        e.ctx.model = Some(RERANKER_MODEL_ID.into());
        return Err(e);
    }
    Ok(bytes)
}

/// The cross-encoder on ONNX Runtime (CPU).
#[cfg(feature = "local")]
pub struct LocalReranker {
    model: Arc<Mutex<fastembed::TextRerank>>,
}

#[cfg(feature = "local")]
impl LocalReranker {
    /// Load from verified files (blocking: reads ≈ 91 MB and builds the session).
    pub fn load_blocking(paths: &RerankerPaths, threads: usize, max_tokens: usize) -> Result<Self> {
        let onnx = read_pinned(&paths.onnx, &RERANKER_ONNX)?;
        let side: Vec<Vec<u8>> = RERANKER_TOKENIZER
            .iter()
            .map(|f| read_pinned(&paths.file(f), f))
            .collect::<Result<_>>()?;
        let [tokenizer, config, tokenizer_config, special] =
            <[Vec<u8>; 4]>::try_from(side).map_err(|_| Error::new(ErrorCode::Internal))?;
        let files = fastembed::TokenizerFiles {
            tokenizer_file: tokenizer,
            config_file: config,
            special_tokens_map_file: special,
            tokenizer_config_file: tokenizer_config,
        };
        let model = fastembed::UserDefinedRerankingModel::new(onnx, files);
        let opts = fastembed::RerankInitOptionsUserDefined::new()
            .with_max_length(max_tokens.clamp(32, RERANK_MAX_TOKENS))
            .with_intra_threads(threads.max(1));
        let m = fastembed::TextRerank::try_new_from_user_defined(model, opts).map_err(|e| {
            let mut err = Error::new(ErrorCode::ModelIncompatible)
                .with_detail(format!("reranker session: {e}"));
            err.ctx.model = Some(RERANKER_MODEL_ID.into());
            err
        })?;
        Ok(Self {
            model: Arc::new(Mutex::new(m)),
        })
    }
}

#[cfg(feature = "local")]
impl sr_providers::Reranker for LocalReranker {
    fn model_id(&self) -> &str {
        RERANKER_MODEL_ID
    }

    fn rerank<'a>(
        &'a self,
        query: &'a str,
        docs: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(async move {
            if docs.is_empty() {
                return Ok(Vec::new());
            }
            let m = self.model.clone();
            let q = query.to_string();
            let d: Vec<String> = docs.iter().map(|s| (*s).to_string()).collect();
            let task = tokio::task::spawn_blocking(move || {
                let mut g = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let refs: Vec<&str> = d.iter().map(String::as_str).collect();
                g.rerank(q.as_str(), refs.as_slice(), false, Some(RERANK_BATCH))
                    .map(|rs| {
                        let mut out = vec![0.0f32; refs.len()];
                        for r in rs {
                            if let Some(slot) = out.get_mut(r.index) {
                                *slot = sigmoid(r.score);
                            }
                        }
                        out
                    })
                    .map_err(|e| {
                        Error::new(ErrorCode::Internal).with_detail(format!("rerank: {e}"))
                    })
            });
            tokio::select! {
                () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                r = task => r.map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?,
            }
        })
    }
}

impl EngineHandle {
    fn reranker_state(&self) -> &RerankerState {
        &self.inner().reranker
    }

    /// `reranker.status`.
    pub fn reranker_status(&self) -> RerankerStatus {
        let enabled = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ranking
            .rerank_enabled;
        RerankerStatus {
            enabled,
            model_id: RERANKER_MODEL_ID,
            download_mb: download_mb(),
            downloaded: RerankerPaths::in_data_dir(&self.inner().layout.root).present(),
            phase: self
                .reranker_state()
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// Rebuild the searcher from the current settings (rrf_k, MMR, min score, …) and the loaded
    /// reranker. Reranking runs only when it is turned on **and** loaded.
    pub(crate) fn apply_ranking(&self) {
        let s = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut qc = self
            .inner()
            .cfg
            .query
            .clone()
            .unwrap_or_else(|| crate::query_config(&s));
        if self.inner().cfg.query.is_none() {
            qc.mmr_lambda = s.ranking.mmr_lambda.filter(|l| (0.0..=1.0).contains(l));
            // An explicit query config (tests, the eval bench) keeps its own candidate depth.
            qc.rerank_top_n = (s.ranking.rerank_top_n as usize).clamp(1, 200);
        }
        let rr = if s.ranking.rerank_enabled {
            self.reranker_state().loaded()
        } else {
            None
        };
        qc.rerank_enabled = rr.is_some();
        let core_rr: Option<Arc<dyn sr_core::query::Reranker>> = rr.map(|r| {
            Arc::new(crate::adapters::ProviderReranker::new(
                r,
                self.inner().embed_cancel.clone(),
            )) as Arc<dyn sr_core::query::Reranker>
        });
        let searcher = Arc::new(sr_core::query::Searcher::new(
            self.inner().registry.clone(),
            Arc::new(crate::adapters::CatalogChunks::new(
                self.inner().cat.clone(),
            )),
            core_rr,
            qc,
        ));
        *self
            .inner()
            .searcher
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = searcher;
    }

    /// Make the reranker match the setting: off → unload; on → load (downloading first if
    /// needed; the setting is the consent). Background; progress is pushed.
    pub(crate) fn sync_reranker(&self, allow_metered: bool) {
        let enabled = self
            .inner()
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ranking
            .rerank_enabled;
        let st = self.reranker_state();
        if let Some(c) = st
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            c.cancel();
        }
        if !enabled {
            st.loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            st.set(RerankerPhase::Off);
            self.apply_ranking();
            return;
        }
        if st.loaded().is_some() {
            st.set(RerankerPhase::Ready);
            self.apply_ranking();
            return;
        }
        #[cfg(feature = "local")]
        {
            let cancel = self.inner().shutdown.child_token();
            *st.cancel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
            let h = self.clone();
            let task = tokio::spawn(async move {
                match h.prepare_reranker(allow_metered, &cancel).await {
                    Ok(true) => h.apply_ranking(),
                    Ok(false) => {}
                    Err(e) if e.code == ErrorCode::Cancelled => {}
                    Err(e) => {
                        tracing::warn!(target: "sr_engine::rerank", code = %e.code, "reranker not available; search keeps its normal order");
                        h.reranker_state().set(RerankerPhase::Failed {
                            code: e.code,
                            resumable: e.code != ErrorCode::ModelIntegrity
                                && e.code != ErrorCode::ModelIncompatible,
                        });
                    }
                }
            });
            self.track_task(task);
        }
        #[cfg(not(feature = "local"))]
        {
            let _ = allow_metered;
            st.set(RerankerPhase::Failed {
                code: ErrorCode::ModelIncompatible,
                resumable: false,
            });
        }
    }

    /// Download (if needed) and load. `Ok(false)` = paused on a metered connection.
    #[cfg(feature = "local")]
    async fn prepare_reranker(
        &self,
        allow_metered: bool,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        use sr_common::events::EngineEvent;
        use sr_providers::download::{DownloadConfig, Downloader, FetchOutcome, FileSpec};
        let st = self.reranker_state();
        let paths = RerankerPaths::in_data_dir(&self.inner().layout.root);
        if !paths.present() {
            let cfg = DownloadConfig {
                extra_disk_needed: 256 * 1024 * 1024,
                ..DownloadConfig::default()
            };
            if !allow_metered && (cfg.is_metered)() {
                st.set(RerankerPhase::WaitingForUnmeteredNetwork {
                    have: 0,
                    total: RERANKER_ONNX.bytes.unwrap_or(0),
                });
                return Ok(false);
            }
            let d = Downloader::new(cfg)?;
            let bus = self.inner().bus.clone();
            let fail = |e: Error| -> Error {
                if e.code != ErrorCode::Cancelled {
                    bus.publish(EngineEvent::ModelDownloadFailed {
                        model_id: RERANKER_MODEL_ID.into(),
                        code: e.code,
                        resumable: e.code != ErrorCode::ModelIntegrity,
                    });
                }
                e
            };
            std::fs::create_dir_all(&paths.tokenizer_dir).map_err(|e| fail(Error::from(e)))?;
            for f in &RERANKER_TOKENIZER {
                let dest = paths.file(f);
                let spec = FileSpec {
                    model: RERANKER_MODEL_ID.into(),
                    url: d.url(RERANKER_REPO, RERANKER_REVISION, f.path),
                    sha256: f.sha256.into(),
                    bytes: f.bytes,
                    part: dest.with_extension("json.part"),
                    dest,
                };
                d.fetch(&spec, true, &mut |_, _| {}, cancel)
                    .await
                    .map_err(fail)?;
            }
            let total = RERANKER_ONNX.bytes.unwrap_or(0);
            st.set(RerankerPhase::Downloading { bytes: 0, total });
            let spec = FileSpec {
                model: RERANKER_MODEL_ID.into(),
                url: d.url(RERANKER_REPO, RERANKER_REVISION, RERANKER_ONNX.path),
                sha256: RERANKER_ONNX.sha256.into(),
                bytes: RERANKER_ONNX.bytes,
                dest: paths.onnx.clone(),
                part: paths.onnx.with_extension("part"),
            };
            let bus2 = self.inner().bus.clone();
            let mut last = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now);
            let mut progress = |bytes: u64, total: u64| {
                if last.elapsed() >= std::time::Duration::from_millis(250) || bytes == total {
                    last = std::time::Instant::now();
                    st.set(RerankerPhase::Downloading { bytes, total });
                    bus2.publish(EngineEvent::ModelDownloadProgress {
                        model_id: RERANKER_MODEL_ID.into(),
                        bytes,
                        total,
                    });
                }
            };
            match d
                .fetch(&spec, allow_metered, &mut progress, cancel)
                .await
                .map_err(fail)?
            {
                FetchOutcome::PausedMetered { have } => {
                    st.set(RerankerPhase::WaitingForUnmeteredNetwork { have, total });
                    return Ok(false);
                }
                FetchOutcome::Done { .. } | FetchOutcome::AlreadyPresent => {
                    self.inner()
                        .bus
                        .publish(EngineEvent::ModelDownloadCompleted {
                            model_id: RERANKER_MODEL_ID.into(),
                            sha256: RERANKER_ONNX.sha256.into(),
                        });
                }
            }
        }
        st.set(RerankerPhase::Loading);
        let threads = sr_common::tier::TierProfile::cpu_threads(
            self.inner()
                .detection
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map_or(2, |d| d.0.physical_cores),
        ) as usize;
        let p2 = paths.clone();
        let max_tokens = self
            .inner()
            .cfg
            .rerank_max_tokens
            .unwrap_or(RERANK_MAX_TOKENS);
        let loaded = tokio::task::spawn_blocking(move || {
            LocalReranker::load_blocking(&p2, threads, max_tokens)
        })
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        let r = match loaded {
            Ok(r) => r,
            Err(e) => {
                if e.code == ErrorCode::ModelIntegrity {
                    // A damaged file is never used: delete it so [Retry] downloads it again.
                    let _ = std::fs::remove_file(&paths.onnx);
                    let _ = std::fs::remove_dir_all(&paths.tokenizer_dir);
                }
                return Err(e);
            }
        };
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        *st.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(r));
        st.set(RerankerPhase::Ready);
        tracing::info!(target: "sr_engine::rerank", "reranker ready");
        Ok(true)
    }

    /// `reranker.download {allow_metered}`: retry or "Download anyway".
    pub fn reranker_download(&self, allow_metered: bool) -> RerankerStatus {
        self.sync_reranker(allow_metered);
        self.reranker_status()
    }

    /// Test hook: use `r` as the loaded reranker (no download).
    #[doc(hidden)]
    pub fn set_reranker_for_tests(&self, r: Option<Arc<dyn sr_providers::Reranker>>) {
        let st = self.reranker_state();
        *st.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = r;
        st.set(if st.loaded().is_some() {
            RerankerPhase::Ready
        } else {
            RerankerPhase::Off
        });
        self.apply_ranking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_layout() {
        assert_eq!(download_mb(), 91);
        for f in std::iter::once(&RERANKER_ONNX).chain(RERANKER_TOKENIZER.iter()) {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        let p = RerankerPaths::in_data_dir(Path::new("C:\\d"));
        assert!(p.onnx.ends_with(format!("{}.onnx", RERANKER_ONNX.sha256)));
        assert!(
            p.tokenizer_dir
                .ends_with("tok-Xenova--ms-marco-MiniLM-L-6-v2@a09144355ade")
        );
        assert!(!p.present());
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6 && sigmoid(10.0) > 0.99 && sigmoid(-10.0) < 0.01);
    }

    #[test]
    fn a_tampered_file_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("tokenizer.json");
        std::fs::write(&path, b"{}").unwrap();
        let e = read_pinned(&path, &RERANKER_TOKENIZER[0]).unwrap_err();
        assert_eq!(e.code, ErrorCode::ModelIntegrity);
    }
}
