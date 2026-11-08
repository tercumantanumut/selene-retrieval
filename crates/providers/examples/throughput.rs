//! Minimum-model throughput on CPU and DirectML (M1-D task 5).
//!
//! ```text
//! cargo run --release -p sr-providers --example throughput -- <model_dir> [n_chunks=256] [cpu|dml|both] [dxgi_index=0] [dml_dir]
//! ```
//! Corpus: this repo's `docs/**/*.md` (prose + tables + code), cut into chunks of ~1 600 chars
//! (≈ 400 tokens, the production chunk size, Amendment 3.5) and of ~2 600 chars (truncated to
//! 512 tokens by the tokenizer: M0-E's worst case). Batches come from the TierProfile for the EP
//! (CPU 8, DirectML 64), calls go through `EmbedBatcher` exactly as indexing does.
#![allow(clippy::expect_used)] // dev benchmark binary

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sr_common::tier::{EpActive, EpPlan, MINIMUM, Thresholds, TierProfile};
use sr_providers::local::{LocalEmbedder, LocalOptions};
use sr_providers::{BatcherConfig, EmbedBatcher, ModelPaths, NoopSink};
use tokio_util::sync::CancellationToken;

fn corpus(chars: usize, n: usize) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let mut files = Vec::new();
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for p in v {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "md") {
                out.push(p);
            }
        }
    }
    walk(&root, &mut files);
    let text: String = files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let chars_v: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while out.len() < n {
        if i + chars > chars_v.len() {
            i = 0;
        }
        // distinct suffix so the batcher's dedupe never kicks in
        out.push(format!(
            "{} #{}",
            chars_v[i..i + chars].iter().collect::<String>(),
            out.len()
        ));
        i += chars;
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).expect("model dir"));
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(256);
    let which = args.get(3).map(String::as_str).unwrap_or("both").to_owned();
    let dxgi: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let dml_dir = args.get(5).map(PathBuf::from);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let physical = std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1) as u32;
    println!(
        "machine: logical={} assumed physical={physical} cpu intra-op threads={}",
        physical * 2,
        TierProfile::cpu_threads(physical)
    );
    for (ep_name, plan) in [
        ("cpu", EpPlan::Cpu),
        ("dml", EpPlan::DirectMl { dxgi_index: dxgi }),
    ] {
        if which != "both" && which != ep_name {
            continue;
        }
        let mut o = LocalOptions::new(
            &MINIMUM,
            MINIMUM.gpu_file,
            ModelPaths::in_dir(&dir, &MINIMUM.gpu_file),
            plan,
        );
        o.physical_cores = physical;
        if let Some(d) = &dml_dir {
            o.dml_search_dirs = vec![d.clone()];
        }
        let t = Instant::now();
        let e = Arc::new(LocalEmbedder::load_blocking(o, Arc::new(NoopSink)).expect("load"));
        let r = e.load_report().clone();
        println!(
            "[{ep_name}] load {:.2}s active={:?} reason={:?} canary_min_cos={:?} dml={:?}",
            t.elapsed().as_secs_f64(),
            r.ep,
            r.reason,
            r.canary_min_cos,
            r.dml.as_ref().map(|m| &m.path)
        );
        if ep_name == "dml" && r.ep != EpActive::DirectMl {
            println!("[dml] DirectML not active, skipping");
            continue;
        }
        let cfg = BatcherConfig::for_tier(&MINIMUM, r.ep, None, &Thresholds::default());
        rt.block_on(async {
            let b = EmbedBatcher::spawn(e.clone(), cfg, Arc::new(NoopSink));
            for (label, chars) in [("~400 tok", 1600usize), ("512 tok (trunc)", 2600)] {
                let texts = corpus(chars, n);
                // warm-up (DirectML compiles kernels per shape)
                b.embed(texts[..cfg.batch_size.min(n)].to_vec(), &CancellationToken::new()).await.expect("warm-up");
                let t = Instant::now();
                let v = b.embed(texts, &CancellationToken::new()).await.expect("embed");
                let s = t.elapsed().as_secs_f64();
                println!(
                    "[{ep_name}] {label}: {n} chunks in {s:.2}s = {:.1} chunks/s (batch {}, in-flight {}, dims {})",
                    n as f64 / s,
                    cfg.batch_size,
                    cfg.max_in_flight,
                    v[0].len()
                );
            }
            b.shutdown().await;
        });
    }
}
