//! Narrow catalog traits used by the IndexWriter and the searcher.
//!
//! sr-catalog (M1-A) is written concurrently; sr-engine adapts it to these traits. They mirror
//! docs/02 §2.4: `index_ops` (the outbox), `files` + `chunks` (current version per file),
//! `chunk_text` (zstd text by `text_hash`), `store_watermarks` (cache).
//!
//! The methods are synchronous because the catalog is rusqlite behind a read pool; they must be
//! fast (indexed lookups) since the writer and the query path call them inline.

use std::collections::HashMap;

use sr_common::Result;
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash};

use super::Watermark;

/// `index_ops.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// Make the stores hold exactly the file's current chunks (or nothing if it is gone).
    Upsert,
    /// Remove the file from the stores.
    Delete,
    /// Remove every row of a folder, then compact/prune/merge so the data leaves the disk (M9).
    PurgeFolder(FolderId),
}

/// One outbox row (`index_ops`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxOp {
    /// Monotonic across the whole catalog.
    pub seq: i64,
    pub collection_id: CollectionId,
    /// Ignored for `PurgeFolder`.
    pub file_id: FileId,
    pub kind: OpKind,
    pub file_version: u64,
}

/// One chunk row of a file's current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRecord {
    pub chunk_id: ChunkId,
    pub text_hash: TextHash,
    pub ord: u32,
}

/// A file's current version as the catalog knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    pub version: u64,
    /// Root-relative path (lexical ranking signal).
    pub rel_path: String,
    /// Lexical `lang` value (`ContentKind::lang`).
    pub lang: String,
    pub chunks: Vec<ChunkRecord>,
}

/// What a query hit needs from the catalog. Only chunks of a file's **current** version are
/// returned, which is the query-time validation of D3: hits from a store that lags the catalog
/// (crash, replay pending) are dropped instead of shown with wrong text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedChunk {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    pub rel_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub text: String,
    /// `blake3(NFC(text))`: equal for identical text in different files (query-time duplicate
    /// collapsing, docs/06 change 4).
    pub text_hash: TextHash,
}

/// Chunk/file reads.
pub trait ChunkSource: Send + Sync {
    /// The file's current version, or `None` if it no longer exists (deleted, folder purged).
    fn file_snapshot(&self, file_id: FileId) -> Result<Option<FileSnapshot>>;
    /// Chunk text by hash (`chunk_text`, decompressed). Missing hashes are left out.
    fn texts(&self, hashes: &[TextHash]) -> Result<HashMap<TextHash, String>>;
    /// Hydrate hits; chunk ids that are not part of their file's current version are left out.
    fn hydrate(&self, chunk_ids: &[ChunkId]) -> Result<HashMap<ChunkId, HydratedChunk>>;
}

/// Acknowledgement after a batch reached both stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ack {
    /// New per-scope watermarks (write to `store_watermarks`, prune ops covered by both of their
    /// collection's watermarks).
    pub watermarks: Vec<Watermark>,
    /// Files whose op reached both stores: `files.state = indexed` if `version` is still current.
    pub indexed: Vec<(FileId, u64)>,
}

/// The outbox.
pub trait OutboxSource: Send + Sync {
    /// Ops with `seq > after`, ascending, at most `limit`.
    fn ops_after(&self, after: i64, limit: usize) -> Result<Vec<OutboxOp>>;
    /// Record progress (T2 in docs/02 §2.3).
    fn acknowledge(&self, ack: &Ack) -> Result<()>;
}
