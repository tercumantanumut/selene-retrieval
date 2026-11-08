//! Per-provider request pacing (§2.5 "a governor limit per provider").
//!
//! * Optional steady rate (`network.rate_limit_rpm`, Advanced): requests are spaced
//!   `60 s / rpm` apart (no burst beyond one).
//! * **Cool-down:** a `429` with `Retry-After` pauses *every* call to that provider until the time
//!   has passed, not just the call that hit it, so parallel embed batches don't hammer a limited
//!   account.
//! * Waiting is cancellable.
//!
//! A small hand-rolled limiter replaces the planned `governor` dependency (same behaviour for one
//! key; fewer dependencies).

use std::time::Duration;

use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct State {
    next_free: Instant,
    cool_until: Option<Instant>,
}

#[derive(Debug)]
pub struct Limiter {
    interval: Option<Duration>,
    state: Mutex<State>,
}

impl Limiter {
    /// `rpm = None` → no steady limit (cool-downs still apply).
    pub fn new(rpm: Option<u32>) -> Self {
        Self {
            interval: rpm
                .filter(|r| *r > 0)
                .map(|r| Duration::from_secs_f64(60.0 / f64::from(r))),
            state: Mutex::new(State {
                next_free: Instant::now(),
                cool_until: None,
            }),
        }
    }

    /// Waits for this call's slot. `Cancelled` if `cancel` fires while waiting.
    pub async fn acquire(&self, cancel: &CancellationToken) -> Result<()> {
        let wait_until = {
            let mut s = self.state.lock().await;
            let now = Instant::now();
            let mut at = now;
            if let Some(c) = s.cool_until {
                if c > now {
                    at = c;
                } else {
                    s.cool_until = None;
                }
            }
            if let Some(iv) = self.interval {
                at = at.max(s.next_free);
                s.next_free = at + iv;
            }
            at
        };
        if wait_until <= Instant::now() {
            return Ok(());
        }
        tokio::select! {
            () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
            () = tokio::time::sleep_until(wait_until) => Ok(()),
        }
    }

    /// Pause every caller for `d` (a provider's `Retry-After`).
    pub async fn cool_down(&self, d: Duration) {
        let mut s = self.state.lock().await;
        let until = Instant::now() + d;
        s.cool_until = Some(s.cool_until.map_or(until, |c| c.max(until)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn spacing_and_cool_down() {
        let l = Limiter::new(Some(60)); // one per second
        let c = CancellationToken::new();
        let t0 = Instant::now();
        assert!(l.acquire(&c).await.is_ok());
        assert!(l.acquire(&c).await.is_ok());
        assert!(Instant::now() - t0 >= Duration::from_secs(1));

        let free = Limiter::new(None);
        free.cool_down(Duration::from_secs(5)).await;
        let t1 = Instant::now();
        assert!(free.acquire(&c).await.is_ok());
        assert!(Instant::now() - t1 >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_is_cancellable() {
        let l = Limiter::new(None);
        l.cool_down(Duration::from_secs(600)).await;
        let c = CancellationToken::new();
        c.cancel();
        let e = l.acquire(&c).await.err().map(|e| e.code);
        assert_eq!(e, Some(ErrorCode::Cancelled));
    }
}
