//! GPU runtime policy on the bundled Minimum model (M2-G, §2.8): batch halving above 500 ms, the
//! fullscreen pause (and cancelling while paused), runtime-fault recovery onto a fresh CPU session.
#![cfg(all(feature = "local", windows))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sr_common::error::ErrorCode;
use sr_common::tier::{EpActive, EpPlan, MINIMUM};
use sr_providers::local::{FaultInjection, LocalEmbedder, LocalOptions, min_cosine};
use sr_providers::{EmbedKind, EmbeddingProvider, EpReason, MemorySink, ProviderEvent};
use tokio_util::sync::CancellationToken;

fn dml() -> EpPlan {
    EpPlan::DirectMl {
        dxgi_index: common::dxgi_index(),
    }
}

async fn load_gpu(o: LocalOptions, sink: Arc<MemorySink>) -> Option<LocalEmbedder> {
    let e = LocalEmbedder::load(o, sink).await.expect("load");
    if e.active_ep() != EpActive::DirectMl {
        assert!(
            !common::require_dml(),
            "SR_REQUIRE_DML set but DirectML not active: {:?}",
            e.load_report()
        );
        eprintln!("SKIP: DirectML not available");
        return None;
    }
    Some(e)
}

fn texts(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("chunk {i}: local search keeps working while the index is rebuilt"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_gpu_batches_halve_down_to_the_cpu_batch() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let mut o = common::opts(&dir, dml());
    o.fault = FaultInjection::DmlSlowBatches;
    let Some(e) = load_gpu(o, MemorySink::new()).await else {
        return;
    };
    assert_eq!(e.current_batch(), 64);
    let t = texts(64 * 2 + 32 * 2);
    let refs: Vec<&str> = t.iter().map(String::as_str).collect();
    let v = e.embed_blocking(EmbedKind::Doc, &refs).unwrap();
    assert_eq!(v.len(), refs.len());
    // 64 (warm-up), 64 (> 500 ms → 32), 32 (warm-up), 32 (→ 16).
    assert_eq!(e.current_batch(), 16, "halved twice");
    let v2 = e.embed_blocking(EmbedKind::Doc, &refs[..32]).unwrap();
    assert_eq!(v2.len(), 32);
    // 16 (warm-up), 16 (→ 8 = floor).
    assert_eq!(
        e.current_batch(),
        MINIMUM.batch_cpu as usize,
        "floor = CPU batch"
    );
    assert_eq!(e.active_ep(), EpActive::DirectMl, "slow is not a fault");
}

#[tokio::test(flavor = "multi_thread")]
async fn fullscreen_pause_holds_gpu_batches_and_cancel_ends_the_wait() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let busy_for = Arc::new(AtomicUsize::new(0));
    let mut o = common::opts(&dir, dml());
    let (c, b) = (calls.clone(), busy_for.clone());
    o.gpu_pause = Some(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
        b.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }));
    let Some(e) = load_gpu(o, MemorySink::new()).await else {
        return;
    };
    let t = texts(4);
    let refs: Vec<&str> = t.iter().map(String::as_str).collect();
    // One "busy" answer → one poll interval of waiting before the GPU batch runs.
    busy_for.store(1, Ordering::SeqCst);
    let t0 = Instant::now();
    e.embed_blocking(EmbedKind::Doc, &refs).unwrap();
    let waited = t0.elapsed();
    assert!(
        waited >= Duration::from_millis(1900),
        "paused one poll: {waited:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 2);
    // Queries never pause (CPU session).
    busy_for.store(usize::MAX, Ordering::SeqCst);
    let t0 = Instant::now();
    e.embed_blocking(EmbedKind::Query, &refs[..1]).unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "query did not wait for the GPU"
    );
    // A paused document batch gives up with Cancelled once its job is cancelled.
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let e = Arc::new(e);
    let e2 = e.clone();
    let r = tokio::task::spawn_blocking(move || {
        e2.embed_blocking_cancellable(EmbedKind::Doc, &["paused"], &cancel)
    })
    .await
    .unwrap();
    assert_eq!(r.unwrap_err().code, ErrorCode::Cancelled);
    busy_for.store(0, Ordering::SeqCst);
    let v = e
        .embed_documents(&["free again"], &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(v.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_fault_rebuilds_cpu_and_queries_keep_working() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let sink = MemorySink::new();
    let mut o = common::opts(&dir, dml());
    o.fault = FaultInjection::DmlRunFailsOnce;
    let Some(e) = load_gpu(o, sink.clone()).await else {
        return;
    };
    let cpu = LocalEmbedder::load(common::opts(&dir, EpPlan::Cpu), MemorySink::new())
        .await
        .unwrap();
    let t = texts(3);
    let refs: Vec<&str> = t.iter().map(String::as_str).collect();
    let v = e.embed_blocking(EmbedKind::Doc, &refs).unwrap();
    let want = cpu.embed_blocking(EmbedKind::Doc, &refs).unwrap();
    assert!(min_cosine(&v, &want) >= 0.999_99);
    assert_eq!(e.active_ep(), EpActive::Cpu);
    assert_eq!(e.current_batch(), MINIMUM.batch_cpu as usize);
    assert!(sink.take().iter().any(|ev| matches!(
        ev,
        ProviderEvent::EpChanged {
            from: Some(EpActive::DirectMl),
            to: EpActive::Cpu,
            reason_code: EpReason::RuntimeFault,
            ..
        }
    )));
    let q = e.embed_blocking(EmbedKind::Query, &refs[..1]).unwrap();
    assert_eq!(q[0].len(), 384);
}

/// iGPU / small-dGPU path: the one-off probe must show DirectML ≥ `probe_min_speedup`× the CPU.
/// With an unreachable bar the probe runs (canary passed, timing done) and the provider stays on
/// the CPU silently, reporting the measured speedup for the per-driver cache.
#[tokio::test(flavor = "multi_thread")]
async fn probe_below_the_speedup_bar_stays_on_cpu_silently() {
    let Some(dir) = common::model_dir() else {
        return;
    };
    let plan = EpPlan::ProbeDirectMl {
        dxgi_index: common::dxgi_index(),
    };
    let mut o = common::opts(&dir, plan);
    o.thresholds.probe_min_speedup = 1.0e6;
    let e = LocalEmbedder::load(o, MemorySink::new()).await.unwrap();
    let r = e.load_report().clone();
    println!("probe report: {r:?}");
    if r.reason == EpReason::DllMissing || r.reason == EpReason::RegistrationFailed {
        assert!(!common::require_dml(), "{r:?}");
        return;
    }
    assert_eq!((r.ep, r.reason), (EpActive::Cpu, EpReason::ProbeTooSlow));
    assert!(
        r.canary_min_cos.unwrap() >= 0.999,
        "canary ran before timing"
    );
    let s = r.probe_speedup.expect("speedup measured");
    assert!(s > 0.0 && s < 1.0e6);
    assert_eq!(e.current_batch(), MINIMUM.batch_cpu as usize);
    // Normal bar on this dGPU: the probe passes and DirectML is used.
    let e = LocalEmbedder::load(common::opts(&dir, plan), MemorySink::new())
        .await
        .unwrap();
    let r = e.load_report();
    assert_eq!(
        (r.ep, r.reason),
        (EpActive::DirectMl, EpReason::Planned),
        "{r:?}"
    );
    assert!(r.probe_speedup.unwrap() >= 1.2);
}
