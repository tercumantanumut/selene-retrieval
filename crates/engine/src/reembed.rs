//! Background re-embed into a shadow vector index and the atomic swap (Amendment 1; docs/02 §2.8
//! "Re-embed and atomic swap"). Owner: M2-G.
//!
//! ```text
//! [Use it] ─► consent ─► download (sr_providers::download, resumable, sha256) ─► load provider
//!   ─► assert declared dims == tier dims and canary output dims == declared (else ModelIncompatible,
//!      NOTHING written; search stays on the old index)
//!   ─► vector_indexes row (state=building, start_seq = outbox max seq) + Lance table vec_<id>
//!   ─► bulk pass: current chunks in chunk_id order after checkpoint_after_chunk_id
//!        text (catalog) → embed (vectors reused by text_hash inside the shadow) → len == dims
//!        (else shadow discarded, ModelIncompatible) → Lance merge_insert (watermark kept) → commit
//!        → THEN checkpoint (data first: a crash in between re-upserts the same chunk ids = no-op)
//!        → reembed.progress (≤ 4 Hz)
//!   ─► catch-up: outbox ops after the shadow's watermark, coalesced per file (same catalog
//!        coalescing as replay), applied to the shadow; its catalog watermark advances, so pruning
//!        keeps every op the shadow still needs
//!   ─► validate: shadow chunk-id set == catalog chunk set (repair stragglers), dims; ANN upkeep
//!   ─► swap: ONE catalog transaction (old → retired, new → active, collections.active_index_id)
//!        then the StoreRegistry entry (queries snapshot it per call: in-flight queries finish on
//!        the old table) → index.swapped
//!   ─► GC: the retired table is dropped once no query holds it (≥ 15 min, or at the next start)
//! ```
//!
//! * **Chunk ids never change**: the shadow is keyed by the catalog's chunk ids (chunking is
//!   tier-independent, Amendment 3.5); only vectors differ.
//! * **Live edits never wait**: they go to the active index as usual. After the swap the
//!   IndexWriter re-applies every op above the new table's watermark (it is per-store-watermark
//!   driven), so nothing done between the last catch-up and the swap is lost.
//! * **Resumable and kill-safe**: the building row, its checkpoint and the Lance table are all the
//!   state there is. `kill -9` at any point leaves search on the old index; the next run resumes
//!   from the checkpoint (`reembed.started{resumed_from}`). Tested by killing a child process at
//!   random points (`tests/reembed.rs`).
//!
//! App-facing operations (M2-R / M2-A call these; see `docs/research/m2-g-gpu-models.md` §6) are
//! the `impl EngineHandle` block at the bottom: [`EngineHandle::search_model_status`],
//! [`EngineHandle::use_recommended_search_model`], [`EngineHandle::choose_search_model`],
//! [`EngineHandle::start_reembed_with`], [`EngineHandle::cancel_search_model_change`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sr_catalog::model::{IndexState, Store};
use sr_catalog::repo::{meta, outbox, profiles};
use sr_catalog::{Catalog, CoalescedOp, DbResultExt};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash, VectorIndexId};
use sr_common::tier::{Tier, TierProfile};
use sr_core::index::lance::{VectorBatch, VectorRow};
use sr_core::index::source::ChunkSource;
use sr_core::index::{
    AnnConfig, CollectionStores, LexConfig, LexicalIndex, StoreRegistry, VectorStore, VectorTable,
};
use sr_providers::EmbeddingProvider;
use tokio_util::sync::CancellationToken;

use crate::EngineHandle;
use crate::adapters::ProviderEmbedder;
use crate::bus::EventBus;
use crate::clock::SharedClock;

/// Retired tables are kept at least this long after the swap (in-flight queries, §2.8 GC).
pub const RETIRED_GC_AFTER: Duration = Duration::from_secs(15 * 60);

/// Canary text checked before anything is written (declared vs returned dims).
const CANARY: &str = "Selene Retrieval canary: search keeps working while the index is rebuilt.";

fn internal(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal).with_detail(detail)
}

fn incompatible(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::ModelIncompatible).with_detail(detail)
}

fn hex32(b: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*b).to_hex().to_string()
}

// ------------------------------------------------------------------------- catalog reads (SQL)
//
// Read-only queries over `chunks`/`files`/`folders`/`vector_indexes`. They belong in sr-catalog
// (requested from M2-W in docs/research/m2-g-progress.md); kept here, read-only, until then.

/// One current chunk of a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkRow {
    chunk_id: ChunkId,
    text_hash: TextHash,
    file_id: FileId,
    folder_id: FolderId,
}

const CURRENT_CHUNKS: &str = "FROM chunks c
     JOIN files f ON f.id = c.file_id AND c.file_version = f.version
     JOIN folders fo ON fo.id = f.folder_id
     WHERE fo.collection_id = ?1";

fn count_chunks(c: &rusqlite::Connection, coll: CollectionId) -> Result<u64, Error> {
    c.query_row(
        &format!("SELECT count(*) {CURRENT_CHUNKS}"),
        [coll.to_bytes()],
        |r| r.get(0),
    )
    .db()
}

fn chunks_after(
    c: &rusqlite::Connection,
    coll: CollectionId,
    after: Option<ChunkId>,
    limit: usize,
) -> Result<Vec<ChunkRow>, Error> {
    let after: Vec<u8> = after.map_or_else(Vec::new, |a| a.as_bytes().to_vec());
    let mut st = c
        .prepare_cached(&format!(
            "SELECT c.chunk_id, c.text_hash, c.file_id, f.folder_id {CURRENT_CHUNKS}
             AND c.chunk_id > ?2 ORDER BY c.chunk_id LIMIT ?3"
        ))
        .db()?;
    st.query_map(
        params![
            coll.to_bytes(),
            after,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
        |r| {
            Ok(ChunkRow {
                chunk_id: ChunkId::from_bytes(r.get(0)?),
                text_hash: TextHash::from_bytes(r.get(1)?),
                file_id: FileId::from_bytes(r.get(2)?),
                folder_id: FolderId::from_bytes(r.get(3)?),
            })
        },
    )
    .db()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .db()
}

fn chunk_files(
    c: &rusqlite::Connection,
    coll: CollectionId,
) -> Result<HashMap<ChunkId, FileId>, Error> {
    let mut st = c
        .prepare(&format!("SELECT c.chunk_id, c.file_id {CURRENT_CHUNKS}"))
        .db()?;
    st.query_map([coll.to_bytes()], |r| {
        Ok((
            ChunkId::from_bytes(r.get(0)?),
            FileId::from_bytes(r.get(1)?),
        ))
    })
    .db()?
    .collect::<rusqlite::Result<HashMap<_, _>>>()
    .db()
}

/// `(index id, lance table, state, retired_at or created_at)` of retired / failed indexes.
fn dead_indexes(
    c: &rusqlite::Connection,
) -> Result<Vec<(VectorIndexId, String, String, i64)>, Error> {
    let mut st = c
        .prepare(
            "SELECT id, lance_table, state, coalesce(retired_at, created_at) FROM vector_indexes
             WHERE state IN ('retired','failed')",
        )
        .db()?;
    st.query_map([], |r| {
        Ok((
            VectorIndexId::from_bytes(r.get(0)?),
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
        ))
    })
    .db()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .db()
}

fn building_index(
    c: &rusqlite::Connection,
    coll: CollectionId,
) -> Result<Option<profiles::VectorIndexRecord>, Error> {
    Ok(profiles::live_indexes(c, coll)?
        .into_iter()
        .find(|i| i.state == IndexState::Building))
}

// ------------------------------------------------------------------------------- public types

/// What a re-embed needs from the engine (all shared handles; tests build it by hand).
#[derive(Clone)]
pub struct ReembedDeps {
    pub cat: Arc<Catalog>,
    pub vectors: VectorStore,
    pub registry: Arc<StoreRegistry>,
    pub chunks: Arc<dyn ChunkSource>,
    pub bus: Arc<EventBus>,
    pub clock: SharedClock,
    /// tantivy root (to open the lexical index if the collection is not registered yet).
    pub tantivy_root: PathBuf,
    /// Cancels query embeddings of the new provider at shutdown (the engine's `embed_cancel`).
    pub embed_cancel: CancellationToken,
}

/// Test-only fault points. `None` in production.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReembedFault {
    #[default]
    None,
    /// Fail after the N-th bulk batch's data commit, before its checkpoint (a crash in between).
    AfterDataBeforeCheckpoint(u32),
    /// Fail after validation, before the swap transaction.
    BeforeSwap,
}

#[derive(Debug, Clone)]
pub struct ReembedOptions {
    /// Chunks per bulk batch (one Lance commit + one checkpoint). 512 = the group-commit bound.
    pub batch: usize,
    /// `reembed.progress` at most this often (4 Hz).
    pub progress_every: Duration,
    /// Catch-up / validation rounds before giving up (resumable).
    pub rounds: usize,
    /// Keep a retired table at least this long before dropping it.
    pub gc_after: Duration,
    #[doc(hidden)]
    pub fault: ReembedFault,
}

impl Default for ReembedOptions {
    fn default() -> Self {
        Self {
            batch: 512,
            progress_every: Duration::from_millis(250),
            rounds: 5,
            gc_after: RETIRED_GC_AFTER,
            fault: ReembedFault::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReembedOutcome {
    Swapped {
        old: Option<VectorIndexId>,
        new: VectorIndexId,
        rows: u64,
    },
    /// The active index already uses this embedding profile.
    AlreadyActive,
}

// ---------------------------------------------------------------------------------- the run

struct Shadow<'a> {
    deps: &'a ReembedDeps,
    coll: CollectionId,
    id: VectorIndexId,
    table: Arc<VectorTable>,
    provider: Arc<dyn EmbeddingProvider>,
    dims: usize,
    cancel: &'a CancellationToken,
}

impl Shadow<'_> {
    async fn texts(&self, hashes: Vec<TextHash>) -> Result<HashMap<TextHash, String>, Error> {
        let src = self.deps.chunks.clone();
        tokio::task::spawn_blocking(move || src.texts(&hashes))
            .await
            .map_err(|e| internal(format!("texts task: {e}")))?
    }

    /// Vector rows for `rows`: vectors already in the shadow are reused by text_hash (resume,
    /// duplicate text), the rest are embedded with the new provider and checked.
    async fn vector_rows(&self, rows: &[ChunkRow]) -> Result<Vec<VectorRow>, Error> {
        let mut hashes: Vec<TextHash> = rows.iter().map(|r| r.text_hash).collect();
        hashes.sort();
        hashes.dedup();
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut vecs = self.table.vectors_by_text_hash(&hashes).await?;
        let missing: Vec<TextHash> = hashes
            .iter()
            .copied()
            .filter(|h| !vecs.contains_key(h))
            .collect();
        if !missing.is_empty() {
            let texts = self.texts(missing.clone()).await?;
            let have: Vec<TextHash> = missing
                .into_iter()
                .filter(|h| texts.contains_key(h))
                .collect();
            for part in have.chunks(self.provider.max_batch().max(1)) {
                if self.cancel.is_cancelled() {
                    return Err(Error::new(ErrorCode::Cancelled));
                }
                let batch: Vec<&str> = part
                    .iter()
                    .filter_map(|h| texts.get(h).map(String::as_str))
                    .collect();
                let out = self.provider.embed_documents(&batch, self.cancel).await?;
                if out.len() != part.len() {
                    return Err(incompatible(format!(
                        "provider returned {} vectors for {} texts",
                        out.len(),
                        part.len()
                    )));
                }
                if let Some(v) = out.iter().find(|v| v.len() != self.dims) {
                    return Err(incompatible(format!(
                        "provider returned {}-dim vectors for a {}-dim index",
                        v.len(),
                        self.dims
                    )));
                }
                if out.iter().flatten().any(|x| !x.is_finite()) {
                    return Err(incompatible("provider returned non-finite vectors"));
                }
                for (h, v) in part.iter().zip(out) {
                    vecs.insert(*h, v);
                }
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        for r in rows {
            if let Some(v) = vecs.get(&r.text_hash)
                && seen.insert(r.chunk_id)
            {
                out.push(VectorRow {
                    chunk_id: r.chunk_id,
                    text_hash: r.text_hash,
                    file_id: r.file_id,
                    folder_id: r.folder_id,
                    collection_id: self.coll,
                    vector: v.clone(),
                });
            }
        }
        Ok(out)
    }

    /// Re-apply whole files from their current catalog snapshot (catch-up and repair).
    async fn apply_files(
        &self,
        files: Vec<FileId>,
        purge: Vec<FolderId>,
        seq: i64,
    ) -> Result<(), Error> {
        let src = self.deps.chunks.clone();
        let coll = self.coll;
        let f2 = files.clone();
        let rows: Vec<ChunkRow> = tokio::task::spawn_blocking(move || {
            let mut rows = Vec::new();
            for f in f2 {
                if let Some(s) = src.file_snapshot(f)?
                    && s.collection_id == coll
                {
                    rows.extend(s.chunks.iter().map(|c| ChunkRow {
                        chunk_id: c.chunk_id,
                        text_hash: c.text_hash,
                        file_id: s.file_id,
                        folder_id: s.folder_id,
                    }));
                }
            }
            Ok::<_, Error>(rows)
        })
        .await
        .map_err(|e| internal(format!("snapshot task: {e}")))??;
        let vrows = self.vector_rows(&rows).await?;
        self.table
            .apply(&VectorBatch {
                rows: vrows,
                replace_files: files,
                purge_folders: purge,
                seq,
            })
            .await
    }

    /// Outbox ops above the shadow's watermark → shadow. Returns raw ops applied.
    async fn catch_up(&self) -> Result<usize, Error> {
        let mut total = 0;
        loop {
            if self.cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::Cancelled));
            }
            let after = self.table.watermark().await?;
            let coll = self.coll;
            let batch = self
                .deps
                .cat
                .read_async(move |c| outbox::ops_after(c, coll, after, 1000))
                .await?;
            if batch.raw == 0 {
                return Ok(total);
            }
            let mut files = Vec::new();
            let mut purge = Vec::new();
            for (_, op) in &batch.ops {
                match op {
                    CoalescedOp::Upsert { file_id, .. } | CoalescedOp::Delete { file_id } => {
                        files.push(*file_id);
                    }
                    CoalescedOp::PurgeFolder { folder_id } => purge.push(*folder_id),
                }
            }
            self.apply_files(files, purge, batch.through_seq).await?;
            let (id, seq) = (self.id, batch.through_seq);
            self.deps
                .cat
                .write(move |c| outbox::set_watermark(c, Store::Lance, &id.to_bytes(), seq))
                .await?;
            total += batch.raw;
        }
    }

    /// Shadow chunk-id set vs catalog; repairs files that differ. True when they match.
    async fn validate_and_repair(&self) -> Result<bool, Error> {
        let coll = self.coll;
        let want = self
            .deps
            .cat
            .read_async(move |c| chunk_files(c, coll))
            .await?;
        let have: HashMap<ChunkId, FileId> = self.table.rows(None).await?.into_iter().collect();
        let mut bad: HashSet<FileId> = HashSet::new();
        for (c, f) in &want {
            if !have.contains_key(c) {
                bad.insert(*f);
            }
        }
        for (c, f) in &have {
            if !want.contains_key(c) {
                bad.insert(*f);
            }
        }
        if bad.is_empty() {
            return Ok(true);
        }
        tracing::info!(target: "sr_engine::reembed", files = bad.len(), "repairing files that changed during the rebuild");
        let wm = self.table.watermark().await?;
        self.apply_files(bad.into_iter().collect(), Vec::new(), wm)
            .await?;
        Ok(false)
    }
}

/// Re-embed `coll` with `provider` into a shadow index and swap it in (see module docs).
pub async fn run(
    deps: &ReembedDeps,
    coll: CollectionId,
    provider: Arc<dyn EmbeddingProvider>,
    opts: &ReembedOptions,
    cancel: &CancellationToken,
) -> Result<ReembedOutcome, Error> {
    let profile = provider.profile().clone();
    let spec = crate::profile_spec(&profile);
    let fp = spec.fingerprint();
    let dims = profile.dims as usize;

    // 0. Before anything is written: declared dims must be the tier's, returned dims declared.
    if let Some(t) = profile.tier
        && profile.provider == "local"
        && TierProfile::for_tier(t).dims != profile.dims
    {
        return Err(incompatible(format!(
            "model declares {} dims, the {} profile has {}",
            profile.dims,
            t.as_str(),
            TierProfile::for_tier(t).dims
        )));
    }
    let canary = provider.embed_documents(&[CANARY], cancel).await?;
    match canary.first() {
        Some(v) if canary.len() == 1 && v.len() == dims && v.iter().all(|x| x.is_finite()) => {}
        Some(v) => {
            return Err(incompatible(format!(
                "model declares {dims} dims but returned {}",
                v.len()
            )));
        }
        None => return Err(incompatible("model returned no vector")),
    }

    // 1. Already there?
    let active = deps
        .cat
        .read_async(move |c| profiles::active_index(c, coll))
        .await?;
    if active.as_ref().is_some_and(|(_, p)| p.fingerprint == fp) {
        return Ok(ReembedOutcome::AlreadyActive);
    }
    let old_id = active.as_ref().map(|(i, _)| i.id);

    // 2. Resume the shadow for this profile, or replace a shadow for another one.
    let now = deps.clock.now_ms();
    let spec2 = spec.clone();
    let (rec, resumed, superseded) = deps
        .cat
        .write(move |c| {
            sr_catalog::atomic(c, |c| {
                let mut superseded = None;
                if let Some(b) = building_index(c, coll)? {
                    let p = profiles::profile(c, b.profile_id)?;
                    if p.fingerprint == fp {
                        return Ok((b, true, None));
                    }
                    profiles::fail_index(c, b.id, ErrorCode::Cancelled)?;
                    superseded = Some(b.lance_table);
                }
                let pid = profiles::ensure_profile(c, &spec2, now)?;
                let id = profiles::create_vector_index(c, coll, pid, IndexState::Building, now)?;
                Ok((profiles::vector_index(c, id)?, false, superseded))
            })
        })
        .await?;
    if let Some(t) = superseded {
        let _ = deps.vectors.drop_table(&t).await;
    }
    let id = rec.id;
    let ann = profile.tier.map_or_else(AnnConfig::default, |t| {
        AnnConfig::from(&TierProfile::for_tier(t).ann)
    });
    let table = match deps.vectors.open_table(&rec.lance_table, dims, ann).await {
        Ok(t) => Arc::new(t),
        Err(e) => {
            discard(deps, id, &rec.lance_table, e.code).await;
            return Err(e);
        }
    };
    let sh = Shadow {
        deps,
        coll,
        id,
        table,
        provider: provider.clone(),
        dims,
        cancel,
    };
    let r = build_and_swap(&sh, &rec, resumed, old_id, &profile, opts).await;
    match r {
        Ok(o) => Ok(o),
        Err(e) => {
            let resumable = e.code != ErrorCode::ModelIncompatible;
            if !resumable {
                discard(deps, id, &rec.lance_table, e.code).await;
            }
            tracing::warn!(target: "sr_engine::reembed", code = %e.code, resumable, err = %e, "search model change stopped; current search kept");
            deps.bus.publish(EngineEvent::ReembedFailed {
                index_id: id,
                code: e.code,
                resumable,
            });
            Err(e)
        }
    }
}

/// Mark a shadow failed and drop its table; the active index is untouched.
async fn discard(deps: &ReembedDeps, id: VectorIndexId, table: &str, code: ErrorCode) {
    if let Err(e) = deps
        .cat
        .write(move |c| profiles::fail_index(c, id, code))
        .await
    {
        tracing::warn!(target: "sr_engine::reembed", err = %e, "could not mark shadow failed");
    }
    if let Err(e) = deps.vectors.drop_table(table).await {
        tracing::warn!(target: "sr_engine::reembed", err = %e, "could not drop shadow table (GC retries)");
    }
}

async fn build_and_swap(
    sh: &Shadow<'_>,
    rec: &profiles::VectorIndexRecord,
    resumed: bool,
    old_id: Option<VectorIndexId>,
    profile: &sr_providers::EmbeddingProfile,
    opts: &ReembedOptions,
) -> Result<ReembedOutcome, Error> {
    let deps = sh.deps;
    let coll = sh.coll;
    let id = sh.id;
    let total = deps.cat.read_async(move |c| count_chunks(c, coll)).await?;
    let mut done = if resumed { rec.rows_done } else { 0 };
    let mut after = if resumed {
        rec.checkpoint_after_chunk_id
    } else {
        None
    };
    deps.bus.publish(EngineEvent::ReembedStarted {
        index_id: id,
        model_id: profile.model.clone(),
        dims: profile.dims,
        total,
        resumed_from: done,
    });
    tracing::info!(target: "sr_engine::reembed", index = %id, model = %profile.model, total, resumed_from = done, "rebuilding search index in the background");

    // 3. Bulk pass. The sentinel watermark never drops below start_seq (and never below what a
    //    resumed catch-up already reached).
    let wm = sh.table.watermark().await?.max(rec.start_seq);
    let t0 = Instant::now();
    let done0 = done;
    let mut last_progress = Instant::now();
    let mut batches = 0u32;
    loop {
        if sh.cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let lim = opts.batch.max(1);
        let rows = deps
            .cat
            .read_async(move |c| chunks_after(c, coll, after, lim))
            .await?;
        let Some(last) = rows.last().map(|r| r.chunk_id) else {
            break;
        };
        let vrows = sh.vector_rows(&rows).await?;
        sh.table
            .apply(&VectorBatch {
                rows: vrows,
                seq: wm,
                ..VectorBatch::default()
            })
            .await?;
        batches += 1;
        if opts.fault == ReembedFault::AfterDataBeforeCheckpoint(batches) {
            return Err(internal(
                "injected crash between data commit and checkpoint",
            ));
        }
        done += rows.len() as u64;
        after = Some(last);
        let (d, t) = (done, total.max(done));
        deps.cat
            .write(move |c| profiles::update_index_progress(c, id, d, t, Some(last)))
            .await?;
        if last_progress.elapsed() >= opts.progress_every {
            last_progress = Instant::now();
            let rate = (done - done0) as f64 / t0.elapsed().as_secs_f64().max(1e-3);
            let eta_s = (rate > 0.0).then(|| ((t.saturating_sub(done)) as f64 / rate) as u64);
            deps.bus.publish(EngineEvent::ReembedProgress {
                index_id: id,
                done,
                total: t,
                eta_s,
            });
        }
    }

    // 4–5. Catch up with edits made meanwhile, validate (repair stragglers), repeat if needed.
    let mut ok = false;
    for _ in 0..opts.rounds.max(1) {
        sh.catch_up().await?;
        if sh.validate_and_repair().await? {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(internal(
            "shadow index still differs from the catalog after catch-up (retried later)",
        ));
    }
    if sh.table.dims() != profile.dims as usize {
        return Err(incompatible("shadow table dims differ from the profile"));
    }
    sh.table.maintain().await?; // scalar indexes; ANN once rows ≥ ann.min_rows
    let rows = sh.table.count(None).await? as u64;
    if opts.fault == ReembedFault::BeforeSwap {
        return Err(internal("injected crash before the swap"));
    }

    // 6. Swap: catalog first (one transaction), then the registry.
    let now = deps.clock.now_ms();
    let retired = deps
        .cat
        .write(move |c| profiles::activate_index(c, coll, id, now))
        .await?;
    let retired = retired.or(old_id);
    let lexical = match deps.registry.get(coll) {
        Some(s) => s.lexical,
        None => {
            let root = deps.tantivy_root.clone();
            Arc::new(
                tokio::task::spawn_blocking(move || {
                    LexicalIndex::open(&root, coll, LexConfig::default())
                })
                .await
                .map_err(|e| internal(format!("lexical open: {e}")))??,
            )
        }
    };
    let old_stores = deps.registry.get(coll);
    let spec = crate::profile_spec(profile);
    deps.registry.register(CollectionStores {
        collection_id: coll,
        vectors: sh.table.clone(),
        lexical,
        embedder: Arc::new(ProviderEmbedder::new(
            sh.provider.clone(),
            hex32(&spec.fingerprint()),
            deps.embed_cancel.clone(),
        )),
    });
    deps.bus.publish(EngineEvent::IndexSwapped {
        collection_id: coll,
        old_index_id: retired,
        new_index_id: id,
        model_id: profile.model.clone(),
        dims: profile.dims,
    });
    tracing::info!(target: "sr_engine::reembed", index = %id, rows, "search upgraded: new index active");

    // 7. GC of the retired table once nothing holds it: in-flight queries, and the IndexWriter's
    //    pending upkeep (it keeps `CollectionStores` of tables it wrote until its next
    //    maintenance; dropping a table it still maintains would fail the writer). The startup
    //    sweep (`gc_dead_tables`) is the backstop.
    if let Some(old) = old_stores {
        let vectors = deps.vectors.clone();
        let wait = opts.gc_after;
        let table = old.vectors.clone();
        drop(old);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            loop {
                if Arc::strong_count(&table) <= 1 {
                    let name = table.name().to_string();
                    drop(table);
                    if let Err(e) = vectors.drop_table(&name).await {
                        tracing::warn!(target: "sr_engine::reembed", err = %e, "retired table not dropped yet (startup GC retries)");
                    }
                    return;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    Ok(ReembedOutcome::Swapped {
        old: retired,
        new: id,
        rows,
    })
}

/// Drop Lance tables of retired indexes older than `min_age` and of failed shadows (startup GC;
/// a fresh process has no readers). Returns tables dropped.
pub async fn gc_dead_tables(deps: &ReembedDeps, min_age: Duration) -> Result<usize, Error> {
    let dead = deps.cat.read_async(dead_indexes).await?;
    let names: HashSet<String> = deps.vectors.table_names().await?.into_iter().collect();
    let live: HashSet<String> = deps
        .registry
        .all()
        .iter()
        .map(|s| s.vectors.name().to_string())
        .collect();
    let now = deps.clock.now_ms();
    let min_ms = i64::try_from(min_age.as_millis()).unwrap_or(i64::MAX);
    let mut n = 0;
    for (_, table, state, at) in dead {
        let old_enough = state == "failed" || now.saturating_sub(at) >= min_ms;
        if old_enough && names.contains(&table) && !live.contains(&table) {
            deps.vectors.drop_table(&table).await?;
            n += 1;
        }
    }
    Ok(n)
}

// ------------------------------------------------------------------- engine integration (M2-G)

/// What the user chose ([Use it] / Advanced) and has not finished yet. Persisted in catalog
/// `meta` so a restart resumes the download and the rebuild without asking again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeIntent {
    pub tier: Tier,
    pub sha256: String,
    pub allow_metered: bool,
    pub at_ms: i64,
    /// `Some("multilingual")` for the multilingual profile (M3-Q); `None` = the tier's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// [`UpgradeIntent::model`] of the multilingual profile.
pub const MULTILINGUAL_INTENT: &str = "multilingual";

impl UpgradeIntent {
    pub const META_KEY: &'static str = "search_model_intent";
}

/// Where a search-model change is (`EngineHandle::search_model_status`). Progress also arrives as
/// `model.download.*` / `reembed.*` / `index.swapped` events (push; never poll for it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum UpgradePhase {
    Idle,
    Downloading {
        bytes: u64,
        total: u64,
    },
    /// Metered connection: waiting for "Download anyway" or another connection.
    WaitingForUnmeteredNetwork {
        have: u64,
        total: u64,
    },
    Rebuilding {
        done: u64,
        total: u64,
    },
    Done {
        tier: Tier,
    },
    Failed {
        code: ErrorCode,
        resumable: bool,
    },
}

/// Snapshot for Settings → Search (Basic line + Advanced tier picker).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchModelStatus {
    /// Tier of the active index's model.
    pub active: Tier,
    /// Detected recommendation (`select_tier`).
    pub recommended: Tier,
    /// The Basic line, if one should be shown ("Recommended for your computer: Better search
    /// (208 MB download)"); `None` when the recommendation is in use or below it.
    pub recommendation: Option<sr_common::tier::Recommendation>,
    pub phase: UpgradePhase,
    /// Advanced warning for a manual choice above the recommendation.
    pub override_warning: Option<&'static str>,
    /// The multilingual model is the active search model (M3-Q).
    pub multilingual_active: bool,
    /// "Search in many languages ({size} download)": MB for this computer (page 12 [M3]; the
    /// app shows the offer only when the Windows display language isn't English).
    pub multilingual_download_mb: u64,
}

struct Upgrade {
    phase: Mutex<UpgradePhase>,
    cancel: Mutex<Option<CancellationToken>>,
}

/// Per-engine upgrade state, keyed by data dir (one engine per data dir).
fn upgrade_state(root: &Path) -> Arc<Upgrade> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Arc<Upgrade>>>> = OnceLock::new();
    let mut g = M
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.entry(root.to_path_buf())
        .or_insert_with(|| {
            Arc::new(Upgrade {
                phase: Mutex::new(UpgradePhase::Idle),
                cancel: Mutex::new(None),
            })
        })
        .clone()
}

fn set_phase(root: &Path, p: UpgradePhase) {
    *upgrade_state(root)
        .phase
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
}

impl EngineHandle {
    fn reembed_deps(&self) -> ReembedDeps {
        let i = self.inner();
        ReembedDeps {
            cat: i.cat.clone(),
            vectors: i.vectors.clone(),
            registry: i.registry.clone(),
            chunks: Arc::new(crate::adapters::CatalogChunks::new(i.cat.clone())),
            bus: i.bus.clone(),
            clock: i.clock.clone(),
            tantivy_root: i.layout.tantivy.clone(),
            embed_cancel: i.embed_cancel.clone(),
        }
    }

    /// Model id of the default collection's active index.
    pub(crate) fn active_model_id(&self) -> Option<String> {
        let coll = self.inner().default_collection;
        self.inner()
            .cat
            .read(|c| profiles::active_index(c, coll))
            .ok()
            .flatten()
            .map(|(_, p)| p.spec.model)
    }

    pub(crate) fn active_tier(&self) -> Tier {
        let coll = self.inner().default_collection;
        self.inner()
            .cat
            .read(|c| profiles::active_index(c, coll))
            .ok()
            .flatten()
            .and_then(|(_, p)| Tier::parse(p.spec.tier.as_str()))
            .unwrap_or(Tier::Minimum)
    }

    /// GPU plan for a new session, from the last detection + the persisted GPU state.
    async fn gpu_context(&self) -> Result<crate::gpu::GpuContext, Error> {
        let i = self.inner();
        let det = i
            .detection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let choices = crate::hw::TierChoices::from_settings(
            &i.settings
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        match det {
            Some((hw, d)) => {
                let st = i.cat.read_async(crate::gpu::load_state).await?;
                Ok(crate::gpu::GpuContext::fixed(
                    hw,
                    d,
                    choices.use_gpu,
                    st,
                    i.cat.clone(),
                    i.clock.clone(),
                ))
            }
            None => {
                crate::gpu::GpuContext::detect(
                    i.cfg.hw_probe.clone(),
                    &i.layout.root,
                    i.cat.clone(),
                    i.clock.clone(),
                    choices,
                )
                .await
            }
        }
    }

    /// Settings → Search: active and recommended tier, the Basic "Recommended…" line, and where a
    /// change is. Cheap; the UI also listens to the events.
    pub async fn search_model_status(&self) -> Result<SearchModelStatus, Error> {
        let gpu = self.gpu_context().await?;
        let active = self.active_tier();
        let recommended = gpu.decision.tier;
        let mut phase = upgrade_state(&self.inner().layout.root)
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if matches!(phase, UpgradePhase::Rebuilding { .. }) {
            // The shadow's catalog row is the truth (checkpointed after every batch).
            let coll = self.inner().default_collection;
            if let Some(b) = self
                .inner()
                .cat
                .read_async(move |c| building_index(c, coll))
                .await?
            {
                phase = UpgradePhase::Rebuilding {
                    done: b.rows_done,
                    total: b.rows_total,
                };
            }
        }
        let choices = crate::hw::TierChoices::from_settings(
            &self
                .inner()
                .settings
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let override_warning = choices
            .tier_override
            .and_then(|t| sr_common::tier::effective_tier(&gpu.decision, Some(t)).1);
        let multilingual_active =
            self.active_model_id().as_deref() == Some(crate::multilingual::MULTILINGUAL_MODEL_ID);
        Ok(SearchModelStatus {
            active,
            recommended,
            // A tier upgrade line would switch away from the multilingual model: not offered.
            recommendation: if multilingual_active {
                None
            } else {
                sr_common::tier::recommendation(recommended, active, gpu.gpu_resolved())
            },
            phase,
            override_warning,
            multilingual_active,
            multilingual_download_mb: crate::multilingual::download_mb(gpu.gpu_resolved()),
        })
    }

    /// [Use it] on the Basic line: download the recommended model (consent = this click) and
    /// rebuild in the background. Returns at once; progress is pushed as events.
    #[cfg(feature = "local")]
    pub async fn use_recommended_search_model(&self, allow_metered: bool) -> Result<Tier, Error> {
        let st = self.search_model_status().await?;
        let Some(r) = st.recommendation else {
            return Err(Error::new(ErrorCode::InvalidInput)
                .with_detail("no better search model is recommended for this computer"));
        };
        self.choose_search_model(r.tier, allow_metered).await?;
        Ok(r.tier)
    }

    /// Advanced → search model (or [Use it]): switch to `tier`, any direction. Never blocks
    /// search; a choice above the recommendation only carries the plain warning.
    #[cfg(feature = "local")]
    pub async fn choose_search_model(&self, tier: Tier, allow_metered: bool) -> Result<(), Error> {
        let gpu = self.gpu_context().await?;
        let tp = TierProfile::for_tier(tier);
        // M5-E: only the built-in model when downloads are managed off.
        if !tp.bundled {
            self.check_download_policy("search model")?;
        }
        let file = tp.file_for_new_index(gpu.gpu_resolved());
        let intent = UpgradeIntent {
            tier,
            sha256: file.sha256.into(),
            allow_metered,
            at_ms: self.inner().clock.now_ms(),
            model: None,
        };
        let v = serde_json::to_string(&intent).map_err(|e| internal(e.to_string()))?;
        self.inner()
            .cat
            .write(move |c| meta::set(c, UpgradeIntent::META_KEY, &v))
            .await?;
        self.spawn_upgrade(intent);
        Ok(())
    }

    /// Settings → Search quality → [M3] "Search in many languages" [Use it]: download the
    /// multilingual model (consent = this click) and rebuild in the background, exactly like a
    /// tier change. Returns at once; progress is pushed as events.
    #[cfg(feature = "local")]
    pub async fn choose_multilingual_search_model(&self, allow_metered: bool) -> Result<(), Error> {
        self.check_download_policy("multilingual search model")?;
        let gpu = self.gpu_context().await?;
        let file = crate::multilingual::MULTILINGUAL.file_for_new_index(gpu.gpu_resolved());
        let intent = UpgradeIntent {
            // Resource class only; the model is named below.
            tier: Tier::Minimum,
            sha256: file.sha256.into(),
            allow_metered,
            at_ms: self.inner().clock.now_ms(),
            model: Some(MULTILINGUAL_INTENT.into()),
        };
        let v = serde_json::to_string(&intent).map_err(|e| internal(e.to_string()))?;
        self.inner()
            .cat
            .write(move |c| meta::set(c, UpgradeIntent::META_KEY, &v))
            .await?;
        // The user's choice lives in settings.toml ([search] search_model, one home).
        self.update_settings(|s| {
            s.search.search_model = sr_common::config::SearchModel::Multilingual
        })
        .await?;
        self.spawn_upgrade(intent);
        Ok(())
    }

    /// Stop a download / rebuild in progress (both resume later from where they stopped).
    pub async fn cancel_search_model_change(&self) -> Result<(), Error> {
        let up = upgrade_state(&self.inner().layout.root);
        if let Some(c) = up
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            c.cancel();
        }
        self.inner()
            .cat
            .write(|c| meta::delete(c, UpgradeIntent::META_KEY))
            .await?;
        set_phase(&self.inner().layout.root, UpgradePhase::Idle);
        Ok(())
    }

    /// Rebuild the default collection with an already-loaded provider (tests, remote profiles,
    /// the local path below). Runs to the swap; errors keep the current search.
    pub async fn start_reembed_with(
        &self,
        provider: Arc<dyn EmbeddingProvider>,
        opts: ReembedOptions,
    ) -> Result<ReembedOutcome, Error> {
        let deps = self.reembed_deps();
        let root = self.inner().layout.root.clone();
        let cancel = self.inner().shutdown.child_token();
        *upgrade_state(&root)
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
        set_phase(&root, UpgradePhase::Rebuilding { done: 0, total: 0 });
        let r = run(
            &deps,
            self.inner().default_collection,
            provider,
            &opts,
            &cancel,
        )
        .await;
        // M2-G G2: the writer drops its upkeep references to the retired table (and its old
        // embedder / tantivy handles) now, so GC can drop the table and nothing outlives use.
        if r.is_ok() {
            self.inner().writer.release_unregistered().await;
        }
        set_phase(
            &root,
            match &r {
                Ok(_) => UpgradePhase::Done {
                    tier: self.active_tier(),
                },
                Err(e) => UpgradePhase::Failed {
                    code: e.code,
                    resumable: e.code != ErrorCode::ModelIncompatible,
                },
            },
        );
        r
    }

    #[cfg(feature = "local")]
    fn spawn_upgrade(&self, intent: UpgradeIntent) {
        let h = self.clone();
        let root = self.inner().layout.root.clone();
        let cancel = self.inner().shutdown.child_token();
        let up = upgrade_state(&root);
        if let Some(prev) = up
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(cancel.clone())
        {
            prev.cancel();
        }
        let tracker = h.clone();
        let task = tokio::spawn(async move {
            let r = h.upgrade_task(&intent, &cancel).await;
            match r {
                Ok(()) => {
                    let _ = h
                        .inner()
                        .cat
                        .write(|c| meta::delete(c, UpgradeIntent::META_KEY))
                        .await;
                }
                Err(e) if e.code == ErrorCode::Cancelled => {}
                Err(e) => {
                    let resumable = !matches!(
                        e.code,
                        ErrorCode::ModelIncompatible
                            | ErrorCode::ModelIntegrity
                            | ErrorCode::InvalidInput
                    );
                    if !resumable {
                        let _ = h
                            .inner()
                            .cat
                            .write(|c| meta::delete(c, UpgradeIntent::META_KEY))
                            .await;
                    }
                    set_phase(
                        &root,
                        UpgradePhase::Failed {
                            code: e.code,
                            resumable,
                        },
                    );
                }
            }
        });
        tracker.track_task(task);
    }

    #[cfg(feature = "local")]
    async fn upgrade_task(
        &self,
        intent: &UpgradeIntent,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        use sr_providers::download::{
            DownloadConfig, DownloadConsent, DownloadOutcome, Downloader, ModelStore,
        };
        if intent.model.as_deref() == Some(MULTILINGUAL_INTENT) {
            return self.multilingual_task(intent, cancel).await;
        }
        let i = self.inner();
        let root = i.layout.root.clone();
        let tp = TierProfile::for_tier(intent.tier);
        let file = [tp.gpu_file, tp.cpu_file]
            .into_iter()
            .find(|f| f.sha256 == intent.sha256)
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput).with_detail("unknown model file"))?;
        let store = ModelStore::in_data_dir(&root);
        let paths = if tp.bundled {
            sr_providers::ModelPaths::bundled(&i.cfg.app_dir)
        } else {
            // Download (or verify) with the consent recorded in the intent.
            let coll = i.default_collection;
            let rows = i.cat.read_async(move |c| count_chunks(c, coll)).await?;
            let extra = sr_common::tier::upgrade_disk_needed(0, rows, tp.dims);
            let cfg = DownloadConfig {
                extra_disk_needed: extra,
                ..DownloadConfig::default()
            };
            let d = Downloader::new(cfg)?;
            let mut consent = DownloadConsent::for_file(tp, &file);
            consent.allow_metered = intent.allow_metered;
            let bus = i.bus.clone();
            let r2 = root.clone();
            let sink: sr_providers::download::DownloadSink = Arc::new(move |e: EngineEvent| {
                if let EngineEvent::ModelDownloadProgress { bytes, total, .. } = &e {
                    set_phase(
                        &r2,
                        UpgradePhase::Downloading {
                            bytes: *bytes,
                            total: *total,
                        },
                    );
                }
                bus.publish(e);
            });
            set_phase(
                &root,
                UpgradePhase::Downloading {
                    bytes: 0,
                    total: file.bytes,
                },
            );
            match d
                .download_model(&store, tp, &file, &consent, &sink, cancel)
                .await?
            {
                DownloadOutcome::PausedMetered { have, total } => {
                    set_phase(
                        &root,
                        UpgradePhase::WaitingForUnmeteredNetwork { have, total },
                    );
                    return Err(Error::new(ErrorCode::Cancelled).with_detail("metered connection"));
                }
                DownloadOutcome::Completed { .. } | DownloadOutcome::AlreadyPresent => {}
            }
            store.paths(tp, &file)
        };
        let gpu = self.gpu_context().await?;
        let mut o = sr_providers::LocalOptions::new(tp, file, paths, gpu.plan);
        gpu.apply_with_wait(&mut o, Some(&i.fullscreen));
        let sink = gpu.sink(Arc::new(crate::BusSink(i.bus.clone())));
        let e = sr_providers::LocalEmbedder::load(o.clone(), sink.clone())
            .await
            .map_err(|e| {
                // A downloaded file that will not load is not usable: ModelIncompatible (#37).
                if e.code == ErrorCode::ModelIntegrity {
                    e
                } else {
                    incompatible(e.to_string())
                }
            })?;
        let _ = gpu.record_load(e.load_report());
        let mut opts = ReembedOptions::default();
        opts.batch = opts.batch.max(e.current_batch());
        // R-UX32: the rebuild also continues on the processor behind a long full-screen wait.
        let provider = gpu.with_fullscreen_fallback(e, &o, sink, &i.fullscreen);
        let r = self.start_reembed_with(provider, opts).await?;
        tracing::info!(target: "sr_engine::reembed", ?r, "search model change finished");
        Ok(())
    }
}

impl EngineHandle {
    /// The multilingual change: pinned download (resumable, verified, metered-aware) of the e5
    /// files, then the same load → re-embed → swap as a tier change.
    #[cfg(feature = "local")]
    async fn multilingual_task(
        &self,
        intent: &UpgradeIntent,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        use sr_providers::download::{DownloadConfig, Downloader, FetchOutcome, FileSpec};
        let ml = &crate::multilingual::MULTILINGUAL;
        let model = crate::multilingual::MULTILINGUAL_MODEL_ID;
        let i = self.inner();
        let root = i.layout.root.clone();
        let file = crate::multilingual::file_for(&intent.sha256)
            .ok_or_else(|| Error::new(ErrorCode::InvalidInput).with_detail("unknown model file"))?;
        let paths = crate::multilingual::paths(&root, &file);
        let coll = i.default_collection;
        let rows = i.cat.read_async(move |c| count_chunks(c, coll)).await?;
        let cfg = DownloadConfig {
            extra_disk_needed: sr_common::tier::upgrade_disk_needed(0, rows, ml.dims),
            ..DownloadConfig::default()
        };
        if !intent.allow_metered && (cfg.is_metered)() {
            set_phase(
                &root,
                UpgradePhase::WaitingForUnmeteredNetwork {
                    have: 0,
                    total: file.bytes,
                },
            );
            return Err(Error::new(ErrorCode::Cancelled).with_detail("metered connection"));
        }
        let d = Downloader::new(cfg)?;
        let bus = i.bus.clone();
        let fail = |e: Error| -> Error {
            if e.code != ErrorCode::Cancelled {
                bus.publish(EngineEvent::ModelDownloadFailed {
                    model_id: model.to_owned(),
                    code: e.code,
                    resumable: e.code != ErrorCode::ModelIntegrity,
                });
            }
            e
        };
        std::fs::create_dir_all(&paths.tokenizer_dir).map_err(|e| fail(Error::from(e)))?;
        for (name, sha) in crate::multilingual::E5_TOKENIZER_FILES {
            let dest = paths.tokenizer_dir.join(name);
            let spec = FileSpec {
                model: model.to_owned(),
                url: d.url(
                    crate::multilingual::E5_TOKENIZER_REPO,
                    crate::multilingual::E5_TOKENIZER_REVISION,
                    name,
                ),
                sha256: sha.to_owned(),
                bytes: None,
                part: paths.tokenizer_dir.join(format!("{name}.part")),
                dest,
            };
            d.fetch(&spec, true, &mut |_, _| {}, cancel)
                .await
                .map_err(fail)?;
        }
        set_phase(
            &root,
            UpgradePhase::Downloading {
                bytes: 0,
                total: file.bytes,
            },
        );
        let spec = FileSpec {
            model: model.to_owned(),
            url: d.url(file.hf_repo, file.hf_revision, file.path),
            sha256: file.sha256.to_owned(),
            bytes: Some(file.bytes),
            dest: paths.onnx.clone(),
            part: paths.onnx.with_extension("part"),
        };
        let (bus2, r2) = (i.bus.clone(), root.clone());
        let mut last = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        let mut progress = |bytes: u64, total: u64| {
            if last.elapsed() >= Duration::from_millis(250) || bytes == total {
                last = std::time::Instant::now();
                set_phase(&r2, UpgradePhase::Downloading { bytes, total });
                bus2.publish(EngineEvent::ModelDownloadProgress {
                    model_id: model.to_owned(),
                    bytes,
                    total,
                });
            }
        };
        match d
            .fetch(&spec, intent.allow_metered, &mut progress, cancel)
            .await
            .map_err(fail)?
        {
            FetchOutcome::PausedMetered { have } => {
                set_phase(
                    &root,
                    UpgradePhase::WaitingForUnmeteredNetwork {
                        have,
                        total: file.bytes,
                    },
                );
                return Err(Error::new(ErrorCode::Cancelled).with_detail("metered connection"));
            }
            FetchOutcome::Done { .. } => {
                i.bus.publish(EngineEvent::ModelDownloadCompleted {
                    model_id: model.to_owned(),
                    sha256: file.sha256.to_owned(),
                });
            }
            FetchOutcome::AlreadyPresent => {}
        }
        let gpu = self.gpu_context().await?;
        let mut o = sr_providers::LocalOptions::new(ml, file, paths, gpu.plan);
        gpu.apply_with_wait(&mut o, Some(&i.fullscreen));
        let sink = gpu.sink(Arc::new(crate::BusSink(i.bus.clone())));
        let e = sr_providers::LocalEmbedder::load(o.clone(), sink.clone())
            .await
            .map_err(|e| {
                if e.code == ErrorCode::ModelIntegrity {
                    e
                } else {
                    incompatible(e.to_string())
                }
            })?;
        let mut opts = ReembedOptions::default();
        opts.batch = opts.batch.max(e.current_batch());
        let provider = gpu.with_fullscreen_fallback(e, &o, sink, &i.fullscreen);
        let r = self.start_reembed_with(provider, opts).await?;
        tracing::info!(target: "sr_engine::reembed", ?r, "multilingual search model active");
        Ok(())
    }
}

/// Startup: the provider for the default collection's **active** index (sticky variant). A
/// downloaded Standard/High model is loaded from `<data>\models`; if its files are gone or
/// damaged the bundled model is used and [`on_engine_ready`] rebuilds with it (search would
/// otherwise have no usable index).
#[cfg(feature = "local")]
pub(crate) async fn load_active_local(
    cfg: &crate::EngineConfig,
    cat: &Arc<Catalog>,
    bus: &Arc<EventBus>,
    gpu: &crate::gpu::GpuContext,
    wait: &Arc<crate::fullscreen::FullscreenWait>,
) -> Result<Arc<dyn EmbeddingProvider>, Error> {
    use sr_providers::download::ModelStore;
    let active = cat
        .read_async(|c| {
            let coll: Option<[u8; 16]> = c
                .query_row("SELECT id FROM collections WHERE is_default = 1", [], |r| {
                    r.get(0)
                })
                .optional()
                .db()?;
            match coll {
                Some(b) => profiles::active_index(c, CollectionId::from_bytes(b)),
                None => Ok(None),
            }
        })
        .await?;
    let sink = gpu.sink(Arc::new(crate::BusSink(bus.clone())));
    // M3-Q: the multilingual profile (not a tier).
    if let Some((_, p)) = &active
        && p.spec.provider == "local"
        && p.spec.model == crate::multilingual::MULTILINGUAL_MODEL_ID
        && let Some(file) = crate::multilingual::file_for(&p.spec.variant_sha256)
    {
        let ml = &crate::multilingual::MULTILINGUAL;
        let paths = crate::multilingual::paths(&cfg.data_dir, &file);
        let mut o = sr_providers::LocalOptions::new(ml, file, paths, gpu.plan);
        gpu.apply_with_wait(&mut o, Some(wait));
        match sr_providers::LocalEmbedder::load(o.clone(), sink.clone()).await {
            Ok(e) => {
                let _ = gpu.record_load(e.load_report());
                tracing::info!(target: "sr_engine::embed", model = ml.model_id, ep = ?e.active_ep(), "active search model loaded");
                return Ok(gpu.with_fullscreen_fallback(e, &o, sink.clone(), wait));
            }
            Err(e) => {
                tracing::error!(target: "sr_engine::embed", code = %e.code, err = %e, "multilingual search model unusable; falling back to the built-in model and rebuilding");
                repair_needed()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(cfg.data_dir.clone());
            }
        }
    }
    if let Some((_, p)) = &active
        && p.spec.provider == "local"
        && let Some(t) = Tier::parse(p.spec.tier.as_str())
        && t != Tier::Minimum
    {
        let tp = TierProfile::for_tier(t);
        if let Some(file) = [tp.gpu_file, tp.cpu_file]
            .into_iter()
            .find(|f| f.sha256 == p.spec.variant_sha256)
        {
            let store = ModelStore::in_data_dir(&cfg.data_dir);
            let mut o = sr_providers::LocalOptions::new(tp, file, store.paths(tp, &file), gpu.plan);
            gpu.apply_with_wait(&mut o, Some(wait));
            match sr_providers::LocalEmbedder::load(o.clone(), sink.clone()).await {
                Ok(e) => {
                    let _ = gpu.record_load(e.load_report());
                    tracing::info!(target: "sr_engine::embed", model = tp.model_id, ep = ?e.active_ep(), "active search model loaded");
                    return Ok(gpu.with_fullscreen_fallback(e, &o, sink.clone(), wait));
                }
                Err(e) => {
                    tracing::error!(target: "sr_engine::embed", code = %e.code, err = %e, "active search model unusable; falling back to the built-in model and rebuilding");
                    repair_needed()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(cfg.data_dir.clone());
                }
            }
        }
    }
    let mut o = sr_providers::LocalOptions::bundled(&cfg.app_dir, gpu.plan);
    gpu.apply_with_wait(&mut o, Some(wait));
    let e = sr_providers::LocalEmbedder::load(o.clone(), sink.clone()).await?;
    let _ = gpu.record_load(e.load_report());
    tracing::info!(target: "sr_engine::embed", ep = ?e.active_ep(), load_ms = e.load_report().load_ms, "bundled model loaded");
    Ok(gpu.with_fullscreen_fallback(e, &o, sink, wait))
}

fn repair_needed() -> &'static Mutex<HashSet<PathBuf>> {
    static R: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// After `HostState::Ready`: GC dead tables, then resume an unfinished search-model change
/// (download and/or rebuild) or repair a missing active model. Background; never fails start.
pub(crate) fn on_engine_ready(h: &EngineHandle) {
    let outer = h.clone();
    let h = h.clone();
    // Tracked in `Inner.tasks` so drain aborts **and joins** it: it holds an EngineHandle, and an
    // untracked clone kept the engine (and the tantivy writer lock) alive after drain (M2-R O2).
    let task = tokio::spawn(async move {
        let deps = h.reembed_deps();
        match gc_dead_tables(&deps, RETIRED_GC_AFTER).await {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(target: "sr_engine::reembed", tables = n, "dropped retired vector tables")
            }
            Err(e) => {
                tracing::warn!(target: "sr_engine::reembed", err = %e, "vector table GC failed")
            }
        }
        #[cfg(feature = "local")]
        {
            let root = h.inner().layout.root.clone();
            let repair = repair_needed()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&root);
            let intent: Option<UpgradeIntent> = h
                .inner()
                .cat
                .read_async(|c| meta::get(c, UpgradeIntent::META_KEY))
                .await
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok());
            if matches!(h.inner().cfg.embedder, crate::EmbedderSource::Provided(_)) {
                return; // tests / custom providers drive re-embeds themselves
            }
            if let Some(i) = intent {
                tracing::info!(target: "sr_engine::reembed", tier = ?i.tier, "resuming the search model change");
                h.spawn_upgrade(i);
            } else if repair {
                let _ = h.choose_search_model(Tier::Minimum, false).await;
            }
        }
    });
    outer.track_task(task);
}
