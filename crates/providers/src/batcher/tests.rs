use std::sync::Arc;
use std::time::Duration;

use sr_common::error::ErrorCode;
use sr_common::tier::{EpActive, HIGH, MINIMUM, Thresholds};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::events::{MemorySink, ProviderEvent};
use crate::testing::HashEmbedder;

fn cfg(batch: usize, in_flight: usize, queue: usize) -> BatcherConfig {
    BatcherConfig {
        batch_size: batch,
        max_in_flight: in_flight,
        queue_items: queue,
        max_wait: Duration::from_millis(20),
    }
}

fn texts(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix} {i}")).collect()
}

fn expect(model: &str, t: &[String]) -> Vec<Vec<f32>> {
    t.iter()
        .map(|s| HashEmbedder::vector(model, s, 8))
        .collect()
}

/// Yield until `f` holds or ~2 s pass.
async fn until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f()
}

#[test]
fn config_from_tier_profiles() {
    let t = Thresholds::default();
    let c = BatcherConfig::for_tier(&MINIMUM, EpActive::Cpu, None, &t);
    assert_eq!((c.batch_size, c.max_in_flight), (8, 1));
    let g = BatcherConfig::for_tier(&MINIMUM, EpActive::DirectMl, None, &t);
    assert_eq!((g.batch_size, g.max_in_flight), (64, 2));
    assert!(g.queue_items >= g.batch_size * (g.max_in_flight + 1));
    // VRAM cap (3.6): High on a 4 GiB card starts at half its GPU batch.
    let small = BatcherConfig::for_tier(&HIGH, EpActive::DirectMl, Some(4 << 30), &t);
    assert_eq!(small.batch_size, (HIGH.batch_gpu / 2) as usize);
    let big = BatcherConfig::for_tier(&HIGH, EpActive::DirectMl, Some(24 << 30), &t);
    assert_eq!(big.batch_size, HIGH.batch_gpu as usize);
}

#[tokio::test]
async fn batches_by_size_and_keeps_order() {
    let p = Arc::new(HashEmbedder::new("m", 8));
    let sink = MemorySink::new();
    let b = EmbedBatcher::spawn(p.clone(), cfg(8, 1, 64), sink.clone());
    let input = texts("doc", 20);
    let out = b
        .embed(input.clone(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out, expect("m", &input));
    let calls = p.calls();
    assert!(calls.iter().all(|c| c.len() <= 8), "{calls:?}");
    assert_eq!(calls.iter().map(Vec::len).sum::<usize>(), 20);
    let ev = sink.take();
    assert!(
        ev.iter()
            .all(|e| matches!(e, ProviderEvent::EmbedBatch { ok: true, .. }))
    );
    assert_eq!(b.stats().texts, 20);
    b.shutdown().await;
}

#[tokio::test]
async fn coalesces_small_requests_from_many_callers() {
    let p = Arc::new(HashEmbedder::new("m", 8));
    let b = Arc::new(EmbedBatcher::spawn(
        p.clone(),
        cfg(16, 1, 64),
        Arc::new(crate::events::NoopSink),
    ));
    let mut hs = Vec::new();
    for i in 0..8 {
        let b = b.clone();
        hs.push(tokio::spawn(async move {
            let t = texts(&format!("caller{i}"), 2);
            let v = b.embed(t.clone(), &CancellationToken::new()).await.unwrap();
            assert_eq!(v, expect("m", &t));
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
    // 16 texts from 8 callers within max_wait → far fewer than 8 provider calls.
    assert!(p.calls().len() < 8, "calls = {}", p.calls().len());
}

#[tokio::test]
async fn dedupes_identical_texts_within_a_batch() {
    let p = Arc::new(HashEmbedder::new("m", 8));
    let sink = MemorySink::new();
    let b = EmbedBatcher::spawn(p.clone(), cfg(16, 1, 64), sink.clone());
    // NFC: "é" composed vs decomposed hash equally (sr_common::ids::TextHash).
    let input: Vec<String> = ["a", "b", "a", "caf\u{e9}", "cafe\u{301}", "b", "a", "c"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let out = b
        .embed(input.clone(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.len(), 8);
    assert_eq!(out[0], out[2]);
    assert_eq!(out[0], out[6]);
    assert_eq!(out[1], out[5]);
    assert_eq!(out[3], out[4], "NFC-equal texts share one embedding");
    let sent: usize = p.calls().iter().map(Vec::len).sum();
    assert_eq!(sent, 4, "a, b, café, c");
    let ev = sink.take();
    assert!(
        ev.iter().any(|e| matches!(
            e,
            ProviderEvent::EmbedBatch {
                items: 8,
                unique: 4,
                ..
            }
        )),
        "{ev:?}"
    );
    b.shutdown().await;
}

#[tokio::test]
async fn backpressure_blocks_producers_when_queue_is_full() {
    let gate = Arc::new(Semaphore::new(0));
    let mut fake = HashEmbedder::new("m", 8);
    fake.gate = Some(gate.clone());
    let p = Arc::new(fake);
    // batch 8, one call in flight, 16 texts queued + in flight at most.
    let b = Arc::new(EmbedBatcher::spawn(
        p.clone(),
        cfg(8, 1, 16),
        Arc::new(crate::events::NoopSink),
    ));
    let spawn = |name: &'static str| {
        let b = b.clone();
        tokio::spawn(async move { b.embed(texts(name, 8), &CancellationToken::new()).await })
    };
    let a = spawn("a");
    assert!(until(|| p.calls().len() == 1).await, "A is in flight");
    let bb = spawn("b");
    assert!(
        until(|| b.stats().queued_texts == 16).await,
        "A + B hold all 16 permits"
    );
    let c = spawn("c");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(b.stats().queued_texts, 16, "C must wait for permits");
    assert_eq!(
        p.calls().len(),
        1,
        "only A reached the provider; one call in flight"
    );
    assert!(!c.is_finished());
    gate.add_permits(100);
    let (ra, rb, rc) = (a.await.unwrap(), bb.await.unwrap(), c.await.unwrap());
    assert_eq!(ra.unwrap(), expect("m", &texts("a", 8)));
    assert_eq!(rb.unwrap(), expect("m", &texts("b", 8)));
    assert_eq!(rc.unwrap(), expect("m", &texts("c", 8)));
    assert_eq!(b.stats().queued_texts, 0, "all permits returned");
}

#[tokio::test]
async fn bounded_concurrency() {
    let gate = Arc::new(Semaphore::new(0));
    let mut fake = HashEmbedder::new("m", 8);
    fake.gate = Some(gate.clone());
    let p = Arc::new(fake);
    let b = Arc::new(EmbedBatcher::spawn(
        p.clone(),
        cfg(4, 2, 64),
        Arc::new(crate::events::NoopSink),
    ));
    let mut hs = Vec::new();
    for i in 0..5 {
        let b = b.clone();
        hs.push(tokio::spawn(async move {
            b.embed(texts(&format!("r{i}"), 4), &CancellationToken::new())
                .await
        }));
    }
    assert!(until(|| p.calls().len() == 2).await);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        p.calls().len(),
        2,
        "never more than max_in_flight calls at once"
    );
    assert_eq!(b.stats().in_flight, 2);
    gate.add_permits(100);
    for h in hs {
        h.await.unwrap().unwrap();
    }
    assert_eq!(p.calls().len(), 5);
}

#[tokio::test]
async fn cancel_while_queued_never_reaches_provider() {
    let gate = Arc::new(Semaphore::new(0));
    let mut fake = HashEmbedder::new("m", 8);
    fake.gate = Some(gate.clone());
    let p = Arc::new(fake);
    let b = Arc::new(EmbedBatcher::spawn(
        p.clone(),
        cfg(4, 1, 64),
        Arc::new(crate::events::NoopSink),
    ));
    let b1 = b.clone();
    let first =
        tokio::spawn(async move { b1.embed(texts("first", 4), &CancellationToken::new()).await });
    assert!(until(|| p.calls().len() == 1).await);
    let tok = CancellationToken::new();
    let (b2, t2) = (b.clone(), tok.clone());
    let queued = tokio::spawn(async move { b2.embed(texts("queued", 4), &t2).await });
    assert!(until(|| b.stats().queued_texts == 8).await);
    tok.cancel();
    let r = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.unwrap_err().code, ErrorCode::Cancelled, "returns at once");
    gate.add_permits(100);
    first.await.unwrap().unwrap();
    // Let the dispatcher pick up (and drop) the cancelled piece.
    assert!(until(|| b.stats().queued_texts == 0).await);
    assert!(
        p.calls().iter().flatten().all(|t| !t.starts_with("queued")),
        "{:?}",
        p.calls()
    );
}

#[tokio::test]
async fn cancel_in_flight_returns_immediately_and_spares_others() {
    let gate = Arc::new(Semaphore::new(0));
    let mut fake = HashEmbedder::new("m", 8);
    fake.gate = Some(gate.clone());
    let p = Arc::new(fake);
    let b = Arc::new(EmbedBatcher::spawn(
        p.clone(),
        cfg(8, 1, 64),
        Arc::new(crate::events::NoopSink),
    ));
    let tok = CancellationToken::new();
    let (b1, t1) = (b.clone(), tok.clone());
    let mine = tokio::spawn(async move { b1.embed(texts("mine", 2), &t1).await });
    let b2 = b.clone();
    let other =
        tokio::spawn(async move { b2.embed(texts("other", 2), &CancellationToken::new()).await });
    assert!(until(|| p.calls().len() == 1).await);
    tok.cancel();
    let r = tokio::time::timeout(Duration::from_secs(2), mine)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.unwrap_err().code, ErrorCode::Cancelled);
    gate.add_permits(100);
    let o = other.await.unwrap().unwrap();
    assert_eq!(o, expect("m", &texts("other", 2)));
}

#[tokio::test]
async fn provider_dims_mismatch_is_model_incompatible() {
    let mut fake = HashEmbedder::new("m", 8);
    fake.return_dims = Some(7);
    let b = EmbedBatcher::spawn(
        Arc::new(fake),
        cfg(8, 1, 64),
        Arc::new(crate::events::NoopSink),
    );
    let e = b
        .embed(texts("x", 3), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    b.shutdown().await;
}

#[tokio::test]
async fn provider_error_fans_out_to_every_caller() {
    let mut fake = HashEmbedder::new("m", 8);
    fake.fail = Some(ErrorCode::RateLimited);
    let sink = MemorySink::new();
    let b = Arc::new(EmbedBatcher::spawn(
        Arc::new(fake),
        cfg(16, 1, 64),
        sink.clone(),
    ));
    let (b1, b2) = (b.clone(), b.clone());
    let x = tokio::spawn(async move { b1.embed(texts("x", 2), &CancellationToken::new()).await });
    let y = tokio::spawn(async move { b2.embed(texts("y", 2), &CancellationToken::new()).await });
    for r in [x.await.unwrap(), y.await.unwrap()] {
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrorCode::RateLimited);
        assert!(e.retryable);
    }
    assert!(
        sink.take()
            .iter()
            .all(|e| matches!(e, ProviderEvent::EmbedBatch { ok: false, .. }))
    );
}

#[tokio::test]
async fn shutdown_fails_queued_requests() {
    let gate = Arc::new(Semaphore::new(0));
    let mut fake = HashEmbedder::new("m", 8);
    fake.gate = Some(gate.clone());
    let p = Arc::new(fake);
    let b = EmbedBatcher::spawn(p.clone(), cfg(4, 1, 64), Arc::new(crate::events::NoopSink));
    let shared = Arc::new(b);
    let (s1, s2) = (shared.clone(), shared.clone());
    let a = tokio::spawn(async move { s1.embed(texts("a", 4), &CancellationToken::new()).await });
    assert!(until(|| p.calls().len() == 1).await);
    let q = tokio::spawn(async move { s2.embed(texts("q", 4), &CancellationToken::new()).await });
    assert!(until(|| shared.stats().queued_texts == 8).await);
    shared.shutdown.cancel();
    let r = tokio::time::timeout(Duration::from_secs(2), q)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.unwrap_err().code, ErrorCode::Cancelled);
    gate.add_permits(100);
    // The in-flight call still completes for its caller.
    a.await.unwrap().unwrap();
}

#[tokio::test]
async fn follows_provider_max_batch_after_fallback() {
    let mut fake = HashEmbedder::new("m", 8);
    fake.max_batch = 3; // e.g. GPU → CPU fallback lowered the provider's batch
    let p = Arc::new(fake);
    let b = EmbedBatcher::spawn(p.clone(), cfg(16, 1, 64), Arc::new(crate::events::NoopSink));
    let input = texts("z", 10);
    let out = b
        .embed(input.clone(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out, expect("m", &input));
    assert!(p.calls().iter().all(|c| c.len() <= 3), "{:?}", p.calls());
    b.shutdown().await;
}
