//! Store, writer and query tests over real Lance + tantivy on disk and the in-memory catalog.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::future::BoxFuture;
use proptest::prelude::*;
use sr_common::error::ErrorCode;
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId};

use super::filter::{Filter, Scope};
use super::lance::{AnnConfig, DensePath, VectorStore};
use super::lexical::{LexConfig, LexicalIndex};
use super::{CollectionStores, IndexWriter, StoreRegistry, WriterConfig};
use crate::query::{QueryConfig, Reranker, SearchMode, SearchRequest, Searcher};
use crate::testkit::{BagOfWordsEmbedder, MemCatalog};

const DIMS: usize = 64;

fn lex_cfg() -> LexConfig {
    LexConfig {
        writer_heap_bytes: 20_000_000,
        writer_threads: 1,
        path_boost: 2.0,
    }
}

fn table_name(c: CollectionId) -> String {
    format!("vec_{}", c.to_string().to_ascii_lowercase())
}

async fn open_stores(root: &Path, c: CollectionId, ann: AnnConfig) -> CollectionStores {
    let vs = VectorStore::open(&root.join("lance")).await.unwrap();
    let t = vs.open_table(&table_name(c), DIMS, ann).await.unwrap();
    let lex = LexicalIndex::open(&root.join("tantivy"), c, lex_cfg()).unwrap();
    CollectionStores {
        collection_id: c,
        vectors: Arc::new(t),
        lexical: Arc::new(lex),
        embedder: Arc::new(BagOfWordsEmbedder::new(DIMS)),
    }
}

struct Env {
    dir: tempfile::TempDir,
    cat: Arc<MemCatalog>,
    reg: Arc<StoreRegistry>,
    writer: IndexWriter,
    colls: Vec<CollectionId>,
    ann: AnnConfig,
}

impl Env {
    async fn new(n_colls: usize, ann: AnnConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cat = Arc::new(MemCatalog::new());
        let colls: Vec<CollectionId> = (0..n_colls).map(|_| CollectionId::new()).collect();
        let (reg, writer) = Self::open(dir.path(), &cat, &colls, &ann).await;
        Self {
            dir,
            cat,
            reg,
            writer,
            colls,
            ann,
        }
    }

    async fn open(
        root: &Path,
        cat: &Arc<MemCatalog>,
        colls: &[CollectionId],
        ann: &AnnConfig,
    ) -> (Arc<StoreRegistry>, IndexWriter) {
        let reg = StoreRegistry::new();
        for c in colls {
            reg.register(open_stores(root, *c, ann.clone()).await);
        }
        let writer = IndexWriter::new(
            cat.clone(),
            cat.clone(),
            reg.clone(),
            WriterConfig::default(),
        );
        (reg, writer)
    }

    /// Simulated process restart: drop every store handle, reopen from disk.
    async fn restart(&mut self) {
        let (reg, writer) = {
            // drop old handles first (tantivy writer lock)
            let empty = StoreRegistry::new();
            let old_reg = std::mem::replace(&mut self.reg, empty.clone());
            let old_writer = std::mem::replace(
                &mut self.writer,
                IndexWriter::new(
                    self.cat.clone(),
                    self.cat.clone(),
                    empty,
                    WriterConfig::default(),
                ),
            );
            drop(old_writer);
            for c in &self.colls {
                old_reg.remove(*c);
            }
            drop(old_reg);
            Self::open(self.dir.path(), &self.cat, &self.colls, &self.ann).await
        };
        self.reg = reg;
        self.writer = writer;
    }

    fn stores(&self, i: usize) -> CollectionStores {
        self.reg.get(self.colls[i]).unwrap()
    }

    fn searcher(&self, cfg: QueryConfig, rr: Option<Arc<dyn Reranker>>) -> Searcher {
        Searcher::new(self.reg.clone(), self.cat.clone(), rr, cfg)
    }

    /// Both stores hold exactly the catalog's current chunks: no duplicates, no orphans.
    async fn assert_converged(&self) {
        for (i, c) in self.colls.iter().enumerate() {
            let want = self.cat.current_chunk_ids(*c);
            let s = self.stores(i);
            let rows = s
                .vectors
                .rows(Some(&Filter::CollectionIn(vec![*c])))
                .await
                .unwrap();
            let lance: HashSet<ChunkId> = rows.iter().map(|r| r.0).collect();
            assert_eq!(lance.len(), rows.len(), "duplicate Lance rows");
            assert_eq!(lance, want, "Lance differs from catalog");
            let tan = s.lexical.all_chunk_ids().unwrap();
            let tset: HashSet<ChunkId> = tan.iter().copied().collect();
            assert_eq!(tset.len(), tan.len(), "duplicate tantivy docs");
            assert_eq!(tset, want, "tantivy differs from catalog");
        }
    }
}

fn grep_dir(dir: &Path, needle: &[u8]) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p)
                && bytes.windows(needle.len()).any(|w| w == needle)
            {
                return true;
            }
        }
    }
    false
}

fn doc(topic: &str, n: u32) -> String {
    format!(
        "Notes about {topic} number {n}. The {topic} pipeline handles retries.\n\n\
         Shared licence boilerplate: permission is hereby granted free of charge.\n\n\
         Section {n} on {topic}: measurements and caveats for {topic}."
    )
}

#[tokio::test]
async fn apply_search_and_watermarks() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (c, folder) = (env.colls[0], FolderId::new());
    let files: Vec<FileId> = (0..3).map(|_| FileId::new()).collect();
    for (i, (f, topic)) in files.iter().zip(["sqlite", "tantivy", "lance"]).enumerate() {
        env.cat.upsert_text(
            *f,
            folder,
            c,
            &format!("docs/{topic}.md"),
            &doc(topic, i as u32),
        );
    }
    let rep = env.writer.drain().await.unwrap();
    assert_eq!(rep.files_upserted, 3);
    // 3 files × 3 paragraphs, one paragraph shared by all → 7 unique texts embedded
    assert_eq!(rep.chunks_embedded, 7);
    env.assert_converged().await;
    let last = env.cat.last_seq();
    for w in env.writer.watermarks().await.unwrap() {
        assert_eq!(w.seq, last, "{w:?}");
    }
    assert_eq!(env.cat.pending_ops(), 0, "acknowledged ops are pruned");
    assert!(files.iter().all(|f| env.cat.is_indexed(*f)));

    let s = env.searcher(QueryConfig::default(), None);
    let r = s
        .search(&SearchRequest::new("tantivy pipeline", vec![c]))
        .await
        .unwrap();
    assert!(!r.hits.is_empty());
    assert_eq!(r.hits[0].path, "docs/tantivy.md");
    assert!(r.hits[0].scores.bm25.is_some() && r.hits[0].scores.dense.is_some());

    // idempotent: draining again changes nothing
    let v = env.stores(0).vectors.version().await.unwrap();
    let rep2 = env.writer.drain().await.unwrap();
    assert_eq!(rep2.ops_read, 0);
    assert_eq!(env.stores(0).vectors.version().await.unwrap(), v);
}

#[tokio::test]
async fn watermark_is_committed_with_the_data() {
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let t = env.stores(0).vectors;
    assert_eq!(t.watermark().await.unwrap(), 0);
    let v0 = t.version().await.unwrap();
    env.cat
        .upsert_text(FileId::new(), FolderId::new(), c, "a.txt", "alpha beta");
    env.writer.drain().await.unwrap();
    // exactly one Lance commit for data + watermark (maintain may add index versions after)
    assert_eq!(t.watermark().await.unwrap(), 1);
    let versions_after_apply = t.version().await.unwrap();
    assert!(versions_after_apply > v0);
    assert_eq!(env.stores(0).lexical.watermark().unwrap(), 1);
    // replaying an already-applied batch keeps rows unchanged
    let before = t.rows(None).await.unwrap();
    let rep = env.writer.replay().await.unwrap();
    assert_eq!(rep.ops_read, 0);
    assert_eq!(t.rows(None).await.unwrap(), before);
}

#[tokio::test]
async fn duplicate_files_do_not_cross_delete_and_reuse_vectors() {
    let env = Env::new(2, AnnConfig::default()).await;
    let (c0, c1) = (env.colls[0], env.colls[1]);
    let (fa, fb, fc) = (FileId::new(), FileId::new(), FileId::new());
    let (d0, d1) = (FolderId::new(), FolderId::new());
    let text = doc("licence", 1);
    env.cat.upsert_text(fa, d0, c0, "LICENSE", &text);
    env.cat.upsert_text(fb, d0, c0, "vendor/LICENSE", &text);
    env.cat.upsert_text(fc, d1, c1, "LICENSE", &text);
    let rep = env.writer.drain().await.unwrap();
    // c0: one embed per unique text (fb copies fa's vectors); c1 has its own table
    assert_eq!(rep.chunks_embedded, 3 + 3);
    let a_ids = env.cat.chunk_ids_of(fa);
    let b_ids = env.cat.chunk_ids_of(fb);
    assert!(
        a_ids.iter().all(|a| !b_ids.contains(a)),
        "duplicate files have distinct chunk ids"
    );

    env.cat.delete_file(fa);
    env.writer.drain().await.unwrap();
    env.assert_converged().await;
    let rows = env.stores(0).vectors.rows(None).await.unwrap();
    assert!(rows.iter().all(|r| r.1 == fb));
    assert_eq!(rows.len(), b_ids.len());
    let s = env.searcher(QueryConfig::default(), None);
    // Every copy as its own hit (collapsing off) to see which rows the stores still hold.
    let r = s
        .search(&SearchRequest {
            collapse_duplicates: false,
            ..SearchRequest::new("permission granted free of charge", vec![c0, c1])
        })
        .await
        .unwrap();
    let files: HashSet<FileId> = r.hits.iter().map(|h| h.file_id).collect();
    assert!(files.contains(&fb) && files.contains(&fc) && !files.contains(&fa));
}

/// docs/06 change 4: identical text in two files is one hit (the shorter path), the other copy
/// is named in `also_at` with its own lines, and the freed slot goes to a distinct passage.
#[tokio::test]
async fn identical_text_collapses_before_max_results() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (c, d) = (env.colls[0], FolderId::new());
    let shared = "merge insert keeps the newest version of every matched row";
    env.cat
        .upsert_text(FileId::new(), d, c, "spikes/x/patches/merge.rs", shared);
    // The second copy sits lower in its file (own line range) next to a distinct paragraph:
    // a partial duplicate only collapses the shared chunk.
    env.cat.upsert_text(
        FileId::new(),
        d,
        c,
        "patches/merge.rs",
        &format!("merge planner notes for the insert path\n\n{shared}"),
    );
    env.cat.upsert_text(
        FileId::new(),
        d,
        c,
        "docs/merge.md",
        "merge insert overview for readers",
    );
    env.writer.drain().await.unwrap();
    let s = env.searcher(QueryConfig::default(), None);
    let req = SearchRequest {
        max_results: Some(3),
        min_score: Some(0.0),
        ..SearchRequest::new("merge insert newest version matched row", vec![c])
    };
    let r = s.search(&req).await.unwrap();
    let copy = r
        .hits
        .iter()
        .find(|h| h.snippet == shared)
        .expect("the shared text is a hit");
    assert_eq!(copy.path, "patches/merge.rs", "shorter path shown");
    assert_eq!((copy.line_start, copy.line_end), (3, 3));
    assert_eq!(
        copy.also_at,
        vec![crate::query::AlsoAt {
            folder_id: d,
            path: "spikes/x/patches/merge.rs".into(),
            line_start: 1,
            line_end: 1,
        }]
    );
    assert_eq!(r.hits.iter().filter(|h| h.snippet == shared).count(), 1);
    // 4 chunks, 3 distinct texts: all three fill the 3 slots.
    assert_eq!(r.hits.len(), 3);
    let texts: HashSet<&str> = r.hits.iter().map(|h| h.snippet.as_str()).collect();
    assert_eq!(texts.len(), 3);
    // Collapsing off: both copies, no `also_at`.
    let off = s
        .search(&SearchRequest {
            collapse_duplicates: false,
            max_results: Some(10),
            ..req.clone()
        })
        .await
        .unwrap();
    assert_eq!(off.hits.iter().filter(|h| h.snippet == shared).count(), 2);
    assert!(off.hits.iter().all(|h| h.also_at.is_empty()));
    // Deterministic: the same request shows the same copy every time.
    for _ in 0..3 {
        let again = s.search(&req).await.unwrap();
        assert_eq!(
            again.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
            r.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }
}

/// `also_at` never names a copy outside the request's folders (the MCP client's scope).
#[tokio::test]
async fn collapsed_copies_stay_inside_the_scope() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (c, inside, outside) = (env.colls[0], FolderId::new(), FolderId::new());
    let text = "quarterly budget figures for the orbit project";
    env.cat.upsert_text(FileId::new(), inside, c, "a.md", text);
    env.cat
        .upsert_text(FileId::new(), inside, c, "copy/a.md", text);
    env.cat
        .upsert_text(FileId::new(), outside, c, "private/a.md", text);
    env.writer.drain().await.unwrap();
    let s = env.searcher(QueryConfig::default(), None);
    let mut req = SearchRequest::new("quarterly budget orbit", vec![c]);
    req.folders = Some(vec![inside]);
    let r = s.search(&req).await.unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path, "a.md");
    assert_eq!(r.hits[0].folder_id, inside);
    assert_eq!(
        r.hits[0]
            .also_at
            .iter()
            .map(|a| (a.folder_id, a.path.as_str()))
            .collect::<Vec<_>>(),
        vec![(inside, "copy/a.md")]
    );
    // Unscoped, the third copy is listed too.
    req.folders = None;
    let r = s.search(&req).await.unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].also_at.len(), 2);
}

#[tokio::test]
async fn rename_does_not_touch_derived_stores() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (c, f) = (env.colls[0], FileId::new());
    env.cat
        .upsert_text(f, FolderId::new(), c, "old/name.md", &doc("quasar", 7));
    env.writer.drain().await.unwrap();
    let s0 = env.stores(0);
    let (lv, lw, tw) = (
        s0.vectors.version().await.unwrap(),
        s0.vectors.watermark().await.unwrap(),
        s0.lexical.watermark().unwrap(),
    );
    assert!(env.cat.rename_file(f, "new/place.md"));
    assert_eq!(env.cat.pending_ops(), 0, "rename writes no outbox op");
    let rep = env.writer.drain().await.unwrap();
    assert_eq!(rep.ops_read, 0);
    assert_eq!(s0.vectors.version().await.unwrap(), lv);
    assert_eq!(s0.vectors.watermark().await.unwrap(), lw);
    assert_eq!(s0.lexical.watermark().unwrap(), tw);
    let r = env
        .searcher(QueryConfig::default(), None)
        .search(&SearchRequest::new("quasar measurements", vec![c]))
        .await
        .unwrap();
    assert_eq!(
        r.hits[0].path, "new/place.md",
        "path comes from the catalog"
    );
}

#[tokio::test]
async fn purge_removes_data_from_old_lance_versions_and_tantivy_segments() {
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let (gone, kept) = (FolderId::new(), FolderId::new());
    let secret = "zqxjvplmw";
    let f_gone = FileId::new();
    env.cat.upsert_text(
        f_gone,
        gone,
        c,
        "private/diary.txt",
        &format!("my {secret} diary entry\n\nmore {secret} thoughts"),
    );
    // several commits so old versions/segments exist
    for i in 0..4 {
        env.cat.upsert_text(
            FileId::new(),
            kept,
            c,
            &format!("keep{i}.txt"),
            &doc("kept", i),
        );
        env.writer.drain().await.unwrap();
    }
    let gone_ids = env.cat.chunk_ids_of(f_gone);
    let s = env.stores(0);
    let lance_dir = env.dir.path().join("lance");
    let tan_dir = s.lexical.dir().to_path_buf();
    let hex = gone_ids[0].to_hex();
    let lance_had = grep_dir(&lance_dir, hex.as_bytes());
    assert!(
        lance_had,
        "the Lance grep must be meaningful (chunk id stored raw before purge)"
    );
    assert!(s.lexical.term_in_dictionary(secret).unwrap());
    assert!(s.lexical.search(secret, None, 10).unwrap().len() == 2);
    assert!(s.vectors.version_count().await.unwrap() > 1);

    env.cat.purge_folder(c, gone);
    let rep = env.writer.drain().await.unwrap();
    assert_eq!(rep.folders_purged, 1);
    env.assert_converged().await;
    assert!(s.lexical.search(secret, None, 10).unwrap().is_empty());
    assert_eq!(
        s.vectors
            .count(Some(&Filter::FolderIn(vec![gone])))
            .await
            .unwrap(),
        0
    );
    // physically gone: old Lance versions pruned, tantivy segments merged + GC'd
    assert_eq!(
        s.vectors.version_count().await.unwrap(),
        1,
        "only the latest version is kept"
    );
    assert!(
        !grep_dir(&lance_dir, hex.as_bytes()),
        "purged chunk id still on disk (lance)"
    );
    // tantivy's term dictionary is block-compressed, so check the dictionary itself: the term
    // must be gone from every segment (merged away), not just hidden by a delete bitset.
    assert!(
        !s.lexical.term_in_dictionary(secret).unwrap(),
        "purged term still in a tantivy segment"
    );
    assert!(!grep_dir(&tan_dir, secret.as_bytes()));
    // the rest is intact
    assert!(env.cat.current_chunk_ids(c).len() >= 4);
}

#[tokio::test]
async fn query_drops_hits_from_stale_file_versions() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (c, f) = (env.colls[0], FileId::new());
    env.cat
        .upsert_text(f, FolderId::new(), c, "a.md", "alpha original wording here");
    env.writer.drain().await.unwrap();
    // new version in the catalog; stores not yet updated (writer lagging / crashed)
    env.cat
        .upsert_text(f, FolderId::new(), c, "a.md", "beta replacement wording");
    let s = env.searcher(QueryConfig::default(), None);
    let r = s
        .search(&SearchRequest::new("alpha original", vec![c]))
        .await
        .unwrap();
    assert!(r.hits.is_empty(), "stale chunk shown: {:?}", r.hits);
    assert!(r.stats.dropped_stale >= 1);
    env.writer.drain().await.unwrap();
    let r = s
        .search(&SearchRequest::new("beta replacement", vec![c]))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);
    assert!(r.hits[0].snippet.contains("beta"));
}

/// docs/06 issues 2 and 3: a big share of stale candidates (a re-index in progress) costs a
/// bounded number of batched catalog lookups, and the cap holds (fewer hits instead of an
/// unbounded walk). Records time, lookups and hits for 50 % and 90 % stale.
#[tokio::test]
async fn many_stale_candidates_cost_bounded_batched_lookups() {
    use std::sync::atomic::Ordering::Relaxed;
    for stale_pct in [50usize, 90] {
        let env = Env::new(1, AnnConfig::default()).await;
        let (c, d) = (env.colls[0], FolderId::new());
        let n = 400usize;
        let files: Vec<FileId> = (0..n).map(|_| FileId::new()).collect();
        for (i, f) in files.iter().enumerate() {
            env.cat.upsert_text(
                *f,
                d,
                c,
                &format!("f{i}.md"),
                &format!("orbit telemetry report {i} with measurement {i}"),
            );
        }
        env.writer.drain().await.unwrap();
        // New versions in the catalog, stores not updated yet: the old chunks are stale hits.
        let stale = n * stale_pct / 100;
        for (i, f) in files.iter().take(stale).enumerate() {
            env.cat
                .upsert_text(*f, d, c, &format!("f{i}.md"), &format!("rewritten {i}"));
        }
        let req = SearchRequest {
            max_results: Some(20),
            min_score: Some(0.0),
            ..SearchRequest::new("orbit telemetry report measurement", vec![c])
        };
        let s = env.searcher(QueryConfig::default(), None);
        let (calls0, ids0) = (
            env.cat.hydrate_calls.load(Relaxed),
            env.cat.hydrate_ids.load(Relaxed),
        );
        let t = std::time::Instant::now();
        let r = s.search(&req).await.unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let calls = env.cat.hydrate_calls.load(Relaxed) - calls0;
        let ids = env.cat.hydrate_ids.load(Relaxed) - ids0;
        eprintln!(
            "stale {stale_pct}%: {ms:.1} ms, {} candidates, {ids} ids looked up in {calls} batches, \
             {} dropped stale, {} hits, capped={}",
            r.stats.candidates,
            r.stats.dropped_stale,
            r.hits.len(),
            r.stats.hydrate_capped
        );
        // One batched lookup per window; the stats count exactly what the catalog saw.
        assert_eq!(calls, r.stats.hydrate_windows);
        assert_eq!(ids, r.stats.hydrate_lookups);
        assert!(ids <= r.stats.candidates && ids <= QueryConfig::default().hydrate_max_ids);
        assert!(
            r.hits.iter().all(|h| h.snippet.starts_with("orbit")),
            "no stale text"
        );
        assert!(!r.hits.is_empty());

        // The cap: at most `hydrate_max_ids` lookups, in few windows; fewer hits, flagged.
        let capped = env.searcher(
            QueryConfig {
                hydrate_max_ids: 30,
                ..QueryConfig::default()
            },
            None,
        );
        let (calls0, ids0) = (
            env.cat.hydrate_calls.load(Relaxed),
            env.cat.hydrate_ids.load(Relaxed),
        );
        let r = capped.search(&req).await.unwrap();
        let calls = env.cat.hydrate_calls.load(Relaxed) - calls0;
        let ids = env.cat.hydrate_ids.load(Relaxed) - ids0;
        eprintln!(
            "stale {stale_pct}%, cap 30: {ids} ids in {calls} batches, {} hits, capped={}",
            r.hits.len(),
            r.stats.hydrate_capped
        );
        assert!(ids <= 30, "{ids} lookups over the cap");
        assert!(calls <= 2, "bounded windows: {calls}");
        assert!(r.stats.hydrate_capped);
        assert!(r.hits.len() <= 30 - r.stats.dropped_stale);
        if stale_pct == 90 {
            assert!(
                r.hits.len() < 20,
                "fewer results instead of an unbounded walk"
            );
        }
    }
}

struct LengthReranker;
impl Reranker for LengthReranker {
    fn rerank<'a>(
        &'a self,
        _q: &'a str,
        docs: &'a [&'a str],
    ) -> BoxFuture<'a, sr_common::Result<Vec<f32>>> {
        Box::pin(async move { Ok(docs.iter().map(|d| 1.0 / (1.0 + d.len() as f32)).collect()) })
    }
}

#[tokio::test]
async fn query_semantics_scope_limits_modes_rerank_mmr() {
    let env = Env::new(2, AnnConfig::default()).await;
    let (c0, c1) = (env.colls[0], env.colls[1]);
    let (d0, d1) = (FolderId::new(), FolderId::new());
    for i in 0..40u32 {
        let folder = if i % 2 == 0 { d0 } else { d1 };
        env.cat.upsert_text(
            FileId::new(),
            folder,
            c0,
            &format!("f{i}.md"),
            &doc("orbit", i),
        );
    }
    env.cat.upsert_text(
        FileId::new(),
        FolderId::new(),
        c1,
        "other.md",
        &doc("orbit", 99),
    );
    env.writer.drain().await.unwrap();
    let s = env.searcher(QueryConfig::default(), None);

    // validation
    let mut bad = SearchRequest::new("   ", vec![c0]);
    assert_eq!(
        s.search(&bad).await.unwrap_err().code,
        ErrorCode::InvalidInput
    );
    bad.query = "orbit".into();
    bad.min_score = Some(1.5);
    assert_eq!(
        s.search(&bad).await.unwrap_err().code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        s.search(&SearchRequest::new("orbit", vec![]))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        s.search(&SearchRequest::new("orbit", vec![CollectionId::new()]))
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    // maxResults: default 50, cap 150, floor 1
    let mut req = SearchRequest::new("orbit pipeline measurements", vec![c0, c1]);
    let r = s.search(&req).await.unwrap();
    assert_eq!(r.hits.len(), 50);
    req.max_results = Some(1000);
    let r = s.search(&req).await.unwrap();
    assert!(r.hits.len() > 50 && r.hits.len() <= 150, "{}", r.hits.len());
    req.max_results = Some(0);
    assert_eq!(s.search(&req).await.unwrap().hits.len(), 1);

    // both collections fused
    req.max_results = Some(150);
    let r = s.search(&req).await.unwrap();
    let colls: HashSet<CollectionId> = r.hits.iter().map(|h| h.collection_id).collect();
    assert_eq!(colls.len(), 2);

    // folder scope
    let mut scoped = SearchRequest::new("orbit", vec![c0]);
    scoped.folders = Some(vec![d1]);
    let r = s.search(&scoped).await.unwrap();
    assert!(!r.hits.is_empty() && r.hits.iter().all(|h| h.folder_id == d1));

    // dense-only switch
    let mut dense = SearchRequest::new("orbit retries", vec![c0]);
    dense.mode = Some(SearchMode::DenseOnly);
    let r = s.search(&dense).await.unwrap();
    assert!(!r.hits.is_empty());
    assert!(
        r.hits
            .iter()
            .all(|h| h.scores.bm25.is_none() && h.scores.dense.is_some())
    );

    // minScore applies to dense similarity; BM25 top-10 hits pass regardless
    let mut strict = SearchRequest::new("orbit number 17", vec![c0]);
    strict.min_score = Some(0.99);
    let r = s.search(&strict).await.unwrap();
    assert!(!r.hits.is_empty() && r.hits.len() <= 10);
    strict.mode = Some(SearchMode::DenseOnly);
    assert!(s.search(&strict).await.unwrap().hits.is_empty());

    // rerank hook: off by default, used when enabled. `Replace` orders the head by the
    // reranker's scores (checked here); the M5 default `Rrf` blends it with the fused score.
    let rr = env.searcher(
        QueryConfig {
            rerank_enabled: true,
            rerank_fusion: crate::query::RerankFusion::Replace,
            ..QueryConfig::default()
        },
        Some(Arc::new(LengthReranker)),
    );
    let mut q = SearchRequest::new("orbit caveats", vec![c0]);
    q.min_score = Some(0.0);
    let r = rr.search(&q).await.unwrap();
    assert!(r.stats.reranked);
    let scores: Vec<f32> = r
        .hits
        .iter()
        .map(|h| h.scores.rerank.unwrap_or(0.0))
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "reranked order");
    let plain = s.search(&q).await.unwrap();
    assert!(!plain.stats.reranked && plain.hits.iter().all(|h| h.scores.rerank.is_none()));

    // MMR: optional, returns a subset of the same candidates
    let m = env.searcher(
        QueryConfig {
            mmr_lambda: Some(0.5),
            ..QueryConfig::default()
        },
        None,
    );
    let mut mq = SearchRequest::new("orbit", vec![c0]);
    mq.max_results = Some(5);
    let r = m.search(&mq).await.unwrap();
    assert_eq!(r.hits.len(), 5);
    let ids: HashSet<ChunkId> = r.hits.iter().map(|h| h.chunk_id).collect();
    assert_eq!(ids.len(), 5, "no duplicates after MMR");
}

#[tokio::test]
async fn ann_index_is_built_and_selective_scopes_use_flat_scan() {
    let ann = AnnConfig {
        min_rows: 300,
        num_partitions: Some(2),
        ..AnnConfig::default()
    };
    let env = Env::new(1, ann).await;
    let c = env.colls[0];
    let (big, small) = (FolderId::new(), FolderId::new());
    for i in 0..110u32 {
        env.cat.upsert_text(
            FileId::new(),
            big,
            c,
            &format!("b{i}.md"),
            &doc(&format!("topic{i}"), i),
        );
    }
    env.cat.upsert_text(
        FileId::new(),
        small,
        c,
        "s.md",
        "tiny folder note about comets",
    );
    env.writer.drain().await.unwrap();
    let t = env.stores(0).vectors;
    assert!(t.count(None).await.unwrap() >= 300);
    let q = BagOfWordsEmbedder::new(DIMS).embed("topic42 pipeline retries");
    let (hits, path) = t.search(&q, &Scope::collection(c), 10).await.unwrap();
    assert_eq!(path, DensePath::Ann);
    assert_eq!(hits.len(), 10);
    let scoped = Scope {
        collections: vec![c],
        folders: Some(vec![small]),
    };
    let (hits, path) = t.search(&q, &scoped, 10).await.unwrap();
    assert_eq!(path, DensePath::FlatSelective);
    assert_eq!(hits.len(), 1);
    // a query vector with the wrong dims is refused, never compared
    assert_eq!(
        t.search(&[0.5; 3], &Scope::collection(c), 5)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Internal
    );
}

#[tokio::test]
async fn crash_between_lance_and_tantivy_commit_then_replay_converges() {
    let mut env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let folder = FolderId::new();
    let files: Vec<FileId> = (0..5).map(|_| FileId::new()).collect();
    for (i, f) in files.iter().enumerate() {
        env.cat
            .upsert_text(*f, folder, c, &format!("{i}.md"), &doc("crash", i as u32));
    }
    env.writer
        .faults()
        .fail_after_lance_commits
        .store(1, Ordering::SeqCst);
    assert!(env.writer.drain().await.is_err());
    let s = env.stores(0);
    assert!(s.vectors.watermark().await.unwrap() > s.lexical.watermark().unwrap());
    drop(s);
    env.restart().await;
    let rep = env.writer.replay().await.unwrap();
    assert!(rep.ops_read > 0);
    env.assert_converged().await;
    assert_eq!(env.cat.pending_ops(), 0);
}

// ---------------------------------------------------------------------------------------------
// Property test: random edits, drains with injected crashes, restarts → always converges.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Act {
    New {
        coll: usize,
        folder: usize,
        topic: u8,
    },
    Edit {
        file: usize,
        topic: u8,
    },
    Copy {
        file: usize,
    },
    Delete {
        file: usize,
    },
    Rename {
        file: usize,
    },
    Purge {
        coll: usize,
        folder: usize,
    },
    Drain {
        crash_after: u32,
    },
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        4 => (0usize..2, 0usize..2, 0u8..6).prop_map(|(coll, folder, topic)| Act::New { coll, folder, topic }),
        2 => (0usize..16, 0u8..6).prop_map(|(file, topic)| Act::Edit { file, topic }),
        1 => (0usize..16).prop_map(|file| Act::Copy { file }),
        1 => (0usize..16).prop_map(|file| Act::Delete { file }),
        1 => (0usize..16).prop_map(|file| Act::Rename { file }),
        1 => (0usize..2, 0usize..2).prop_map(|(coll, folder)| Act::Purge { coll, folder }),
        3 => (0u32..3).prop_map(|crash_after| Act::Drain { crash_after }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10, ..ProptestConfig::default() })]
    #[test]
    fn replay_after_random_crashes_converges(acts in prop::collection::vec(act(), 4..16)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let mut env = Env::new(2, AnnConfig::default()).await;
            let folders: Vec<Vec<FolderId>> = (0..2).map(|_| (0..2).map(|_| FolderId::new()).collect()).collect();
            // live files: (file, coll index, folder index)
            let mut live: Vec<(FileId, usize, usize)> = Vec::new();
            let mut crashes = 0;
            for a in acts {
                match a {
                    Act::New { coll, folder, topic } => {
                        let f = FileId::new();
                        env.cat.upsert_text(f, folders[coll][folder], env.colls[coll], "n.md", &doc(&format!("t{topic}"), u32::from(topic)));
                        live.push((f, coll, folder));
                    }
                    Act::Edit { file, topic } if !live.is_empty() => {
                        let (f, coll, folder) = live[file % live.len()];
                        env.cat.upsert_text(f, folders[coll][folder], env.colls[coll], "e.md", &doc(&format!("e{topic}"), 1000 + u32::from(topic)));
                    }
                    Act::Copy { file } if !live.is_empty() => {
                        let (_, coll, folder) = live[file % live.len()];
                        let f = FileId::new();
                        env.cat.upsert_text(f, folders[coll][folder], env.colls[coll], "copy.md", &doc("dup", 1));
                        live.push((f, coll, folder));
                    }
                    Act::Delete { file } if !live.is_empty() => {
                        let (f, _, _) = live.remove(file % live.len());
                        env.cat.delete_file(f);
                    }
                    Act::Rename { file } if !live.is_empty() => {
                        let (f, _, _) = live[file % live.len()];
                        env.cat.rename_file(f, "renamed.md");
                    }
                    Act::Purge { coll, folder } => {
                        env.cat.purge_folder(env.colls[coll], folders[coll][folder]);
                        live.retain(|(_, c, d)| !(*c == coll && *d == folder));
                    }
                    Act::Drain { crash_after } => {
                        env.writer.faults().fail_after_lance_commits.store(crash_after, Ordering::SeqCst);
                        if env.writer.drain().await.is_err() {
                            crashes += 1;
                            env.restart().await;
                        }
                        env.writer.faults().fail_after_lance_commits.store(0, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            // final restart + replay must converge
            env.restart().await;
            env.writer.replay().await.unwrap();
            env.assert_converged().await;
            assert_eq!(env.cat.pending_ops(), 0, "every op acknowledged and pruned");
            for (f, _, _) in &live {
                assert!(env.cat.is_indexed(*f));
            }
            let _ = crashes;
        });
    }
}

/// CO-1: sr-core holds no ANN numbers of its own; its defaults are sr-common's measured ones.
#[test]
fn ann_config_defaults_come_from_sr_common() {
    use sr_common::tier::{ANN_DEFAULT, AnnIndexType, AnnParams, HIGH, MINIMUM};
    let d = AnnConfig::default();
    assert_eq!(d, AnnConfig::from(&ANN_DEFAULT));
    assert_eq!(d, AnnConfig::from(&MINIMUM.ann));
    assert_eq!(d.min_rows, 50_000);
    assert_eq!(d.num_partitions, None);
    assert_eq!(d.rows_per_partition, 16_384);
    assert_eq!((d.hnsw_m, d.hnsw_ef_construction), (20, 150));
    assert_eq!((d.nprobes, d.ef, d.refine_factor), (16, Some(100), Some(5)));
    assert!((d.flat_selectivity - 0.05).abs() < f64::EPSILON);
    assert!((d.partial_widen_min - 4.0).abs() < f64::EPSILON);
    assert!((d.partial_widen_max - 20.0).abs() < f64::EPSILON);
    assert_eq!(d.max_ef, 2_000);
    assert!((d.retrain_growth - 2.0).abs() < f64::EPSILON);
    assert!((d.reindex_unindexed_fraction - 0.05).abs() < f64::EPSILON);
    // Tier overrides map through; `Flat` never builds an index.
    let h = AnnConfig::from(&HIGH.ann);
    assert_eq!((h.nprobes, h.refine_factor, h.hnsw_m), (32, Some(10), 24));
    let flat = AnnConfig::from(&AnnParams {
        index_type: AnnIndexType::Flat,
        ..ANN_DEFAULT
    });
    assert_eq!(flat.min_rows, usize::MAX);
    // Lenient deserialisation: missing knobs take the shared defaults.
    let partial: AnnConfig = serde_json::from_str(r#"{"nprobes": 8}"#).unwrap();
    assert_eq!(partial.nprobes, 8);
    assert_eq!(partial.max_ef, d.max_ef);
}

// ---------------------------------------------------------------------------------------------
// Amendment 4.25: bounded group commit.
// ---------------------------------------------------------------------------------------------

use super::Embedder;

/// Calls through to BagOfWords, but on call number `stop_on` it fires `stop` and never answers
/// (an embedding window in flight when the shutdown drain's stop arrives).
struct StopOnCall {
    inner: BagOfWordsEmbedder,
    calls: std::sync::atomic::AtomicUsize,
    stop_on: usize,
    stop: tokio_util::sync::CancellationToken,
}

impl Embedder for StopOnCall {
    fn dims(&self) -> usize {
        self.inner.dims()
    }
    fn fingerprint(&self) -> &str {
        self.inner.fingerprint()
    }
    fn embed_documents<'a>(
        &'a self,
        texts: &'a [&'a str],
    ) -> BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.stop_on {
                self.stop.cancel();
                futures::future::pending::<()>().await;
            }
            self.inner.embed_documents(texts).await
        })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, sr_common::Result<Vec<f32>>> {
        self.inner.embed_query(text)
    }
}

fn unique_files(env: &Env, n: usize) -> Vec<FileId> {
    let c = env.colls[0];
    let folder = FolderId::new();
    (0..n)
        .map(|i| {
            let f = FileId::new();
            env.cat.upsert_text(
                f,
                folder,
                c,
                &format!("f{i}.md"),
                &format!("alpha{i} unique body {i}.\n\nsecond{i} paragraph about item {i}."),
            );
            f
        })
        .collect()
}

#[tokio::test]
async fn batches_close_at_the_embedded_chunk_budget_and_converge() {
    let env = Env::new(1, AnnConfig::default()).await;
    unique_files(&env, 20); // 40 unique texts
    let w = IndexWriter::new(
        env.cat.clone(),
        env.cat.clone(),
        env.reg.clone(),
        WriterConfig {
            batch_chunks: 10,
            embed_batch: 4,
            ..WriterConfig::default()
        },
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let first = w.drain_batch(&stop).await.unwrap();
    assert_eq!(first.batches, 1);
    assert!(
        first.more,
        "a 40-text backlog does not fit one 10-chunk batch"
    );
    // A batch closes at the first window boundary at/after the budget: ≤ budget + one window.
    assert!((10..=10 + 4).contains(&first.chunks_embedded), "{first:?}");
    // The committed prefix is searchable and acknowledged already.
    assert_eq!(
        env.stores(0).vectors.watermark().await.unwrap(),
        first.last_seq
    );
    let rest = w.drain_until(&stop).await.unwrap();
    assert!(!rest.more && !rest.stopped);
    assert!(rest.batches >= 2, "{rest:?}");
    assert!(rest.max_batch_embedded <= 14, "{rest:?}");
    assert_eq!(first.chunks_embedded + rest.chunks_embedded, 40);
    env.assert_converged().await;
    assert_eq!(env.cat.pending_ops(), 0);
}

#[tokio::test]
async fn stop_abandons_the_window_in_flight_and_the_rest_replays_exactly_once() {
    let mut env = Env::new(1, AnnConfig::default()).await;
    let files = unique_files(&env, 12); // 24 texts, 2 per file
    let stop = tokio_util::sync::CancellationToken::new();
    // Already stopped: nothing is read or written.
    let pre = tokio_util::sync::CancellationToken::new();
    pre.cancel();
    let r = env.writer.drain_batch(&pre).await.unwrap();
    assert!(r.stopped && r.batches == 0);
    assert_eq!(env.stores(0).vectors.watermark().await.unwrap(), 0);

    let mut s = env.stores(0);
    s.embedder = Arc::new(StopOnCall {
        inner: BagOfWordsEmbedder::new(DIMS),
        calls: std::sync::atomic::AtomicUsize::new(0),
        stop_on: 3,
        stop: stop.clone(),
    });
    env.reg.register(s);
    let w = IndexWriter::new(
        env.cat.clone(),
        env.cat.clone(),
        env.reg.clone(),
        WriterConfig {
            batch_chunks: 4, // one window (2 files, 4 texts) per batch; window 3 stops
            embed_batch: 4,
            ..WriterConfig::default()
        },
    );
    let t0 = std::time::Instant::now();
    let r = w.drain_until(&stop).await.unwrap();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "stop is prompt"
    );
    assert!(r.stopped && r.more, "{r:?}");
    assert_eq!(r.batches, 2, "{r:?}");
    let committed: HashSet<ChunkId> = env
        .stores(0)
        .vectors
        .rows(None)
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.0)
        .collect();
    assert_eq!(
        committed.len(),
        8,
        "4 files committed, the stopped window not"
    );
    assert!(env.cat.pending_ops() > 0, "the rest stays in the outbox");
    let indexed = files.iter().filter(|f| env.cat.is_indexed(**f)).count();
    assert_eq!(indexed, 4);

    // Restart (plain embedder): the remainder is applied once, committed rows are reused.
    drop(w);
    env.restart().await;
    let rep = env.writer.replay().await.unwrap();
    assert_eq!(
        rep.chunks_embedded,
        24 - 8,
        "only the remainder is embedded"
    );
    env.assert_converged().await;
    assert!(files.iter().all(|f| env.cat.is_indexed(*f)));
    assert_eq!(env.cat.pending_ops(), 0);
}

// ---------------------------------------------------------------------------------------------
// R-UX18: "no good match" cut-off.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn no_good_match_cutoff_empties_unrelated_results_only() {
    use crate::query::{NoMatchCutoff, NoMatchMode};
    let env = Env::new(1, AnnConfig::default()).await;
    unique_files(&env, 5);
    env.writer.drain().await.unwrap();
    let c = env.colls[0];
    let cfg = |m: NoMatchMode| QueryConfig {
        no_match: m,
        ..QueryConfig::default()
    };
    let req = |q: &str| SearchRequest {
        min_score: Some(0.0),
        ..SearchRequest::new(q, vec![c])
    };
    // Off: an unrelated query still returns (unrelated) nearest neighbours.
    let r = env
        .searcher(cfg(NoMatchMode::Off), None)
        .search(&req("zebrafish embryo"))
        .await
        .unwrap();
    assert!(!r.hits.is_empty() && !r.stats.no_good_match);
    assert!(r.stats.top_dense.is_some());
    // Floors above anything this query reaches: an empty answer, flagged.
    let strict = NoMatchMode::Fixed(NoMatchCutoff {
        min_top_dense: 0.99,
        min_top_bm25: 1000.0,
    });
    let r = env
        .searcher(cfg(strict), None)
        .search(&req("zebrafish embryo"))
        .await
        .unwrap();
    assert!(r.hits.is_empty() && r.stats.no_good_match);
    // Lexical evidence keeps a real match even when the dense floor is not met.
    let lex = NoMatchMode::Fixed(NoMatchCutoff {
        min_top_dense: 0.99,
        min_top_bm25: 0.1,
    });
    let r = env
        .searcher(cfg(lex), None)
        .search(&req("alpha3"))
        .await
        .unwrap();
    assert!(!r.hits.is_empty() && !r.stats.no_good_match);
    assert_eq!(r.hits[0].path, "f3.md");
    // M2 tester: the real bge-small floors on a 5-file collection. BM25 can't reach the
    // calibrated 20 here, so the floor scales with the index size: an exact unique token is kept,
    // a query with no lexical match is still emptied.
    let bge = NoMatchMode::Fixed(NoMatchCutoff {
        min_top_dense: 0.99, // the test embedder never reaches it: only BM25 decides
        ..NoMatchCutoff::BGE_SMALL
    });
    let r = env
        .searcher(cfg(bge), None)
        .search(&req("alpha3"))
        .await
        .unwrap();
    assert!(!r.stats.no_good_match, "{:?}", r.stats.top_bm25);
    assert_eq!(r.hits[0].path, "f3.md");
    let r = env
        .searcher(cfg(bge), None)
        .search(&req("zebrafish embryo"))
        .await
        .unwrap();
    assert!(r.hits.is_empty() && r.stats.no_good_match);
    let c20 = NoMatchCutoff::BGE_SMALL;
    assert_eq!(c20.bm25_floor(500), 20.0);
    assert_eq!(c20.bm25_floor(17_000), 20.0);
    assert!(
        c20.bm25_floor(6) > 0.5 && c20.bm25_floor(6) < 1.5,
        "{}",
        c20.bm25_floor(6)
    );
    assert!(c20.bm25_floor(499) < 20.0 && c20.bm25_floor(499) > c20.bm25_floor(50));
    // Model mode with an uncalibrated embedder (the test embedder) never fires.
    let r = env
        .searcher(cfg(NoMatchMode::Model), None)
        .search(&req("zebrafish embryo"))
        .await
        .unwrap();
    assert!(!r.hits.is_empty() && !r.stats.no_good_match);
    assert_eq!(
        NoMatchCutoff::for_model("BAAI/bge-small-en-v1.5"),
        Some(NoMatchCutoff::BGE_SMALL)
    );
    assert_eq!(NoMatchCutoff::for_model("other"), None);
}

/// R-UX33 (M2 tester, save-latency run): an exact identifier inside a longer file must rank first
/// even when short notes share its sub-tokens; a digits-only query finds the number inside the
/// token; a number that is nowhere is "nothing matched"; a one-word query counts any keyword hit.
#[tokio::test]
async fn exact_identifiers_and_numbers_rank_first_and_pass_the_no_match_floor() {
    use crate::query::{NoMatchCutoff, NoMatchMode};
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let folder = FolderId::new();
    let mut journal: String = (0..40)
        .map(|i| format!("Day {i}: notes about the garden and the weather today.\n\n"))
        .collect();
    journal.push_str("Edit marker savetokedit19q76666 appended here.\n\nAnd zebrapathtoken too.");
    env.cat
        .upsert_text(FileId::new(), folder, c, "journal.md", &journal);
    for n in 0..18 {
        env.cat.upsert_text(
            FileId::new(),
            folder,
            c,
            &format!("note{n}.md"),
            &format!("savetoknew19q{n} saved"),
        );
    }
    env.writer.drain().await.unwrap();
    let bge = QueryConfig {
        no_match: NoMatchMode::Fixed(NoMatchCutoff {
            min_top_dense: 0.99, // the test embedder never reaches it: only lexical evidence counts
            ..NoMatchCutoff::BGE_SMALL
        }),
        ..QueryConfig::default()
    };
    let s = env.searcher(bge.clone(), None);
    let req = |q: &str| SearchRequest::new(q, vec![c]);

    let r = s.search(&req("savetokedit19q76666")).await.unwrap();
    assert!(!r.stats.no_good_match);
    assert_eq!(
        r.hits[0].path,
        "journal.md",
        "{:?}",
        r.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    assert!(r.stats.exact_hits >= 1);

    let r = s.search(&req("76666")).await.unwrap();
    assert!(
        !r.stats.no_good_match,
        "digits inside a longer token are found"
    );
    assert_eq!(r.hits[0].path, "journal.md");

    let r = s.search(&req("76667")).await.unwrap();
    assert!(
        r.hits.is_empty() && r.stats.no_good_match,
        "a number nobody wrote down"
    );

    // One plain word in a long file: kept even below the small-folder BM25 floor.
    let r = s.search(&req("zebrapathtoken")).await.unwrap();
    assert!(!r.stats.no_good_match);
    assert_eq!(r.hits[0].path, "journal.md");

    // Identifier-like negatives with no exact match are still cut by the floors.
    let r = s.search(&req("frobnicateGlacierWidget")).await.unwrap();
    assert!(r.hits.is_empty() && r.stats.no_good_match);
    // A near-miss of an identifier that exists (sub-tokens and embedding are close) is not it.
    let r = s.search(&req("savetokedit19q76667")).await.unwrap();
    assert!(
        r.hits.is_empty() && r.stats.no_good_match,
        "near-miss identifier"
    );

    // The exact lane can be switched off (A/B); digits-only queries then fall back to the floors.
    let off = env.searcher(
        QueryConfig {
            exact_lane: false,
            ..bge
        },
        None,
    );
    let r = off.search(&req("savetokedit19q76666")).await.unwrap();
    assert_eq!(r.stats.exact_hits, 0);
}

// ---------------------------------------------------------------------------------------------
// R30: stash rows for big windows; G2: the writer releases retired stores.
// ---------------------------------------------------------------------------------------------

/// Counts calls and texts; cancels `stop` when call number `cancel_after` has produced its
/// vectors (0 = never). The call itself still succeeds, so its part is complete.
struct CountingStop {
    inner: BagOfWordsEmbedder,
    calls: std::sync::atomic::AtomicUsize,
    texts: std::sync::atomic::AtomicUsize,
    cancel_after: usize,
    stop: tokio_util::sync::CancellationToken,
}

impl CountingStop {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn texts(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingStop {
    fn dims(&self) -> usize {
        self.inner.dims()
    }
    fn fingerprint(&self) -> &str {
        self.inner.fingerprint()
    }
    fn embed_documents<'a>(
        &'a self,
        texts: &'a [&'a str],
    ) -> BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let out: Vec<Vec<f32>> = texts.iter().map(|t| self.inner.embed(t)).collect();
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.cancel_after {
                self.stop.cancel();
            }
            Ok(out)
        })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, sr_common::Result<Vec<f32>>> {
        self.inner.embed_query(text)
    }
}

/// Register a [`CountingStop`] embedder for collection 0 (same table and tantivy index).
fn counting_embedder(
    env: &Env,
    cancel_after: usize,
    stop: &tokio_util::sync::CancellationToken,
) -> Arc<CountingStop> {
    let e = Arc::new(CountingStop {
        inner: BagOfWordsEmbedder::new(DIMS),
        calls: std::sync::atomic::AtomicUsize::new(0),
        texts: std::sync::atomic::AtomicUsize::new(0),
        cancel_after,
        stop: stop.clone(),
    });
    let mut s = env.stores(0);
    s.embedder = e.clone();
    env.reg.register(s);
    e
}

/// `batch_chunks` 8, one huge embedding window: a 20-chunk file is embedded in parts 8 + 8 + 4.
fn stash_writer(env: &Env) -> IndexWriter {
    IndexWriter::new(
        env.cat.clone(),
        env.cat.clone(),
        env.reg.clone(),
        WriterConfig {
            batch_chunks: 8,
            embed_batch: 1000,
            ..WriterConfig::default()
        },
    )
}

fn big_chunks(tag: &str, range: std::ops::Range<usize>) -> Vec<String> {
    range
        .map(|i| format!("{tag}word{i} distinct paragraph number {i} about {tag}{i}"))
        .collect()
}

/// A 20-chunk file whose drain stops after the first part: the part is stashed (watermark
/// unchanged, nothing acknowledged, the op stays in the outbox).
async fn stop_after_first_stash(env: &Env, w: &IndexWriter, f: FileId, folder: FolderId) {
    let c = env.colls[0];
    env.cat
        .upsert_chunks(f, folder, c, "big.md", &big_chunks("big", 0..20));
    let stop = tokio_util::sync::CancellationToken::new();
    let emb = counting_embedder(env, 1, &stop);
    let r = w.drain_until(&stop).await.unwrap();
    assert!(r.stopped && r.more && r.batches == 0, "{r:?}");
    assert_eq!(r.chunks_stashed, 8, "{r:?}");
    assert_eq!((emb.calls(), emb.texts()), (1, 8), "no part after the stop");
    let s = env.stores(0);
    assert_eq!(
        s.vectors.watermark().await.unwrap(),
        0,
        "watermark unchanged"
    );
    assert_eq!(s.lexical.watermark().unwrap(), 0);
    assert!(!env.cat.is_indexed(f));
    assert_eq!(env.cat.pending_ops(), 1, "the op stays in the outbox");
    let ids = env.cat.chunk_ids_of(f);
    let stashed: HashSet<ChunkId> = s
        .vectors
        .rows(None)
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.0)
        .collect();
    let first: HashSet<ChunkId> = ids[..8].iter().copied().collect();
    assert_eq!(stashed, first, "the first part, in chunk order");
}

#[tokio::test]
async fn big_file_parts_are_stashed_on_stop_and_reused_by_the_next_run() {
    let env = Env::new(1, AnnConfig::default()).await;
    let (f, folder) = (FileId::new(), FolderId::new());
    let w = stash_writer(&env);
    stop_after_first_stash(&env, &w, f, folder).await;

    let stop = tokio_util::sync::CancellationToken::new();
    let emb = counting_embedder(&env, 0, &stop);
    let r = w.drain_until(&stop).await.unwrap();
    assert!(!r.stopped && !r.more && r.batches == 1, "{r:?}");
    assert_eq!(
        r.chunks_embedded, 12,
        "only the remainder is embedded: {r:?}"
    );
    assert!(r.chunks_reused >= 8, "{r:?}");
    assert_eq!(emb.texts(), 12);
    // 12 new texts → parts 8 + 4: the first is stashed again before the batch commits.
    assert_eq!(r.chunks_stashed, 8, "{r:?}");
    env.assert_converged().await;
    assert!(env.cat.is_indexed(f));
    assert_eq!(env.cat.pending_ops(), 0);
    assert_eq!(
        env.stores(0).vectors.watermark().await.unwrap(),
        env.cat.last_seq()
    );
}

#[tokio::test]
async fn windows_within_the_chunk_budget_write_no_stash() {
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let f = FileId::new();
    // exactly `batch_chunks` new texts: one part, no stash
    env.cat.upsert_chunks(
        f,
        FolderId::new(),
        c,
        "small.md",
        &big_chunks("small", 0..8),
    );
    let w = stash_writer(&env);
    let r = w.drain().await.unwrap();
    assert_eq!(r.chunks_stashed, 0, "{r:?}");
    assert_eq!(r.chunks_embedded, 8);
    assert_eq!(r.batches, 1);
    env.assert_converged().await;
    assert!(env.cat.is_indexed(f));
}

#[tokio::test]
async fn stashed_rows_of_an_edited_file_leave_no_orphans() {
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    let (f, folder) = (FileId::new(), FolderId::new());
    let w = stash_writer(&env);
    stop_after_first_stash(&env, &w, f, folder).await;
    let stale = env.cat.chunk_ids_of(f);

    // New version: the first 10 texts are kept, 14 are new; every chunk id changes.
    let mut v2 = big_chunks("big", 0..10);
    v2.extend(big_chunks("edit", 0..14));
    env.cat.upsert_chunks(f, folder, c, "big.md", &v2);
    let stop = tokio_util::sync::CancellationToken::new();
    let emb = counting_embedder(&env, 0, &stop);
    let r = w.drain_until(&stop).await.unwrap();
    assert!(!r.stopped && !r.more, "{r:?}");
    assert!(r.chunks_reused >= 8, "stashed texts are reused: {r:?}");
    assert_eq!(r.chunks_embedded, 24 - 8, "{r:?}");
    assert_eq!(emb.texts(), 16);
    env.assert_converged().await;
    let rows = env.stores(0).vectors.rows(None).await.unwrap();
    assert_eq!(rows.len(), 24);
    assert!(
        rows.iter().all(|(id, _)| !stale.contains(id)),
        "stale stash rows removed"
    );
    assert!(env.cat.is_indexed(f));
    assert_eq!(env.cat.pending_ops(), 0);
}

#[tokio::test]
async fn release_unregistered_drops_the_retired_table_handles() {
    let env = Env::new(1, AnnConfig::default()).await;
    let c = env.colls[0];
    env.cat.upsert_text(
        FileId::new(),
        FolderId::new(),
        c,
        "a.md",
        "alpha beta gamma",
    );
    let w = IndexWriter::new(
        env.cat.clone(),
        env.cat.clone(),
        env.reg.clone(),
        WriterConfig {
            maintain: false,
            ..WriterConfig::default()
        },
    );
    w.drain().await.unwrap();
    let old = env.stores(0);
    let old_name = old.vectors.name().to_string();
    // Written since the last compaction → pending; still registered → kept.
    assert_eq!(w.pending_tables().await, vec![old_name.clone()]);
    w.release_unregistered().await;
    assert_eq!(w.pending_tables().await, vec![old_name.clone()]);

    // Index swap (M2): the collection's registry entry now points at another table.
    let vs = VectorStore::open(&env.dir.path().join("lance"))
        .await
        .unwrap();
    let t = vs
        .open_table("vec_swapped", DIMS, AnnConfig::default())
        .await
        .unwrap();
    env.reg.register(CollectionStores {
        collection_id: c,
        vectors: Arc::new(t),
        lexical: old.lexical.clone(),
        embedder: old.embedder.clone(),
    });
    let weak = Arc::downgrade(&old.vectors);
    drop(old);
    assert!(
        weak.strong_count() >= 1,
        "the writer's pending entry still holds the retired table"
    );
    w.release_unregistered().await;
    let pending = w.pending_tables().await;
    assert!(!pending.contains(&old_name), "{pending:?}");
    assert_eq!(weak.strong_count(), 0, "retired table handle released");
    assert!(weak.upgrade().is_none());
}
