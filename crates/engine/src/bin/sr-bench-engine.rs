//! sr-bench-engine: the engine behind the xtask eval harness's `cmd:` adapter (m1-f-eval.md §3).
//!
//! Builds (or reopens) an index over a corpus with the **real engine** (catalog, sandboxed
//! extraction, chunker, embedder, Lance + tantivy, IndexWriter), then answers line-JSON queries:
//! stdin `{"qid","query","k"}` → stdout `{"hits":[{"path","line_start","line_end","score"}],
//! "latency_ms"}`. Nothing else is ever written to stdout.
//!
//! ```text
//! sr-bench-engine --index <short dir> (--corpus-dir <dir> | --corpus-jsonl <corpus.jsonl>)
//!                 [--model fp32|int8] [--int8-onnx <file>] [--ep cpu|dml] [--mode hybrid|dense]
//!                 [--app-dir <dir with models\bge-small-en-v1.5>] [--worker <sr-extract.exe>]
//!                 [--rebuild] [--verbose] [--cutoff on|off|<dense>,<bm25>]
//!                 [--exact-lane on|off|ungated] [--rerank on|off] [--rerank-top-n <n>]
//!                 [--rerank-mode replace|rrf:<weight>] [--rerank-skip-lookups on|off]
//!                 [--rerank-max-tokens <n>] [--first-stage search|answer]
//! ```
//! `--first-stage answer` (docs/08) returns, per query, the `k` passages an answer with a first
//! search of `k` would show the model (a wider search, then the file-by-file pick) instead of the
//! search's top `k`.
//! `--corpus-jsonl` (BEIR: `{"_id","title","text"}`) is materialised as one `<id>.txt` per document
//! under the index dir; hit paths are then reported as the document id. Every run checks that each
//! document file is present with the expected size (before and after indexing) and fails with the
//! missing ids otherwise (antivirus may quarantine posts that quote malicious code); `--rebuild`
//! rewrites an incomplete corpus.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sr_engine::{EmbedderSource, Engine, EngineConfig, ExtractSetup, SearchArgs, SearchMode};

#[derive(Debug, Default)]
struct Args {
    index: PathBuf,
    corpus_dir: Option<PathBuf>,
    corpus_jsonl: Option<PathBuf>,
    model: String,
    int8_onnx: Option<PathBuf>,
    ep: String,
    mode: String,
    app_dir: Option<PathBuf>,
    worker: Option<PathBuf>,
    rebuild: bool,
    verbose: bool,
    /// R-UX18 calibration: `on` (default), `off`, or `<min_top_dense>,<min_top_bm25>`.
    cutoff: String,
    /// R-UX33 A/B: the exact identifier / number lane, `on` (default) or `off`.
    exact_lane: bool,
    /// M5-E (R-UX37): `--exact-lane ungated` = the M3/M4 lane (ranks for every query).
    exact_lane_ungated: bool,
    /// M5-E (R-UX36) reranker A/B knobs; `None` = the engine default.
    rerank_top_n: Option<usize>,
    rerank_mode: Option<sr_core::query::RerankFusion>,
    rerank_skip_lookups: Option<bool>,
    rerank_max_tokens: Option<usize>,
    /// docs/08: rank what an answer's first search would pick (`answer`) instead of the plain
    /// search order (`search`, default).
    answer_stage: bool,
    /// M3 reranker A/B: `on` turns the Advanced reranker on (downloads its pinned files into the
    /// index's data dir on first use, like the app's switch) and waits until it is loaded.
    rerank: bool,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        model: "fp32".into(),
        ep: "cpu".into(),
        mode: "hybrid".into(),
        cutoff: "on".into(),
        exact_lane: true,
        ..Args::default()
    };
    let mut it = std::env::args().skip(1);
    let mut index = None;
    while let Some(x) = it.next() {
        let mut val = || it.next().ok_or(format!("{x} needs a value"));
        match x.as_str() {
            "--index" => index = Some(PathBuf::from(val()?)),
            "--corpus-dir" => a.corpus_dir = Some(PathBuf::from(val()?)),
            "--corpus-jsonl" => a.corpus_jsonl = Some(PathBuf::from(val()?)),
            "--model" => a.model = val()?,
            "--int8-onnx" => a.int8_onnx = Some(PathBuf::from(val()?)),
            "--ep" => a.ep = val()?,
            "--mode" => a.mode = val()?,
            "--app-dir" => a.app_dir = Some(PathBuf::from(val()?)),
            "--worker" => a.worker = Some(PathBuf::from(val()?)),
            "--rebuild" => a.rebuild = true,
            "--verbose" => a.verbose = true,
            "--cutoff" => a.cutoff = val()?,
            "--exact-lane" => {
                let v = val()?;
                a.exact_lane = match v.as_str() {
                    "on" | "ungated" => true,
                    "off" => false,
                    _ => return Err("--exact-lane needs on, off or ungated".into()),
                };
                a.exact_lane_ungated = v == "ungated";
            }
            "--rerank-max-tokens" => {
                a.rerank_max_tokens = Some(val()?.parse().map_err(|_| "bad --rerank-max-tokens")?);
            }
            "--rerank-top-n" => {
                a.rerank_top_n = Some(val()?.parse().map_err(|_| "bad --rerank-top-n")?);
            }
            "--rerank-mode" => {
                let v = val()?;
                a.rerank_mode = Some(match v.split_once(':') {
                    None if v == "replace" => sr_core::query::RerankFusion::Replace,
                    Some(("rrf", w)) => sr_core::query::RerankFusion::Rrf {
                        weight: w.parse().map_err(|_| "bad --rerank-mode rrf weight")?,
                    },
                    _ => return Err("--rerank-mode needs replace or rrf:<weight>".into()),
                });
            }
            "--rerank-skip-lookups" => {
                a.rerank_skip_lookups = Some(match val()?.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--rerank-skip-lookups needs on or off".into()),
                });
            }
            "--first-stage" => {
                a.answer_stage = match val()?.as_str() {
                    "answer" => true,
                    "search" => false,
                    _ => return Err("--first-stage needs search or answer".into()),
                }
            }
            "--rerank" => {
                a.rerank = match val()?.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--rerank needs on or off".into()),
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    a.index = index.ok_or("--index is required")?;
    if a.corpus_dir.is_none() == a.corpus_jsonl.is_none() {
        return Err("exactly one of --corpus-dir / --corpus-jsonl is required".into());
    }
    Ok(a)
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One BEIR document as the bench writes it: the id and the file body (title + text).
struct Doc {
    id: String,
    body: String,
}

/// Reads a BEIR corpus (`{"_id","title","text"}` per line) into documents.
fn read_jsonl(src: &Path) -> Result<Vec<Doc>, String> {
    let f = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let mut docs = Vec::new();
    for line in std::io::BufReader::new(f).lines() {
        let line = line.map_err(|e| format!("{}: {e}", src.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(&line).map_err(|e| format!("{}: {e}", src.display()))?;
        let id = v["_id"].as_str().ok_or("document without _id")?;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("unsupported document id {id:?}"));
        }
        let title = v["title"].as_str().unwrap_or_default();
        let text = v["text"].as_str().unwrap_or_default();
        let body = if title.is_empty() {
            text.to_string()
        } else {
            format!("{title}\n\n{text}")
        };
        docs.push(Doc {
            id: id.to_string(),
            body,
        });
    }
    Ok(docs)
}

/// The documents whose `<id>.txt` is absent or does not hold the expected bytes (sorted, unique).
fn missing_docs(dir: &Path, docs: &[Doc]) -> Vec<String> {
    let mut missing: Vec<String> = docs
        .iter()
        .filter(|d| {
            !std::fs::metadata(dir.join(format!("{}.txt", d.id)))
                .is_ok_and(|m| m.is_file() && m.len() == d.body.len() as u64)
        })
        .map(|d| d.id.clone())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// `a, b, c and N more` for stderr.
fn id_list(ids: &[String]) -> String {
    const SHOWN: usize = 20;
    let mut s = ids
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > SHOWN {
        s.push_str(&format!(" and {} more", ids.len() - SHOWN));
    }
    s
}

const DEFENDER_HINT: &str = "Some BEIR posts quote malicious web code, so antivirus (Windows \
    Defender) may block or quarantine these inert text files. Add a Defender exclusion for the \
    bench index directory, then rerun with --rebuild.";

/// BEIR corpus → one text file per document (title + text). Returns the folder.
///
/// The engine indexes files in a folder, and a hit's path must be the document id, so the corpus
/// stays one `<id>.txt` per document. A `.complete` marker (the document count) is only trusted
/// when every expected document is present with the expected size: otherwise the run fails with
/// the missing ids (`rebuild` = false), or rewrites the corpus (`rebuild` = true, the stores are
/// already dropped). Write failures are collected and reported together.
fn materialise_jsonl(
    src: &Path,
    index: &Path,
    rebuild: bool,
) -> Result<(PathBuf, Vec<Doc>), String> {
    let dir = index.join("corpus-files");
    let marker = dir.join(".complete");
    let docs = read_jsonl(src)?;
    if marker.exists() {
        let recorded = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());
        let missing = missing_docs(&dir, &docs);
        if recorded == Some(docs.len()) && missing.is_empty() {
            return Ok((dir, docs));
        }
        let what = format!(
            "corpus {} is incomplete: marker says {}, the source has {} documents, {} missing or \
             changed ({})",
            dir.display(),
            recorded.map_or_else(|| "unreadable".to_string(), |n| n.to_string()),
            docs.len(),
            missing.len(),
            id_list(&missing)
        );
        if !rebuild {
            return Err(format!("{what}. {DEFENDER_HINT}"));
        }
        eprintln!("{what}; rewriting it (--rebuild)");
        std::fs::remove_file(&marker).map_err(|e| format!("{}: {e}", marker.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut failed = Vec::new();
    for d in &docs {
        let p = dir.join(format!("{}.txt", d.id));
        if let Err(e) = std::fs::write(&p, &d.body) {
            eprintln!("write {}: {e}", p.display());
            failed.push(d.id.clone());
        }
    }
    // Catches files removed right after they were written (real-time antivirus scan on close).
    let mut missing = missing_docs(&dir, &docs);
    missing.extend(failed);
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!(
            "could not materialise {} of {} documents in {} ({}). {DEFENDER_HINT}",
            missing.len(),
            docs.len(),
            dir.display(),
            id_list(&missing)
        ));
    }
    std::fs::write(&marker, docs.len().to_string())
        .map_err(|e| format!("{}: {e}", marker.display()))?;
    Ok((dir, docs))
}

#[cfg(feature = "local")]
async fn embedder(
    a: &Args,
    app_dir: &Path,
) -> Result<Arc<dyn sr_providers::EmbeddingProvider>, String> {
    use sr_common::tier::{EpPlan, MINIMUM, TierProfile, files};
    use sr_providers::{LocalEmbedder, LocalOptions, ModelPaths, NoopSink};
    let ep = match a.ep.as_str() {
        "cpu" => EpPlan::Cpu,
        "dml" => EpPlan::DirectMl { dxgi_index: 0 },
        other => return Err(format!("unknown --ep {other}")),
    };
    let bundled = app_dir
        .join("models")
        .join(sr_providers::models::BUNDLED_MODEL_DIR);
    let opts = match a.model.as_str() {
        "fp32" => LocalOptions::bundled(app_dir, ep),
        "int8" => {
            // Eval-only (Amendment 3.4): a Minimum profile whose CPU variant is the pinned int8
            // file, so the verifying loader accepts it. Never used by the daemon.
            let onnx = a
                .int8_onnx
                .clone()
                .ok_or("--model int8 needs --int8-onnx <model_int8.onnx>")?;
            let tier: &'static TierProfile = Box::leak(Box::new(TierProfile {
                cpu_file: files::BGE_SMALL_INT8,
                ..MINIMUM
            }));
            LocalOptions::new(
                tier,
                files::BGE_SMALL_INT8,
                ModelPaths {
                    onnx,
                    tokenizer_dir: bundled,
                },
                EpPlan::Cpu,
            )
        }
        other => return Err(format!("unknown --model {other}")),
    };
    let e = LocalEmbedder::load(opts, Arc::new(NoopSink))
        .await
        .map_err(|e| format!("model: {e}"))?;
    if a.verbose {
        eprintln!(
            "embedder: {:?} ({} ms)",
            e.active_ep(),
            e.load_report().load_ms
        );
    }
    Ok(Arc::new(e))
}

#[cfg(not(feature = "local"))]
async fn embedder(_: &Args, _: &Path) -> Result<Arc<dyn sr_providers::EmbeddingProvider>, String> {
    Err("built without the `local` feature".into())
}

async fn run(a: Args) -> Result<(), String> {
    let app_dir = a.app_dir.clone().unwrap_or_else(exe_dir);
    if a.rebuild && a.index.exists() {
        // Keep the materialised corpus; drop the stores and catalog.
        for sub in [
            "catalog.db",
            "catalog.db-wal",
            "catalog.db-shm",
            "lance",
            "tantivy",
        ] {
            let p = a.index.join(sub);
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    std::fs::create_dir_all(&a.index).map_err(|e| e.to_string())?;
    let (corpus, jsonl_docs) = match (&a.corpus_dir, &a.corpus_jsonl) {
        (Some(d), _) => (d.clone(), Vec::new()),
        (None, Some(j)) => materialise_jsonl(j, &a.index, a.rebuild)?,
        _ => return Err("no corpus".into()),
    };
    let provider = embedder(&a, &app_dir).await?;
    let mut cfg = EngineConfig::with_embedder(
        a.index.join("data"),
        app_dir.clone(),
        EmbedderSource::Provided(provider),
    );
    let worker = a
        .worker
        .clone()
        .unwrap_or_else(|| exe_dir().join("sr-extract.exe"));
    let mut xc = sr_extract_client::ExtractConfig::new(worker, None);
    // Its own sandbox profile (deleted when the process ends), never the production one: a dev
    // tool must not create or leak `SeleneRetrieval.Extract` (M2 tester; M2-I finding).
    xc.app_container_name = BENCH_PROFILE.into();
    xc.workers = 4;
    xc.queue = 8;
    cfg.extract = ExtractSetup::Config(Box::new(xc));
    cfg.background = false;
    cfg.job_permits = 8;
    cfg.tokenizer_json = Some(
        app_dir
            .join("models")
            .join(sr_providers::models::BUNDLED_MODEL_DIR)
            .join("tokenizer.json"),
    );
    let mut q = sr_core_query_default();
    if a.mode == "dense" {
        q.mode = SearchMode::DenseOnly;
    }
    q.no_match = match a.cutoff.as_str() {
        "on" => sr_core::query::NoMatchMode::Model,
        "off" => sr_core::query::NoMatchMode::Off,
        custom => {
            let (d, b) = custom
                .split_once(',')
                .ok_or("--cutoff needs on, off or <dense>,<bm25>")?;
            sr_core::query::NoMatchMode::Fixed(sr_core::query::NoMatchCutoff {
                min_top_dense: d.parse().map_err(|_| "bad --cutoff dense value")?,
                min_top_bm25: b.parse().map_err(|_| "bad --cutoff bm25 value")?,
            })
        }
    };
    q.exact_lane = a.exact_lane;
    q.exact_lane_gate = !a.exact_lane_ungated;
    if let Some(n) = a.rerank_top_n {
        q.rerank_top_n = n;
    }
    if let Some(m) = a.rerank_mode {
        q.rerank_fusion = m;
    }
    if let Some(s) = a.rerank_skip_lookups {
        q.rerank_skip_lookups = s;
    }
    cfg.query = Some(q);
    cfg.rerank_max_tokens = a.rerank_max_tokens;
    let engine = Engine::start(cfg)
        .await
        .map_err(|e| format!("engine: {e}"))?;
    let folders = engine.list_folders().await.map_err(|e| e.to_string())?;
    let corpus_s = corpus.to_string_lossy().to_string();
    if folders.is_empty() {
        engine
            .add_folder(&corpus_s, Some("corpus".into()))
            .await
            .map_err(|e| format!("add folder: {e}"))?;
    }
    let t0 = Instant::now();
    let jobs = engine
        .run_pending_jobs()
        .await
        .map_err(|e| format!("index: {e}"))?;
    let f = engine.list_folders().await.map_err(|e| e.to_string())?;
    if a.verbose {
        eprintln!(
            "indexed: jobs={jobs} files={}/{} skipped={} in {:.1}s",
            f.first().map_or(0, |x| x.files_indexed),
            f.first().map_or(0, |x| x.files_total),
            f.first().map_or(0, |x| x.files_skipped),
            t0.elapsed().as_secs_f64()
        );
    }
    // A document removed while the engine was indexing (antivirus) would silently lower recall.
    let missing = missing_docs(&corpus, &jsonl_docs);
    if !missing.is_empty() {
        return Err(format!(
            "{} of {} corpus documents disappeared from {} during indexing ({}). {DEFENDER_HINT}",
            missing.len(),
            jsonl_docs.len(),
            corpus.display(),
            id_list(&missing)
        ));
    }
    if a.rerank {
        enable_reranker(&engine, a.verbose).await?;
    }
    let strip_txt = a.corpus_jsonl.is_some();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let q: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(out, "{}", json!({ "error": e.to_string() }))
                    .map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                continue;
            }
        };
        let k = q["k"].as_u64().unwrap_or(50).clamp(1, 150) as u32;
        let t = Instant::now();
        let query = q["query"].as_str().unwrap_or_default().to_string();
        // The harness ranks the whole list: no minScore cut.
        let r = if a.answer_stage {
            engine
                .answer_passages(&query, k as usize, None, Some(0.0))
                .await
        } else {
            engine
                .search(SearchArgs {
                    query,
                    max_results: Some(k),
                    min_score: Some(0.0),
                    folder_ids: None,
                    mode: None,
                    collapse_duplicates: None,
                })
                .await
        };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let resp = match r {
            Ok(o) => json!({
                "hits": o.hits.iter().map(|h| {
                    let p = if strip_txt { h.rel_path.trim_end_matches(".txt").to_string() } else { h.rel_path.clone() };
                    json!({"path": p, "line_start": h.line_start, "line_end": h.line_end, "score": h.score})
                }).collect::<Vec<_>>(),
                "latency_ms": ms,
                "no_good_match": o.no_good_match,
                "top_dense": o.top_dense,
                "top_bm25": o.top_bm25,
            }),
            Err(e) => json!({ "error": e.to_string() }),
        };
        writeln!(out, "{resp}").map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    tokio::time::timeout(Duration::from_secs(60), engine.drain())
        .await
        .ok();
    Ok(())
}

/// Turns the reranker on (same path as the app's Advanced switch) and waits until it is loaded
/// (download included; up to 20 min).
async fn enable_reranker(engine: &sr_engine::EngineHandle, verbose: bool) -> Result<(), String> {
    use sr_engine::rerank::RerankerPhase;
    engine
        .update_advanced(sr_engine::appops::AdvancedUpdate {
            rerank_enabled: Some(true),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("rerank on: {e}"))?;
    let _ = engine.reranker_download(true);
    let t0 = Instant::now();
    loop {
        let s = engine.reranker_status();
        match s.phase {
            RerankerPhase::Ready => {
                if verbose {
                    eprintln!("reranker: ready after {:.1}s", t0.elapsed().as_secs_f64());
                }
                return Ok(());
            }
            RerankerPhase::Failed { .. } => return Err(format!("reranker failed: {s:?}")),
            _ if t0.elapsed() > Duration::from_secs(1200) => {
                return Err(format!("reranker not ready after 20 min: {s:?}"));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// The bench's sandbox profile for the extract workers.
const BENCH_PROFILE: &str = "SeleneRetrieval.Extract.Bench";

fn sr_core_query_default() -> sr_core::query::QueryConfig {
    sr_core::query::QueryConfig::default()
}

fn main() -> std::process::ExitCode {
    sr_providers::dml::harden_dll_search();
    let a = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(1);
        }
    };
    let r = rt.block_on(run(a));
    drop(rt);
    let _ = sr_extract_client::delete_app_container_profile(BENCH_PROFILE);
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(dir: &Path) -> PathBuf {
        let src = dir.join("corpus.jsonl");
        std::fs::write(
            &src,
            concat!(
                "{\"_id\":\"1\",\"title\":\"T\",\"text\":\"one\"}\n",
                "\n",
                "{\"_id\":\"2\",\"title\":\"\",\"text\":\"two\"}\n",
                "{\"_id\":\"3\",\"text\":\"three\"}\n",
            ),
        )
        .unwrap();
        src
    }

    #[test]
    fn complete_corpus_is_reused() {
        let t = tempfile::tempdir().unwrap();
        let src = corpus(t.path());
        let index = t.path().join("idx");
        let (dir, docs) = materialise_jsonl(&src, &index, false).unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(
            std::fs::read_to_string(dir.join("1.txt")).unwrap(),
            "T\n\none"
        );
        assert_eq!(std::fs::read_to_string(dir.join("2.txt")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(dir.join(".complete")).unwrap(), "3");
        assert!(missing_docs(&dir, &docs).is_empty());
        materialise_jsonl(&src, &index, false).unwrap();
    }

    #[test]
    fn missing_or_truncated_document_fails_reuse_and_rebuild_restores_it() {
        let t = tempfile::tempdir().unwrap();
        let src = corpus(t.path());
        let index = t.path().join("idx");
        let (dir, _) = materialise_jsonl(&src, &index, false).unwrap();
        // As if quarantined after the marker was written, plus a truncated file.
        std::fs::remove_file(dir.join("2.txt")).unwrap();
        std::fs::write(dir.join("3.txt"), "thr").unwrap();
        let err = materialise_jsonl(&src, &index, false).err().unwrap();
        assert!(err.contains("2 missing or changed (2, 3)"), "{err}");
        assert!(err.contains("--rebuild"), "{err}");
        let (dir, docs) = materialise_jsonl(&src, &index, true).unwrap();
        assert!(missing_docs(&dir, &docs).is_empty());
    }

    #[test]
    fn marker_count_mismatch_fails_reuse() {
        let t = tempfile::tempdir().unwrap();
        let src = corpus(t.path());
        let index = t.path().join("idx");
        let (dir, _) = materialise_jsonl(&src, &index, false).unwrap();
        std::fs::write(dir.join(".complete"), "4").unwrap();
        let err = materialise_jsonl(&src, &index, false).err().unwrap();
        assert!(err.contains("marker says 4"), "{err}");
    }

    #[test]
    fn id_list_is_capped() {
        let ids: Vec<String> = (0..25).map(|i| i.to_string()).collect();
        assert!(id_list(&ids).ends_with("19 and 5 more"));
    }
}
