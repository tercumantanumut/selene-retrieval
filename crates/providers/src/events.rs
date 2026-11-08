//! Provider-side events. The engine forwards these onto the one bus (`sr_common::events`):
//! `EpChanged` → `ep.changed` (diagnostics only, never a user-visible error, §2.5), and the
//! batcher metrics → diagnostics / `IndexProgress` throughput.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sr_common::error::Error;
use sr_common::tier::EpActive;

/// Why a session runs on the EP it runs on. Wire names are stable (`ep.changed.reason_code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpReason {
    /// The plan asked for this EP and it passed every check.
    Planned,
    /// The plan was CPU (no usable GPU, WARP, no FL 11.0).
    CpuPlan,
    /// "Use graphics card: Off" (Advanced).
    DisabledByUser,
    /// The model file is int8: CPU-pinned (§2.8).
    CpuPinnedVariant,
    /// Not on Windows: DirectML does not exist.
    Unsupported,
    /// No `DirectML.dll` in the app dir or System32.
    DllMissing,
    /// `DirectML.dll` failed WinVerifyTrust / is not Microsoft-signed, or a different copy was
    /// already mapped.
    DllUntrusted,
    /// `LoadLibraryExW` of the verified file failed.
    DllLoadFailed,
    /// DirectML EP registration (`error_on_failure`) or session commit failed.
    RegistrationFailed,
    /// Canary produced NaN/inf.
    CanaryNonFinite,
    /// Canary cosine vs CPU < 0.999.
    CanaryMismatch,
    /// iGPU / small dGPU probe: DirectML not ≥ `probe_min_speedup`× the CPU.
    ProbeTooSlow,
    /// A DirectML run failed mid-indexing (device removed / TDR); rebuilt on CPU and re-ran.
    RuntimeFault,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// `from: None` on the first session of a provider.
    EpChanged {
        model_id: String,
        from: Option<EpActive>,
        to: EpActive,
        reason_code: EpReason,
        /// Internal detail (diagnostics only).
        detail: Option<String>,
    },
    /// One provider call made by the [`EmbedBatcher`](crate::batcher::EmbedBatcher).
    EmbedBatch {
        model_id: String,
        /// Texts requested in this batch.
        items: u32,
        /// Distinct texts actually embedded (dedupe by `text_hash`).
        unique: u32,
        duration_ms: u32,
        /// Batches running concurrently when this one finished (incl. itself).
        in_flight: u32,
        ok: bool,
    },
}

/// Where providers push events. Implementations must not block.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: ProviderEvent);
}

/// Drops everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _ev: ProviderEvent) {}
}

/// Collects events in memory (tests, diagnostics snapshots).
#[derive(Debug, Default)]
pub struct MemorySink(Mutex<Vec<ProviderEvent>>);

impl MemorySink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn take(&self) -> Vec<ProviderEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
    pub fn snapshot(&self) -> Vec<ProviderEvent> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl EventSink for MemorySink {
    fn emit(&self, ev: ProviderEvent) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(ev);
    }
}

/// [`EventSink`] backed by an unbounded tokio channel (engine side drains it onto the bus).
impl EventSink for tokio::sync::mpsc::UnboundedSender<ProviderEvent> {
    fn emit(&self, ev: ProviderEvent) {
        let _ = self.send(ev);
    }
}

/// Copy of an error for fan-out to several waiters (the source chain is folded into `detail`).
pub(crate) fn clone_error(e: &Error) -> Error {
    let mut ctx = (*e.ctx).clone();
    let chain = e.chain();
    if !chain.is_empty() {
        let joined = chain.join(": ");
        ctx.detail = Some(match ctx.detail.take() {
            Some(d) => format!("{d}: {joined}"),
            None => joined,
        });
    }
    let mut c = Error::new(e.code).with_ctx(ctx);
    c.retryable = e.retryable;
    c.retry_after = e.retry_after;
    c.trace_id = e.trace_id;
    c
}

#[cfg(feature = "local")]
pub(crate) fn internal(detail: impl Into<String>) -> Error {
    Error::new(sr_common::error::ErrorCode::Internal).with_detail(detail)
}
