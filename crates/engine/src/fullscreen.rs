//! R-UX32 (M3-Q; Amendment 6.4): GPU indexing behind a full-screen app is no longer a silent,
//! endless wait.
//!
//! R19's back-off stays: while `SHQueryUserNotificationState` says a full-screen app or a
//! presentation is in front, the local provider pauses its GPU batches. On top of it:
//!
//! * **Plain status.** The wait is tracked in [`FullscreenWait`]; while indexing is pending the
//!   status summary says "Waiting while a full-screen app is open" and `gpu.fullscreen_wait
//!   {waiting}` is pushed when the wait starts and ends (`EngineStatus.waiting_for_fullscreen`).
//! * **Continue on the processor after 5 minutes.** [`FullscreenFallback`] wraps the GPU embedder.
//!   When a document batch has waited [`CPU_AFTER`], the GPU call is cancelled (the provider gives
//!   up while paused, before any GPU work starts) and the batch runs on a CPU session of the
//!   **same model file**: fp32/fp16 files are EP-portable (cos ≥ 0.99998, Amendment 1), so the
//!   index and its embedding space are unchanged and nothing is re-embedded. While the app stays
//!   in front, later batches go straight to the CPU; once it closes, indexing returns to the GPU
//!   and the CPU session is dropped (memory). No setting is touched; nothing is shown as an
//!   error.
//! * Queries always embed on the CPU anyway (4.27) and never wait.
//!
//! Tests set `SR_NO_GPU_PAUSE` (Amendment 6.1: no pause at all) or the debug-only
//! `SR_FULLSCREEN_CPU_AFTER_MS` (shorter threshold); both are ignored in release builds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_providers::{EmbedKind, EmbeddingProfile, EmbeddingProvider};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bus::EventBus;

/// After this long behind a full-screen app, indexing continues on the processor.
pub const CPU_AFTER: Duration = Duration::from_secs(5 * 60);

/// How often a waiting batch re-checks the foreground.
const TICK: Duration = Duration::from_millis(500);

/// Builds the CPU session of the same model file (lazily, only when needed).
pub type CpuBuilder =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn EmbeddingProvider>>> + Send + Sync>;

/// "Is a full-screen app in front" (production: `sr_providers::dxgi::foreground_busy`).
pub type BusyProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The current wait behind a full-screen app (one per engine).
pub struct FullscreenWait {
    bus: Arc<EventBus>,
    since: Mutex<Option<Instant>>,
    /// Batches run on the processor because the wait passed [`CPU_AFTER`].
    on_cpu: AtomicBool,
    /// What was last pushed as `gpu.fullscreen_wait`.
    published: AtomicBool,
    cpu_after: Duration,
}

impl std::fmt::Debug for FullscreenWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FullscreenWait")
            .field("waiting", &self.waiting())
            .field("on_cpu", &self.on_cpu.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl FullscreenWait {
    pub fn new(bus: Arc<EventBus>) -> Arc<Self> {
        let cpu_after = sr_client::naming::dev_env("SR_FULLSCREEN_CPU_AFTER_MS")
            .and_then(|v| v.to_string_lossy().parse::<u64>().ok())
            .map_or(CPU_AFTER, Duration::from_millis);
        Self::with_threshold(bus, cpu_after)
    }

    pub fn with_threshold(bus: Arc<EventBus>, cpu_after: Duration) -> Arc<Self> {
        Arc::new(Self {
            bus,
            since: Mutex::new(None),
            on_cpu: AtomicBool::new(false),
            published: AtomicBool::new(false),
            cpu_after,
        })
    }

    /// Indexing is waiting for a full-screen app right now (not yet on the processor).
    pub fn waiting(&self) -> bool {
        self.published.load(Ordering::SeqCst)
    }

    pub fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::SeqCst)
    }

    fn publish(&self, waiting: bool) {
        if self.published.swap(waiting, Ordering::SeqCst) != waiting {
            if waiting {
                tracing::info!(target: "sr_engine::gpu", "waiting while a full-screen app is open");
            } else {
                tracing::info!(target: "sr_engine::gpu", on_cpu = self.on_cpu(), "no longer waiting for the full-screen app");
            }
            self.bus.publish(EngineEvent::GpuFullscreenWait { waiting });
        }
    }

    /// Record what the foreground check said (from the provider's pause hook or a waiting
    /// batch). Returns how long the current wait has lasted.
    pub fn observe(&self, busy: bool) -> Duration {
        let mut g = self
            .since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if busy {
            let since = *g.get_or_insert_with(Instant::now);
            drop(g);
            if !self.on_cpu() {
                self.publish(true);
            }
            since.elapsed()
        } else {
            let was = g.take().is_some();
            drop(g);
            if was || self.on_cpu() {
                self.on_cpu.store(false, Ordering::SeqCst);
                self.publish(false);
            }
            Duration::ZERO
        }
    }

    fn go_cpu(&self) {
        if !self.on_cpu.swap(true, Ordering::SeqCst) {
            tracing::info!(target: "sr_engine::gpu", minutes = self.cpu_after.as_secs() / 60, "a full-screen app stayed open: indexing continues on the processor");
        }
        self.publish(false);
    }

    /// The pause hook for `LocalOptions::gpu_pause`: the real check, recorded here.
    pub fn hook(self: &Arc<Self>, busy: BusyProbe) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let me = self.clone();
        Arc::new(move || {
            let b = busy();
            // On the processor the GPU is not used; never pause what the wrapper sends there.
            me.observe(b);
            b
        })
    }
}

/// The GPU document embedder with the 5-minute processor fallback (see the module docs).
pub struct FullscreenFallback {
    gpu: Arc<dyn EmbeddingProvider>,
    cpu: tokio::sync::Mutex<Option<Arc<dyn EmbeddingProvider>>>,
    build_cpu: CpuBuilder,
    busy: BusyProbe,
    wait: Arc<FullscreenWait>,
}

impl FullscreenFallback {
    pub fn new(
        gpu: Arc<dyn EmbeddingProvider>,
        build_cpu: CpuBuilder,
        busy: BusyProbe,
        wait: Arc<FullscreenWait>,
    ) -> Self {
        Self {
            gpu,
            cpu: tokio::sync::Mutex::new(None),
            build_cpu,
            busy,
            wait,
        }
    }

    async fn cpu_session(&self) -> Result<Arc<dyn EmbeddingProvider>> {
        let mut g = self.cpu.lock().await;
        if let Some(c) = g.as_ref() {
            return Ok(c.clone());
        }
        let c = (self.build_cpu)().await?;
        if c.profile().dims != self.gpu.profile().dims
            || c.profile().variant_sha256 != self.gpu.profile().variant_sha256
        {
            // Never mix embedding spaces (Amendment 1): same file or nothing.
            return Err(Error::new(ErrorCode::ModelIncompatible)
                .with_detail("processor session uses another model file"));
        }
        *g = Some(c.clone());
        Ok(c)
    }

    async fn drop_cpu_session(&self) {
        self.cpu.lock().await.take();
    }

    async fn on_cpu(
        &self,
        kind: EmbedKind,
        texts: &[&str],
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f32>>> {
        let c = self.cpu_session().await?;
        c.embed_batch(kind, texts, cancel).await
    }
}

impl EmbeddingProvider for FullscreenFallback {
    fn profile(&self) -> &EmbeddingProfile {
        self.gpu.profile()
    }

    fn max_batch(&self) -> usize {
        self.gpu.max_batch()
    }

    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            if kind == EmbedKind::Query {
                return self.gpu.embed_batch(kind, texts, cancel).await;
            }
            if self.wait.on_cpu() {
                if (self.busy)() {
                    return self.on_cpu(kind, texts, cancel).await;
                }
                // The app closed: back to the graphics card.
                self.wait.observe(false);
                self.drop_cpu_session().await;
            }
            let child = cancel.child_token();
            let mut call = self.gpu.embed_batch(kind, texts, &child);
            let mut switched = false;
            loop {
                tokio::select! {
                    r = &mut call => {
                        return match r {
                            Err(e) if e.code == ErrorCode::Cancelled && switched && !cancel.is_cancelled() => {
                                drop(call);
                                self.on_cpu(kind, texts, cancel).await
                            }
                            other => other,
                        };
                    }
                    () = tokio::time::sleep(TICK) => {
                        if switched {
                            continue;
                        }
                        let waited = self.wait.observe((self.busy)());
                        if waited >= self.wait.cpu_after {
                            self.wait.go_cpu();
                            switched = true;
                            child.cancel();
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use sr_providers::testing::HashEmbedder;
    use tokio::sync::Semaphore;

    use super::*;

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::default())
    }

    /// A "GPU" embedder that waits (like the provider's pause loop) until a permit is added,
    /// giving up with `Cancelled` when its token fires.
    fn paused_gpu() -> (Arc<HashEmbedder>, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let mut e = HashEmbedder::new("m", 8);
        e.gate = Some(gate.clone());
        (Arc::new(e), gate)
    }

    fn cpu_builder(calls: Arc<AtomicUsize>) -> CpuBuilder {
        Arc::new(move || {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(HashEmbedder::new("m", 8)) as Arc<dyn EmbeddingProvider>)
            })
        })
    }

    #[tokio::test]
    async fn after_the_threshold_the_batch_runs_on_the_processor_in_the_same_space() {
        let b = bus();
        let mut rx = b.subscribe();
        let wait = FullscreenWait::with_threshold(b.clone(), Duration::from_millis(900));
        let busy_flag = Arc::new(AtomicBool::new(true));
        let f = busy_flag.clone();
        let busy: BusyProbe = Arc::new(move || f.load(Ordering::SeqCst));
        let (gpu, gate) = paused_gpu();
        let builds = Arc::new(AtomicUsize::new(0));
        let w =
            FullscreenFallback::new(gpu.clone(), cpu_builder(builds.clone()), busy, wait.clone());
        let cancel = CancellationToken::new();
        let t0 = std::time::Instant::now();
        let v = w
            .embed_batch(EmbedKind::Doc, &["alpha", "beta"], &cancel)
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(900));
        assert_eq!(
            v,
            vec![
                HashEmbedder::vector("m", "alpha", 8),
                HashEmbedder::vector("m", "beta", 8)
            ]
        );
        assert!(wait.on_cpu() && !wait.waiting());
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        // Events: waiting true, then false (continued on the processor).
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::GpuFullscreenWait { waiting } = ev.payload {
                seen.push(waiting);
            }
        }
        assert_eq!(seen, vec![true, false]);
        // Still in front: the next batch goes straight to the processor (no GPU call waits).
        let t1 = std::time::Instant::now();
        w.embed_batch(EmbedKind::Doc, &["gamma"], &cancel)
            .await
            .unwrap();
        assert!(t1.elapsed() < Duration::from_millis(500));
        // The app closed: back to the GPU path (released here) and the CPU session is dropped.
        busy_flag.store(false, Ordering::SeqCst);
        gate.add_permits(10);
        w.embed_batch(EmbedKind::Doc, &["delta"], &cancel)
            .await
            .unwrap();
        assert!(!wait.on_cpu());
        assert!(w.cpu.lock().await.is_none());
        // Queries never wait.
        w.embed_batch(EmbedKind::Query, &["q"], &cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_short_wait_stays_on_the_gpu_and_clears_the_status() {
        let b = bus();
        let wait = FullscreenWait::with_threshold(b.clone(), Duration::from_secs(60));
        let busy_flag = Arc::new(AtomicBool::new(true));
        let f = busy_flag.clone();
        let busy: BusyProbe = Arc::new(move || f.load(Ordering::SeqCst));
        let (gpu, gate) = paused_gpu();
        let builds = Arc::new(AtomicUsize::new(0));
        let w = Arc::new(FullscreenFallback::new(
            gpu,
            cpu_builder(builds.clone()),
            busy,
            wait.clone(),
        ));
        let w2 = w.clone();
        let task = tokio::spawn(async move {
            let c = CancellationToken::new();
            w2.embed_batch(EmbedKind::Doc, &["x"], &c).await
        });
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(wait.waiting(), "the status says it waits");
        busy_flag.store(false, Ordering::SeqCst);
        gate.add_permits(1);
        assert!(task.await.unwrap().is_ok());
        wait.observe(false);
        assert!(!wait.waiting() && !wait.on_cpu());
        assert_eq!(
            builds.load(Ordering::SeqCst),
            0,
            "no processor session was needed"
        );
    }

    #[tokio::test]
    async fn shutdown_cancel_is_not_turned_into_a_processor_run() {
        let wait = FullscreenWait::with_threshold(bus(), Duration::from_secs(60));
        let busy: BusyProbe = Arc::new(|| true);
        let (gpu, _gate) = paused_gpu();
        let builds = Arc::new(AtomicUsize::new(0));
        let w = FullscreenFallback::new(gpu, cpu_builder(builds.clone()), busy, wait);
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let e = w.embed_batch(EmbedKind::Doc, &["x"], &c).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(builds.load(Ordering::SeqCst), 0);
    }
}
