//! M1 exit benchmark (C5): synthetic 1M-chunk, 384-dim index; p95 hybrid latency and dense
//! recall@10 against an exact (flat) scan. Target: p95 < 200 ms with recall@10 ≥ 0.95 (CPU, no
//! rerank, query embedding excluded and reported separately).
//!
//! ```text
//! cargo bench -p sr-core --bench hybrid_1m --profile ci -- [--rows N] [--dims D] [--queries Q]
//!     [--dir PATH] [--rebuild] [--partitions P] [--m M] [--efc E] [--sweep]
//! ```
//!
//! Data: a Gaussian mixture (2 000 clusters, per-row noise) normalised to the unit sphere, so
//! neighbourhoods look like real embedding space (cos to centroid ≈ 0.89) rather than uniform
//! noise. Each row also gets synthetic text (Zipf words + cluster topic words + a unique id) for
//! BM25. Ground truth is an exact in-memory cosine scan over the same vectors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use sr_common::ids::{ChunkCfgFp, ChunkId, CollectionId, ContentHash, FileId, FolderId, TextHash};
use sr_core::index::filter::Scope;
use sr_core::index::lance::{AnnConfig, VectorRow, VectorStore};
use sr_core::index::lexical::{LexBatch, LexConfig, LexDoc, LexicalIndex};
use sr_core::index::source::{ChunkSource, FileSnapshot, HydratedChunk};
use sr_core::index::{CollectionStores, Embedder, StoreRegistry};
use sr_core::query::{QueryConfig, SearchRequest, Searcher};

const CLUSTERS: usize = 2000;
const FOLDERS: usize = 100;
const VOCAB: usize = 50_000;

// ---------------------------------------------------------------------------------------------
// deterministic RNG
// ---------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f32 {
        let u1 = self.f64().max(1e-12);
        let u2 = self.f64();
        ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    /// Approximate Zipf(1) over 0..n via inverse CDF of a log-uniform.
    fn zipf(&mut self, n: usize) -> usize {
        let x = ((n as f64).ln() * self.f64()).exp();
        (x as usize).saturating_sub(1).min(n - 1)
    }
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

struct Corpus {
    dims: usize,
    vectors: Vec<f32>, // rows × dims
    rows: usize,
}

impl Corpus {
    fn generate(rows: usize, dims: usize) -> Self {
        let mut rng = Rng(42);
        let mut centroids = vec![0f32; CLUSTERS * dims];
        for c in centroids.chunks_mut(dims) {
            c.iter_mut().for_each(|x| *x = rng.normal());
            normalize(c);
        }
        let sigma = 0.5 / (dims as f32).sqrt();
        let mut vectors = vec![0f32; rows * dims];
        // parallel generation, deterministic per row block
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let block = rows.div_ceil(threads);
        std::thread::scope(|s| {
            for (t, chunk) in vectors.chunks_mut(block * dims).enumerate() {
                let centroids = &centroids;
                s.spawn(move || {
                    let mut rng = Rng(1000 + t as u64);
                    for (i, v) in chunk.chunks_mut(dims).enumerate() {
                        let row = t * block + i;
                        let c = &centroids[(row % CLUSTERS) * dims..(row % CLUSTERS + 1) * dims];
                        for (x, cx) in v.iter_mut().zip(c) {
                            *x = cx + sigma * rng.normal();
                        }
                        normalize(v);
                    }
                });
            }
        });
        Self {
            dims,
            vectors,
            rows,
        }
    }

    fn row(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dims..(i + 1) * self.dims]
    }

    fn text(&self, i: usize) -> String {
        let mut rng = Rng(i as u64 ^ 0xABCD);
        let cluster = i % CLUSTERS;
        let mut s = format!("doc{i} topic{cluster} theme{} ", cluster / 10);
        for _ in 0..30 {
            s.push_str(&format!("w{} ", rng.zipf(VOCAB)));
        }
        s
    }

    /// Exact top-k by cosine (vectors are unit-norm, so dot product).
    fn exact_topk(&self, q: &[f32], k: usize) -> Vec<usize> {
        self.exact_topk_folders(q, k, FOLDERS)
    }

    /// Exact top-k among rows whose folder index (`row % FOLDERS`) is below `folders`.
    fn exact_topk_folders(&self, q: &[f32], k: usize, folders: usize) -> Vec<usize> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let block = self.rows.div_ceil(threads);
        let parts: Vec<Vec<(f32, usize)>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads)
                .map(|t| {
                    s.spawn(move || {
                        let mut best: Vec<(f32, usize)> = Vec::with_capacity(k + 1);
                        let end = ((t + 1) * block).min(self.rows);
                        for i in t * block..end {
                            if i % FOLDERS >= folders {
                                continue;
                            }
                            let d: f32 = self.row(i).iter().zip(q).map(|(a, b)| a * b).sum();
                            if best.len() < k || d > best[best.len() - 1].0 {
                                let pos = best.partition_point(|x| x.0 >= d);
                                best.insert(pos, (d, i));
                                best.truncate(k);
                            }
                        }
                        best
                    })
                })
                .collect();
            hs.into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let mut all: Vec<(f32, usize)> = parts.into_iter().flatten().collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        all.truncate(k);
        all.into_iter().map(|x| x.1).collect()
    }
}

fn ids_for(i: usize) -> (ChunkId, FileId, FolderId, TextHash) {
    let file = FileId::from_bytes(((i / 4) as u128 + 1).to_be_bytes());
    let folder = FolderId::from_bytes(((i % FOLDERS) as u128 + 1).to_be_bytes());
    let cfg = ChunkCfgFp::compute("bench", "{}", "none", "1");
    let chunk = ChunkId::derive(
        file,
        &ContentHash::of_bytes(&(i / 4).to_le_bytes()),
        &cfg,
        (i % 4) as u32,
    );
    let th = TextHash::of_text(&i.to_string());
    (chunk, file, folder, th)
}

fn row_of_chunk(rows: usize) -> HashMap<ChunkId, usize> {
    (0..rows).map(|i| (ids_for(i).0, i)).collect()
}

// ---------------------------------------------------------------------------------------------
// fakes: precomputed query vectors, hydrate-everything catalog
// ---------------------------------------------------------------------------------------------

struct QueryVectors {
    dims: usize,
    map: HashMap<String, Vec<f32>>,
}

impl Embedder for QueryVectors {
    fn dims(&self) -> usize {
        self.dims
    }
    fn fingerprint(&self) -> &str {
        "bench"
    }
    fn embed_documents<'a>(
        &'a self,
        texts: &'a [&'a str],
    ) -> BoxFuture<'a, sr_common::Result<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(texts.iter().map(|_| vec![0.0; self.dims]).collect()) })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, sr_common::Result<Vec<f32>>> {
        Box::pin(async move {
            Ok(self
                .map
                .get(text)
                .cloned()
                .unwrap_or_else(|| vec![0.0; self.dims]))
        })
    }
}

struct BenchCatalog {
    collection: CollectionId,
    rows: Arc<HashMap<ChunkId, usize>>,
}

impl ChunkSource for BenchCatalog {
    fn file_snapshot(&self, _f: FileId) -> sr_common::Result<Option<FileSnapshot>> {
        Ok(None)
    }
    fn texts(&self, _h: &[TextHash]) -> sr_common::Result<HashMap<TextHash, String>> {
        Ok(HashMap::new())
    }
    fn hydrate(&self, ids: &[ChunkId]) -> sr_common::Result<HashMap<ChunkId, HydratedChunk>> {
        Ok(ids
            .iter()
            .filter_map(|id| {
                let i = *self.rows.get(id)?;
                let (_, file, folder, _) = ids_for(i);
                Some((
                    *id,
                    HydratedChunk {
                        chunk_id: *id,
                        file_id: file,
                        folder_id: folder,
                        collection_id: self.collection,
                        rel_path: format!("f{}.txt", i / 4),
                        line_start: 1,
                        line_end: 10,
                        text: String::new(),
                        // Distinct per row: the bench measures ranking, not duplicate collapsing.
                        text_hash: TextHash::of_text(&i.to_string()),
                    },
                ))
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------------------------

struct Args {
    rows: usize,
    dims: usize,
    queries: usize,
    dir: PathBuf,
    rebuild: bool,
    partitions: Option<u32>,
    m: u32,
    efc: u32,
    sweep: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        rows: 1_000_000,
        dims: 384,
        queries: 200,
        // Inside the project (`<root>/target/m1-e/bd`), short so Lance paths stay under MAX_PATH,
        // and without `..` (the store-root rule refuses it).
        dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .map_or_else(
                || PathBuf::from("bd"),
                |root| root.join("target").join("m1-e").join("bd"),
            ),
        rebuild: false,
        partitions: None,
        m: 20,
        efc: 150,
        sweep: false,
    };
    let v: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < v.len() {
        let next = v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--rows" => a.rows = next.parse().unwrap_or(a.rows),
            "--dims" => a.dims = next.parse().unwrap_or(a.dims),
            "--queries" => a.queries = next.parse().unwrap_or(a.queries),
            "--dir" => a.dir = PathBuf::from(next),
            "--partitions" => a.partitions = next.parse().ok(),
            "--m" => a.m = next.parse().unwrap_or(a.m),
            "--efc" => a.efc = next.parse().unwrap_or(a.efc),
            "--rebuild" => {
                a.rebuild = true;
                i += 1;
                continue;
            }
            "--sweep" => {
                a.sweep = true;
                i += 1;
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    a
}

fn pct(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    if s.is_empty() {
        return 0.0;
    }
    s[((s.len() as f64 - 1.0) * p).round() as usize]
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                match e.metadata() {
                    Ok(m) if m.is_dir() => stack.push(e.path()),
                    Ok(m) => total += m.len(),
                    Err(_) => {}
                }
            }
        }
    }
    total
}

fn peak_rss_mb() -> String {
    let pid = std::process::id();
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("$p=Get-Process -Id {pid}; '{{0}} {{1}}' -f [math]::Round($p.PeakWorkingSet64/1MB), [math]::Round($p.WorkingSet64/1MB)"),
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "n/a".into(),
    }
}

fn log(line: &str) {
    println!("{line}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("bench failed: {e:?}");
        std::process::exit(1);
    }
}

async fn run() -> sr_common::Result<()> {
    let a = parse_args();
    log(&format!(
        "# hybrid_1m rows={} dims={} queries={} dir={} partitions={:?} m={} efc={}",
        a.rows,
        a.dims,
        a.queries,
        a.dir.display(),
        a.partitions,
        a.m,
        a.efc
    ));
    let collection = CollectionId::from_bytes([7; 16]);
    let t = Instant::now();
    let corpus = Corpus::generate(a.rows, a.dims);
    log(&format!(
        "generate vectors: {:.1}s",
        t.elapsed().as_secs_f64()
    ));

    let marker = a.dir.join("built.ok");
    if a.rebuild && a.dir.exists() {
        std::fs::remove_dir_all(&a.dir).ok();
    }
    std::fs::create_dir_all(&a.dir).ok();
    let base_cfg = AnnConfig {
        min_rows: 50_000,
        num_partitions: a.partitions,
        hnsw_m: a.m,
        hnsw_ef_construction: a.efc,
        ..AnnConfig::default()
    };
    let store = VectorStore::open(&a.dir.join("lance")).await?;
    let table = store
        .open_table("bench_1m", a.dims, base_cfg.clone())
        .await?;
    let lex_cfg = LexConfig {
        writer_heap_bytes: 512 * 1024 * 1024,
        writer_threads: 4,
        path_boost: 2.0,
    };
    let lexical = Arc::new(LexicalIndex::open(
        &a.dir.join("tantivy"),
        collection,
        lex_cfg,
    )?);

    if !marker.exists() {
        // ---- build ----
        let t = Instant::now();
        let batch = 100_000;
        for start in (0..a.rows).step_by(batch) {
            let end = (start + batch).min(a.rows);
            let rows: Vec<VectorRow> = (start..end)
                .map(|i| {
                    let (chunk_id, file_id, folder_id, text_hash) = ids_for(i);
                    VectorRow {
                        chunk_id,
                        text_hash,
                        file_id,
                        folder_id,
                        collection_id: collection,
                        vector: corpus.row(i).to_vec(),
                    }
                })
                .collect();
            table.bulk_append(&rows, end as i64).await?;
        }
        let lance_write = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let rep = table.maintain().await?;
        let lance_index = t.elapsed().as_secs_f64();
        log(&format!(
            "lance: write {lance_write:.1}s, scalar+ANN index {lance_index:.1}s ({rep:?})"
        ));
        let t = Instant::now();
        for start in (0..a.rows).step_by(batch) {
            let end = (start + batch).min(a.rows);
            let docs: Vec<LexDoc> = (start..end)
                .map(|i| {
                    let (chunk_id, file_id, folder_id, _) = ids_for(i);
                    LexDoc {
                        chunk_id,
                        file_id,
                        folder_id,
                        collection_id: collection,
                        body: corpus.text(i),
                        path: format!("dir{}/f{}.txt", i % FOLDERS, i / 4),
                        lang: "text".into(),
                    }
                })
                .collect();
            lexical.apply(&LexBatch {
                docs,
                seq: end as i64,
                ..Default::default()
            })?;
        }
        log(&format!(
            "tantivy: index {} docs {:.1}s",
            a.rows,
            t.elapsed().as_secs_f64()
        ));
        std::fs::write(&marker, b"ok").ok();
    } else {
        log("reusing existing index (pass --rebuild to rebuild)");
    }
    log(&format!(
        "disk: lance {:.0} MiB, tantivy {:.0} MiB",
        dir_size(&a.dir.join("lance")) as f64 / 1048576.0,
        dir_size(&a.dir.join("tantivy")) as f64 / 1048576.0
    ));

    // ---- queries + ground truth ----
    let mut rng = Rng(7);
    let mut queries: Vec<(String, Vec<f32>, Vec<ChunkId>)> = Vec::new();
    let t = Instant::now();
    for qi in 0..a.queries {
        let r = rng.below(a.rows);
        let mut v: Vec<f32> = corpus
            .row(r)
            .iter()
            .map(|x| x + (0.3 / (a.dims as f32).sqrt()) * rng.normal())
            .collect();
        normalize(&mut v);
        let words: Vec<String> = corpus
            .text(r)
            .split_whitespace()
            .skip(3)
            .take(4)
            .map(String::from)
            .collect();
        let text = format!("{} q{qi}", words.join(" "));
        let truth: Vec<ChunkId> = corpus
            .exact_topk(&v, 10)
            .into_iter()
            .map(|i| ids_for(i).0)
            .collect();
        queries.push((text, v, truth));
    }
    log(&format!(
        "ground truth (exact scan, {} queries): {:.1}s",
        a.queries,
        t.elapsed().as_secs_f64()
    ));
    let rows_map = Arc::new(row_of_chunk(a.rows));

    // ---- flat reference latency ----
    {
        let mut lat = Vec::new();
        let flat_cfg = AnnConfig {
            flat_selectivity: 1.1, // force exact scan
            ..base_cfg.clone()
        };
        let ft = store.open_table("bench_1m", a.dims, flat_cfg).await?;
        for (_, v, _) in queries.iter().take(10) {
            let t = Instant::now();
            ft.search(v, &Scope::collection(collection), 10).await?;
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        log(&format!(
            "lance flat scan (10 q): p50 {:.1} ms p95 {:.1} ms",
            pct(&lat, 0.5),
            pct(&lat, 0.95)
        ));
    }

    // ---- knob grid ----
    let mut grid: Vec<(usize, Option<usize>, Option<u32>)> =
        vec![(base_cfg.nprobes, base_cfg.ef, base_cfg.refine_factor)];
    if a.sweep {
        grid.clear();
        for np in [4usize, 8, 16] {
            for rf in [None, Some(5u32)] {
                for ef in [Some(50usize), Some(100)] {
                    grid.push((np, ef, rf));
                }
            }
        }
    }
    log(
        "| nprobes | ef | refine | dense recall@10 | dense k=10 p50/p95 ms | hybrid p50/p95 ms (dense k=100 + BM25 + RRF + hydrate) | dense ms p95 | bm25 ms p95 |",
    );
    log("|---|---|---|---|---|---|---|---|");
    for (np, ef, rf) in grid {
        let cfg = AnnConfig {
            nprobes: np,
            ef,
            refine_factor: rf,
            ..base_cfg.clone()
        };
        let t = Arc::new(store.open_table("bench_1m", a.dims, cfg).await?);
        // warm-up
        for (_, v, _) in queries.iter().take(5) {
            t.search(v, &Scope::collection(collection), 10).await?;
        }
        let mut hits = 0usize;
        let mut lat10 = Vec::new();
        for (_, v, truth) in &queries {
            let s = Instant::now();
            let (h, _) = t.search(v, &Scope::collection(collection), 10).await?;
            lat10.push(s.elapsed().as_secs_f64() * 1000.0);
            hits += h.iter().filter(|x| truth.contains(&x.chunk_id)).count();
        }
        let recall = hits as f64 / (queries.len() * 10) as f64;

        let registry = StoreRegistry::new();
        registry.register(CollectionStores {
            collection_id: collection,
            vectors: t.clone(),
            lexical: lexical.clone(),
            embedder: Arc::new(QueryVectors {
                dims: a.dims,
                map: queries.iter().map(|q| (q.0.clone(), q.1.clone())).collect(),
            }),
        });
        let searcher = Searcher::new(
            registry,
            Arc::new(BenchCatalog {
                collection,
                rows: rows_map.clone(),
            }),
            None,
            QueryConfig::default(),
        );
        let mut lat = Vec::new();
        let mut dense_ms = Vec::new();
        let mut bm25_ms = Vec::new();
        for (text, _, _) in &queries {
            let s = Instant::now();
            let r = searcher
                .search(&SearchRequest::new(text.clone(), vec![collection]))
                .await?;
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
            dense_ms.push(r.stats.dense_ms);
            bm25_ms.push(r.stats.lexical_ms);
        }
        log(&format!(
            "| {np} | {} | {} | {recall:.3} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} | {:.1} |",
            ef.map_or("-".into(), |e| e.to_string()),
            rf.map_or("-".into(), |r| r.to_string()),
            pct(&lat10, 0.5),
            pct(&lat10, 0.95),
            pct(&lat, 0.5),
            pct(&lat, 0.95),
            pct(&dense_ms, 0.95),
            pct(&bm25_ms, 0.95),
        ));
    }

    // ---- partial scopes (pre-filter, widened). Each cluster lives in one folder, so "topic out
    // of scope" queries are the hard case: their in-scope neighbours are in other clusters.
    for (n_folders, in_topic) in [(20usize, true), (20, false), (60, true), (60, false)] {
        let t = store
            .open_table("bench_1m", a.dims, base_cfg.clone())
            .await?;
        let scope = Scope {
            collections: vec![collection],
            folders: Some((0..n_folders).map(|i| ids_for(i).2).collect()),
        };
        let mut lat = Vec::new();
        let mut hits = 0usize;
        let mut path = None;
        let nq = 100usize;
        let _ = &queries;
        let mut qrng = Rng(99 + n_folders as u64);
        for _ in 0..nq {
            let r = loop {
                let r = qrng.below(a.rows);
                if (r % FOLDERS < n_folders) == in_topic {
                    break r;
                }
            };
            let mut v: Vec<f32> = corpus
                .row(r)
                .iter()
                .map(|x| x + (0.3 / (a.dims as f32).sqrt()) * qrng.normal())
                .collect();
            normalize(&mut v);
            let v = &v;
            let truth: Vec<ChunkId> = corpus
                .exact_topk_folders(v, 10, n_folders)
                .into_iter()
                .map(|i| ids_for(i).0)
                .collect();
            let s = Instant::now();
            let (h, p) = t.search(v, &scope, 10).await?;
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
            hits += h.iter().filter(|x| truth.contains(&x.chunk_id)).count();
            path = Some(p);
        }
        log(&format!(
            "scope {n_folders}% of rows, query topic {} (path {:?}): recall@10 {:.3}, p50 {:.1} ms p95 {:.1} ms",
            if in_topic {
                "in scope"
            } else {
                "outside scope"
            },
            path,
            hits as f64 / (nq * 10) as f64,
            pct(&lat, 0.5),
            pct(&lat, 0.95)
        ));
    }

    // ---- selective scope (1 folder = 1 % of rows → exact filtered scan) ----
    {
        let t = store
            .open_table("bench_1m", a.dims, base_cfg.clone())
            .await?;
        let scope = Scope {
            collections: vec![collection],
            folders: Some(vec![ids_for(0).2]),
        };
        let mut lat = Vec::new();
        let mut path = None;
        for (_, v, _) in queries.iter().take(50) {
            let s = Instant::now();
            let (_, p) = t.search(v, &scope, 50).await?;
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
            path = Some(p);
        }
        log(&format!(
            "selective scope (1 folder, 1% rows, path {:?}): p50 {:.1} ms p95 {:.1} ms",
            path,
            pct(&lat, 0.5),
            pct(&lat, 0.95)
        ));
    }

    // ---- incremental write at 1M (group commit of 1k new chunks over 250 files) ----
    {
        let t = store
            .open_table("bench_1m", a.dims, base_cfg.clone())
            .await?;
        let base = a.rows + 10_000_000;
        let rows: Vec<VectorRow> = (0..1000)
            .map(|j| {
                let (chunk_id, file_id, folder_id, text_hash) = ids_for(base + j);
                VectorRow {
                    chunk_id,
                    text_hash,
                    file_id,
                    folder_id,
                    collection_id: collection,
                    vector: corpus.row(j).to_vec(),
                }
            })
            .collect();
        let files: Vec<FileId> = rows.iter().map(|r| r.file_id).collect();
        let s = Instant::now();
        t.apply(&sr_core::index::lance::VectorBatch {
            rows: rows.clone(),
            replace_files: files.clone(),
            purge_folders: vec![],
            seq: (a.rows + 1) as i64,
        })
        .await?;
        let first = s.elapsed().as_secs_f64() * 1000.0;
        let s = Instant::now();
        t.apply(&sr_core::index::lance::VectorBatch {
            rows,
            replace_files: files,
            purge_folders: vec![],
            seq: (a.rows + 2) as i64,
        })
        .await?;
        let replay = s.elapsed().as_secs_f64() * 1000.0;
        log(&format!(
            "incremental merge_insert at {} rows: 1k new chunks {first:.0} ms; same batch replayed {replay:.0} ms",
            a.rows
        ));
    }
    log(&format!(
        "process memory MiB (peak working set, current): {}",
        peak_rss_mb()
    ));
    Ok(())
}
