//! Bundled Minimum model on the CPU EP: parity with sentence-transformers, prefix, pins, batcher.
#![cfg(feature = "local")]
#![allow(clippy::expect_used, clippy::unwrap_used)] // test code (clippy.toml allows both in tests)

mod common;

use std::sync::Arc;

use sr_common::error::ErrorCode;
use sr_common::tier::{EpActive, EpPlan, MINIMUM, Thresholds};
use sr_providers::local::LocalEmbedder;
use sr_providers::{
    BatcherConfig, EmbedBatcher, EmbedKind, EmbeddingProfile, EmbeddingProvider, EpReason,
    IndexModel, MemorySink, ProviderEvent,
};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn cpu_parity_with_reference_embeddings() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let sink = MemorySink::new();
    let e = LocalEmbedder::load(common::opts(&dir, EpPlan::Cpu), sink.clone())
        .await
        .unwrap();
    assert_eq!(e.active_ep(), EpActive::Cpu);
    assert_eq!(e.load_report().reason, EpReason::CpuPlan);
    assert_eq!(e.dims(), 384);
    assert_eq!(e.variant_sha256(), MINIMUM.gpu_file.sha256);
    assert!(matches!(
        sink.take().as_slice(),
        [ProviderEvent::EpChanged {
            from: None,
            to: EpActive::Cpu,
            reason_code: EpReason::CpuPlan,
            ..
        }]
    ));

    let s = common::sentences();
    let refs: Vec<&str> = s.iter().map(String::as_str).collect();
    let got = e
        .embed_documents(&refs, &CancellationToken::new())
        .await
        .unwrap();
    for v in &got {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "L2-normalised, got {n}");
    }
    let (min, mean) = common::stats(&got, &common::reference());
    eprintln!(
        "CPU vs sentence-transformers: n={} min={min:.6} mean={mean:.6}",
        got.len()
    );
    assert!(min >= 0.999, "min cosine {min}");
}

#[tokio::test(flavor = "multi_thread")]
async fn query_prefix_and_index_check() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let e = LocalEmbedder::load(common::opts(&dir, EpPlan::Cpu), common_sink())
        .await
        .unwrap();
    let c = CancellationToken::new();
    let q = e
        .embed_query("how do I rebuild the vector index?", &c)
        .await
        .unwrap();
    let d = e
        .embed_batch(
            EmbedKind::Doc,
            &["Represent this sentence for searching relevant passages: how do I rebuild the vector index?"],
            &c,
        )
        .await
        .unwrap();
    assert!(
        sr_providers::local::min_cosine(std::slice::from_ref(&q), &d) > 0.99999,
        "query = prefix + text"
    );
    // fixture sentence #1 is exactly that prefixed text
    let (min, _) = common::stats(&[q], &common::reference()[1..2]);
    assert!(min >= 0.999);

    let index = IndexModel::from(&EmbeddingProfile::local(&MINIMUM, &MINIMUM.gpu_file));
    assert_eq!(e.embed_query_for(&index, "x", &c).await.unwrap().len(), 384);
    let other = IndexModel { dims: 768, ..index };
    assert_eq!(
        e.embed_query_for(&other, "x", &c).await.unwrap_err().code,
        ErrorCode::ModelIncompatible
    );
}

fn common_sink() -> Arc<dyn sr_providers::EventSink> {
    Arc::new(sr_providers::NoopSink)
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_or_missing_files_are_refused() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let tmp = std::env::temp_dir().join(format!("sr-m1d-tamper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for f in [
        "model_optimized.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        std::fs::copy(dir.join(f), tmp.join(f)).unwrap();
    }
    // One flipped byte in the tokenizer: refused before ORT ever sees the model.
    let mut tok = std::fs::read(tmp.join("tokenizer.json")).unwrap();
    tok[100] ^= 0x01;
    std::fs::write(tmp.join("tokenizer.json"), tok).unwrap();
    let e = LocalEmbedder::load(common::opts(&tmp, EpPlan::Cpu), common_sink())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIntegrity);
    assert_eq!(e.ctx.model.as_deref(), Some(MINIMUM.model_id));
    std::fs::remove_file(tmp.join("config.json")).unwrap();
    std::fs::copy(dir.join("tokenizer.json"), tmp.join("tokenizer.json")).unwrap();
    let e = LocalEmbedder::load(common::opts(&tmp, EpPlan::Cpu), common_sink())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn batcher_over_local_embedder_matches_direct_calls() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let e = Arc::new(
        LocalEmbedder::load(common::opts(&dir, EpPlan::Cpu), common_sink())
            .await
            .unwrap(),
    );
    let cfg = BatcherConfig::for_tier(&MINIMUM, e.active_ep(), None, &Thresholds::default());
    assert_eq!(cfg.batch_size, 8);
    let sink = MemorySink::new();
    let b = EmbedBatcher::spawn(e.clone(), cfg, sink.clone());
    // 40 texts, each sentence twice in a row → each pair lands in the same batch of 8.
    let s: Vec<String> = common::sentences()
        .into_iter()
        .flat_map(|x| [x.clone(), x])
        .collect();
    let got = b.embed(s, &CancellationToken::new()).await.unwrap();
    let firsts: Vec<Vec<f32>> = got.iter().step_by(2).cloned().collect();
    let (min, _) = common::stats(&firsts, &common::reference());
    assert!(min >= 0.999);
    for pair in got.chunks(2) {
        assert_eq!(pair[0], pair[1], "duplicates get identical vectors");
    }
    let ev = sink.take();
    let (items, unique) = ev.iter().fold((0, 0), |(i, u), e| match e {
        ProviderEvent::EmbedBatch {
            items,
            unique,
            ok: true,
            ..
        } => (i + items, u + unique),
        _ => (i, u),
    });
    assert_eq!(items, 40);
    assert_eq!(unique, 20, "dedupe by text_hash within a batch");
    b.shutdown().await;
}
