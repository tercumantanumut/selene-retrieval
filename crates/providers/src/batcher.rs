//! `EmbedBatcher` (§2.3, §2.7): one per embedding profile, between chunking and the IndexWriter.
//!
//! * **Batching.** Requests are coalesced into provider calls of up to `batch_size` texts (the
//!   TierProfile batch for the active EP, VRAM-capped on the GPU, never more than the provider's
//!   current `max_batch()`), flushed when full or `max_wait` (50 ms) after the first text arrived.
//! * **Bounded concurrency.** At most `max_in_flight` provider calls run at once
//!   (`TierProfile::concurrency`: GPU 2, CPU 1).
//! * **Backpressure.** Every queued or in-flight text holds one of `queue_items` permits; `embed()`
//!   waits for permits before its texts enter the queue, so a producer can never run ahead of the
//!   model by more than `queue_items` texts (the claim loop's permits then stop claiming jobs).
//! * **Cancellation.** Each request carries its token: cancelled while queued → dropped before the
//!   provider sees it; cancelled while in flight → the caller returns `Cancelled` at once and the
//!   result is discarded. `shutdown()` fails everything still queued with `Cancelled`.
//! * **Dedupe.** Within one batch, texts with the same `TextHash` (blake3 of NFC text) are embedded
//!   once and fanned out. (Cross-batch reuse is the Lance `text_hash` lookup upstream, §2.3.)
//! * **Metrics.** One `ProviderEvent::EmbedBatch` per provider call.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::TextHash;
use sr_common::tier::{EpActive, Thresholds, TierProfile};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::embed::{EmbedKind, EmbeddingProvider, check_vectors};
use crate::events::{EventSink, ProviderEvent, clone_error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatcherConfig {
    /// Texts per provider call (upper bound; the provider's `max_batch()` may lower it).
    pub batch_size: usize,
    /// Provider calls running at once.
    pub max_in_flight: usize,
    /// Texts queued + in flight before `embed()` waits (backpressure). ≥ `batch_size`.
    pub queue_items: usize,
    /// Flush a partial batch this long after its first text arrived.
    pub max_wait: Duration,
}

impl BatcherConfig {
    /// From the active index's TierProfile and the EP the provider actually runs on.
    pub fn for_tier(
        tier: &TierProfile,
        ep: EpActive,
        dedicated_vram: Option<u64>,
        t: &Thresholds,
    ) -> Self {
        let batch = match (ep, dedicated_vram) {
            (EpActive::DirectMl, Some(v)) => tier.gpu_batch_for_vram(v, t),
            _ => tier.batch(ep),
        }
        .max(1) as usize;
        let in_flight = tier.concurrency(ep).max(1) as usize;
        Self {
            batch_size: batch,
            max_in_flight: in_flight,
            queue_items: batch * (in_flight + 2),
            max_wait: Duration::from_millis(50),
        }
    }

    fn normalised(mut self) -> Self {
        self.batch_size = self.batch_size.max(1);
        self.max_in_flight = self.max_in_flight.max(1);
        self.queue_items = self.queue_items.max(self.batch_size);
        self
    }
}

struct Piece {
    texts: Vec<String>,
    hashes: Vec<TextHash>,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<Vec<Vec<f32>>>>,
    /// Released when the piece is answered (queued + in-flight accounting).
    _permit: OwnedSemaphorePermit,
}

impl Piece {
    fn fail(self, e: Error) {
        let _ = self.reply.send(Err(e));
    }
}

fn cancelled() -> Error {
    Error::new(ErrorCode::Cancelled)
}

/// Point-in-time counters (diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatcherStats {
    pub batches: usize,
    pub texts: usize,
    pub embedded: usize,
    pub in_flight: usize,
    pub queued_texts: usize,
}

#[derive(Default)]
struct Counters {
    batches: AtomicUsize,
    texts: AtomicUsize,
    embedded: AtomicUsize,
    in_flight: AtomicUsize,
}

pub struct EmbedBatcher {
    tx: mpsc::Sender<Piece>,
    queue: Arc<Semaphore>,
    cfg: BatcherConfig,
    shutdown: CancellationToken,
    counters: Arc<Counters>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for EmbedBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedBatcher")
            .field("cfg", &self.cfg)
            .field("stats", &self.stats())
            .finish()
    }
}

impl EmbedBatcher {
    /// Start the dispatcher on the current tokio runtime.
    pub fn spawn(
        provider: Arc<dyn EmbeddingProvider>,
        cfg: BatcherConfig,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        let cfg = cfg.normalised();
        let (tx, rx) = mpsc::channel(cfg.queue_items);
        let shutdown = CancellationToken::new();
        let counters = Arc::new(Counters::default());
        let task = tokio::spawn(dispatch(
            provider,
            cfg,
            rx,
            sink,
            shutdown.clone(),
            counters.clone(),
        ));
        Self {
            tx,
            queue: Arc::new(Semaphore::new(cfg.queue_items)),
            cfg,
            shutdown,
            counters,
            task: Some(task),
        }
    }

    pub fn config(&self) -> BatcherConfig {
        self.cfg
    }

    pub fn stats(&self) -> BatcherStats {
        BatcherStats {
            batches: self.counters.batches.load(Ordering::Relaxed),
            texts: self.counters.texts.load(Ordering::Relaxed),
            embedded: self.counters.embedded.load(Ordering::Relaxed),
            in_flight: self.counters.in_flight.load(Ordering::Relaxed),
            queued_texts: self.cfg.queue_items - self.queue.available_permits(),
        }
    }

    /// Embed document texts; vectors come back in input order. Waits (backpressure) while the
    /// queue is full; returns `Cancelled` as soon as `cancel` fires.
    pub async fn embed(
        &self,
        texts: Vec<String>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut replies = Vec::new();
        let mut it = texts.into_iter().peekable();
        while it.peek().is_some() {
            let part: Vec<String> = it.by_ref().take(self.cfg.batch_size).collect();
            let n = part.len() as u32;
            let permit = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(cancelled()),
                () = self.shutdown.cancelled() => return Err(cancelled()),
                p = self.queue.clone().acquire_many_owned(n) => p.map_err(|_| cancelled())?,
            };
            let (reply, rx) = oneshot::channel();
            let hashes = part.iter().map(|t| TextHash::of_text(t)).collect();
            let piece = Piece {
                texts: part,
                hashes,
                cancel: cancel.clone(),
                reply,
                _permit: permit,
            };
            self.tx.send(piece).await.map_err(|_| cancelled())?;
            replies.push(rx);
        }
        let mut out = Vec::new();
        for rx in replies {
            let r = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(cancelled()),
                r = rx => r.map_err(|_| cancelled())?,
            };
            out.extend(r?);
        }
        Ok(out)
    }

    /// Stop: queued requests fail with `Cancelled`; in-flight calls finish, results dropped.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for EmbedBatcher {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn dispatch(
    provider: Arc<dyn EmbeddingProvider>,
    cfg: BatcherConfig,
    mut rx: mpsc::Receiver<Piece>,
    sink: Arc<dyn EventSink>,
    shutdown: CancellationToken,
    counters: Arc<Counters>,
) {
    let in_flight = Arc::new(Semaphore::new(cfg.max_in_flight));
    let mut carry: Option<Piece> = None;
    'outer: loop {
        let first = match carry.take() {
            Some(p) => p,
            None => tokio::select! {
                biased;
                () = shutdown.cancelled() => break 'outer,
                p = rx.recv() => match p { Some(p) => p, None => break 'outer },
            },
        };
        // The provider may have dropped to a smaller batch (GPU → CPU fallback).
        let limit = cfg.batch_size.min(provider.max_batch().max(1));
        let mut n = first.texts.len();
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + cfg.max_wait;
        while n < limit {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    for p in batch { p.fail(cancelled()); }
                    break 'outer;
                }
                p = rx.recv() => match p {
                    Some(p) if n + p.texts.len() > limit => { carry = Some(p); break; }
                    Some(p) => { n += p.texts.len(); batch.push(p); }
                    None => break,
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        let (live, dead): (Vec<_>, Vec<_>) =
            batch.into_iter().partition(|p| !p.cancel.is_cancelled());
        for p in dead {
            p.fail(cancelled());
        }
        if live.is_empty() {
            continue;
        }
        let permit = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                for p in live { p.fail(cancelled()); }
                break 'outer;
            }
            p = in_flight.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break 'outer },
        };
        // Requests may have been cancelled while we waited for a slot.
        let (live, dead): (Vec<_>, Vec<_>) =
            live.into_iter().partition(|p| !p.cancel.is_cancelled());
        for p in dead {
            p.fail(cancelled());
        }
        if live.is_empty() {
            continue;
        }
        tokio::spawn(run_batch(
            provider.clone(),
            live,
            permit,
            sink.clone(),
            counters.clone(),
        ));
    }
    if let Some(p) = carry {
        p.fail(cancelled());
    }
    rx.close();
    while let Some(p) = rx.recv().await {
        p.fail(cancelled());
    }
}

async fn run_batch(
    provider: Arc<dyn EmbeddingProvider>,
    pieces: Vec<Piece>,
    _permit: OwnedSemaphorePermit,
    sink: Arc<dyn EventSink>,
    counters: Arc<Counters>,
) {
    let t0 = Instant::now();
    let running = counters.in_flight.fetch_add(1, Ordering::AcqRel) + 1;

    // Dedupe by text_hash within the batch.
    let mut slot: HashMap<TextHash, usize> = HashMap::new();
    let mut unique: Vec<&str> = Vec::new();
    let mut index: Vec<Vec<usize>> = Vec::with_capacity(pieces.len());
    for p in &pieces {
        let idx = p
            .texts
            .iter()
            .zip(&p.hashes)
            .map(|(t, h)| {
                *slot.entry(*h).or_insert_with(|| {
                    unique.push(t.as_str());
                    unique.len() - 1
                })
            })
            .collect();
        index.push(idx);
    }
    let items: usize = pieces.iter().map(|p| p.texts.len()).sum();

    // Token for the provider: cancelled only if *every* request in the batch is.
    let all = CancellationToken::new();
    if pieces.iter().all(|p| p.cancel.is_cancelled()) {
        all.cancel();
    }
    let watch = {
        let tokens: Vec<CancellationToken> = pieces.iter().map(|p| p.cancel.clone()).collect();
        let all = all.clone();
        tokio::spawn(async move {
            for t in tokens {
                t.cancelled().await;
            }
            all.cancel();
        })
    };

    let mut result: Result<Vec<Vec<f32>>> = Ok(Vec::with_capacity(unique.len()));
    for part in unique.chunks(provider.max_batch().max(1)) {
        let r = provider
            .embed_batch(EmbedKind::Doc, part, &all)
            .await
            .and_then(|v| {
                check_vectors(provider.dims(), part.len(), &v)?;
                Ok(v)
            });
        match (r, &mut result) {
            (Ok(v), Ok(acc)) => acc.extend(v),
            (Err(e), _) => {
                result = Err(e);
                break;
            }
            (Ok(_), Err(_)) => break,
        }
    }
    watch.abort();
    let ok = result.is_ok();
    match result {
        Ok(vecs) => {
            for (p, idx) in pieces.into_iter().zip(index) {
                let v = idx
                    .iter()
                    .map(|&i| vecs.get(i).cloned().unwrap_or_default())
                    .collect();
                let _ = p.reply.send(Ok(v));
            }
        }
        Err(e) => {
            for p in pieces {
                p.fail(clone_error(&e));
            }
        }
    }
    counters.batches.fetch_add(1, Ordering::Relaxed);
    counters.texts.fetch_add(items, Ordering::Relaxed);
    counters.embedded.fetch_add(slot.len(), Ordering::Relaxed);
    counters.in_flight.fetch_sub(1, Ordering::AcqRel);
    sink.emit(ProviderEvent::EmbedBatch {
        model_id: provider.model_id().to_owned(),
        items: items as u32,
        unique: slot.len() as u32,
        duration_ms: t0.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
        in_flight: running as u32,
        ok,
    });
}

#[cfg(test)]
mod tests;
