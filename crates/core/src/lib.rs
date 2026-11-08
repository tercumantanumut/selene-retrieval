//! sr-core: ingest (extract client, chunking, EmbedBatcher), index (Lance, tantivy, outbox apply,
//! replay, purge), query (dense, BM25, RRF, rerank, MMR, hydrate), answer, enhance, iterate, files.
//!
//! M1 (agent M1-E) implements:
//! * [`ingest::chunk`]: fixed 400/48-token chunking (Amendment 3.5) with tree-sitter for code,
//!   `chunk_cfg_fp`, `chunk_id` and `text_hash` per D2.
//! * [`index::lance`]: one Lance table per vector index (embedding profile), scalar + IVF_HNSW_SQ
//!   indexes, filtered flat scans for selective scopes, a same-commit watermark, purge compaction.
//! * [`index::lexical`]: one tantivy index per collection (body not stored, `CodeTokenizer`, boosted
//!   path field, commit payload = outbox seq).
//! * [`index::writer`]: the single IndexWriter applying outbox ops (D3) and replaying them
//!   idempotently from the lowest store watermark.
//! * [`query`]: hybrid dense + BM25 search fused by RRF, rerank hook, optional MMR, scope filters,
//!   `maxResults`/`minScore` semantics from docs/03 and §2.5, and a dense-only switch.
//!
//! Catalog and provider access goes through the narrow traits in [`index::source`] and
//! [`index::Embedder`] / [`query::Reranker`]; sr-engine adapts them to sr-catalog / sr-providers.
//! [`answer`], [`enhance`] and [`iterate`] (M3) use `sr_providers::LlmProvider` directly (the trait
//! crate only: the workspace entry has no default features, so no ORT, TLS or genai here).
//! Every LanceDB call runs under [`guard`] (`catch_unwind`, Amendment 3.12).

pub mod guard;
pub mod index;
pub mod ingest;
pub mod query;
/// In-memory fake catalog + deterministic embedders (tests, benchmarks, engine tests).
#[doc(hidden)]
pub mod testkit;

pub mod files {
    use sr_common::Result;
    use sr_common::error::Error;
    use sr_common::sandbox::ResolvedPath;

    /// `read_file` limit, matching Selene's read-utils.ts.
    pub const READ_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;

    /// Read a sandbox-resolved file (UTF-8, size-limited).
    pub fn read_file(path: &ResolvedPath) -> Result<String> {
        let _ = path;
        Err(Error::not_implemented("files::read_file"))
    }
}

/// Answers from the user's files: bounded, cancellable `read_file` loop, cited (M3-Q).
pub mod answer;
/// Prompt enhancement, ported from Selene's V2 enhancer (M3-Q).
pub mod enhance;
/// Deeper answers: the secondary LLM iteration pass, off by default (M3-Q).
pub mod iterate;
