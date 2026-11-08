//! Adapters from the real catalog and providers to sr-core's narrow traits (Amendment 4.4,
//! m1-e-core.md §11).
//!
//! * [`CatalogOutbox`]: `OutboxSource` over `index_ops` (+ T2 on acknowledge).
//! * [`CatalogChunks`]: `ChunkSource` over `files`/`chunks`/`chunk_text` (current version only).
//! * [`ProviderEmbedder`]: `sr_core::index::Embedder` over `Arc<dyn EmbeddingProvider>`, with the
//!   query-embedding LRU of §2.7 (moka, 1k entries).
//!
//! The sr-core traits are synchronous; they are called from the writer's blocking task and from
//! the search path (which also runs on a blocking thread, see `search.rs`), never from a runtime
//! worker that serves other requests.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::BoxFuture;
use rusqlite::params;
use sr_catalog::model::IndexOpKind;
use sr_catalog::repo::{chunks, files, folders, outbox};
use sr_catalog::{Catalog, DbResultExt, Store};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{EngineEvent, StoreKind};
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash, VectorIndexId};
use sr_core::index::Embedder;
use sr_core::index::source::{
    Ack, ChunkRecord, ChunkSource, FileSnapshot, HydratedChunk, OpKind, OutboxOp, OutboxSource,
};
use sr_core::ingest::ContentKind;
use sr_providers::{EmbedBatcher, EmbeddingProvider};
use tokio_util::sync::CancellationToken;

use crate::bus::EventBus;
use crate::clock::SharedClock;

/// File id used for `PurgeFolder` ops (the field is ignored for them).
const NIL_FILE: FileId = FileId::from_bytes([0; 16]);

/// Lance table name → vector index id (`vec_<ulid lower-case>`, see `create_vector_index`).
pub fn index_id_of_table(table: &str) -> Option<VectorIndexId> {
    table
        .strip_prefix("vec_")
        .and_then(|s| VectorIndexId::from_str(s).ok())
}

// ------------------------------------------------------------------------------------------------
// Outbox

/// `OutboxSource` over the catalog.
pub struct CatalogOutbox {
    cat: Arc<Catalog>,
    clock: SharedClock,
    bus: Option<Arc<EventBus>>,
    /// Test hook: fail this many acknowledgements (a crash after the store commits, before T2).
    ack_faults: Arc<AtomicU32>,
}

impl CatalogOutbox {
    pub fn new(cat: Arc<Catalog>, clock: SharedClock, bus: Option<Arc<EventBus>>) -> Self {
        Self {
            cat,
            clock,
            bus,
            ack_faults: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Shares a fault counter (tests).
    pub fn with_ack_faults(mut self, f: Arc<AtomicU32>) -> Self {
        self.ack_faults = f;
        self
    }
}

/// Write the stores' own watermarks (the truth) into the catalog cache (monotonic). Startup does
/// this after replay so `finalize_applied` sees what the stores really hold, also when the daemon
/// died between a store commit and its acknowledgement.
pub fn sync_watermark_cache(
    c: &rusqlite::Connection,
    wms: &[sr_core::index::Watermark],
) -> Result<()> {
    for w in wms {
        let (store, scope) = match w.store {
            StoreKind::Lance => match index_id_of_table(&w.scope_id) {
                Some(id) => (Store::Lance, id.to_bytes()),
                None => continue,
            },
            StoreKind::Tantivy => (Store::Tantivy, w.collection_id.to_bytes()),
        };
        outbox::set_watermark(c, store, &scope, w.seq)?;
    }
    Ok(())
}

impl OutboxSource for CatalogOutbox {
    fn ops_after(&self, after: i64, limit: usize) -> Result<Vec<OutboxOp>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.cat.read(move |c| {
            // Raw ops across every collection, ascending (the writer coalesces per file itself
            // and reads each file's current snapshot, so raw rows are what it needs).
            let mut st = c
                .prepare_cached(
                    "SELECT seq, collection_id, folder_id, file_id, op, file_version FROM index_ops
                     WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                )
                .db()?;
            let rows = st
                .query_map(params![after, limit], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, [u8; 16]>(1)?,
                        r.get::<_, Option<[u8; 16]>>(2)?,
                        r.get::<_, Option<[u8; 16]>>(3)?,
                        r.get::<_, IndexOpKind>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .db()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .db()?;
            let mut out = Vec::with_capacity(rows.len());
            for (seq, coll, folder, file, op, ver) in rows {
                let kind = match (op, folder) {
                    (IndexOpKind::Upsert, _) => OpKind::Upsert,
                    (IndexOpKind::Delete, _) => OpKind::Delete,
                    (IndexOpKind::PurgeFolder, Some(f)) => {
                        OpKind::PurgeFolder(FolderId::from_bytes(f))
                    }
                    (IndexOpKind::PurgeFolder, None) => continue, // CHECK forbids it
                };
                if matches!(kind, OpKind::Upsert | OpKind::Delete) && file.is_none() {
                    continue; // CHECK forbids it
                }
                out.push(OutboxOp {
                    seq,
                    collection_id: CollectionId::from_bytes(coll),
                    file_id: file.map_or(NIL_FILE, FileId::from_bytes),
                    kind,
                    file_version: u64::try_from(ver).unwrap_or(0),
                });
            }
            Ok(out)
        })
    }

    fn acknowledge(&self, ack: &Ack) -> Result<()> {
        if self
            .ack_faults
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::new(ErrorCode::Internal).with_detail("injected crash before T2"));
        }
        let ack = ack.clone();
        let now = self.clock.now_ms();
        let ack2 = ack.clone();
        self.cat.write_blocking(move |c| {
            sr_catalog::atomic(c, |c| {
                for w in &ack2.watermarks {
                    let (store, scope) = match w.store {
                        StoreKind::Lance => (
                            Store::Lance,
                            index_id_of_table(&w.scope_id)
                                .ok_or_else(|| {
                                    Error::new(ErrorCode::Internal)
                                        .with_detail(format!("unknown lance table {}", w.scope_id))
                                })?
                                .to_bytes(),
                        ),
                        StoreKind::Tantivy => (Store::Tantivy, w.collection_id.to_bytes()),
                    };
                    outbox::set_watermark(c, store, &scope, w.seq)?;
                }
                for (file, version) in &ack2.indexed {
                    let v = i64::try_from(*version).unwrap_or(i64::MAX);
                    chunks::finalize_file(c, *file, v, now)?;
                }
                outbox::prune(c)?;
                Ok(())
            })
        })?;
        if let Some(bus) = &self.bus {
            for w in &ack.watermarks {
                bus.publish(EngineEvent::IndexCommitted {
                    store: w.store,
                    collection_id: w.collection_id,
                    seq: u64::try_from(w.seq).unwrap_or(0),
                });
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------
// Chunks

/// `ChunkSource` over the catalog.
pub struct CatalogChunks {
    cat: Arc<Catalog>,
}

impl CatalogChunks {
    pub fn new(cat: Arc<Catalog>) -> Self {
        Self { cat }
    }
}

fn not_found_to_none<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl ChunkSource for CatalogChunks {
    fn file_snapshot(&self, file_id: FileId) -> Result<Option<FileSnapshot>> {
        self.cat.read(move |c| {
            let Some(f) = not_found_to_none(files::file(c, file_id))? else {
                return Ok(None);
            };
            let Some(fo) = not_found_to_none(folders::folder(c, f.folder_id))? else {
                return Ok(None);
            };
            let recs = chunks::file_chunks(c, file_id)?;
            let chunks = recs
                .into_iter()
                .filter(|r| r.file_version == f.version)
                .map(|r| ChunkRecord {
                    chunk_id: r.chunk_id,
                    text_hash: r.text_hash,
                    ord: r.ord,
                })
                .collect();
            Ok(Some(FileSnapshot {
                file_id,
                folder_id: f.folder_id,
                collection_id: fo.collection_id,
                version: u64::try_from(f.version).unwrap_or(0),
                rel_path: f.rel_path,
                lang: ContentKind::from_extension(&f.ext).lang().to_string(),
                chunks,
            }))
        })
    }

    fn texts(&self, hashes: &[TextHash]) -> Result<HashMap<TextHash, String>> {
        let hashes = hashes.to_vec();
        self.cat.read(move |c| {
            let mut out = HashMap::with_capacity(hashes.len());
            for h in hashes {
                if let Some(t) = chunks::text(c, &h)? {
                    out.insert(h, t);
                }
            }
            Ok(out)
        })
    }

    fn hydrate(&self, chunk_ids: &[ChunkId]) -> Result<HashMap<ChunkId, HydratedChunk>> {
        let ids = chunk_ids.to_vec();
        self.cat.read(move |c| {
            let rows = chunks::hydrate(c, &ids)?;
            let folders_needed: BTreeSet<FolderId> = rows.iter().map(|h| h.folder_id).collect();
            let mut coll: HashMap<FolderId, CollectionId> = HashMap::new();
            for f in folders_needed {
                if let Some(fo) = not_found_to_none(folders::folder(c, f))? {
                    coll.insert(f, fo.collection_id);
                }
            }
            Ok(rows
                .into_iter()
                .filter_map(|h| {
                    let collection_id = *coll.get(&h.folder_id)?;
                    Some((
                        h.chunk_id,
                        HydratedChunk {
                            chunk_id: h.chunk_id,
                            file_id: h.file_id,
                            folder_id: h.folder_id,
                            collection_id,
                            rel_path: h.rel_path,
                            line_start: h.line_start,
                            line_end: h.line_end,
                            text: h.text,
                            text_hash: h.text_hash,
                        },
                    ))
                })
                .collect())
        })
    }
}

// ------------------------------------------------------------------------------------------------
// Embedder

/// `sr_core::index::Embedder` over a provider. `fingerprint` is the catalog's
/// `embedding_profiles.fingerprint` of the index this embedder serves.
pub struct ProviderEmbedder {
    provider: Arc<dyn EmbeddingProvider>,
    /// Document embedding streams through the profile's EmbedBatcher (Amendment 4.25): batching,
    /// calls in flight, backpressure, and queued texts dropped at a writer stop.
    batcher: Option<Arc<EmbedBatcher>>,
    fingerprint: String,
    queries: moka::sync::Cache<String, Arc<Vec<f32>>>,
    cancel: CancellationToken,
}

impl ProviderEmbedder {
    /// Query-embedding LRU size (§2.7).
    pub const QUERY_CACHE: u64 = 1000;

    pub fn new(
        provider: Arc<dyn EmbeddingProvider>,
        fingerprint: String,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            provider,
            batcher: None,
            fingerprint,
            queries: moka::sync::Cache::new(Self::QUERY_CACHE),
            cancel,
        }
    }

    /// Send document batches through `batcher` (queries stay direct: one short text, 4.27).
    pub fn with_batcher(mut self, batcher: Option<Arc<EmbedBatcher>>) -> Self {
        self.batcher = batcher;
        self
    }

    pub fn provider(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.provider
    }
}

impl Embedder for ProviderEmbedder {
    fn dims(&self) -> usize {
        self.provider.dims() as usize
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn embed_documents<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        self.embed_documents_until(texts, &self.cancel)
    }

    fn embed_documents_until<'a>(
        &'a self,
        texts: &'a [&'a str],
        stop: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let Some(b) = &self.batcher else {
                // No batcher: the provider checks `stop` between its own slices.
                return tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                    () = stop.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                    r = self.provider.embed_documents(texts, stop) => r,
                };
            };
            let owned: Vec<String> = texts.iter().map(|t| (*t).to_string()).collect();
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                r = b.embed(owned, stop) => r,
            }
        })
    }

    fn no_match_cutoff(&self) -> Option<sr_core::query::NoMatchCutoff> {
        sr_core::query::NoMatchCutoff::for_model(self.provider.model_id())
    }

    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(async move {
            if let Some(v) = self.queries.get(text) {
                return Ok(v.as_ref().clone());
            }
            let v = self.provider.embed_query(text, &self.cancel).await?;
            self.queries.insert(text.to_string(), Arc::new(v.clone()));
            Ok(v)
        })
    }
}

/// `sr_core::query::Reranker` over an `sr_providers::Reranker` (M3-Q: the local cross-encoder).
/// Cancelled only at engine shutdown; a failure degrades that query to the RRF order (sr-core).
pub(crate) struct ProviderReranker {
    inner: Arc<dyn sr_providers::Reranker>,
    cancel: CancellationToken,
}

impl ProviderReranker {
    pub(crate) fn new(inner: Arc<dyn sr_providers::Reranker>, cancel: CancellationToken) -> Self {
        Self { inner, cancel }
    }
}

impl sr_core::query::Reranker for ProviderReranker {
    fn rerank<'a>(
        &'a self,
        query: &'a str,
        docs: &'a [&'a str],
    ) -> BoxFuture<'a, sr_common::Result<Vec<f32>>> {
        self.inner.rerank(query, docs, &self.cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names_map_back_to_index_ids() {
        let id = VectorIndexId::new();
        let t = format!("vec_{}", id.to_string().to_ascii_lowercase());
        assert_eq!(index_id_of_table(&t), Some(id));
        assert_eq!(index_id_of_table("other"), None);
        assert_eq!(index_id_of_table("vec_nope"), None);
    }
}
