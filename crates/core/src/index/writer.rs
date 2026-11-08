//! The single IndexWriter (D3, docs/02 §2.3, Amendment 4.25).
//!
//! ```text
//! T1 (catalog): files.version++, chunks, chunk_text, index_ops(seq)        ← done by ingest
//! IndexWriter, one *bounded batch* at a time (ascending seq):
//!   select a prefix of the pending ops, closing at the first of
//!       2 s wall time | 512 newly embedded chunks | 1 000 outbox ops | stop requested
//!     (embedding streams window by window while the prefix grows: each window's texts are
//!      looked up in Lance by text_hash first, only new text is embedded)
//!   per collection of the prefix:
//!   Lance:   merge_insert(chunk_id) + delete stale rows of the touched files + watermark → 1 commit
//!   tantivy: delete_term(file_id) + add docs → commit(payload = seq)       → 1 commit
//!   T2: acknowledge {watermarks, indexed files}; catalog prunes covered ops
//! startup: the same batches, from min(store watermarks), run by the engine's background writer
//!          after the pipe is open (Amendment 4.24); each store skips ops ≤ its watermark
//! ```
//!
//! **At-least-once + idempotent.** Applying an op always makes a store hold exactly the file's
//! current chunks, so re-applying (after a crash between the Lance and tantivy commits, or before
//! the acknowledgement) converges to the same state. Duplicate files never cross-delete because
//! chunk ids include `file_id` (D2) and deletes are by `file_id`. Renames are catalog-only.
//!
//! **Bounded batches (4.25).** A batch is always a *prefix* of the ops in seq order, so the
//! watermark (`max seq` of the prefix) stays exact. The cut happens at a window boundary (one
//! embedder call's worth of texts, `WriterConfig::embed_batch`), never inside a file, so a single
//! file larger than the budget forms its own batch. A stop request abandons the window being
//! embedded (nothing of it is committed; it stays in the outbox) and returns at once.
//!
//! **Stash rows (R30).** Inside such a big window the texts are embedded in parts of at most
//! `batch_chunks`; each completed part but the last is written to Lance with the *unchanged*
//! watermark, so a stopped run's work is reused by `text_hash` on the next run instead of being
//! re-embedded (see `select_prefix`).
//!
//! **Handles (G2).** Upkeep only keeps stores that are still registered: entries of tables no
//! longer registered for their collection (index swap, collection closed) are dropped at the
//! start of every batch and upkeep, or at once via [`IndexWriter::release_unregistered`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::StoreKind;
use sr_common::ids::{CollectionId, FileId, FolderId, TextHash};
use tokio_util::sync::CancellationToken;

use super::lance::{VectorBatch, VectorRow};
use super::lexical::{LexBatch, LexDoc};
use super::source::{Ack, ChunkSource, FileSnapshot, OpKind, OutboxOp, OutboxSource};
use super::{CollectionStores, StoreRegistry, Watermark};

/// Writer knobs (docs/02 §2.7, Amendment 4.25: a batch closes at 2 s, 512 embedded chunks or
/// 1 000 outbox ops, whichever comes first; the engine owns the timer between batches).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WriterConfig {
    /// Outbox ops per batch (upper bound).
    pub batch_ops: usize,
    /// Newly embedded chunks per batch (checked at window boundaries).
    pub batch_chunks: usize,
    /// Wall time per batch in ms (checked at window boundaries; the first window always runs).
    pub batch_ms: u64,
    /// Texts per embedding window (one streamed embedder call; the engine sets it from the
    /// EmbedBatcher: provider batch × calls in flight). Also the Lance lookup granularity.
    pub embed_batch: usize,
    /// Run index upkeep (scalar/ANN/optimize) when a backlog is done (and periodically during a
    /// long one).
    pub maintain: bool,
    /// During a long backlog, run upkeep at most this often (ms).
    pub maintain_every_ms: u64,
    /// Compact small Lance fragments after this many committed batches (0 = never). Bounded
    /// batches make one small fragment per commit; compaction keeps queries fast.
    pub compact_every_batches: u32,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            batch_ops: 1000,
            batch_chunks: 512,
            batch_ms: 2000,
            embed_batch: 32,
            maintain: true,
            maintain_every_ms: 60_000,
            compact_every_batches: 64,
        }
    }
}

/// What one batch (or a whole drain) did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainReport {
    pub ops_read: usize,
    pub batches: usize,
    pub files_upserted: usize,
    pub files_removed: usize,
    pub folders_purged: usize,
    pub chunks_embedded: usize,
    pub chunks_reused: usize,
    /// Rows written as R30 stash (completed embedding parts of a big window, watermark
    /// unchanged; reused by `text_hash` when the window is embedded again).
    #[serde(default)]
    pub chunks_stashed: usize,
    /// Highest seq applied (the prefix end); the start cursor when nothing was applied.
    pub last_seq: i64,
    /// Ops may remain after this batch / drain (cut by the budget, or stopped).
    pub more: bool,
    /// A stop request ended the batch / drain early.
    pub stopped: bool,
    /// Longest batch: wall time (ms) and newly embedded chunks (tests, diagnostics).
    pub max_batch_ms: u64,
    pub max_batch_embedded: usize,
}

impl DrainReport {
    /// Add one batch's report to a running total (`more` / `stopped` describe the latest).
    pub fn absorb(&mut self, b: &DrainReport) {
        self.ops_read += b.ops_read;
        self.batches += b.batches;
        self.files_upserted += b.files_upserted;
        self.files_removed += b.files_removed;
        self.folders_purged += b.folders_purged;
        self.chunks_embedded += b.chunks_embedded;
        self.chunks_reused += b.chunks_reused;
        self.chunks_stashed += b.chunks_stashed;
        if b.batches > 0 || self.batches == 0 {
            self.last_seq = self.last_seq.max(b.last_seq);
        }
        self.more = b.more;
        self.stopped |= b.stopped;
        self.max_batch_ms = self.max_batch_ms.max(b.max_batch_ms);
        self.max_batch_embedded = self.max_batch_embedded.max(b.max_batch_embedded);
    }

    /// Something reached the stores.
    pub fn wrote(&self) -> bool {
        self.files_upserted + self.files_removed + self.folders_purged > 0
    }
}

/// Test-only fault injection: fail after the N-th Lance commit, before tantivy (a "crash" between
/// the two store commits).
#[derive(Debug, Default)]
pub struct FaultPlan {
    /// 0 = off; otherwise the countdown of Lance commits until the injected failure.
    pub fail_after_lance_commits: AtomicU32,
}

/// Upkeep and cursor state carried between batches (guarded by the writer lock).
#[derive(Default)]
struct State {
    /// Tables written since the last upkeep / compaction.
    maintain_pending: BTreeMap<String, CollectionStores>,
    compact_pending: BTreeMap<String, CollectionStores>,
    last_maintain: Option<Instant>,
    batches_since_compact: u32,
    /// `(store watermarks after the last batch, prefix end)`: if the stores still hold exactly
    /// these watermarks, every op up to the prefix end was already handled (ops of collections
    /// without open stores are skipped), so the next batch resumes after it instead of
    /// re-reading them forever.
    resume: Option<(Vec<(String, i64)>, i64)>,
}

/// The single writer. Cheap to share; batches are serialised internally.
pub struct IndexWriter {
    outbox: Arc<dyn OutboxSource>,
    chunks: Arc<dyn ChunkSource>,
    registry: Arc<StoreRegistry>,
    cfg: WriterConfig,
    lock: tokio::sync::Mutex<State>,
    faults: Arc<FaultPlan>,
}

impl std::fmt::Debug for IndexWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexWriter")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Plan {
    files: BTreeSet<FileId>,
    purge: BTreeSet<FolderId>,
}

fn plan(ops: &[&OutboxOp]) -> Plan {
    let mut p = Plan::default();
    for op in ops {
        match op.kind {
            OpKind::Upsert | OpKind::Delete => {
                p.files.insert(op.file_id);
            }
            OpKind::PurgeFolder(f) => {
                p.purge.insert(f);
            }
        }
    }
    p
}

/// Work done while selecting a batch, reused by the apply step.
#[derive(Default)]
struct Prepared {
    /// Catalog snapshot per touched file (`None`: gone).
    snaps: HashMap<FileId, Option<FileSnapshot>>,
    /// Lance table → text_hash → vector (looked up or embedded in this batch).
    vectors: HashMap<String, HashMap<TextHash, Vec<f32>>>,
    /// Lance table → hashes whose vector was copied from the table (not embedded).
    reused: HashMap<String, HashSet<TextHash>>,
    /// Chunk texts fetched for embedding (the lexical apply reuses them).
    texts: HashMap<TextHash, String>,
}

fn internal(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal).with_detail(detail)
}

fn wm_key(lance: &HashMap<String, i64>, lex: &HashMap<CollectionId, i64>) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = lance
        .iter()
        .map(|(k, s)| (format!("l:{k}"), *s))
        .chain(lex.iter().map(|(k, s)| (format!("t:{k}"), *s)))
        .collect();
    v.sort();
    v
}

/// Files a window may hold even when none of them needs embedding (lookups stay batched).
const WINDOW_MAX_FILES: usize = 64;

impl IndexWriter {
    pub fn new(
        outbox: Arc<dyn OutboxSource>,
        chunks: Arc<dyn ChunkSource>,
        registry: Arc<StoreRegistry>,
        cfg: WriterConfig,
    ) -> Self {
        Self {
            outbox,
            chunks,
            registry,
            cfg,
            lock: tokio::sync::Mutex::new(State::default()),
            faults: Arc::new(FaultPlan::default()),
        }
    }

    pub fn config(&self) -> &WriterConfig {
        &self.cfg
    }

    /// Fault injection handle (tests).
    pub fn faults(&self) -> Arc<FaultPlan> {
        self.faults.clone()
    }

    /// Current watermarks, read from the stores themselves.
    pub async fn watermarks(&self) -> Result<Vec<Watermark>> {
        let mut out = Vec::new();
        for s in self.registry.all() {
            out.push(Watermark {
                store: StoreKind::Lance,
                scope_id: s.vectors.name().to_string(),
                collection_id: s.collection_id,
                seq: s.vectors.watermark().await?,
            });
            out.push(Watermark {
                store: StoreKind::Tantivy,
                scope_id: s.collection_id.to_string(),
                collection_id: s.collection_id,
                seq: s.lexical.watermark()?,
            });
        }
        Ok(out)
    }

    /// Lowest store watermark (the replay start); `None` without open stores.
    pub async fn min_watermark(&self) -> Result<Option<i64>> {
        Ok(self.watermarks().await?.iter().map(|w| w.seq).min())
    }

    /// Startup replay as one call: identical to [`Self::drain`]. The engine instead replays in
    /// the background with [`Self::drain_batch`] after the pipe is open (Amendment 4.24).
    pub async fn replay(&self) -> Result<DrainReport> {
        self.drain().await
    }

    /// Apply every pending outbox op (bounded batches until the outbox is empty).
    pub async fn drain(&self) -> Result<DrainReport> {
        self.drain_until(&CancellationToken::new()).await
    }

    /// Bounded batches until the outbox is empty or `stop` fires (then at the next batch
    /// boundary; the window being embedded is abandoned and stays in the outbox).
    pub async fn drain_until(&self, stop: &CancellationToken) -> Result<DrainReport> {
        let mut total = DrainReport::default();
        loop {
            let b = self.drain_batch(stop).await?;
            total.absorb(&b);
            if !b.more || b.stopped || b.batches == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Wait until no batch is running (a stopped drain's batch has finished or been abandoned).
    pub async fn wait_batch_boundary(&self) {
        drop(self.lock.lock().await);
    }

    /// Drop the upkeep handles of tables that are no longer registered (G2). The engine calls
    /// this right after an index swap so the retired table's `CollectionStores` (Lance table,
    /// tantivy index, embedder) is not kept alive by the writer until its next upkeep, and the
    /// retired table can be dropped. Waits for a running batch to finish.
    pub async fn release_unregistered(&self) {
        let mut st = self.lock.lock().await;
        self.prune_pending(&mut st);
    }

    /// Tables waiting for upkeep (maintain or compaction), sorted (tests).
    #[doc(hidden)]
    pub async fn pending_tables(&self) -> Vec<String> {
        let st = self.lock.lock().await;
        let mut v: Vec<String> = st
            .maintain_pending
            .keys()
            .chain(st.compact_pending.keys())
            .cloned()
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Keep only pending entries whose table is still the registered table of its collection
    /// (refreshed to the registered handles, so a re-registered embedder/tantivy handle of the
    /// same table does not keep the old one alive either). With `maintain == false` nothing is
    /// kept for maintenance at all; compaction keeps its entries while the table is registered.
    fn prune_pending(&self, st: &mut State) {
        fn prune(reg: &StoreRegistry, m: &mut BTreeMap<String, CollectionStores>) -> usize {
            let before = m.len();
            m.retain(|table, s| match reg.get(s.collection_id) {
                Some(cur) if cur.vectors.name() == table.as_str() => {
                    *s = cur;
                    true
                }
                _ => false,
            });
            before - m.len()
        }
        let mut dropped = prune(&self.registry, &mut st.compact_pending);
        if self.cfg.maintain {
            dropped += prune(&self.registry, &mut st.maintain_pending);
        } else {
            dropped += st.maintain_pending.len();
            st.maintain_pending.clear();
        }
        if dropped > 0 {
            tracing::debug!(
                target: "sr_core::writer",
                dropped,
                "released upkeep handles of unregistered tables"
            );
        }
    }

    /// Select and commit **one** bounded batch (Amendment 4.25). Returns `batches == 0` when
    /// nothing was pending or `stop` fired before anything could be committed.
    pub async fn drain_batch(&self, stop: &CancellationToken) -> Result<DrainReport> {
        let mut st = self.lock.lock().await;
        self.prune_pending(&mut st);
        let mut rep = DrainReport::default();
        if stop.is_cancelled() {
            rep.stopped = true;
            rep.more = true;
            return Ok(rep);
        }
        let stores = self.registry.all();
        if stores.is_empty() {
            return Ok(rep);
        }
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_millis(self.cfg.batch_ms.max(1));
        let by_coll: HashMap<CollectionId, CollectionStores> = stores
            .iter()
            .map(|s| (s.collection_id, s.clone()))
            .collect();
        let mut lance_wm: HashMap<String, i64> = HashMap::new();
        let mut lex_wm: HashMap<CollectionId, i64> = HashMap::new();
        for s in &stores {
            lance_wm.insert(s.vectors.name().to_string(), s.vectors.watermark().await?);
            lex_wm.insert(s.collection_id, s.lexical.watermark()?);
        }
        let min = lance_wm
            .values()
            .chain(lex_wm.values())
            .copied()
            .min()
            .unwrap_or(0);
        let key = wm_key(&lance_wm, &lex_wm);
        let cursor = match &st.resume {
            Some((k, at)) if *k == key => (*at).max(min),
            _ => min,
        };
        let ops = self.outbox.ops_after(cursor, self.cfg.batch_ops.max(1))?;
        let Some(last) = ops.last() else {
            rep.last_seq = cursor;
            self.upkeep(&mut st, false, stop).await?;
            return Ok(rep);
        };
        if last.seq <= cursor {
            return Err(internal("outbox returned ops at or below the cursor"));
        }

        let mut prep = Prepared::default();
        let n = self
            .select_prefix(
                &ops, &by_coll, &lance_wm, stop, deadline, &mut prep, &mut rep,
            )
            .await?;
        if n == 0 {
            // Stopped before the first window finished: nothing committed.
            rep.stopped = true;
            rep.more = true;
            rep.last_seq = cursor;
            return Ok(rep);
        }
        let prefix = &ops[..n];
        let last_seq = prefix[n - 1].seq;
        rep.ops_read = n;
        rep.batches = 1;
        let wrote_lance = self
            .apply_ops(
                prefix,
                &by_coll,
                &mut lance_wm,
                &mut lex_wm,
                &mut rep,
                &mut st,
                &mut prep,
            )
            .await?;
        rep.last_seq = last_seq;
        st.resume = Some((wm_key(&lance_wm, &lex_wm), last_seq));
        rep.more = n < ops.len() || !self.outbox.ops_after(last_seq, 1)?.is_empty();
        rep.stopped = stop.is_cancelled() && rep.more;
        rep.max_batch_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        rep.max_batch_embedded = rep.chunks_embedded;
        if wrote_lance {
            st.batches_since_compact += 1;
        }
        self.upkeep(&mut st, rep.more, stop).await?;
        Ok(rep)
    }

    /// Index upkeep between batches: maintain when a backlog is done (or periodically during a
    /// long one); compact small fragments every `compact_every_batches` commits. Skipped once a
    /// stop was requested (shutdown must not wait for an ANN build).
    async fn upkeep(&self, st: &mut State, more: bool, stop: &CancellationToken) -> Result<()> {
        self.prune_pending(st);
        if stop.is_cancelled() {
            return Ok(());
        }
        let every = self.cfg.compact_every_batches;
        if every > 0 && st.batches_since_compact >= every {
            for s in std::mem::take(&mut st.compact_pending).values() {
                s.vectors.compact_small_fragments().await?;
            }
            st.batches_since_compact = 0;
        }
        if !self.cfg.maintain || st.maintain_pending.is_empty() {
            return Ok(());
        }
        let due = st.last_maintain.is_none_or(|t| {
            t.elapsed() >= Duration::from_millis(self.cfg.maintain_every_ms.max(1))
        });
        if !more || due {
            for s in std::mem::take(&mut st.maintain_pending).values() {
                s.vectors.maintain().await?;
            }
            st.last_maintain = Some(Instant::now());
        }
        Ok(())
    }

    /// Grow the batch window by window until a budget closes it; embed each window's new text
    /// (streamed). Returns how many ops of `ops` form the batch.
    ///
    /// **Stash rows (R30).** A window always holds at least one op, so a single big file forms
    /// one window (and its own batch). When a window needs more than `batch_chunks` new texts
    /// for one table, they are embedded in parts of at most `batch_chunks` (in the order of the
    /// files' chunks), and every completed part but the last is *stashed*: written to the table
    /// with the table's **current** watermark (`VectorTable::apply` is a merge_insert keyed by
    /// chunk id that rewrites the sentinel with the same seq), so the watermark does not move and
    /// nothing is acknowledged. If a stop abandons the window, the next run finds the stashed
    /// vectors by `text_hash` and reuses them instead of re-embedding the file from zero.
    ///
    /// Stash rows are real rows of the current snapshot (chunk/file/folder/collection ids), so
    /// they never outlive their file: the file's op stays in the outbox until applied, and that
    /// apply replaces the file's rows (`replace_files` deletes rows not in the batch, e.g. the
    /// old chunk ids after an edit); a delete or folder purge removes them like any other row.
    /// Meanwhile the query path's hydrate drops chunks that are not part of the file's current
    /// version, so dense search can only ever show current chunks. A window with a single part
    /// writes no stash (no extra Lance commit on the normal path).
    #[allow(clippy::too_many_arguments)]
    async fn select_prefix(
        &self,
        ops: &[OutboxOp],
        by_coll: &HashMap<CollectionId, CollectionStores>,
        lance_wm: &HashMap<String, i64>,
        stop: &CancellationToken,
        deadline: Instant,
        prep: &mut Prepared,
        rep: &mut DrainReport,
    ) -> Result<usize> {
        /// Texts one table needs in this window (chunk order, deduplicated) and their files.
        #[derive(Default)]
        struct Need {
            order: Vec<TextHash>,
            seen: HashSet<TextHash>,
            files: Vec<FileId>,
            seen_files: HashSet<FileId>,
        }
        let window = self.cfg.embed_batch.max(1);
        let batch_chunks = self.cfg.batch_chunks.max(1);
        let mut taken = 0usize;
        let mut embedded = 0usize;
        while taken < ops.len() {
            if taken > 0
                && (embedded >= batch_chunks || Instant::now() >= deadline || stop.is_cancelled())
            {
                break;
            }
            // Newly embedded texts of the windows already part of the batch.
            let embedded_before = embedded;
            // 1. The window: ops until `window` texts need a vector (at least one op).
            let mut need: BTreeMap<String, Need> = BTreeMap::new();
            let mut n_need = 0usize;
            let mut n_files = 0usize;
            let mut end = taken;
            while end < ops.len()
                && (end == taken || (n_need < window && n_files < WINDOW_MAX_FILES))
            {
                let op = &ops[end];
                end += 1;
                if !matches!(op.kind, OpKind::Upsert | OpKind::Delete) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = prep.snaps.entry(op.file_id) {
                    e.insert(self.chunks.file_snapshot(op.file_id)?);
                    n_files += 1;
                }
                let Some(stores) = by_coll.get(&op.collection_id) else {
                    continue;
                };
                let table = stores.vectors.name();
                if op.seq <= lance_wm.get(table).copied().unwrap_or(0) {
                    continue; // Lance already holds it; only tantivy lags
                }
                let Some(Some(s)) = prep.snaps.get(&op.file_id) else {
                    continue;
                };
                if s.collection_id != op.collection_id {
                    continue;
                }
                let known = prep.vectors.get(table);
                let v = need.entry(table.to_string()).or_default();
                if v.seen_files.insert(s.file_id) {
                    v.files.push(s.file_id);
                }
                for c in &s.chunks {
                    if known.is_none_or(|k| !k.contains_key(&c.text_hash))
                        && v.seen.insert(c.text_hash)
                    {
                        v.order.push(c.text_hash);
                        n_need += 1;
                    }
                }
            }
            // 2. Copy existing vectors from Lance (dedupe across files, batches and stashes).
            // 3. Embed only new text, streamed through the embedder; a stop abandons the window.
            for (table, need_t) in need {
                let Need { order, files, .. } = need_t;
                if order.is_empty() {
                    continue;
                }
                let Some(stores) = by_coll.values().find(|s| s.vectors.name() == table) else {
                    continue;
                };
                let found = stores.vectors.vectors_by_text_hash(&order).await?;
                prep.reused
                    .entry(table.clone())
                    .or_default()
                    .extend(found.keys().copied());
                let vecs = prep.vectors.entry(table.clone()).or_default();
                vecs.extend(found);
                // Chunk order is kept (the parts below follow the file).
                let missing: Vec<TextHash> = order
                    .into_iter()
                    .filter(|h| !vecs.contains_key(h))
                    .collect();
                if missing.is_empty() {
                    continue;
                }
                let dims = stores.embedder.dims();
                if dims != stores.vectors.dims() {
                    return Err(
                        Error::new(ErrorCode::ModelIncompatible).with_detail(format!(
                            "embedder has {dims} dims, index {} has {}",
                            stores.vectors.name(),
                            stores.vectors.dims()
                        )),
                    );
                }
                let need_text: Vec<TextHash> = missing
                    .iter()
                    .copied()
                    .filter(|h| !prep.texts.contains_key(h))
                    .collect();
                if !need_text.is_empty() {
                    prep.texts.extend(self.chunks.texts(&need_text)?);
                }
                let have: Vec<TextHash> = missing
                    .into_iter()
                    .filter(|h| prep.texts.contains_key(h))
                    .collect();
                if have.is_empty() {
                    continue;
                }
                // R30: more than `batch_chunks` new texts → parts (one part = one call, as before).
                let parts: Vec<&[TextHash]> = have.chunks(batch_chunks).collect();
                let n_parts = parts.len();
                for (i, part) in parts.into_iter().enumerate() {
                    if i > 0 && stop.is_cancelled() {
                        // Stop between parts: abandon the window; the stashed parts stay.
                        tracing::info!(
                            target: "sr_core::writer",
                            table = %table,
                            parts_done = i,
                            parts = n_parts,
                            "stop between embedding parts: window abandoned, stashed parts kept"
                        );
                        rep.chunks_embedded += embedded_before;
                        return Ok(taken);
                    }
                    let batch: Vec<&str> = part
                        .iter()
                        .filter_map(|h| prep.texts.get(h).map(String::as_str))
                        .collect();
                    let out = match stores.embedder.embed_documents_until(&batch, stop).await {
                        Ok(v) => v,
                        Err(e) if e.code == ErrorCode::Cancelled && stop.is_cancelled() => {
                            // Abandon this window: the batch is what was taken before it.
                            rep.chunks_embedded += embedded_before;
                            return Ok(taken);
                        }
                        Err(e) => return Err(e),
                    };
                    if out.len() != part.len() || out.iter().any(|v| v.len() != dims) {
                        return Err(Error::new(ErrorCode::ModelIncompatible)
                            .with_detail("embedder returned the wrong number or size of vectors"));
                    }
                    embedded += part.len();
                    let vecs = prep.vectors.entry(table.clone()).or_default();
                    for (h, v) in part.iter().zip(out) {
                        vecs.insert(*h, v);
                    }
                    if n_parts >= 2 && i + 1 < n_parts {
                        // Same condition as `need`: only tables whose watermark is below the op.
                        let seq = lance_wm.get(&table).copied().unwrap_or(0);
                        let n = self.stash(stores, &files, part, prep, seq).await?;
                        rep.chunks_stashed += n;
                        tracing::debug!(
                            target: "sr_core::writer",
                            table = %table,
                            part = i + 1,
                            parts = n_parts,
                            rows = n,
                            seq,
                            "stashed an embedded part (watermark unchanged)"
                        );
                    }
                }
            }
            taken = end;
        }
        rep.chunks_embedded += embedded;
        Ok(taken)
    }

    /// Write the rows of one completed embedding part (R30) without moving the watermark: `seq`
    /// is the table's current watermark and nothing is deleted (no `replace_files` /
    /// `purge_folders`), so the table's committed state only gains rows of current-snapshot
    /// chunks, which the file's pending op later keeps or replaces. Returns the rows written.
    async fn stash(
        &self,
        stores: &CollectionStores,
        files: &[FileId],
        part: &[TextHash],
        prep: &Prepared,
        seq: i64,
    ) -> Result<usize> {
        let part: HashSet<&TextHash> = part.iter().collect();
        let Some(vecs) = prep.vectors.get(stores.vectors.name()) else {
            return Ok(0);
        };
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for f in files {
            let Some(Some(s)) = prep.snaps.get(f) else {
                continue;
            };
            if s.collection_id != stores.collection_id {
                continue;
            }
            for c in &s.chunks {
                if !part.contains(&c.text_hash) || !seen.insert(c.chunk_id) {
                    continue;
                }
                let Some(v) = vecs.get(&c.text_hash) else {
                    continue;
                };
                rows.push(VectorRow {
                    chunk_id: c.chunk_id,
                    text_hash: c.text_hash,
                    file_id: s.file_id,
                    folder_id: s.folder_id,
                    collection_id: s.collection_id,
                    vector: v.clone(),
                });
            }
        }
        let n = rows.len();
        if n == 0 {
            return Ok(0);
        }
        stores
            .vectors
            .apply(&VectorBatch {
                rows,
                replace_files: vec![],
                purge_folders: vec![],
                seq,
            })
            .await?;
        Ok(n)
    }

    /// Commit the batch: per collection, Lance → tantivy → T2. Returns whether Lance was written.
    #[allow(clippy::too_many_arguments)]
    async fn apply_ops(
        &self,
        ops: &[OutboxOp],
        by_coll: &HashMap<CollectionId, CollectionStores>,
        lance_wm: &mut HashMap<String, i64>,
        lex_wm: &mut HashMap<CollectionId, i64>,
        rep: &mut DrainReport,
        st: &mut State,
        prep: &mut Prepared,
    ) -> Result<bool> {
        let mut wrote_lance = false;
        let mut grouped: BTreeMap<CollectionId, Vec<&OutboxOp>> = BTreeMap::new();
        for op in ops {
            grouped.entry(op.collection_id).or_default().push(op);
        }
        for (coll, ops_c) in grouped {
            let Some(stores) = by_coll.get(&coll).cloned() else {
                // Not open (yet): its stores keep their old watermark and replay later.
                continue;
            };
            let table = stores.vectors.name().to_string();
            let lw = lance_wm.get(&table).copied().unwrap_or(0);
            let xw = lex_wm.get(&coll).copied().unwrap_or(0);
            let max_seq = ops_c.iter().map(|o| o.seq).max().unwrap_or(0);
            let lance_ops: Vec<&OutboxOp> = ops_c.iter().copied().filter(|o| o.seq > lw).collect();
            let lex_ops: Vec<&OutboxOp> = ops_c.iter().copied().filter(|o| o.seq > xw).collect();
            if lance_ops.is_empty() && lex_ops.is_empty() {
                continue;
            }
            let lance_plan = plan(&lance_ops);
            let lex_plan = plan(&lex_ops);

            // Current snapshot of every touched file (coalescing: the latest catalog state wins).
            let mut snaps: HashMap<FileId, FileSnapshot> = HashMap::new();
            for f in lance_plan.files.union(&lex_plan.files) {
                let s = match prep.snaps.get(f) {
                    Some(s) => s.clone(),
                    None => self.chunks.file_snapshot(*f)?,
                };
                // The catalog snapshot is the truth: a file that exists now is indexed even if its
                // folder id was purged earlier in the same batch (deletes apply before adds).
                if let Some(s) = s
                    && s.collection_id == coll
                {
                    snaps.insert(*f, s);
                }
            }

            if !lance_ops.is_empty() {
                let rows = self
                    .vector_rows(&stores, &lance_plan, &snaps, prep, rep)
                    .await?;
                stores
                    .vectors
                    .apply(&VectorBatch {
                        rows,
                        replace_files: lance_plan.files.iter().copied().collect(),
                        purge_folders: lance_plan.purge.iter().copied().collect(),
                        seq: max_seq,
                    })
                    .await?;
                wrote_lance = true;
                lance_wm.insert(table.clone(), max_seq);
                if self.cfg.maintain {
                    st.maintain_pending.insert(table.clone(), stores.clone());
                }
                st.compact_pending.insert(table.clone(), stores.clone());
                let n = self.faults.fail_after_lance_commits.load(Ordering::SeqCst);
                if n > 0 {
                    self.faults
                        .fail_after_lance_commits
                        .store(n - 1, Ordering::SeqCst);
                    if n == 1 {
                        return Err(internal("injected crash after lance commit"));
                    }
                }
            }

            if !lex_ops.is_empty() {
                let mut need: Vec<TextHash> = lex_plan
                    .files
                    .iter()
                    .filter_map(|f| snaps.get(f))
                    .flat_map(|s| s.chunks.iter().map(|c| c.text_hash))
                    .filter(|h| !prep.texts.contains_key(h))
                    .collect();
                need.sort();
                need.dedup();
                if !need.is_empty() {
                    prep.texts.extend(self.chunks.texts(&need)?);
                }
                let mut docs = Vec::new();
                for f in &lex_plan.files {
                    let Some(s) = snaps.get(f) else { continue };
                    for c in &s.chunks {
                        let Some(body) = prep.texts.get(&c.text_hash) else {
                            tracing::warn!(target: "sr_core::writer", file = %s.file_id, "chunk text missing");
                            continue;
                        };
                        docs.push(LexDoc {
                            chunk_id: c.chunk_id,
                            file_id: s.file_id,
                            folder_id: s.folder_id,
                            collection_id: s.collection_id,
                            body: body.clone(),
                            path: s.rel_path.clone(),
                            lang: s.lang.clone(),
                        });
                    }
                }
                stores.lexical.apply(&LexBatch {
                    docs,
                    replace_files: lex_plan.files.iter().copied().collect(),
                    purge_folders: lex_plan.purge.iter().copied().collect(),
                    seq: max_seq,
                })?;
                lex_wm.insert(coll, max_seq);
            }

            let all_files: BTreeSet<FileId> =
                lance_plan.files.union(&lex_plan.files).copied().collect();
            rep.files_upserted += all_files.iter().filter(|f| snaps.contains_key(f)).count();
            rep.files_removed += all_files.iter().filter(|f| !snaps.contains_key(f)).count();
            let purge: BTreeSet<FolderId> =
                lance_plan.purge.union(&lex_plan.purge).copied().collect();
            rep.folders_purged += purge.len();

            let ack = Ack {
                watermarks: vec![
                    Watermark {
                        store: StoreKind::Lance,
                        scope_id: table.clone(),
                        collection_id: coll,
                        seq: lance_wm.get(&table).copied().unwrap_or(lw),
                    },
                    Watermark {
                        store: StoreKind::Tantivy,
                        scope_id: coll.to_string(),
                        collection_id: coll,
                        seq: lex_wm.get(&coll).copied().unwrap_or(xw),
                    },
                ],
                indexed: snaps.values().map(|s| (s.file_id, s.version)).collect(),
            };
            self.outbox.acknowledge(&ack)?;

            if !purge.is_empty() {
                // M9: physically remove the purged data now (SLA 15 min).
                stores.vectors.compact_and_purge_old_versions().await?;
                stores.lexical.merge_and_gc()?;
            }
        }
        Ok(wrote_lance)
    }

    /// Vector rows for the upserted files: vectors prepared while selecting the batch (copied by
    /// `text_hash` or embedded); anything still missing is looked up / embedded here.
    async fn vector_rows(
        &self,
        stores: &CollectionStores,
        p: &Plan,
        snaps: &HashMap<FileId, FileSnapshot>,
        prep: &mut Prepared,
        rep: &mut DrainReport,
    ) -> Result<Vec<VectorRow>> {
        let table = stores.vectors.name().to_string();
        let files: Vec<&FileSnapshot> = p.files.iter().filter_map(|f| snaps.get(f)).collect();
        let mut hashes: Vec<TextHash> = files
            .iter()
            .flat_map(|s| s.chunks.iter().map(|c| c.text_hash))
            .collect();
        hashes.sort();
        hashes.dedup();
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let known = prep.vectors.entry(table.clone()).or_default();
        let unknown: Vec<TextHash> = hashes
            .iter()
            .copied()
            .filter(|h| !known.contains_key(h))
            .collect();
        if !unknown.is_empty() {
            // Safety net (the selection normally prepared everything).
            let found = stores.vectors.vectors_by_text_hash(&unknown).await?;
            prep.reused
                .entry(table.clone())
                .or_default()
                .extend(found.keys().copied());
            let known = prep.vectors.entry(table.clone()).or_default();
            known.extend(found);
            let missing: Vec<TextHash> = unknown
                .into_iter()
                .filter(|h| !known.contains_key(h))
                .collect();
            if !missing.is_empty() {
                let need: Vec<TextHash> = missing
                    .iter()
                    .copied()
                    .filter(|h| !prep.texts.contains_key(h))
                    .collect();
                if !need.is_empty() {
                    prep.texts.extend(self.chunks.texts(&need)?);
                }
                let have: Vec<TextHash> = missing
                    .into_iter()
                    .filter(|h| prep.texts.contains_key(h))
                    .collect();
                let dims = stores.embedder.dims();
                if dims != stores.vectors.dims() {
                    return Err(
                        Error::new(ErrorCode::ModelIncompatible).with_detail(format!(
                            "embedder has {dims} dims, index {} has {}",
                            stores.vectors.name(),
                            stores.vectors.dims()
                        )),
                    );
                }
                for part in have.chunks(self.cfg.embed_batch.max(1)) {
                    let batch: Vec<&str> = part
                        .iter()
                        .filter_map(|h| prep.texts.get(h).map(String::as_str))
                        .collect();
                    let out = stores.embedder.embed_documents(&batch).await?;
                    if out.len() != part.len() || out.iter().any(|v| v.len() != dims) {
                        return Err(Error::new(ErrorCode::ModelIncompatible)
                            .with_detail("embedder returned the wrong number or size of vectors"));
                    }
                    rep.chunks_embedded += part.len();
                    let known = prep.vectors.entry(table.clone()).or_default();
                    for (h, v) in part.iter().zip(out) {
                        known.insert(*h, v);
                    }
                }
            }
        }
        let vectors = prep.vectors.entry(table.clone()).or_default();
        let reused = prep.reused.entry(table).or_default();
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for s in files {
            for c in &s.chunks {
                let Some(v) = vectors.get(&c.text_hash) else {
                    tracing::warn!(target: "sr_core::writer", file = %s.file_id, "chunk text missing; not embedded");
                    continue;
                };
                if seen.insert(c.chunk_id) {
                    if reused.contains(&c.text_hash) {
                        rep.chunks_reused += 1;
                    }
                    rows.push(VectorRow {
                        chunk_id: c.chunk_id,
                        text_hash: c.text_hash,
                        file_id: s.file_id,
                        folder_id: s.folder_id,
                        collection_id: s.collection_id,
                        vector: v.clone(),
                    });
                }
            }
        }
        Ok(rows)
    }
}
