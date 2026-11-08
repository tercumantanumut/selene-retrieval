//! Hybrid search (docs/02 §2.3 "Query", docs/03 search rows).
//!
//! ```text
//! search(q, scope) → per collection: query-embed with the collection's ACTIVE index embedder
//!   ┬─ Lance dense (filtered by collection/folders; flat scan when the scope is < 5 % of rows)
//!   └─ tantivy BM25 (skipped in dense-only mode)
//!   → RRF(k = 60) over every lane of every collection
//!   → hydrate from the catalog; drop hits not in their file's current version
//!   → [rerank top N, off unless the TierProfile/settings enable it] → minScore → [MMR] → maxResults
//! ```
//!
//! `maxResults`: default 50, capped at 150 (aliases `limit`/`topK` are the MCP layer's job).
//! `minScore` under RRF (§2.5): applies to the rerank score when reranking ran; otherwise to the
//! dense cosine similarity, and any hit in a BM25 lane's top 10 passes regardless (exact
//! identifiers keep working). Default 0.1, Selene's meaning.
//!
//! **No good match (R-UX18).** `minScore` 0.1 never filters bge-small (unrelated text scores
//! ≥ 0.3), so a query with no answer used to return unrelated hits. A query-level cut-off on the
//! fused list ([`NoMatchCutoff`]) returns **zero hits** with `no_good_match = true` when neither
//! lane has real evidence: the best dense cosine is below a calibrated floor **and** the lexical
//! lane has no strong hit. Thresholds were tuned on the eval harness (selene-src positives plus
//! negatives with no answer, `docs/research/m2-r-robustness.md`). Skipped when reranking ran.
//!
//! **Exact identifiers and numbers (R-UX33).** A query's identifier-like words, compound tokens and
//! numbers ([`QueryShape`]) also run as a third, **exact lane** over the tantivy `exact` field
//! (whole terms, no length norms), fused by RRF like the other lanes. Natural-language queries
//! have no exact terms, so their ranking is unchanged. For the "no good match" cut-off an exact hit
//! is evidence on its own (a ticket number in a long file is a match whatever its BM25), a
//! one-word query counts any keyword hit as evidence, and a digits-only query with no exact hit is
//! answered "nothing matched" whatever the dense score (number embeddings mean little). So is a
//! query that is exactly one identifier / compound (`Wan99VideoResult`, `v9.81.4`) found nowhere
//! as a whole term (M3 tester: near-miss identifiers otherwise matched on sub-tokens).
//!
//! **Identical text (docs/06 change 4).** Chunks with the same `text_hash` in different files
//! (vendored or copied files) collapse into one hit **before** `maxResults` is applied, so the
//! freed slots go to distinct passages. The representative is the best-ranked copy (a tie in
//! content scores goes to the shorter, then the lexicographically first path); every other copy
//! is listed in [`SearchHit::also_at`] with its own lines. Only candidates of this request are
//! collapsed, and those come from lanes filtered by the request's folders, so `also_at` never
//! names a path outside the caller's scope. On by default ([`SearchRequest::collapse_duplicates`]).
//!
//! **Answers (docs/08).** The search order is unchanged for every caller; an answer searches
//! wider than its passage budget and picks its passages file by file ([`passages`]).

pub mod fusion;
pub mod passages;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash};

pub use crate::index::lexical::QueryShape;
pub use fusion::{cosine, mmr, rrf_fuse};
pub use passages::{AnswerQuery, Passage, answer_query, passage_width, select_passages};

use crate::index::filter::Scope;
use crate::index::lance::DensePath;
use crate::index::source::{ChunkSource, HydratedChunk};
use crate::index::{CollectionStores, StoreRegistry};

/// Hybrid (default) or the dense-only A/B switch (docs/03 `v2/hybrid-search.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Hybrid,
    DenseOnly,
}

/// A search request after scope resolution (client scope ∩ request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub collections: Vec<CollectionId>,
    pub folders: Option<Vec<FolderId>>,
    /// Default [`QueryConfig::default_max_results`], capped at [`QueryConfig::max_results_cap`].
    pub max_results: Option<u32>,
    /// Default [`QueryConfig::default_min_score`]; must be within 0..=1.
    pub min_score: Option<f32>,
    /// `None` = [`QueryConfig::mode`].
    pub mode: Option<SearchMode>,
    /// Skip the "no good match" cut-off for this request: the keyword index is being rebuilt
    /// (M3-Q upgrade path), so missing keyword evidence must not empty the answer.
    #[serde(default)]
    pub relax_no_match: bool,
    /// Collapse chunks with identical text into one hit with [`SearchHit::also_at`] (docs/06
    /// change 4). On by default.
    #[serde(default = "yes")]
    pub collapse_duplicates: bool,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>, collections: Vec<CollectionId>) -> Self {
        Self {
            query: query.into(),
            collections,
            folders: None,
            max_results: None,
            min_score: None,
            mode: None,
            relax_no_match: false,
            collapse_duplicates: true,
        }
    }
}

/// Score breakdown (W17).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Scores {
    pub rrf: f32,
    /// Cosine similarity, when the hit came from a dense lane.
    pub dense: Option<f32>,
    pub bm25: Option<f32>,
    pub rerank: Option<f32>,
}

/// One hydrated hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    /// Root-relative path (from the catalog, so renames show immediately).
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub snippet: String,
    pub scores: Scores,
    pub text_hash: TextHash,
    /// Other places with exactly this text (collapsed duplicates; empty when there are none or
    /// collapsing is off).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_at: Vec<AlsoAt>,
}

/// Another copy of a hit's text (same `text_hash`), with its own lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlsoAt {
    pub folder_id: FolderId,
    /// Root-relative path.
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// Per-request timings and counters (diagnostics, benchmarks).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchStats {
    pub embed_ms: f64,
    pub dense_ms: f64,
    pub lexical_ms: f64,
    pub hydrate_ms: f64,
    pub total_ms: f64,
    pub candidates: usize,
    pub dropped_stale: usize,
    pub reranked: bool,
    pub dense_paths: Vec<DensePath>,
    /// Best dense cosine of any candidate (before hydration / `minScore`).
    pub top_dense: Option<f32>,
    /// Best BM25 score of any candidate.
    pub top_bm25: Option<f32>,
    /// The no-good-match cut-off fired (R-UX18): `hits` is empty on purpose.
    pub no_good_match: bool,
    /// Candidates from the exact lane (R-UX33), before hydration.
    #[serde(default)]
    pub exact_hits: usize,
    /// M5-E: the exact lane took part in the ranking (not only as evidence).
    #[serde(default)]
    pub exact_lane_ranked: bool,
    /// Candidate ids looked up in the catalog, and in how many batched windows (docs/06 issue 2).
    #[serde(default)]
    pub hydrate_lookups: usize,
    #[serde(default)]
    pub hydrate_windows: usize,
    /// The lookup cap ([`QueryConfig::hydrate_max_ids`]) stopped the widening: fewer hits than
    /// asked for may have come back although more current candidates existed.
    #[serde(default)]
    pub hydrate_capped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub stats: SearchStats,
}

/// Cross-encoder hook (adapted from `sr_providers::Reranker`). Object-safe.
pub trait Reranker: Send + Sync {
    fn rerank<'a>(&'a self, query: &'a str, docs: &'a [&'a str])
    -> BoxFuture<'a, Result<Vec<f32>>>;
}

/// Ranking knobs (`Advanced: ranking`, §2.5) plus the TierProfile rerank flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryConfig {
    pub rrf_k: u32,
    /// Candidates taken from each lane (raised to `2 × maxResults` when that is larger).
    pub lane_depth: usize,
    pub default_max_results: u32,
    pub max_results_cap: u32,
    pub default_min_score: f32,
    /// BM25 rank (1-based) up to which a lexical hit passes `minScore`.
    pub lexical_pass_rank: usize,
    /// Off unless the TierProfile (or Advanced) enables it.
    pub rerank_enabled: bool,
    pub rerank_top_n: usize,
    /// `Some(λ)` enables MMR.
    pub mmr_lambda: Option<f32>,
    pub mode: SearchMode,
    /// Query-level "no good match" cut-off (R-UX18).
    #[serde(default)]
    pub no_match: NoMatchMode,
    /// The exact lane for identifiers and numbers (R-UX33); on by default (hybrid mode only).
    #[serde(default = "yes")]
    pub exact_lane: bool,
    /// M5-E (R-UX37): the exact lane ranks only for short queries and sentences with a strong
    /// exact term ([`QueryShape::exact_lane_ranks`]); `false` = it ranks for every query (M3/M4).
    #[serde(default = "yes")]
    pub exact_lane_gate: bool,
    /// M5-E (R-UX36): how the cross-encoder's scores enter the ranking.
    #[serde(default)]
    pub rerank_fusion: RerankFusion,
    /// M5-E (R-UX36): leave one-word identifier / path / number queries to the fused order.
    #[serde(default = "yes")]
    pub rerank_skip_lookups: bool,
    /// Most candidate ids one search looks up in the catalog while skipping stale hits (docs/06
    /// issue 2). Normal searches stay far below it (≤ 2 × lane depth × lanes per collection).
    #[serde(default = "default_hydrate_max_ids")]
    pub hydrate_max_ids: usize,
}

fn yes() -> bool {
    true
}

/// Default [`QueryConfig::hydrate_max_ids`].
pub const HYDRATE_MAX_IDS: usize = 4096;

fn default_hydrate_max_ids() -> usize {
    HYDRATE_MAX_IDS
}

/// M5-E (R-UX36): how reranker scores are used.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum RerankFusion {
    /// M3/M4: the reranked head replaces the fused order; `minScore` applies to rerank scores.
    Replace,
    /// The reranker's order of the head is one more RRF list (weight `weight`) next to the fused
    /// score, so lexical and exact evidence still counts; `minScore` keeps its hybrid meaning.
    Rrf { weight: f32 },
}

impl Default for RerankFusion {
    fn default() -> Self {
        // Chosen with the eval harness (docs/research/m5-e-engine.md §5).
        RerankFusion::Rrf { weight: 1.0 }
    }
}

/// Where the "no good match" floors come from.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoMatchMode {
    /// The calibration of the searched collections' embedding model ([`crate::index::Embedder::no_match_cutoff`];
    /// none for an uncalibrated model, or when collections use different models).
    #[default]
    Model,
    Off,
    Fixed(NoMatchCutoff),
}

/// "No good match" (R-UX18): the result is empty when the best dense cosine is below
/// `min_top_dense` **and** the best BM25 score is below `min_top_bm25` (no lexical hit = 0).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NoMatchCutoff {
    pub min_top_dense: f32,
    pub min_top_bm25: f32,
}

impl NoMatchCutoff {
    /// Calibrated for the bundled bge-small (cosine) + tantivy BM25 on the eval harness.
    pub const BGE_SMALL: NoMatchCutoff = NoMatchCutoff {
        min_top_dense: NO_MATCH_MIN_TOP_DENSE,
        min_top_bm25: NO_MATCH_MIN_TOP_BM25,
    };

    /// `true` when neither lane has real evidence for the query.
    pub fn fires(&self, top_dense: Option<f32>, top_bm25: Option<f32>) -> bool {
        top_dense.unwrap_or(0.0) < self.min_top_dense && top_bm25.unwrap_or(0.0) < self.min_top_bm25
    }

    /// The BM25 floor for lexical indexes holding `docs` documents in total (M2 tester fix).
    ///
    /// BM25 scales with IDF ≈ ln(N / df): a term found in exactly one document scores about
    /// `ln(1 + (N - 0.5) / 1.5)`, which is ≈ 1.5 in a 6-file folder but ≈ 9 at 17 000 documents.
    /// The calibrated floor (20, two corpora of thousands of documents) is unreachable in a small
    /// personal folder, so an exact file name or identifier there was answered "Nothing closely
    /// matched". Below [`NO_MATCH_SMALL_COLLECTION_DOCS`] the floor becomes
    /// [`NO_MATCH_SMALL_BM25_FRACTION`] of that single-unique-term score (never above the
    /// calibrated one). A query with no lexical match still scores 0 and is still cut.
    pub fn bm25_floor(&self, docs: u64) -> f32 {
        if docs >= NO_MATCH_SMALL_COLLECTION_DOCS {
            return self.min_top_bm25;
        }
        #[allow(clippy::cast_precision_loss)]
        let n = docs.max(1) as f32;
        let unique_term = (1.0 + (n - 0.5) / 1.5).ln();
        (NO_MATCH_SMALL_BM25_FRACTION * unique_term).min(self.min_top_bm25)
    }

    /// [`Self::fires`] with the floor for the searched indexes' size ([`Self::bm25_floor`]).
    pub fn fires_for(&self, top_dense: Option<f32>, top_bm25: Option<f32>, docs: u64) -> bool {
        top_dense.unwrap_or(0.0) < self.min_top_dense
            && top_bm25.unwrap_or(0.0) < self.bm25_floor(docs)
    }
}

/// Below this many lexical documents (chunks) the BM25 floor scales with the index size
/// ([`NoMatchCutoff::bm25_floor`]); both calibration corpora are far above it.
pub const NO_MATCH_SMALL_COLLECTION_DOCS: u64 = 500;
/// Share of a single unique term's BM25 that counts as lexical evidence in a small index.
pub const NO_MATCH_SMALL_BM25_FRACTION: f32 = 0.6;

/// Calibrated floors (see [`NoMatchCutoff`]).
/// Calibrated 2026-10-18 (M2-R) on selene-src (180 positives, 20 negatives) and CQADupStack-
/// webmasters (506 positives, the same 20 negatives): positives cut 0 + 0, negatives emptied
/// 16/20 and 20/20. Lowest positive top-dense: 0.639 (BM25 37) and 0.680.
pub const NO_MATCH_MIN_TOP_DENSE: f32 = 0.65;
pub const NO_MATCH_MIN_TOP_BM25: f32 = 20.0;

impl NoMatchCutoff {
    /// The calibrated cut-off for an embedding model, if any. Floors are model-specific (cosine
    /// scales differ), so models without a calibration get none.
    pub fn for_model(model_id: &str) -> Option<NoMatchCutoff> {
        (model_id == "BAAI/bge-small-en-v1.5").then_some(NoMatchCutoff::BGE_SMALL)
    }
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            lane_depth: 100,
            default_max_results: 50,
            max_results_cap: 150,
            default_min_score: 0.1,
            lexical_pass_rank: 10,
            rerank_enabled: false,
            rerank_top_n: 50,
            mmr_lambda: None,
            mode: SearchMode::Hybrid,
            no_match: NoMatchMode::Model,
            exact_lane: true,
            exact_lane_gate: true,
            rerank_fusion: RerankFusion::default(),
            rerank_skip_lookups: true,
            hydrate_max_ids: HYDRATE_MAX_IDS,
        }
    }
}

/// The query engine over the shared [`StoreRegistry`].
pub struct Searcher {
    registry: Arc<StoreRegistry>,
    chunks: Arc<dyn ChunkSource>,
    reranker: Option<Arc<dyn Reranker>>,
    cfg: QueryConfig,
}

impl std::fmt::Debug for Searcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Searcher")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

fn invalid(detail: &str) -> Error {
    Error::new(ErrorCode::InvalidInput).with_detail(detail.to_string())
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[derive(Default)]
struct Signals {
    dense: HashMap<ChunkId, f32>,
    bm25: HashMap<ChunkId, (f32, usize)>,
    /// Chunks holding one of the query's exact terms (R-UX33).
    exact: HashSet<ChunkId>,
    /// Chunks holding one of the query's whole exact terms ([`QueryShape::evidence_terms`]).
    evidence: HashSet<ChunkId>,
}

impl Searcher {
    pub fn new(
        registry: Arc<StoreRegistry>,
        chunks: Arc<dyn ChunkSource>,
        reranker: Option<Arc<dyn Reranker>>,
        cfg: QueryConfig,
    ) -> Self {
        Self {
            registry,
            chunks,
            reranker,
            cfg,
        }
    }

    pub fn config(&self) -> &QueryConfig {
        &self.cfg
    }

    pub async fn search(&self, req: &SearchRequest) -> Result<SearchResponse> {
        let t0 = Instant::now();
        let mut stats = SearchStats::default();
        let query = req.query.trim();
        if query.is_empty() {
            return Err(invalid("query is empty"));
        }
        if req.collections.is_empty() {
            return Err(invalid("no collections in scope"));
        }
        let max_results = req
            .max_results
            .unwrap_or(self.cfg.default_max_results)
            .clamp(1, self.cfg.max_results_cap) as usize;
        let min_score = req.min_score.unwrap_or(self.cfg.default_min_score);
        if !min_score.is_finite() || !(0.0..=1.0).contains(&min_score) {
            return Err(invalid("minScore must be between 0 and 1"));
        }
        let mode = req.mode.unwrap_or(self.cfg.mode);
        let depth = self.cfg.lane_depth.max(max_results * 2);
        let shape = QueryShape::of(query);

        let mut uniq = HashSet::new();
        let stores: Vec<CollectionStores> = req
            .collections
            .iter()
            .filter(|c| uniq.insert(**c))
            .filter_map(|c| self.registry.get(*c))
            .collect();
        if stores.is_empty() {
            return Err(Error::new(ErrorCode::NotFound)
                .with_detail("no index for the requested collections"));
        }

        // Query-embed once per embedder (the active index's profile).
        let te = Instant::now();
        let mut qvecs: HashMap<String, Vec<f32>> = HashMap::new();
        for s in &stores {
            let fp = s.embedder.fingerprint().to_string();
            if let std::collections::hash_map::Entry::Vacant(e) = qvecs.entry(fp) {
                e.insert(s.embedder.embed_query(query).await?);
            }
        }
        stats.embed_ms = ms(te);

        // Lanes.
        let mut lists: Vec<Vec<ChunkId>> = Vec::new();
        let mut sig = Signals::default();
        let td = Instant::now();
        let dense_futs = stores.iter().map(|s| {
            let scope = Scope {
                collections: vec![s.collection_id],
                folders: req.folders.clone(),
            };
            let qv = qvecs
                .get(s.embedder.fingerprint())
                .cloned()
                .unwrap_or_default();
            let table = s.vectors.clone();
            async move { table.search(&qv, &scope, depth).await }
        });
        let dense = futures::future::join_all(dense_futs).await;
        stats.dense_ms = ms(td);
        for r in dense {
            let (hits, path) = r?;
            stats.dense_paths.push(path);
            let mut l = Vec::with_capacity(hits.len());
            for h in hits {
                sig.dense.entry(h.chunk_id).or_insert(h.similarity());
                l.push(h.chunk_id);
            }
            lists.push(l);
        }
        if mode == SearchMode::Hybrid {
            let tl = Instant::now();
            for s in &stores {
                let hits = s.lexical.search(query, req.folders.as_deref(), depth)?;
                let mut l = Vec::with_capacity(hits.len());
                for (rank, h) in hits.into_iter().enumerate() {
                    let e = sig.bm25.entry(h.chunk_id).or_insert((h.score, rank + 1));
                    if rank + 1 < e.1 {
                        *e = (h.score, rank + 1);
                    }
                    l.push(h.chunk_id);
                }
                lists.push(l);
            }
            // The exact lane (R-UX33): only for queries with identifier-like words or numbers.
            // M5-E (R-UX37): it always gathers evidence, but ranks only when the query's shape
            // says the exact terms are what the user is after.
            let shape_terms = &shape.exact_terms;
            let lane_ranks = !self.cfg.exact_lane_gate || shape.exact_lane_ranks();
            stats.exact_lane_ranked = self.cfg.exact_lane && lane_ranks && !shape_terms.is_empty();
            if self.cfg.exact_lane && !shape_terms.is_empty() {
                for s in &stores {
                    let hits =
                        s.lexical
                            .search_exact(shape_terms, req.folders.as_deref(), depth)?;
                    let mut l = Vec::with_capacity(hits.len());
                    for h in hits {
                        sig.exact.insert(h.chunk_id);
                        l.push(h.chunk_id);
                    }
                    if shape.evidence_terms == *shape_terms {
                        sig.evidence.extend(l.iter().copied());
                    }
                    if lane_ranks {
                        lists.push(l);
                    }
                    if shape.evidence_terms != *shape_terms && !shape.evidence_terms.is_empty() {
                        for h in s.lexical.search_exact(
                            &shape.evidence_terms,
                            req.folders.as_deref(),
                            depth,
                        )? {
                            sig.evidence.insert(h.chunk_id);
                        }
                    }
                }
                stats.exact_hits = sig.exact.len();
            }
            stats.lexical_ms = ms(tl);
        }
        stats.top_dense = sig.dense.values().copied().reduce(f32::max);
        stats.top_bm25 = sig.bm25.values().map(|x| x.0).reduce(f32::max);
        let refs: Vec<&[ChunkId]> = lists.iter().map(Vec::as_slice).collect();
        let fused = rrf_fuse(&refs, self.cfg.rrf_k);
        stats.candidates = fused.len();

        // Hydrate (validates current file version), widening the window if stale hits dropped.
        let th = Instant::now();
        let rerank_on = self.cfg.rerank_enabled
            && self.reranker.is_some()
            && !(self.cfg.rerank_skip_lookups && shape.is_lookup());
        let replace = matches!(self.cfg.rerank_fusion, RerankFusion::Replace);
        let want = if rerank_on {
            max_results.max(self.cfg.rerank_top_n)
        } else {
            max_results
        };
        let mut hydrated: Vec<(ChunkId, f32, HydratedChunk)> = Vec::new();
        // With collapsing on, distinct texts count: copies must not leave fewer than
        // `max_results` hits while more distinct passages exist.
        let collapse = req.collapse_duplicates;
        let mut distinct: HashSet<TextHash> = HashSet::new();
        let mut start = 0usize;
        let mut window = (want * 2).max(20);
        // docs/06 issue 2: during a big re-index most candidates can be stale; the widening
        // stops after `hydrate_max_ids` lookups and the search returns what it has.
        let cap = self.cfg.hydrate_max_ids.max(1);
        while start < fused.len()
            && (if collapse {
                distinct.len()
            } else {
                hydrated.len()
            }) < want * 2
        {
            if stats.hydrate_lookups >= cap {
                stats.hydrate_capped = true;
                break;
            }
            let end = (start + window)
                .min(fused.len())
                .min(start + cap - stats.hydrate_lookups);
            stats.hydrate_lookups += end - start;
            stats.hydrate_windows += 1;
            let ids: Vec<ChunkId> = fused[start..end].iter().map(|x| x.0).collect();
            let mut got = self.chunks.hydrate(&ids)?;
            let wanted: HashSet<CollectionId> = stores.iter().map(|s| s.collection_id).collect();
            for (id, rrf) in &fused[start..end] {
                match got.remove(id) {
                    Some(h) if wanted.contains(&h.collection_id) => {
                        distinct.insert(h.text_hash);
                        hydrated.push((*id, *rrf, h));
                    }
                    _ => stats.dropped_stale += 1,
                }
            }
            start = end;
            window *= 2;
        }
        stats.hydrate_ms = ms(th);
        // Evidence for the "no good match" cut-off comes from hits that are still current: a
        // removed folder's chunks stay in the stores until the purge (≤ 15 min) and must not
        // vouch for unrelated live hits (M2 tester). `stats.top_*` keep the raw maxima.
        let live_dense = hydrated
            .iter()
            .filter_map(|(id, _, _)| sig.dense.get(id).copied())
            .reduce(f32::max);
        let live_bm25 = hydrated
            .iter()
            .filter_map(|(id, _, _)| sig.bm25.get(id).map(|x| x.0))
            .reduce(f32::max);
        let live_exact = hydrated.iter().any(|(id, _, _)| sig.evidence.contains(id));

        // Rerank (degrades to RRF order on failure).
        let mut rerank: HashMap<ChunkId, f32> = HashMap::new();
        if rerank_on && let Some(rr) = &self.reranker {
            let n = self.cfg.rerank_top_n.min(hydrated.len());
            let docs: Vec<&str> = hydrated[..n].iter().map(|h| h.2.text.as_str()).collect();
            match rr.rerank(query, &docs).await {
                Ok(scores) if scores.len() == n => {
                    for (h, s) in hydrated[..n].iter().zip(scores) {
                        rerank.insert(h.0, s);
                    }
                    let mut head: Vec<_> = hydrated.drain(..n).collect();
                    let by_rerank =
                        |a: &(ChunkId, f32, HydratedChunk), b: &(ChunkId, f32, HydratedChunk)| {
                            let sa = rerank.get(&a.0).copied().unwrap_or(f32::MIN);
                            let sb = rerank.get(&b.0).copied().unwrap_or(f32::MIN);
                            sb.total_cmp(&sa)
                        };
                    match self.cfg.rerank_fusion {
                        RerankFusion::Replace => head.sort_by(by_rerank),
                        RerankFusion::Rrf { weight } => {
                            // The reranker's order is one more RRF list over the head.
                            let mut order: Vec<ChunkId> = head.iter().map(|h| h.0).collect();
                            order.sort_by(|a, b| {
                                let sa = rerank.get(a).copied().unwrap_or(f32::MIN);
                                let sb = rerank.get(b).copied().unwrap_or(f32::MIN);
                                sb.total_cmp(&sa)
                            });
                            let k = self.cfg.rrf_k as f32;
                            let bonus: HashMap<ChunkId, f32> = order
                                .iter()
                                .enumerate()
                                .map(|(i, id)| (*id, weight.max(0.0) / (k + i as f32 + 1.0)))
                                .collect();
                            for h in &mut head {
                                h.1 += bonus.get(&h.0).copied().unwrap_or(0.0);
                            }
                            head.sort_by(|a, b| b.1.total_cmp(&a.1));
                        }
                    }
                    head.extend(hydrated);
                    hydrated = head;
                    stats.reranked = true;
                }
                Ok(_) => {
                    tracing::warn!(target: "sr_core::query", "reranker returned wrong count; skipped")
                }
                Err(e) => {
                    tracing::warn!(target: "sr_core::query", code = %e.code, "reranker failed; skipped")
                }
            }
        }

        // minScore.
        let pass = |id: &ChunkId| -> bool {
            // A whole exact identifier / number always passes (a cross-encoder scores `76666`
            // or `0x887A0005` low although it is exactly what was asked for, R-UX33).
            if sig.evidence.contains(id) {
                return true;
            }
            if stats.reranked && replace {
                return rerank.get(id).is_some_and(|s| *s >= min_score);
            }
            if sig.exact.contains(id)
                || sig
                    .bm25
                    .get(id)
                    .is_some_and(|(_, r)| *r <= self.cfg.lexical_pass_rank)
            {
                return true;
            }
            sig.dense.get(id).is_some_and(|s| *s >= min_score)
        };
        let before_min_score = hydrated.len();
        hydrated.retain(|h| pass(&h.0));
        // Reranked and nothing reached minScore: the reranker found no real match (R-UX18).
        let rerank_empty =
            stats.reranked && replace && before_min_score > 0 && hydrated.is_empty() && !live_exact;

        // MMR.
        if let Some(lambda) = self.cfg.mmr_lambda
            && hydrated.len() > 1
        {
            let mut vecs: HashMap<ChunkId, Vec<f32>> = HashMap::new();
            for s in &stores {
                let ids: Vec<ChunkId> = hydrated
                    .iter()
                    .filter(|h| h.2.collection_id == s.collection_id)
                    .map(|h| h.0)
                    .collect();
                if !ids.is_empty() {
                    vecs.extend(s.vectors.vectors_by_chunk_id(&ids).await?);
                }
            }
            let cands: Vec<(usize, f32)> = hydrated
                .iter()
                .enumerate()
                .map(|(i, h)| (i, rerank.get(&h.0).copied().unwrap_or(h.1)))
                .collect();
            let vrefs: Vec<Option<&[f32]>> = hydrated
                .iter()
                .map(|h| vecs.get(&h.0).map(Vec::as_slice))
                .collect();
            let order = mmr(&cands, &vrefs, lambda, max_results);
            let mut slots: Vec<Option<(ChunkId, f32, HydratedChunk)>> =
                hydrated.into_iter().map(Some).collect();
            hydrated = order
                .into_iter()
                .filter_map(|i| slots.get_mut(i).and_then(Option::take))
                .collect();
        }

        // No good match (R-UX18): an empty answer beats unrelated hits. Rerank scores are on
        // another scale, so the cut-off only applies to the fused list.
        let cutoff = match self.cfg.no_match {
            NoMatchMode::Off => None,
            NoMatchMode::Fixed(c) => Some(c),
            NoMatchMode::Model => {
                let mut it = stores.iter().map(|s| s.embedder.no_match_cutoff());
                let first = it.next().flatten();
                if it.all(|c| c == first) { first } else { None }
            }
        };
        // Lexical documents behind the BM25 scores (the floor scales below 500, see bm25_floor).
        let lexical_docs: u64 = stores.iter().map(|s| s.lexical.num_docs()).sum();
        // R-UX33: an exact identifier / number hit, or any keyword hit for a one-word query, is
        // a match on its own; a number nobody wrote down is not, whatever the dense score.
        let lexical_evidence = mode == SearchMode::Hybrid
            && (live_exact || (shape.single_plain_word && live_bm25.is_some()));
        // Model-independent, so it also applies to uncalibrated models; `Off` disables it.
        // The same holds for a query that is one identifier / compound as typed (M3 tester): its
        // sub-tokens and a similar embedding (`Wan99VideoResult` vs `Wan22VideoResult`) are not
        // the thing the user asked for.
        let unmatched_number = mode == SearchMode::Hybrid
            && self.cfg.exact_lane
            && self.cfg.no_match != NoMatchMode::Off
            && (shape.digits_only || shape.single_exact_term)
            // A one-digit query has no exact terms: nothing to hold it against.
            && !shape.exact_terms.is_empty()
            && !live_exact;
        // Fused reranking keeps the hybrid scores, so the floors still apply (M5-E).
        let floors_fire = !(stats.reranked && replace)
            && !lexical_evidence
            && cutoff.is_some_and(|c| c.fires_for(live_dense, live_bm25, lexical_docs));
        let cut = !req.relax_no_match && (unmatched_number || rerank_empty || floors_fire);
        if cut {
            stats.no_good_match = true;
            hydrated.clear();
        }
        // R-UX33 (M3 tester): a query that is exactly one identifier / number asks for that term,
        // so chunks holding it whole come first. Stable: the fused / reranked / MMR order is kept
        // inside each group. With bge-small, short notes sharing only sub-tokens
        // (`savetoknew19q…`) otherwise outranked the one file with `savetokedit19q76666` (5th).
        if shape.single_exact_term || shape.digits_only {
            hydrated.sort_by_key(|(id, _, _)| !sig.evidence.contains(id));
        }
        // docs/06 change 4: identical text becomes one hit, before the cut to `max_results`.
        let mut also: HashMap<ChunkId, Vec<AlsoAt>> = HashMap::new();
        if collapse {
            hydrated = collapse_duplicates(hydrated, &sig, req.folders.as_deref(), &mut also);
        }
        hydrated.truncate(max_results);
        let hits = hydrated
            .into_iter()
            .map(|(id, rrf, h)| SearchHit {
                also_at: also.remove(&id).unwrap_or_default(),
                text_hash: h.text_hash,
                chunk_id: id,
                file_id: h.file_id,
                folder_id: h.folder_id,
                collection_id: h.collection_id,
                path: h.rel_path,
                line_start: h.line_start,
                line_end: h.line_end,
                snippet: h.text,
                scores: Scores {
                    rrf,
                    dense: sig.dense.get(&id).copied(),
                    bm25: sig.bm25.get(&id).map(|x| x.0),
                    rerank: rerank.get(&id).copied(),
                },
            })
            .collect();
        stats.total_ms = ms(t0);
        Ok(SearchResponse { hits, stats })
    }
}

/// Collapses hits with the same `text_hash` into the slot of the best-ranked copy (docs/06
/// change 4). Copies with the same content scores as the best-ranked one (equal dense cosine and
/// BM25: only the lane order between equal scores differs) are tied, and the shorter path, then
/// the lexicographically first, is shown (`patches/x` beats `spikes/a/patches/x`). The other
/// copies go to `also` under the shown chunk's id, sorted by path. `folders` is the request's
/// scope: a copy outside it is dropped (the lanes never return one; this pins it).
fn collapse_duplicates(
    hydrated: Vec<(ChunkId, f32, HydratedChunk)>,
    sig: &Signals,
    folders: Option<&[FolderId]>,
    also: &mut HashMap<ChunkId, Vec<AlsoAt>>,
) -> Vec<(ChunkId, f32, HydratedChunk)> {
    let mut groups: HashMap<TextHash, Vec<usize>> = HashMap::new();
    for (i, h) in hydrated.iter().enumerate() {
        groups.entry(h.2.text_hash).or_default().push(i);
    }
    if groups.len() == hydrated.len() {
        return hydrated;
    }
    let content = |id: &ChunkId| {
        (
            sig.dense.get(id).map(|x| x.to_bits()),
            sig.bm25.get(id).map(|x| x.0.to_bits()),
        )
    };
    let in_scope = |f: FolderId| folders.is_none_or(|s| s.contains(&f));
    let mut slots: Vec<Option<(ChunkId, f32, HydratedChunk)>> =
        hydrated.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(groups.len());
    for i in 0..slots.len() {
        let Some(hash) = slots[i].as_ref().map(|h| h.2.text_hash) else {
            continue;
        };
        let members = groups.remove(&hash).unwrap_or_default();
        let mut copies: Vec<(ChunkId, f32, HydratedChunk)> =
            members.iter().filter_map(|m| slots[*m].take()).collect();
        if copies.len() <= 1 {
            out.extend(copies);
            continue;
        }
        // `members` is in rank order, so `copies[0]` is the best-ranked copy.
        let (best_content, best_score) = (content(&copies[0].0), copies[0].1);
        let pick = copies
            .iter()
            .enumerate()
            .filter(|(_, c)| c.1 == best_score || content(&c.0) == best_content)
            .min_by(|(_, a), (_, b)| {
                (a.2.rel_path.len(), &a.2.rel_path).cmp(&(b.2.rel_path.len(), &b.2.rel_path))
            })
            .map_or(0, |(k, _)| k);
        let shown = copies.swap_remove(pick);
        let mut rest: Vec<AlsoAt> = copies
            .into_iter()
            .filter(|c| in_scope(c.2.folder_id))
            .map(|c| AlsoAt {
                folder_id: c.2.folder_id,
                path: c.2.rel_path,
                line_start: c.2.line_start,
                line_end: c.2.line_end,
            })
            .collect();
        rest.sort_by(|a, b| {
            (&a.path, a.line_start, a.folder_id).cmp(&(&b.path, b.line_start, b.folder_id))
        });
        if !rest.is_empty() {
            also.insert(shown.0, rest);
        }
        out.push(shown);
    }
    out
}
