//! The one door to the network for provider calls (§2.5 PrivacyGate, §2.6 "Silent egress").
//!
//! Every remote request (chat, stream, embeddings, rerank, key check, model list) is sent from
//! inside [`Egress::call`], in this order:
//!
//! 1. **Consent:** [`PrivacyGate::check`] for (provider id, purpose). Refused → `PrivacyBlocked`
//!    and the request future is never created, so not a single byte is sent.
//! 2. **Pacing:** the provider's [`Limiter`] (rate limit, 429 cool-down), cancellable.
//! 3. **Audit:** one `egress_audit` row per attempt that is actually sent.
//! 4. **Send** under the per-attempt timeout, raced against `cancel`. Cancelling or timing out
//!    **drops** the request future: reqwest/hyper close the connection (HTTP/1.1) or reset the
//!    stream (HTTP/2) at once, so the provider stops generating and billing. No request outlives
//!    its caller.
//!
//! Retries are decided by [`crate::Resilient`] around this, so every retry passes the gate again
//! (a consent revoked between two attempts stops the third) and is audited separately.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sr_common::Result;
use sr_common::error::{Error, ErrorCode, Purpose};
use tokio_util::sync::CancellationToken;

use super::limiter::Limiter;
use crate::privacy::{EgressCtx, EgressRecord, PrivacyGate};

#[derive(Clone)]
pub struct Egress {
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    gate: Arc<dyn PrivacyGate>,
    limiter: Arc<Limiter>,
    attempt_timeout: Duration,
}

impl std::fmt::Debug for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Egress")
            .field("provider", &self.provider_id)
            .field("attempt_timeout", &self.attempt_timeout)
            .finish_non_exhaustive()
    }
}

impl Egress {
    pub fn new(
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        gate: Arc<dyn PrivacyGate>,
        limiter: Arc<Limiter>,
        attempt_timeout: Duration,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            provider_name: provider_name.into(),
            gate,
            limiter,
            attempt_timeout,
        }
    }

    pub fn attempt_timeout(&self) -> Duration {
        self.attempt_timeout
    }

    /// Consent check only (also used to refuse before any local preparation work).
    pub fn check(&self, purpose: Purpose) -> Result<()> {
        self.gate
            .check(&self.provider_id, purpose)
            .map_err(|mut e| {
                // The gate keys on the id; the user reads the display name.
                if e.code == ErrorCode::PrivacyBlocked {
                    e.ctx.provider = Some(self.provider_name.clone());
                    e.ctx.purpose = Some(purpose);
                }
                e
            })
    }

    /// Runs one attempt of a provider request. `make` builds the request future; it is called
    /// only after consent, pacing and audit, and its future is dropped on cancel / timeout.
    pub async fn call<T, F, Fut>(
        &self,
        purpose: Purpose,
        bytes: u64,
        host: &str,
        ctx: &EgressCtx,
        cancel: &CancellationToken,
        make: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.call_with_timeout(
            purpose,
            bytes,
            host,
            ctx,
            cancel,
            self.attempt_timeout,
            make,
        )
        .await
    }

    /// [`call`](Self::call) with an explicit attempt timeout (stream establishment uses the same
    /// value; the stream body has its own idle timeout).
    #[allow(clippy::too_many_arguments)]
    pub async fn call_with_timeout<T, F, Fut>(
        &self,
        purpose: Purpose,
        bytes: u64,
        host: &str,
        ctx: &EgressCtx,
        cancel: &CancellationToken,
        timeout: Duration,
        make: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.check(purpose)?;
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        self.limiter.acquire(cancel).await?;
        // Consent may have been revoked while we waited for a slot.
        self.check(purpose)?;
        self.gate.audit_ctx(&EgressRecord {
            provider: self.provider_id.clone(),
            purpose,
            bytes,
            host: host.to_string(),
            ctx: ctx.clone(),
        });
        let fut = make();
        let res = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
            r = tokio::time::timeout(timeout, fut) => match r {
                Ok(r) => r,
                Err(_) => Err(Error::new(ErrorCode::Timeout)
                    .with_provider(self.provider_name.clone())
                    .with_detail(format!("no answer from {host} within {} s", timeout.as_secs()))),
            },
        };
        if let Err(e) = &res
            && e.code == ErrorCode::RateLimited
            && let Some(d) = e.retry_after
        {
            self.limiter.cool_down(d).await;
        }
        res
    }
}
