//! Standard / High model files (M2-G, O3): the pinned fp16 GPU files load and embed on the CPU and
//! on DirectML, match the fp32 file, and report their output element type. Files live in
//! `crates/providers/models/{arctic-m-v1.5,arctic-l}/` (gitignored; fetched by
//! `cargo xtask fetch-models --all`). Without them the tests SKIP unless `SR_REQUIRE_MODELS=1`.
#![cfg(all(feature = "local", windows))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};

use sr_common::tier::{EpActive, EpPlan, HIGH, ModelFile, STANDARD, TierProfile};
use sr_providers::local::{LocalEmbedder, LocalOptions, min_cosine};
use sr_providers::{EmbedKind, EpReason, MemorySink, ModelPaths};

fn tier_dir(name: &str, file: &ModelFile) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("models")
        .join(name);
    let p = ModelPaths::in_dir(&dir, file);
    if p.onnx.is_file() {
        return Some(dir);
    }
    assert!(
        std::env::var_os("SR_REQUIRE_MODELS").is_none(),
        "SR_REQUIRE_MODELS set but {} is missing",
        p.onnx.display()
    );
    eprintln!("SKIP: {} missing", p.onnx.display());
    None
}

fn opts(t: &'static TierProfile, file: ModelFile, dir: &Path, ep: EpPlan) -> LocalOptions {
    let mut o = LocalOptions::new(t, file, ModelPaths::in_dir(dir, &file), ep);
    o.dml_search_dirs = common::dml_dirs();
    o
}

fn dml() -> EpPlan {
    EpPlan::DirectMl {
        dxgi_index: common::dxgi_index(),
    }
}

const TEXTS: [&str; 6] = [
    "How do I rebuild the vector index?",
    "Selene Retrieval splits files into chunks of about four hundred tokens with a small overlap, \
     embeds every chunk once and reuses vectors for identical text across a collection.",
    "fn main() { println!(\"hello\"); }",
    "Die Suche funktioniert auch ohne Internetverbindung.",
    "C:\\Users\\me\\Documents\\report-2026.docx",
    "",
];

async fn load(o: LocalOptions) -> LocalEmbedder {
    LocalEmbedder::load(o, MemorySink::new())
        .await
        .expect("a pinned tier file loads (the GPU never makes load fail)")
}

fn embed(e: &LocalEmbedder, kind: EmbedKind) -> Vec<Vec<f32>> {
    e.embed_blocking(kind, &TEXTS).unwrap()
}

/// O3 on the CPU: the fp16 Standard file loads through our own output transformer, reports its
/// output type, and agrees with the fp32 file.
#[tokio::test(flavor = "multi_thread")]
async fn standard_fp16_on_cpu_matches_fp32_and_reports_output_type() {
    let Some(dir) = tier_dir("arctic-m-v1.5", &STANDARD.gpu_file) else {
        return;
    };
    let fp16 = load(opts(&STANDARD, STANDARD.gpu_file, &dir, EpPlan::Cpu)).await;
    let r = fp16.load_report();
    println!("fp16 CPU: {r:?}");
    assert_eq!((r.ep, r.reason), (EpActive::Cpu, EpReason::CpuPlan));
    assert_eq!(
        r.output_dtype.as_deref(),
        Some("f32"),
        "O3: Snowflake's fp16 export keeps f32 I/O"
    );
    let a = embed(&fp16, EmbedKind::Doc);
    assert!(a.iter().all(|v| v.len() == 768));
    let Some(dir32) = tier_dir("arctic-m-v1.5", &STANDARD.cpu_file) else {
        return;
    };
    let fp32 = load(opts(&STANDARD, STANDARD.cpu_file, &dir32, EpPlan::Cpu)).await;
    let b = embed(&fp32, EmbedKind::Doc);
    let c = min_cosine(&a, &b);
    println!("Standard fp16 vs fp32 (CPU) min cos {c:.6}");
    assert!(c >= 0.999, "fp16 vs fp32 min cos {c}");
    // Queries carry the prefix, so they differ from the document vector of the same text.
    let q = embed(&fp16, EmbedKind::Query);
    assert!(min_cosine(&q[..1], &a[..1]) < 0.9999);
}

/// O3 on DirectML (the path the fp16 file exists for), CPU parity, and queries on the CPU (4.27).
#[tokio::test(flavor = "multi_thread")]
async fn standard_fp16_on_directml_matches_cpu_and_queries_stay_on_cpu() {
    let Some(dir) = tier_dir("arctic-m-v1.5", &STANDARD.gpu_file) else {
        return;
    };
    let gpu = load(opts(&STANDARD, STANDARD.gpu_file, &dir, dml())).await;
    let r = gpu.load_report().clone();
    println!("fp16 DirectML: {r:?}");
    if gpu.active_ep() != EpActive::DirectMl {
        assert!(
            !common::require_dml(),
            "SR_REQUIRE_DML set but DirectML not active: {r:?}"
        );
        eprintln!("SKIP: DirectML not available");
        return;
    }
    assert_eq!(r.reason, EpReason::Planned);
    assert!(r.canary_min_cos.unwrap() >= 0.999);
    assert_eq!(gpu.current_batch(), STANDARD.batch_gpu as usize);
    let cpu = load(opts(&STANDARD, STANDARD.gpu_file, &dir, EpPlan::Cpu)).await;
    let g = embed(&gpu, EmbedKind::Doc);
    let c = embed(&cpu, EmbedKind::Doc);
    let m = min_cosine(&g, &c);
    println!("Standard fp16 DirectML vs CPU min cos {m:.6}");
    assert!(m >= 0.999, "{m}");
    // Queries run on the kept CPU session: identical to a CPU-only provider's query vectors.
    let qg = embed(&gpu, EmbedKind::Query);
    let qc = embed(&cpu, EmbedKind::Query);
    let mq = min_cosine(&qg, &qc);
    assert!(
        mq >= 0.999_999,
        "query vectors come from the CPU session: {mq}"
    );
    assert_eq!(
        gpu.active_ep(),
        EpActive::DirectMl,
        "documents still on the GPU"
    );
}

/// High: the fp16 arctic-l file on DirectML vs the same file on the CPU.
#[tokio::test(flavor = "multi_thread")]
async fn high_fp16_on_directml_matches_cpu() {
    let Some(dir) = tier_dir("arctic-l", &HIGH.gpu_file) else {
        return;
    };
    let gpu = load(opts(&HIGH, HIGH.gpu_file, &dir, dml())).await;
    let r = gpu.load_report().clone();
    println!("High fp16 DirectML: {r:?}");
    assert_eq!(r.output_dtype.as_deref(), Some("f32"));
    if gpu.active_ep() != EpActive::DirectMl {
        assert!(
            !common::require_dml(),
            "SR_REQUIRE_DML set but DirectML not active: {r:?}"
        );
        return;
    }
    let g = embed(&gpu, EmbedKind::Doc);
    assert!(g.iter().all(|v| v.len() == 1024));
    let cpu = load(opts(&HIGH, HIGH.gpu_file, &dir, EpPlan::Cpu)).await;
    let c = embed(&cpu, EmbedKind::Doc);
    let m = min_cosine(&g, &c);
    println!("High fp16 DirectML vs CPU min cos {m:.6}");
    assert!(m >= 0.999, "{m}");
}
