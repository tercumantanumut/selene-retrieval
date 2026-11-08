//! In-memory fakes for tests and benchmarks: a catalog implementing [`ChunkSource`] +
//! [`OutboxSource`] with the docs/02 §2.4 semantics (versioned files, refcounted chunk text,
//! a monotonic outbox pruned by watermarks), and deterministic embedders.
//!
//! Not used in production; sr-engine wires sr-catalog and sr-providers instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use futures::future::BoxFuture;
use sr_common::Result;
use sr_common::events::StoreKind;
use sr_common::ids::{ChunkCfgFp, ChunkId, CollectionId, ContentHash, FileId, FolderId, TextHash};

use crate::index::Embedder;
use crate::index::lexical::code_tokens;
use crate::index::source::{
    Ack, ChunkRecord, ChunkSource, FileSnapshot, HydratedChunk, OpKind, OutboxOp, OutboxSource,
};

#[derive(Debug, Clone)]
struct FileRow {
    folder_id: FolderId,
    collection_id: CollectionId,
    version: u64,
    rel_path: String,
    lang: String,
    chunks: Vec<(ChunkRecord, u32, u32)>,
}

#[derive(Debug, Default)]
struct State {
    next_seq: i64,
    ops: BTreeMap<i64, OutboxOp>,
    files: HashMap<FileId, FileRow>,
    texts: HashMap<TextHash, (String, usize)>,
    watermarks: HashMap<(bool, CollectionId), i64>,
    indexed: HashMap<FileId, u64>,
    acks: usize,
}

/// The fake catalog.
#[derive(Debug, Default)]
pub struct MemCatalog {
    st: Mutex<State>,
    /// `hydrate` calls and the chunk ids they looked up (docs/06 issue 3: stale-hit cost).
    pub hydrate_calls: std::sync::atomic::AtomicUsize,
    pub hydrate_ids: std::sync::atomic::AtomicUsize,
}

/// Paragraph splitter used by the fake ingest (`\n\n`-separated chunks).
pub fn split_paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_lance(s: StoreKind) -> bool {
    matches!(s, StoreKind::Lance)
}

fn cfg_fp() -> ChunkCfgFp {
    ChunkCfgFp::compute("testkit", "{\"split\":\"paragraph\"}", "none", "1")
}

impl MemCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn push_op(
        st: &mut State,
        collection_id: CollectionId,
        file_id: FileId,
        kind: OpKind,
        v: u64,
    ) -> i64 {
        st.next_seq += 1;
        let seq = st.next_seq;
        st.ops.insert(
            seq,
            OutboxOp {
                seq,
                collection_id,
                file_id,
                kind,
                file_version: v,
            },
        );
        seq
    }

    fn release_texts(st: &mut State, row: &FileRow) {
        for (c, _, _) in &row.chunks {
            if let Some(e) = st.texts.get_mut(&c.text_hash) {
                e.1 = e.1.saturating_sub(1);
                if e.1 == 0 {
                    st.texts.remove(&c.text_hash);
                }
            }
        }
    }

    /// T1 for one file: new version with `chunks` (texts), outbox `upsert`. Returns the op seq.
    pub fn upsert_chunks(
        &self,
        file_id: FileId,
        folder_id: FolderId,
        collection_id: CollectionId,
        rel_path: &str,
        chunks: &[String],
    ) -> i64 {
        let mut st = self.lock();
        let version = st.files.get(&file_id).map_or(1, |f| f.version + 1);
        if let Some(old) = st.files.remove(&file_id) {
            Self::release_texts(&mut st, &old);
        }
        let joined = chunks.join("\n\n");
        let content = ContentHash::of_bytes(format!("{version}\u{0}{joined}").as_bytes());
        let mut rows = Vec::new();
        let mut line = 1u32;
        for (ord, text) in chunks.iter().enumerate() {
            let ord = u32::try_from(ord).unwrap_or(u32::MAX);
            let h = TextHash::of_text(text);
            let lines = u32::try_from(text.lines().count().max(1)).unwrap_or(1);
            rows.push((
                ChunkRecord {
                    chunk_id: ChunkId::derive(file_id, &content, &cfg_fp(), ord),
                    text_hash: h,
                    ord,
                },
                line,
                line + lines - 1,
            ));
            line += lines + 1;
            let e = st.texts.entry(h).or_insert_with(|| (text.clone(), 0));
            e.1 += 1;
        }
        let lang = rel_path.rsplit('.').next().unwrap_or("text").to_string();
        st.files.insert(
            file_id,
            FileRow {
                folder_id,
                collection_id,
                version,
                rel_path: rel_path.to_string(),
                lang,
                chunks: rows,
            },
        );
        Self::push_op(&mut st, collection_id, file_id, OpKind::Upsert, version)
    }

    /// Convenience: split `text` into paragraphs and upsert.
    pub fn upsert_text(
        &self,
        file_id: FileId,
        folder_id: FolderId,
        collection_id: CollectionId,
        rel_path: &str,
        text: &str,
    ) -> i64 {
        self.upsert_chunks(
            file_id,
            folder_id,
            collection_id,
            rel_path,
            &split_paragraphs(text),
        )
    }

    /// File deleted: rows gone, outbox `delete`.
    pub fn delete_file(&self, file_id: FileId) -> Option<i64> {
        let mut st = self.lock();
        let row = st.files.remove(&file_id)?;
        Self::release_texts(&mut st, &row);
        st.indexed.remove(&file_id);
        Some(Self::push_op(
            &mut st,
            row.collection_id,
            file_id,
            OpKind::Delete,
            row.version,
        ))
    }

    /// Rename: catalog-only, **no outbox op** (file_id is stable, derived stores hold no identity
    /// by path).
    pub fn rename_file(&self, file_id: FileId, new_rel_path: &str) -> bool {
        let mut st = self.lock();
        match st.files.get_mut(&file_id) {
            Some(f) => {
                f.rel_path = new_rel_path.to_string();
                true
            }
            None => false,
        }
    }

    /// Remove a folder: its files and their text are deleted in the same "transaction", plus one
    /// `purge_folder` op.
    pub fn purge_folder(&self, collection_id: CollectionId, folder_id: FolderId) -> i64 {
        let mut st = self.lock();
        let ids: Vec<FileId> = st
            .files
            .iter()
            .filter(|(_, f)| f.folder_id == folder_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(row) = st.files.remove(&id) {
                Self::release_texts(&mut st, &row);
            }
            st.indexed.remove(&id);
        }
        Self::push_op(
            &mut st,
            collection_id,
            FileId::from_bytes([0; 16]),
            OpKind::PurgeFolder(folder_id),
            0,
        )
    }

    /// Chunk ids of every current file in `collection_id`.
    pub fn current_chunk_ids(&self, collection_id: CollectionId) -> HashSet<ChunkId> {
        self.lock()
            .files
            .values()
            .filter(|f| f.collection_id == collection_id)
            .flat_map(|f| f.chunks.iter().map(|c| c.0.chunk_id))
            .collect()
    }

    pub fn chunk_ids_of(&self, file_id: FileId) -> Vec<ChunkId> {
        self.lock()
            .files
            .get(&file_id)
            .map(|f| f.chunks.iter().map(|c| c.0.chunk_id).collect())
            .unwrap_or_default()
    }

    pub fn pending_ops(&self) -> usize {
        self.lock().ops.len()
    }

    pub fn last_seq(&self) -> i64 {
        self.lock().next_seq
    }

    pub fn acks(&self) -> usize {
        self.lock().acks
    }

    pub fn is_indexed(&self, file_id: FileId) -> bool {
        let st = self.lock();
        match (st.indexed.get(&file_id), st.files.get(&file_id)) {
            (Some(v), Some(f)) => *v == f.version,
            _ => false,
        }
    }

    pub fn cached_watermark(&self, store: StoreKind, c: CollectionId) -> Option<i64> {
        self.lock().watermarks.get(&(is_lance(store), c)).copied()
    }

    pub fn text_rows(&self) -> usize {
        self.lock().texts.len()
    }
}

impl ChunkSource for MemCatalog {
    fn file_snapshot(&self, file_id: FileId) -> Result<Option<FileSnapshot>> {
        Ok(self.lock().files.get(&file_id).map(|f| FileSnapshot {
            file_id,
            folder_id: f.folder_id,
            collection_id: f.collection_id,
            version: f.version,
            rel_path: f.rel_path.clone(),
            lang: f.lang.clone(),
            chunks: f.chunks.iter().map(|c| c.0.clone()).collect(),
        }))
    }

    fn texts(&self, hashes: &[TextHash]) -> Result<HashMap<TextHash, String>> {
        let st = self.lock();
        Ok(hashes
            .iter()
            .filter_map(|h| st.texts.get(h).map(|t| (*h, t.0.clone())))
            .collect())
    }

    fn hydrate(&self, chunk_ids: &[ChunkId]) -> Result<HashMap<ChunkId, HydratedChunk>> {
        use std::sync::atomic::Ordering::Relaxed;
        self.hydrate_calls.fetch_add(1, Relaxed);
        self.hydrate_ids.fetch_add(chunk_ids.len(), Relaxed);
        let st = self.lock();
        let want: HashSet<&ChunkId> = chunk_ids.iter().collect();
        let mut out = HashMap::new();
        for (fid, f) in &st.files {
            for (c, ls, le) in &f.chunks {
                if want.contains(&c.chunk_id)
                    && let Some(t) = st.texts.get(&c.text_hash)
                {
                    out.insert(
                        c.chunk_id,
                        HydratedChunk {
                            chunk_id: c.chunk_id,
                            file_id: *fid,
                            folder_id: f.folder_id,
                            collection_id: f.collection_id,
                            rel_path: f.rel_path.clone(),
                            line_start: *ls,
                            line_end: *le,
                            text: t.0.clone(),
                            text_hash: c.text_hash,
                        },
                    );
                }
            }
        }
        Ok(out)
    }
}

impl OutboxSource for MemCatalog {
    fn ops_after(&self, after: i64, limit: usize) -> Result<Vec<OutboxOp>> {
        Ok(self
            .lock()
            .ops
            .range(after + 1..)
            .take(limit)
            .map(|(_, o)| o.clone())
            .collect())
    }

    fn acknowledge(&self, ack: &Ack) -> Result<()> {
        let mut st = self.lock();
        st.acks += 1;
        for w in &ack.watermarks {
            let e = st
                .watermarks
                .entry((is_lance(w.store), w.collection_id))
                .or_insert(0);
            *e = (*e).max(w.seq);
        }
        for (f, v) in &ack.indexed {
            if st.files.get(f).is_some_and(|r| r.version == *v) {
                st.indexed.insert(*f, *v);
            }
        }
        // Prune ops covered by both of their collection's watermarks.
        let wm = st.watermarks.clone();
        st.ops.retain(|seq, op| {
            let l = wm.get(&(true, op.collection_id)).copied().unwrap_or(0);
            let t = wm.get(&(false, op.collection_id)).copied().unwrap_or(0);
            *seq > l.min(t)
        });
        Ok(())
    }
}

/// Feature-hashing bag-of-words embedder: texts sharing words have high cosine similarity, so
/// dense search is meaningful in tests without a model.
#[derive(Debug, Clone)]
pub struct BagOfWordsEmbedder {
    dims: usize,
    fingerprint: String,
}

impl BagOfWordsEmbedder {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            fingerprint: format!("test-bow-{dims}"),
        }
    }

    pub fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        for t in code_tokens(text) {
            let h = ContentHash::of_bytes(t.as_bytes());
            let b = h.as_bytes();
            let idx = (u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                % self.dims as u64) as usize;
            v[idx] += if b[8] & 1 == 0 { 1.0 } else { -1.0 };
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n == 0.0 {
            v[self.dims - 1] = 1.0;
        } else {
            v.iter_mut().for_each(|x| *x /= n);
        }
        v
    }
}

impl Embedder for BagOfWordsEmbedder {
    fn dims(&self) -> usize {
        self.dims
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    fn embed_documents<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(texts.iter().map(|t| self.embed(t)).collect()) })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>>> {
        Box::pin(async move { Ok(self.embed(text)) })
    }
}
