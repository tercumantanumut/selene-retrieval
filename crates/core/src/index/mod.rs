//! Derived stores and the outbox applier (docs/02 §2.3, D3, D6).
//!
//! * [`lance`]: vectors + filter columns, one table per vector index (embedding profile).
//! * [`lexical`]: tantivy BM25, one index per collection.
//! * [`writer`]: the single IndexWriter (outbox apply, replay, purge).
//! * [`source`]: the narrow catalog traits the writer and query read through.
//! * [`filter`]: the one typed Lance filter builder (no string-built filters, W3).
//! * [`datadir`]: the plain-local-path rule for store roots (Amendment 3.12).

pub mod datadir;
pub mod filter;
pub mod lance;
pub mod lexical;
pub mod source;
pub mod writer;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::events::StoreKind;
use sr_common::ids::CollectionId;

pub use lance::{AnnConfig, VectorStore, VectorTable};
pub use lexical::{LexConfig, LexicalIndex};
pub use writer::{IndexWriter, WriterConfig};

/// Watermark of one store scope: every outbox op with `seq <= seq` that belongs to the scope has
/// been applied. The truth lives in the store's own commit (Lance sentinel row, tantivy commit
/// payload); the catalog's `store_watermarks` is a cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub store: StoreKind,
    /// Lance: the table name; tantivy: the collection id.
    pub scope_id: String,
    pub collection_id: CollectionId,
    pub seq: i64,
}

/// Document/query embedder for one embedding profile (adapted from
/// `sr_providers::EmbeddingProvider` by sr-engine). Object-safe on purpose.
pub trait Embedder: Send + Sync {
    /// Output dimension; every vector is checked against it.
    fn dims(&self) -> usize;
    /// `embedding_profiles.fingerprint` of the profile this embedder implements.
    fn fingerprint(&self) -> &str;
    fn embed_documents<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>>;
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>>>;
    /// The calibrated "no good match" floors of this model (R-UX18); `None` = uncalibrated.
    fn no_match_cutoff(&self) -> Option<crate::query::NoMatchCutoff> {
        None
    }
    /// [`Self::embed_documents`] that returns `Cancelled` as soon as `stop` fires (the writer's
    /// batch-boundary stop, Amendment 4.25). The default races the call against `stop`;
    /// implementations that queue work (the EmbedBatcher) override it to drop queued texts too.
    fn embed_documents_until<'a>(
        &'a self,
        texts: &'a [&'a str],
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                () = stop.cancelled() => Err(sr_common::error::Error::new(
                    sr_common::error::ErrorCode::Cancelled,
                )),
                r = self.embed_documents(texts) => r,
            }
        })
    }
}

/// The stores serving one collection: its active vector index's Lance table + embedder, and its
/// tantivy index. Queries embed with **this** embedder (the active index's profile, §2.4).
#[derive(Clone)]
pub struct CollectionStores {
    pub collection_id: CollectionId,
    pub vectors: Arc<VectorTable>,
    pub lexical: Arc<LexicalIndex>,
    pub embedder: Arc<dyn Embedder>,
}

impl std::fmt::Debug for CollectionStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionStores")
            .field("collection_id", &self.collection_id)
            .field("table", &self.vectors.name())
            .finish_non_exhaustive()
    }
}

/// Collection → stores, shared by the writer and the searcher. The engine registers a collection
/// after opening its stores and swaps the entry atomically on `index.swapped` (M2).
#[derive(Debug, Default)]
pub struct StoreRegistry {
    inner: RwLock<HashMap<CollectionId, CollectionStores>>,
}

impl StoreRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, stores: CollectionStores) {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        g.insert(stores.collection_id, stores);
    }

    pub fn remove(&self, id: CollectionId) -> Option<CollectionStores> {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        g.remove(&id)
    }

    pub fn get(&self, id: CollectionId) -> Option<CollectionStores> {
        let g = self.inner.read().unwrap_or_else(|p| p.into_inner());
        g.get(&id).cloned()
    }

    pub fn all(&self) -> Vec<CollectionStores> {
        let g = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<_> = g.values().cloned().collect();
        v.sort_by_key(|s| s.collection_id);
        v
    }
}
