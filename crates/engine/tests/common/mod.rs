//! Shared helpers for sr-engine integration tests: engine configs with the deterministic test
//! embedder, the sandboxed extract worker staged in an install-like dir, corpora.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sr_engine::{EmbedderSource, EngineConfig, ExtractSetup};
use sr_extract_client::{ExtractConfig, delete_app_container_profile};
use sr_providers::testing::HashEmbedder;

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The committed bge-small tokenizer fixture (same bytes as the bundled file; sha256-pinned).
pub fn tokenizer() -> PathBuf {
    manifest_dir().join("../core/testdata/bge-small-tokenizer.json")
}

/// Directory holding `models\bge-small-en-v1.5\` (the dev layout of the bundled model), if present.
pub fn model_app_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SR_TEST_APP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir().join("../providers"));
    if dir
        .join("models/bge-small-en-v1.5/model_optimized.onnx")
        .is_file()
    {
        return Some(
            std::fs::canonicalize(&dir)
                .map(strip_verbatim)
                .unwrap_or(dir),
        );
    }
    if std::env::var_os("SR_REQUIRE_MODELS").is_some() {
        panic!(
            "SR_REQUIRE_MODELS set but no bundled model under {}",
            dir.display()
        );
    }
    eprintln!("SKIP: no bundled model under {}", dir.display());
    None
}

pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(r) => PathBuf::from(r),
        None => p,
    }
}

/// A short temp dir (the data-dir rule allows ≤ 150 chars below it).
pub fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("w2e").tempdir().unwrap()
}

/// `target/<profile>/sr-extract[.exe]`, built on demand.
pub fn worker_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/<test>.exe → target/<profile>/
    let profile_dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
    let w = profile_dir.join(format!("sr-extract{}", std::env::consts::EXE_SUFFIX));
    if !w.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = std::process::Command::new(cargo);
        cmd.args(["build", "-p", "sr-extract", "--bin", "sr-extract"]);
        if profile_dir.file_name().and_then(|n| n.to_str()) == Some("release") {
            cmd.arg("--release");
        }
        let st = cmd.status().unwrap();
        assert!(st.success(), "building sr-extract failed");
    }
    assert!(w.is_file(), "no worker at {}", w.display());
    w
}

/// AppContainer profile per test, deleted on drop.
pub struct Profile(pub String);

impl Profile {
    pub fn new(test: &str) -> Self {
        let name = format!("SeleneRetrieval.Extract.W2E.{test}");
        assert!(name.len() <= 64);
        let _ = delete_app_container_profile(&name);
        Self(name)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = delete_app_container_profile(&self.0);
    }
}

/// Stage the worker into `<dir>\app` (install layout) and return its config.
pub fn worker_config(dir: &Path, profile: &Profile) -> ExtractConfig {
    let app = dir.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let exe = app.join(format!("sr-extract{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(worker_exe(), &exe).unwrap();
    let mut c = ExtractConfig::new(exe, None);
    c.app_container_name = profile.0.clone();
    c
}

pub fn hash_embedder() -> Arc<HashEmbedder> {
    Arc::new(HashEmbedder::new("w2e-test-model", 384))
}

/// Engine config: test embedder, no background tasks, explicit extract setup.
pub fn config(data: &Path, extract: ExtractSetup) -> EngineConfig {
    let mut c = EngineConfig::with_embedder(
        data.to_path_buf(),
        data.join("app"),
        EmbedderSource::Provided(hash_embedder()),
    );
    c.extract = extract;
    c.background = false;
    c.tokenizer_json = Some(tokenizer());
    c
}

/// Write files below `root` (paths with `/`).
pub fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

/// Every file under `dir`, recursively.
pub fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Whether `needle` occurs in any file below `dir`.
pub fn bytes_contain(dir: &Path, needle: &[u8]) -> bool {
    all_files(dir).iter().any(|f| {
        std::fs::read(f)
            .map(|b| b.windows(needle.len()).any(|w| w == needle))
            .unwrap_or(false)
    })
}

/// A deterministic embedder that takes `per_text` per document text (a CPU-speed stand-in for
/// the bounded-batch tests, Amendment 4.25). Same profile as [`hash_embedder`], so an index built
/// with one opens with the other. Counts every embedded document text.
pub struct SlowEmbedder {
    pub inner: Arc<HashEmbedder>,
    pub per_text: std::time::Duration,
    pub max_batch: usize,
    pub texts: std::sync::atomic::AtomicUsize,
}

impl SlowEmbedder {
    pub fn new(per_text: std::time::Duration, max_batch: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: hash_embedder(),
            per_text,
            max_batch,
            texts: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn embedded(&self) -> usize {
        self.texts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl sr_providers::EmbeddingProvider for SlowEmbedder {
    fn profile(&self) -> &sr_providers::EmbeddingProfile {
        self.inner.profile()
    }
    fn max_batch(&self) -> usize {
        self.max_batch
    }
    fn embed_batch<'a>(
        &'a self,
        kind: sr_providers::EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            if kind == sr_providers::EmbedKind::Doc {
                // Like an ORT run: once started it finishes (the caller may stop waiting).
                tokio::time::sleep(self.per_text * texts.len() as u32).await;
                self.texts
                    .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.embed_batch(kind, texts, cancel).await
        })
    }
}

/// Engine config with a slow embedder (background writer on).
pub fn slow_config(data: &Path, extract: ExtractSetup, e: Arc<SlowEmbedder>) -> EngineConfig {
    let mut c = config(data, extract);
    c.embedder = EmbedderSource::Provided(e);
    c.background = true;
    c
}

/// Rows per store vs the catalog: `(catalog chunk ids, lance unique, lance rows, tantivy unique,
/// tantivy docs)` of the default collection.
pub async fn store_counts(e: &sr_engine::EngineHandle) -> (usize, usize, usize, usize, usize) {
    let cat: std::collections::HashSet<Vec<u8>> = e
        .catalog()
        .read(|c| {
            let mut st = c
                .prepare(
                    "SELECT c.chunk_id FROM chunks c JOIN files f ON f.id = c.file_id
                     WHERE c.file_version = f.version",
                )
                .map_err(sr_catalog::map_db_err)?;
            let v = st
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .map_err(sr_catalog::map_db_err)?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()
                .map_err(sr_catalog::map_db_err)?;
            Ok(v)
        })
        .unwrap();
    let s = e.registry().all();
    let s = &s[0];
    let rows = s.vectors.rows(None).await.unwrap();
    let lance_rows = rows.len();
    let lance_unique: std::collections::HashSet<_> = rows.iter().map(|r| r.0).collect();
    let lex = s.lexical.all_chunk_ids().unwrap();
    let lex_n = lex.len();
    let lex_unique: std::collections::HashSet<_> = lex.into_iter().collect();
    (
        cat.len(),
        lance_unique.len(),
        lance_rows,
        lex_unique.len(),
        lex_n,
    )
}
