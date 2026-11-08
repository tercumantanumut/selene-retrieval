//! `Resilient<P>`: the one wrapper around every provider (§2.5).
//!
//! Bounded retries of *retryable* errors (`RateLimited`, `ProviderUnavailable`, `Timeout`,
//! `Offline`, …) with exponential backoff and **equal jitter** (half fixed, half random, so
//! parallel callers don't retry in lockstep). A provider's `Retry-After` is honoured exactly.
//! Cancellation is raced against every attempt *and* every wait: cancelling drops the in-flight
//! attempt, which aborts its HTTP request (M3-P). Streams are never retried after their first
//! item: only establishing a stream is retried. Per-provider pacing and 429 cool-downs live in
//! `remote::Limiter`; consent and audit in `remote::Egress` (each retry passes both again).

use std::time::Duration;

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio_util::sync::CancellationToken;

use crate::embed::{EmbedKind, EmbeddingProfile, EmbeddingProvider};
use crate::llm::{Caps, ChatRequest, ChatResponse, ChatStream, LlmProvider, ModelInfo, Reranker};

/// Retry policy. Defaults: 3 retries, 500 ms doubling, capped at 30 s, jittered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Equal jitter on computed (not `Retry-After`) delays.
    pub jitter: bool,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            jitter: true,
        }
    }
}

/// Cheap per-process pseudo-random `u64` (jitter only; not for anything secret).
fn jitter_rand() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

impl Backoff {
    /// Wait before retry `attempt` (0-based): `retry_after` if the provider sent one, otherwise
    /// `base · 2^attempt`, both capped at `max_delay`.
    pub fn delay(&self, attempt: u32, err: &Error) -> Duration {
        let d = err.retry_after.unwrap_or_else(|| {
            self.base_delay
                .saturating_mul(2u32.saturating_pow(attempt.min(16)))
        });
        d.min(self.max_delay)
    }

    /// The wait actually used: [`delay`](Self::delay), with equal jitter (`d/2 + rand(0..=d/2)`)
    /// unless the provider sent `Retry-After` or jitter is off.
    pub fn wait(&self, attempt: u32, err: &Error) -> Duration {
        let d = self.delay(attempt, err);
        if !self.jitter || err.retry_after.is_some() {
            return d;
        }
        let half = d / 2;
        let span = half.as_millis() as u64;
        let extra = if span == 0 {
            0
        } else {
            jitter_rand() % (span + 1)
        };
        half + Duration::from_millis(extra)
    }
}

#[derive(Debug)]
pub struct Resilient<P> {
    inner: P,
    pub policy: Backoff,
}

impl<P> Resilient<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            policy: Backoff::default(),
        }
    }
    pub fn with_policy(inner: P, policy: Backoff) -> Self {
        Self { inner, policy }
    }
    pub fn inner(&self) -> &P {
        &self.inner
    }

    /// Run `op` until it succeeds, fails non-retryably, runs out of retries or is cancelled.
    pub async fn run<'a, T>(
        &'a self,
        cancel: &CancellationToken,
        mut op: impl FnMut(&'a P) -> BoxFuture<'a, Result<T>>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            // Racing the attempt against `cancel` drops it on cancel: for remote providers that
            // aborts the HTTP request, so nothing keeps generating (and billing) after a cancel.
            let res = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)),
                r = op(&self.inner) => r,
            };
            match res {
                Ok(v) => return Ok(v),
                Err(e) if e.retryable && attempt < self.policy.max_retries => {
                    let wait = self.policy.wait(attempt, &e);
                    tracing::debug!(code = %e.code, attempt, ?wait, "provider call failed, retrying");
                    attempt += 1;
                    tokio::select! {
                        () = cancel.cancelled() => return Err(Error::new(ErrorCode::Cancelled)),
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<P: EmbeddingProvider> EmbeddingProvider for Resilient<P> {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }
    fn max_batch(&self) -> usize {
        self.inner.max_batch()
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(self.run(cancel, move |p| p.embed_batch(kind, texts, cancel)))
    }
}

impl<P: Reranker> Reranker for Resilient<P> {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        docs: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(self.run(cancel, move |p| p.rerank(query, docs, cancel)))
    }
}

impl<P: LlmProvider> LlmProvider for Resilient<P> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn capabilities(&self) -> Caps {
        self.inner.capabilities()
    }
    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>>> {
        Box::pin(self.run(cancel, move |p| p.list_models(cancel)))
    }
    fn chat<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatResponse>> {
        Box::pin(self.run(cancel, move |p| p.chat(req, cancel)))
    }
    fn chat_stream<'a>(
        &'a self,
        req: &'a ChatRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChatStream>> {
        // Only establishing the stream is retried; items are passed through untouched.
        Box::pin(self.run(cancel, move |p| p.chat_stream(req, cancel)))
    }
    fn validate_key<'a>(&'a self, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<()>> {
        // A rejected key is not retryable (ProviderAuth); transient network errors are.
        Box::pin(self.run(cancel, move |p| p.validate_key(cancel)))
    }
}
