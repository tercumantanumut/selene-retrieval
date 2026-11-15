//! Background re-embed + atomic swap (M2-G, Amendment 1, docs/02 §2.8). The M0-E simulation
//! (`spikes/gpu-tier/src/reembed.rs`) turned into integration tests against the real catalog,
//! Lance and tantivy stores, the real IndexWriter and outbox, plus `kill -9` of a child process
//! at random points.
//!
//! Models: deterministic `HashEmbedder`s ("old" 384 dims = the active index, "new" 768 dims = the
//! target), so every stored vector can be checked exactly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use sr_catalog::Catalog;
use sr_catalog::model::{FolderKind, IndexState};
use sr_catalog::repo::chunks::{FileChunks, NewChunk};
use sr_catalog::repo::files::FileObservation;
use sr_catalog::repo::folders::NewFolder;
use sr_catalog::repo::{chunks, files, folders, profiles};
use sr_common::error::ErrorCode;
use sr_common::events::EngineEvent;
use sr_common::ids::{ChunkCfgFp, ChunkId, CollectionId, ContentHash, FileId, FolderId};
use sr_core::index::{
    AnnConfig, CollectionStores, IndexWriter, LexConfig, LexicalIndex, StoreRegistry, VectorStore,
    WriterConfig,
};
use sr_engine::EventBus;
use sr_engine::adapters::{CatalogChunks, CatalogOutbox, ProviderEmbedder};
use sr_engine::reembed::{self, ReembedDeps, ReembedFault, ReembedOptions, ReembedOutcome};
use sr_providers::testing::HashEmbedder;
use sr_providers::{EmbedKind, EmbeddingProfile, EmbeddingProvider};
use tokio_util::sync::CancellationToken;

const OLD: &str = "old-model";
const NEW: &str = "new-model";

fn old_provider() -> Arc<HashEmbedder> {
    Arc::new(HashEmbedder::new(OLD, 384))
}
fn new_provider() -> Arc<HashEmbedder> {
    Arc::new(HashEmbedder::new(NEW, 768))
}

/// Counts document texts; optionally sleeps per call (kill tests) or waits on a gate.
struct Counting {
    inner: Arc<HashEmbedder>,
    texts: AtomicUsize,
    calls: AtomicUsize,
    per_call: Duration,
    /// Return wrong-length vectors from this call number on (1-based; 0 = never).
    wrong_dims_from_call: usize,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Counting {
    fn new(inner: Arc<HashEmbedder>) -> Self {
        Self {
            inner,
            texts: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            per_call: Duration::ZERO,
            wrong_dims_from_call: 0,
            gate: None,
        }
    }
}

impl EmbeddingProvider for Counting {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }
    fn max_batch(&self) -> usize {
        16
    }
    fn embed_batch<'a>(
        &'a self,
        kind: EmbedKind,
        texts: &'a [&'a str],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(g) = &self.gate
                && n > 1
            {
                g.acquire().await.unwrap().forget();
            }
            if !self.per_call.is_zero() {
                tokio::time::sleep(self.per_call).await;
            }
            if kind == EmbedKind::Doc {
                self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            }
            let mut v = self.inner.embed_batch(kind, texts, cancel).await?;
            if self.wrong_dims_from_call > 0 && n >= self.wrong_dims_from_call {
                for x in &mut v {
                    x.push(0.0);
                }
            }
            Ok(v)
        })
    }
}

// --------------------------------------------------------------------------------- the world

struct World {
    root: PathBuf,
    cat: Arc<Catalog>,
    vectors: VectorStore,
    registry: Arc<StoreRegistry>,
    writer: Arc<IndexWriter>,
    bus: Arc<EventBus>,
    coll: CollectionId,
    folder: FolderId,
}

fn fp_hex(p: &EmbeddingProfile) -> String {
    let fp = sr_engine::profile_spec(p).fingerprint();
    blake3::Hash::from_bytes(fp).to_hex().to_string()
}

impl World {
    /// Open (or create) a data dir: catalog, default collection on the old model, one folder,
    /// stores of the ACTIVE index (whatever model it uses now), writer replayed.
    async fn open(root: &Path) -> Self {
        std::fs::create_dir_all(root.join("corpus")).unwrap();
        let cat = Arc::new(Catalog::open(&root.join("catalog.db")).unwrap());
        let old = old_provider();
        let spec = sr_engine::profile_spec(old.profile());
        let coll = cat
            .write(move |c| {
                let pid = profiles::ensure_profile(c, &spec, 1)?;
                profiles::ensure_default_collection(c, pid, 1)
            })
            .await
            .unwrap();
        let corpus = root.join("corpus");
        let folder = cat
            .write(move |c| {
                if let Some(f) = folders::folders(c)?.first() {
                    return Ok(f.id);
                }
                folders::insert_folder(
                    c,
                    &NewFolder {
                        collection_id: coll,
                        kind: FolderKind::Indexed,
                        root_path: corpus,
                        volume_serial: None,
                        is_primary: true,
                        settings: Default::default(),
                    },
                    1,
                )
            })
            .await
            .unwrap();
        let vectors = VectorStore::open(&root.join("lance")).await.unwrap();
        let registry = StoreRegistry::new();
        let (idx, prof) = cat
            .read(move |c| profiles::active_index(c, coll))
            .unwrap()
            .unwrap();
        let provider: Arc<dyn EmbeddingProvider> = match prof.spec.model.as_str() {
            OLD => old_provider(),
            _ => new_provider(),
        };
        let table = vectors
            .open_table(&idx.lance_table, idx.dims as usize, AnnConfig::default())
            .await
            .unwrap();
        let tantivy = root.join("tantivy");
        let lex = LexicalIndex::open(&tantivy, coll, LexConfig::default()).unwrap();
        registry.register(CollectionStores {
            collection_id: coll,
            vectors: Arc::new(table),
            lexical: Arc::new(lex),
            embedder: Arc::new(ProviderEmbedder::new(
                provider.clone(),
                fp_hex(provider.profile()),
                CancellationToken::new(),
            )),
        });
        let clock = sr_engine::clock::system();
        let writer = Arc::new(IndexWriter::new(
            Arc::new(CatalogOutbox::new(cat.clone(), clock, None)),
            Arc::new(CatalogChunks::new(cat.clone())),
            registry.clone(),
            WriterConfig {
                maintain: false,
                ..WriterConfig::default()
            },
        ));
        let w = Self {
            root: root.to_path_buf(),
            cat,
            vectors,
            registry,
            writer,
            bus: Arc::new(EventBus::default()),
            coll,
            folder,
        };
        w.drain().await;
        w
    }

    fn deps(&self) -> ReembedDeps {
        ReembedDeps {
            cat: self.cat.clone(),
            vectors: self.vectors.clone(),
            registry: self.registry.clone(),
            chunks: Arc::new(CatalogChunks::new(self.cat.clone())),
            bus: self.bus.clone(),
            clock: sr_engine::clock::system(),
            tantivy_root: self.root.join("tantivy"),
            embed_cancel: CancellationToken::new(),
        }
    }

    async fn drain(&self) {
        let w = self.writer.clone();
        // The writer's catalog adapters are synchronous: run it on a blocking thread.
        let h = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || h.block_on(w.drain()))
            .await
            .unwrap()
            .unwrap();
    }

    /// T1 for `name` with `n` chunks; `salt` changes the text (an edit → new chunk ids).
    async fn put_file(&self, name: &str, n: u32, salt: u32) -> FileId {
        let folder = self.folder;
        let name = name.to_owned();
        self.cat
            .write(move |c| {
                let scan = files::begin_scan(c, folder, 2)?;
                let obs = files::observe(
                    c,
                    folder,
                    scan,
                    &FileObservation {
                        rel_path: format!("{name}.txt"),
                        path_key: sr_common::sandbox::path_key(&format!("{name}.txt")),
                        ext: "txt".into(),
                        volume_serial: None,
                        ntfs_file_id: None,
                        size: 100 + u64::from(salt),
                        mtime_ns: i64::from(salt),
                    },
                    2,
                )?;
                let chunks: Vec<NewChunk> = (0..n)
                    .map(|i| NewChunk {
                        ord: i,
                        byte_start: u64::from(i) * 100,
                        byte_end: u64::from(i) * 100 + 99,
                        line_start: i + 1,
                        line_end: i + 1,
                        tokens: 20,
                        text: format!("{name} paragraph {i} revision {salt}: local search text"),
                    })
                    .collect();
                chunks::write_file_chunks(
                    c,
                    &FileChunks {
                        file_id: obs.file_id,
                        content_hash: ContentHash::of_bytes(format!("{name}{salt}").as_bytes()),
                        chunk_cfg_fp: ChunkCfgFp::from_bytes([7; 32]),
                        extractor_version: "test".into(),
                        chunks,
                    },
                    3,
                )?;
                Ok(obs.file_id)
            })
            .await
            .unwrap()
    }

    async fn delete(&self, f: FileId) {
        self.cat
            .write(move |c| files::delete_file(c, f, 4))
            .await
            .unwrap();
    }

    /// Current chunk ids with their text (catalog truth).
    fn catalog_chunks(&self) -> HashMap<ChunkId, String> {
        let coll = self.coll;
        self.cat
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT c.chunk_id, c.text_hash FROM chunks c
                         JOIN files f ON f.id = c.file_id AND c.file_version = f.version
                         JOIN folders fo ON fo.id = f.folder_id WHERE fo.collection_id = ?1",
                    )
                    .unwrap();
                let rows: Vec<([u8; 32], [u8; 32])> = st
                    .query_map([coll.to_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                let mut out = HashMap::new();
                for (cid, th) in rows {
                    let t = chunks::text(c, &sr_common::ids::TextHash::from_bytes(th))?
                        .unwrap_or_default();
                    out.insert(ChunkId::from_bytes(cid), t);
                }
                Ok(out)
            })
            .unwrap()
    }

    fn active(&self) -> (profiles::VectorIndexRecord, profiles::ProfileRecord) {
        let coll = self.coll;
        self.cat
            .read(move |c| profiles::active_index(c, coll))
            .unwrap()
            .unwrap()
    }

    fn index(&self, id: sr_common::ids::VectorIndexId) -> profiles::VectorIndexRecord {
        self.cat
            .read(move |c| profiles::vector_index(c, id))
            .unwrap()
    }

    fn building(&self) -> Option<profiles::VectorIndexRecord> {
        let coll = self.coll;
        self.cat
            .read(move |c| profiles::live_indexes(c, coll))
            .unwrap()
            .into_iter()
            .find(|i| i.state == IndexState::Building)
    }

    async fn table_rows(&self, table: &str, dims: usize) -> Vec<(ChunkId, FileId)> {
        let t = self
            .vectors
            .open_table(table, dims, AnnConfig::default())
            .await
            .unwrap();
        t.rows(None).await.unwrap()
    }
}

async fn corpus(w: &World, files_n: u32, chunks_n: u32) -> Vec<FileId> {
    let mut v = Vec::new();
    for i in 0..files_n {
        v.push(w.put_file(&format!("doc{i:03}"), chunks_n, 0).await);
    }
    w.drain().await;
    v
}

fn opts(batch: usize) -> ReembedOptions {
    ReembedOptions {
        batch,
        progress_every: Duration::ZERO,
        gc_after: Duration::from_millis(50),
        ..ReembedOptions::default()
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("m2g").tempdir().unwrap()
}

fn events(bus: &EventBus) -> tokio::sync::broadcast::Receiver<sr_common::events::Seq<EngineEvent>> {
    bus.subscribe()
}

fn drain_events(
    rx: &mut tokio::sync::broadcast::Receiver<sr_common::events::Seq<EngineEvent>>,
) -> Vec<EngineEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e.payload);
    }
    v
}

/// Every vector of `table` equals the new model's embedding of its chunk text.
async fn assert_new_vectors(w: &World, table: &str) {
    let t = w
        .vectors
        .open_table(table, 768, AnnConfig::default())
        .await
        .unwrap();
    let cat = w.catalog_chunks();
    let ids: Vec<ChunkId> = cat.keys().copied().collect();
    let got = t.vectors_by_chunk_id(&ids).await.unwrap();
    assert_eq!(got.len(), cat.len());
    let np = new_provider();
    for (id, text) in &cat {
        let want = HashEmbedder::vector(NEW, &np.profile().prepare(EmbedKind::Doc, text), 768);
        let v = &got[id];
        let d: f32 = v.iter().zip(&want).map(|(a, b)| (a - b).abs()).sum();
        assert!(d < 1e-3, "vector of {id:?} is not the new model's");
    }
}

// ----------------------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_path_swaps_atomically_and_keeps_chunk_ids() {
    let t = tmp();
    let w = World::open(t.path()).await;
    corpus(&w, 30, 5).await;
    let (old_idx, old_prof) = w.active();
    assert_eq!(old_prof.spec.model, OLD);
    let before: HashSet<ChunkId> = w
        .table_rows(&old_idx.lance_table, 384)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(before.len(), 150);
    let mut rx = events(&w.bus);
    // An in-flight query's handle on the old table (G2: the writer itself no longer holds
    // retired stores after a swap, so this is the only holder).
    let held = w.registry.get(w.coll).unwrap().vectors.clone();
    let p = Arc::new(Counting::new(new_provider()));
    let r = reembed::run(
        &w.deps(),
        w.coll,
        p.clone(),
        &opts(32),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let ReembedOutcome::Swapped { old, new, rows } = r else {
        panic!("{r:?}")
    };
    assert_eq!((old, rows), (Some(old_idx.id), 150));
    // Chunk ids identical before and after; vectors are the new model's.
    let (idx, prof) = w.active();
    assert_eq!(
        (idx.id, idx.dims, prof.spec.model.as_str()),
        (new, 768, NEW)
    );
    let after: HashSet<ChunkId> = w
        .table_rows(&idx.lance_table, 768)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(before, after, "chunk ids never change on a model switch");
    assert_new_vectors(&w, &idx.lance_table).await;
    assert_eq!(
        p.texts.load(Ordering::SeqCst),
        150 + 1,
        "each chunk once (+ canary)"
    );
    assert_eq!(w.index(old_idx.id).state, IndexState::Retired);
    // The registry serves the new table + new embedder (queries get 768 dims).
    let s = w.registry.get(w.coll).unwrap();
    assert_eq!(s.vectors.dims(), 768);
    assert_eq!(s.embedder.embed_query("hello").await.unwrap().len(), 768);
    // Events: started → progress… → swapped.
    let ev = drain_events(&mut rx);
    assert!(
        matches!(
            ev.first(),
            Some(EngineEvent::ReembedStarted {
                total: 150,
                resumed_from: 0,
                dims: 768,
                ..
            })
        ),
        "{ev:?}"
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, EngineEvent::ReembedProgress { .. }))
    );
    assert!(
        matches!(ev.last(), Some(EngineEvent::IndexSwapped { new_index_id, dims: 768, .. }) if *new_index_id == new)
    );
    // Live edits after the swap land in the new index through the normal writer.
    w.put_file("after-swap", 3, 0).await;
    w.drain().await;
    assert_new_vectors(&w, &idx.lance_table).await;
    // The retired table is kept while anything still holds it (`held` stands in for an
    // in-flight query; GC waits for the last holder) …
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        w.vectors
            .table_names()
            .await
            .unwrap()
            .contains(&old_idx.lance_table),
        "never dropped under a holder"
    );
    drop(held);
    // … and the next start's sweep drops it (a fresh process holds nothing: empty registry,
    // the active table is registered again before the sweep runs).
    let fresh = ReembedDeps {
        registry: StoreRegistry::new(),
        ..w.deps()
    };
    fresh.registry.register(w.registry.get(w.coll).unwrap());
    let n = reembed::gc_dead_tables(&fresh, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let names = w.vectors.table_names().await.unwrap();
    assert!(!names.contains(&old_idx.lance_table), "retired table GC'd");
    assert!(names.contains(&idx.lance_table));
    // Below the age threshold nothing is dropped.
    assert_eq!(
        reembed::gc_dead_tables(&w.deps(), Duration::from_secs(3600))
            .await
            .unwrap(),
        0
    );
    // Running again is a no-op.
    let r = reembed::run(
        &w.deps(),
        w.coll,
        new_provider(),
        &opts(32),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(r, ReembedOutcome::AlreadyActive);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interruption_resumes_from_checkpoint_and_crash_between_data_and_checkpoint_is_idempotent()
{
    let t = tmp();
    let w = World::open(t.path()).await;
    corpus(&w, 20, 5).await; // 100 chunks
    let (old_idx, _) = w.active();
    // Crash after the 3rd batch's data commit, before its checkpoint.
    let p = Arc::new(Counting::new(new_provider()));
    let mut o = opts(16);
    o.fault = ReembedFault::AfterDataBeforeCheckpoint(3);
    let mut rx = events(&w.bus);
    let e = reembed::run(&w.deps(), w.coll, p.clone(), &o, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal);
    assert!(drain_events(&mut rx).iter().any(|e| matches!(
        e,
        EngineEvent::ReembedFailed {
            resumable: true,
            ..
        }
    )));
    // Search still on the old index and model.
    let (idx, prof) = w.active();
    assert_eq!((idx.id, prof.spec.model.as_str()), (old_idx.id, OLD));
    let b = w.building().expect("shadow kept for resume");
    assert_eq!(b.rows_done, 32, "checkpoint after batch 2");
    // Rows of batch 3 are in the shadow already (data first) beyond the checkpoint.
    assert_eq!(w.table_rows(&b.lance_table, 768).await.len(), 48);
    // Resume: only the remaining chunks are embedded; batch 3's vectors are reused (no dupes).
    let p2 = Arc::new(Counting::new(new_provider()));
    let r = reembed::run(
        &w.deps(),
        w.coll,
        p2.clone(),
        &opts(16),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(r, ReembedOutcome::Swapped { new, rows: 100, .. } if new == b.id));
    let ev = drain_events(&mut rx);
    assert!(
        matches!(
            ev.first(),
            Some(EngineEvent::ReembedStarted {
                resumed_from: 32,
                ..
            })
        ),
        "{ev:?}"
    );
    assert_eq!(
        p2.texts.load(Ordering::SeqCst),
        100 - 48 + 1,
        "only the rest (+ canary)"
    );
    let rows = w.table_rows(&b.lance_table, 768).await;
    let uniq: HashSet<_> = rows.iter().map(|r| r.0).collect();
    assert_eq!((rows.len(), uniq.len()), (100, 100), "no duplicates");
    assert_new_vectors(&w, &b.lance_table).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dimension_mismatch_is_refused_and_search_stays_on_the_old_index() {
    let t = tmp();
    let w = World::open(t.path()).await;
    corpus(&w, 5, 4).await;
    let (old_idx, _) = w.active();
    let tables_before = w.vectors.table_names().await.unwrap();
    let c = CancellationToken::new();

    // (a) A local tier model declaring the wrong dims: refused before any write.
    let mut liar = HashEmbedder::new("Snowflake/snowflake-arctic-embed-m-v1.5", 1024);
    liar.profile.provider = "local".into();
    liar.profile.tier = Some(sr_common::tier::Tier::Standard);
    let e = reembed::run(&w.deps(), w.coll, Arc::new(liar), &opts(8), &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    // (b) Declares 768, returns 1024 (canary): refused before any write.
    let mut wrong = HashEmbedder::new(NEW, 768);
    wrong.return_dims = Some(1024);
    let e = reembed::run(&w.deps(), w.coll, Arc::new(wrong), &opts(8), &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    assert!(w.building().is_none(), "no shadow created");
    assert_eq!(w.vectors.table_names().await.unwrap(), tables_before);

    // (c) Right at the canary, wrong mid-run: the shadow is discarded.
    let mut rx = events(&w.bus);
    let mut p = Counting::new(new_provider());
    p.wrong_dims_from_call = 3;
    let e = reembed::run(&w.deps(), w.coll, Arc::new(p), &opts(4), &c)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::ModelIncompatible);
    let ev = drain_events(&mut rx);
    let failed = ev.iter().find_map(|e| match e {
        EngineEvent::ReembedFailed {
            index_id,
            code,
            resumable,
        } => Some((*index_id, *code, *resumable)),
        _ => None,
    });
    let (sid, code, resumable) = failed.expect("reembed.failed");
    assert_eq!((code, resumable), (ErrorCode::ModelIncompatible, false));
    assert_eq!(w.index(sid).state, IndexState::Failed);
    assert_eq!(
        w.index(sid).failure_code.as_deref(),
        Some("ModelIncompatible").or(w.index(sid).failure_code.as_deref())
    );
    assert_eq!(
        w.vectors.table_names().await.unwrap(),
        tables_before,
        "shadow table dropped"
    );
    let (idx, prof) = w.active();
    assert_eq!(
        (idx.id, prof.spec.model.as_str()),
        (old_idx.id, OLD),
        "active untouched"
    );
    // A query vector of the wrong length is refused for the active index, never compared.
    assert!(profiles::check_query_dims(&idx, 768).is_err());
    assert!(profiles::check_query_dims(&idx, 384).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edits_during_the_rebuild_are_caught_up_before_the_swap() {
    let t = tmp();
    let w = World::open(t.path()).await;
    let ids = corpus(&w, 12, 4).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut p = Counting::new(new_provider());
    p.gate = Some(gate.clone());
    let p = Arc::new(p);
    let deps = w.deps();
    let coll = w.coll;
    let p2 = p.clone();
    let task = tokio::spawn(async move {
        reembed::run(&deps, coll, p2, &opts(8), &CancellationToken::new()).await
    });
    // Wait until the bulk pass is blocked on its first embed call (after the canary).
    for _ in 0..200 {
        if p.calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Meanwhile: an add, an edit (new chunk ids), a delete — applied live to the active index.
    let added = w.put_file("added-during", 3, 0).await;
    w.put_file("doc001", 4, 9).await; // edit
    w.delete(ids[2]).await;
    w.drain().await;
    gate.add_permits(10_000);
    let r = task.await.unwrap().unwrap();
    let ReembedOutcome::Swapped { new, .. } = r else {
        panic!("{r:?}")
    };
    let rec = w.index(new);
    let shadow: HashSet<ChunkId> = w
        .table_rows(&rec.lance_table, 768)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    let want: HashSet<ChunkId> = w.catalog_chunks().keys().copied().collect();
    assert_eq!(
        shadow, want,
        "add, edit and delete all reflected in the new index"
    );
    assert!(
        w.table_rows(&rec.lance_table, 768)
            .await
            .iter()
            .any(|r| r.1 == added)
    );
    assert!(
        !w.table_rows(&rec.lance_table, 768)
            .await
            .iter()
            .any(|r| r.1 == ids[2])
    );
    assert_new_vectors(&w, &rec.lance_table).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_the_swap_keeps_the_old_index_then_completes() {
    let t = tmp();
    let w = World::open(t.path()).await;
    corpus(&w, 8, 4).await;
    let (old_idx, _) = w.active();
    let mut o = opts(8);
    o.fault = ReembedFault::BeforeSwap;
    let e = reembed::run(
        &w.deps(),
        w.coll,
        new_provider(),
        &o,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal);
    assert_eq!(w.active().0.id, old_idx.id, "pointer not swapped");
    let b = w.building().unwrap();
    assert_eq!(b.rows_done, 32);
    let p = Arc::new(Counting::new(new_provider()));
    let r = reembed::run(
        &w.deps(),
        w.coll,
        p.clone(),
        &opts(8),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(r, ReembedOutcome::Swapped { new, .. } if new == b.id));
    assert_eq!(
        p.texts.load(Ordering::SeqCst),
        1,
        "nothing re-embedded (canary only)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_target_supersedes_an_unfinished_shadow_and_gc_drops_dead_tables() {
    let t = tmp();
    let w = World::open(t.path()).await;
    corpus(&w, 6, 4).await;
    let mut o = opts(8);
    o.fault = ReembedFault::AfterDataBeforeCheckpoint(1);
    let _ = reembed::run(
        &w.deps(),
        w.coll,
        new_provider(),
        &o,
        &CancellationToken::new(),
    )
    .await;
    let first = w.building().unwrap();
    // The user picks another model: the old shadow is failed and dropped, a new one is built.
    let other = Arc::new(HashEmbedder::new("other-model", 512));
    let r = reembed::run(
        &w.deps(),
        w.coll,
        other,
        &opts(8),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let ReembedOutcome::Swapped { new, old, .. } = r else {
        panic!()
    };
    assert_ne!(new, first.id);
    assert_eq!(w.index(first.id).state, IndexState::Failed);
    assert!(
        !w.vectors
            .table_names()
            .await
            .unwrap()
            .contains(&first.lance_table)
    );
    // Startup GC: the retired old table (age ≥ 0 here) is dropped; the active one is kept.
    let old_rec = w.index(old.unwrap());
    let n = reembed::gc_dead_tables(&w.deps(), Duration::ZERO)
        .await
        .unwrap();
    assert!(n <= 1);
    let names = w.vectors.table_names().await.unwrap();
    assert!(!names.contains(&old_rec.lance_table));
    assert!(names.contains(&w.index(new).lance_table));
}

// ------------------------------------------------------------------------------- kill -9

const CHILD_ENV: &str = "SR_M2G_REEMBED_CHILD";

/// Child half of the kill test: resumes the rebuild in the given data dir, slowly, until killed.
#[test]
fn kill9_child() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return; // not a child run
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let w = World::open(Path::new(&dir)).await;
        let mut p = Counting::new(new_provider());
        // 240 chunks in batches of 8 = 30 calls; 50 ms each keeps the whole rebuild (≈1.5 s)
        // longer than the longest kill wait (1.2 s), so a kill lands mid-rebuild even on a fast
        // machine (15 ms let an Apple-silicon child finish before the first kill).
        p.per_call = Duration::from_millis(50);
        eprintln!("child: rebuilding");
        let r = reembed::run(
            &w.deps(),
            w.coll,
            Arc::new(p),
            &opts(8),
            &CancellationToken::new(),
        )
        .await;
        eprintln!("child: finished {r:?}");
    });
}

/// Invariants after an abrupt kill: the active index is complete and opens; a shadow, if any, has
/// no duplicate rows and holds at least every chunk up to its checkpoint (data before checkpoint).
async fn check_after_kill(w: &World) -> bool {
    let (idx, prof) = w.active();
    let cat = w.catalog_chunks();
    let rows = w.table_rows(&idx.lance_table, idx.dims as usize).await;
    let active: HashSet<ChunkId> = rows.iter().map(|r| r.0).collect();
    assert_eq!(active.len(), rows.len(), "active index has no duplicates");
    let want: HashSet<ChunkId> = cat.keys().copied().collect();
    assert_eq!(
        active, want,
        "active index ({}) complete after kill",
        prof.spec.model
    );
    if let Some(b) = w.building() {
        let rows = w.table_rows(&b.lance_table, 768).await;
        let have: HashSet<ChunkId> = rows.iter().map(|r| r.0).collect();
        assert_eq!(have.len(), rows.len(), "shadow has no duplicates");
        if let Some(cp) = b.checkpoint_after_chunk_id {
            let upto = want.iter().filter(|c| **c <= cp).count() as u64;
            let have_upto = have
                .iter()
                .filter(|c| **c <= cp && want.contains(c))
                .count() as u64;
            assert!(
                have_upto >= upto.min(b.rows_done),
                "checkpoint never ahead of data"
            );
        }
    }
    prof.spec.model == NEW
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_9_at_random_points_never_breaks_search_and_the_rebuild_completes() {
    let t = tmp();
    let root = t.path().to_path_buf();
    {
        let w = World::open(&root).await;
        corpus(&w, 40, 6).await; // 240 chunks
    }
    let exe = std::env::current_exe().unwrap();
    // Deterministic-but-varied kill points (seeded per run, printed for reproduction).
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    println!("kill seed {seed}");
    let mut rng = seed | 1;
    let mut next = move |lo: u64, hi: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        lo + rng % (hi - lo)
    };
    let mut kills = 0;
    let mut swapped = false;
    for round in 0..7 {
        let mut child = std::process::Command::new(&exe)
            .args([
                "kill9_child",
                "--exact",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(CHILD_ENV, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let wait = next(150, 1_200);
        tokio::time::sleep(Duration::from_millis(wait)).await;
        let finished = child.try_wait().unwrap().is_some();
        if !finished {
            child.kill().unwrap(); // TerminateProcess: no cleanup, no unwinding (kill -9)
            kills += 1;
        }
        let _ = child.wait();
        let w = World::open(&root).await;
        swapped = check_after_kill(&w).await;
        let b = w.building();
        println!(
            "round {round}: killed after {wait} ms (finished: {finished}); shadow rows_done {:?}; swapped {swapped}",
            b.as_ref().map(|b| b.rows_done)
        );
        if swapped {
            break;
        }
        // An edit between crashes: caught up by whichever run finishes.
        w.put_file(&format!("between-{round}"), 2, round).await;
        w.drain().await;
    }
    assert!(kills >= 1, "at least one kill landed mid-rebuild");
    // Finish in-process (resumes whatever the last child left).
    let w = World::open(&root).await;
    if !swapped {
        let r = reembed::run(
            &w.deps(),
            w.coll,
            new_provider(),
            &opts(8),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(r, ReembedOutcome::Swapped { .. }), "{r:?}");
    }
    let (idx, prof) = w.active();
    assert_eq!((prof.spec.model.as_str(), idx.dims), (NEW, 768));
    let have: HashSet<ChunkId> = w
        .table_rows(&idx.lance_table, 768)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    let want: HashSet<ChunkId> = w.catalog_chunks().keys().copied().collect();
    assert_eq!(have, want);
    assert_new_vectors(&w, &idx.lance_table).await;
    assert!(w.building().is_none());
}
