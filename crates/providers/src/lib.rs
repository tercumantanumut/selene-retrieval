//! sr-providers: the **one** provider interface (docs/02-architecture.md §2.5).
//!
//! * [`EmbeddingProvider`] (object-safe), [`LlmProvider`] and [`Reranker`], all wrapped
//!   by [`Resilient`] and gated by [`PrivacyGate`]; keys live in a [`SecretStore`].
//! * [`registry`]: the provider list generated from Selene's registry (`cargo xtask
//!   sync-providers`), embedded; no provider or model string is hardcoded anywhere else.
//! * [`remote`] (feature `remote`, M3-P): genai chat + remote embeddings, live key validation and
//!   model discovery, the system proxy, consent-gated and audited egress ([`remote::Egress`]).
//! * [`secrets`]: Windows Credential Manager store (feature `keyring`), key redaction, purge.
//! * [`local::LocalEmbedder`] (feature `local`): the bundled / tier models on statically linked
//!   ONNX Runtime, CPU always, DirectML with a silent CPU fallback ladder (Amendments 2, 3.3, 3.7).
//! * [`models`]: sha256-pinned model files and the verifying loader (Amendment 3.4).
//! * [`batcher::EmbedBatcher`]: batching, bounded concurrency, backpressure, cancellation, dedupe.
//! * [`dml`]: verified, absolute-path `DirectML.dll` loading (WinVerifyTrust, delay-load).
//! * [`events`]: `EpChanged` / batch metrics for the engine's bus.
//! * [`dxgi`]: DXGI / D3D12 adapter enumeration (tier decision) and the fullscreen-app check.
//! * [`download`] (feature `download`): pinned, resumable, sha256-checked model downloads.
//! * [`sys`]: System32-only symbol resolution, free disk space.
//! * [`cli`] (feature `remote`, M5-C): "Claude (sign in)" / "Codex (sign in)" through the user's
//!   own Claude Code / Codex CLI and its login (no token here), with every tool switched off.

pub mod batcher;
#[cfg(feature = "remote")]
pub mod cli;
pub mod dml;
#[cfg(feature = "download")]
pub mod download;
pub mod dxgi;
pub mod embed;
pub mod events;
pub mod llm;
#[cfg(feature = "local")]
pub mod local;
pub mod models;
pub mod privacy;
pub mod registry;
#[cfg(feature = "remote")]
pub mod remote;
pub mod resilient;
pub mod secrets;
pub mod sys;
pub mod testing;

pub use batcher::{BatcherConfig, BatcherStats, EmbedBatcher};
pub use embed::{
    EmbedKind, EmbeddingProfile, EmbeddingProvider, IndexModel, check_vectors, ensure_matches,
};
pub use events::{EpReason, EventSink, MemorySink, NoopSink, ProviderEvent};
pub use llm::{
    Caps, ChatMessage, ChatRequest, ChatResponse, ChatStream, LlmProvider, ModelInfo, Reranker,
    Role,
};
#[cfg(feature = "local")]
pub use local::{LoadReport, LocalEmbedder, LocalOptions};
pub use models::{ModelPaths, VerifiedModel, load_verified};
pub use privacy::{
    EgressCtx, EgressRecord, LocalOnly, MemoryGate, PrivacyGate, SecretStore, consent_group,
};
pub use registry::{ProviderSpec, manifest};
#[cfg(feature = "remote")]
pub use remote::{KeyCheck, ProviderHandle, RemoteEmbedder, RemoteLlm, RemoteSettings};
pub use resilient::{Backoff, Resilient};
pub use secrets::{ApiKey, MemorySecretStore};
#[cfg(feature = "keyring")]
pub use secrets::{KeyringStore, purge_all};

#[cfg(test)]
mod tests;
