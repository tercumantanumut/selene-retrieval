use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose, SuggestedAction};
use sr_common::tier::{MINIMUM, STANDARD};
use tokio_util::sync::CancellationToken;

use crate::embed::model_incompatible;
use crate::testing::HashEmbedder;
use crate::*;

#[test]
fn local_only_blocks_with_allow_action() {
    let e = LocalOnly.check("OpenAI", Purpose::EmbedQuery).unwrap_err();
    assert_eq!(e.code, ErrorCode::PrivacyBlocked);
    assert_eq!(
        e.user_message().action,
        SuggestedAction::AllowRemote("OpenAI".into(), Purpose::EmbedQuery)
    );
}

#[test]
fn profile_from_tier_and_fingerprint() {
    let p = EmbeddingProfile::local(&MINIMUM, &MINIMUM.gpu_file);
    assert_eq!(p.model, "BAAI/bge-small-en-v1.5");
    assert_eq!(p.dims, 384);
    assert_eq!(p.pooling, "cls");
    assert_eq!(p.precision, "fp32");
    assert_eq!(p.variant_sha256, MINIMUM.gpu_file.sha256);
    assert_eq!(
        p.query_prefix,
        "Represent this sentence for searching relevant passages: "
    );
    assert_eq!(p.prepare(EmbedKind::Doc, "x"), "x");
    assert_eq!(
        p.prepare(EmbedKind::Query, "x"),
        "Represent this sentence for searching relevant passages: x"
    );
    // any field change → different fingerprint, including the variant file (sticky variant)
    let gpu = EmbeddingProfile::local(&STANDARD, &STANDARD.gpu_file);
    let cpu = EmbeddingProfile::local(&STANDARD, &STANDARD.cpu_file);
    assert_ne!(gpu.fingerprint(), cpu.fingerprint());
    assert_eq!(p.fingerprint(), p.clone().fingerprint());
    let q = EmbeddingProfile {
        pooling: "mean".into(),
        ..p.clone()
    };
    assert_ne!(p.fingerprint(), q.fingerprint());
}

#[tokio::test]
async fn query_with_provider_of_other_dims_is_model_incompatible() {
    let index = IndexModel::from(&EmbeddingProfile::local(&MINIMUM, &MINIMUM.gpu_file));
    // A 768-dim provider queried against a 384-dim index: refused before embedding.
    let p = HashEmbedder::new("BAAI/bge-small-en-v1.5", 768);
    let e = p
        .embed_query_for(&index, "hello", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    assert!(p.calls().is_empty(), "never compared, never embedded");
    assert_eq!(e.user_message().action, SuggestedAction::CopyDiagnostics);

    // Same dims, different variant file: also refused.
    let mut same_dims = HashEmbedder::new("BAAI/bge-small-en-v1.5", 384);
    same_dims.profile.variant_sha256 = "0".repeat(64);
    let e = same_dims
        .embed_query_for(&index, "hello", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);

    // A provider that *claims* 384 but returns 383 floats.
    let mut liar = HashEmbedder::new("BAAI/bge-small-en-v1.5", 384);
    liar.profile.variant_sha256 = index.variant_sha256.clone();
    liar.return_dims = Some(383);
    let e = liar
        .embed_query_for(&index, "hello", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);

    // The matching provider works and applies the query prefix.
    let mut ok = HashEmbedder::new("BAAI/bge-small-en-v1.5", 384);
    ok.profile.variant_sha256 = index.variant_sha256.clone();
    let v = ok
        .embed_query_for(&index, "hello", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(v.len(), 384);
    assert_eq!(ok.calls(), vec![vec!["hello".to_owned()]]);
}

#[test]
fn check_vectors_rejects_nan_and_count() {
    assert!(check_vectors(2, 1, &[vec![0.0, 1.0]]).is_ok());
    assert_eq!(
        check_vectors(2, 1, &[vec![f32::NAN, 1.0]])
            .unwrap_err()
            .code,
        ErrorCode::ModelIncompatible
    );
    assert_eq!(
        check_vectors(2, 2, &[vec![0.0, 1.0]]).unwrap_err().code,
        ErrorCode::ModelIncompatible
    );
    assert_eq!(model_incompatible("x").code, ErrorCode::ModelIncompatible);
}

#[tokio::test]
async fn embed_documents_splits_by_max_batch() {
    let mut p = HashEmbedder::new("m", 4);
    p.max_batch = 2;
    let t = ["a", "b", "c", "d", "e"];
    let v = p
        .embed_documents(&t, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(v.len(), 5);
    assert_eq!(
        p.calls().iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
}

/// Fails `fail_times` times with `code`, then succeeds.
struct Flaky {
    inner: HashEmbedder,
    left: AtomicU32,
    code: ErrorCode,
    retry_after: Option<u32>,
}

impl EmbeddingProvider for Flaky {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }
    fn max_batch(&self) -> usize {
        8
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let mut cur = self.left.load(Ordering::Acquire);
            let fail = loop {
                if cur == 0 {
                    break false;
                }
                match self
                    .left
                    .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => break true,
                    Err(now) => cur = now,
                }
            };
            if fail {
                let mut e = Error::new(self.code);
                if let Some(s) = self.retry_after {
                    e = e.with_retry_after(s);
                }
                return Err(e);
            }
            self.inner.embed_batch(kind, texts, cancel).await
        })
    }
}

fn flaky(n: u32, code: ErrorCode, retry_after: Option<u32>) -> Flaky {
    Flaky {
        inner: HashEmbedder::new("m", 4),
        left: AtomicU32::new(n),
        code,
        retry_after,
    }
}

#[tokio::test(start_paused = true)]
async fn resilient_retries_retryable_errors_honouring_retry_after() {
    let r = Resilient::new(flaky(2, ErrorCode::RateLimited, Some(7)));
    let t0 = tokio::time::Instant::now();
    let v = r
        .embed_batch(EmbedKind::Doc, &["a"], &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(
        t0.elapsed(),
        Duration::from_secs(14),
        "two Retry-After waits of 7 s"
    );
}

#[tokio::test(start_paused = true)]
async fn resilient_does_not_retry_non_retryable_or_beyond_limit() {
    let r = Resilient::new(flaky(1, ErrorCode::ProviderAuth, None));
    let e = r
        .embed_batch(EmbedKind::Doc, &["a"], &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ProviderAuth);
    assert_eq!(r.inner().left.load(Ordering::Relaxed), 0);

    let r = Resilient::new(flaky(10, ErrorCode::ProviderUnavailable, None));
    let e = r
        .embed_batch(EmbedKind::Doc, &["a"], &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ProviderUnavailable);
    assert_eq!(
        r.inner().left.load(Ordering::Relaxed),
        10 - 4,
        "1 try + 3 retries"
    );
}

#[tokio::test(start_paused = true)]
async fn resilient_wait_is_cancellable() {
    let r = Arc::new(Resilient::new(flaky(5, ErrorCode::RateLimited, Some(600))));
    let tok = CancellationToken::new();
    let (r2, t2) = (r.clone(), tok.clone());
    let h = tokio::spawn(async move { r2.embed_batch(EmbedKind::Doc, &["a"], &t2).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    tok.cancel();
    assert_eq!(h.await.unwrap().unwrap_err().code, ErrorCode::Cancelled);
}

#[test]
fn backoff_caps() {
    let b = Backoff::default();
    let e = Error::new(ErrorCode::Timeout);
    assert_eq!(b.delay(0, &e), Duration::from_millis(500));
    assert_eq!(b.delay(2, &e), Duration::from_secs(2));
    assert_eq!(b.delay(30, &e), Duration::from_secs(30));
    assert_eq!(
        b.delay(
            0,
            &Error::new(ErrorCode::RateLimited).with_retry_after(3600)
        ),
        Duration::from_secs(30)
    );
}

#[test]
fn provider_traits_are_object_safe() {
    fn _e(_: &dyn EmbeddingProvider) {}
    fn _l(_: &dyn LlmProvider) {}
    fn _r(_: &dyn Reranker) {}
    let p: Arc<dyn EmbeddingProvider> = Arc::new(HashEmbedder::new("m", 4));
    assert_eq!(p.dims(), 4);
    assert_eq!(p.model_id(), "m");
    assert_eq!(p.variant_sha256().len(), 64);
}
