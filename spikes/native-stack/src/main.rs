//! M0-B native-stack spike: lancedb + tantivy + fastembed(ORT) + rusqlite(bundled) in one exe.
//!
//! Subcommands:
//!   e2e <model_dir> <onnx_file> [n_chunks]   test 1 (+ module report, test 4); IVF_HNSW_SQ + IVF_PQ queried
//!   embed <model_dir> <onnx_file> <in.json> <out.json>   test 11 (user-defined, CLS)
//!   builtin <in.json> <out.json>             test 11 (built-in BGESmallENV15, needs --features builtin)
//!   fts5                                     test 12
//!   tantivy-gc <dir> [commits] [hold_reader 0|1]   test 7
//!   lance1051 <dir>                          test 8
//!   modules                                  test 4 (no ORT init)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::index::Index;
use lancedb::index::vector::{IvfHnswSqIndexBuilder, IvfPqIndexBuilder};
use lancedb::query::{ExecutableQuery, QueryBase, Select};

const DIM: i32 = 384;

// ---------------------------------------------------------------------------------------------
// Win32 helpers: peak RSS + loaded-module enumeration
// ---------------------------------------------------------------------------------------------

fn peak_rss_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
        c.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    }
}

fn loaded_modules() -> Vec<String> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleFileNameExW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let p = GetCurrentProcess();
        let mut mods: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
        let mut needed = 0u32;
        EnumProcessModules(
            p,
            mods.as_mut_ptr(),
            (mods.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        );
        let n = needed as usize / std::mem::size_of::<HMODULE>();
        let mut out = Vec::new();
        for m in &mods[..n.min(mods.len())] {
            let mut buf = [0u16; 1024];
            let len = GetModuleFileNameExW(p, *m, buf.as_mut_ptr(), buf.len() as u32);
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }
}

fn module_report(tag: &str) {
    let mods = loaded_modules();
    let onnx: Vec<_> = mods
        .iter()
        .filter(|m| m.to_ascii_lowercase().contains("onnx"))
        .collect();
    println!(
        "[modules:{tag}] total={} onnx-matching={:?}",
        mods.len(),
        onnx
    );
    if std::env::var("SPIKE_LIST_MODULES").is_ok() {
        for m in &mods {
            println!("    {m}");
        }
    }
}

/// Optional hardening (M8): restrict DLL search to System32 + app dir before anything loads.
fn harden_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
    };
    if std::env::var("SPIKE_NO_HARDEN").is_ok() {
        println!("[dll] SetDefaultDllDirectories NOT called");
        return;
    }
    let ok = unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)
    };
    println!("[dll] SetDefaultDllDirectories(SYSTEM32|APPLICATION_DIR) ok={}", ok != 0);
}

// ---------------------------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------------------------

fn load_user_defined(model_dir: &Path, onnx_file: &str) -> Result<TextEmbedding> {
    let rd = |f: &str| std::fs::read(model_dir.join(f)).with_context(|| format!("read {f}"));
    let model = UserDefinedEmbeddingModel::new(
        rd(onnx_file)?,
        TokenizerFiles {
            tokenizer_file: rd("tokenizer.json")?,
            config_file: rd("config.json")?,
            special_tokens_map_file: rd("special_tokens_map.json")?,
            tokenizer_config_file: rd("tokenizer_config.json")?,
        },
    )
    .with_pooling(Pooling::Cls);
    let opts = InitOptionsUserDefined::new().with_max_length(512);
    TextEmbedding::try_new_from_user_defined(model, opts).map_err(|e| anyhow!("{e}"))
}

fn cmd_embed(model_dir: &str, onnx: &str, input: &str, output: &str) -> Result<()> {
    harden_dll_search();
    let texts: Vec<String> = serde_json::from_str(&std::fs::read_to_string(input)?)?;
    let t = Instant::now();
    let mut m = load_user_defined(Path::new(model_dir), onnx)?;
    println!("[embed] model load {:?}", t.elapsed());
    let v = m.embed(&texts, Some(16)).map_err(|e| anyhow!("{e}"))?;
    std::fs::write(output, serde_json::to_string(&v)?)?;
    println!("[embed] wrote {} vectors dim={}", v.len(), v[0].len());
    module_report("after-embed");
    Ok(())
}

#[cfg(feature = "builtin")]
fn cmd_builtin(input: &str, output: &str) -> Result<()> {
    use fastembed::{EmbeddingModel, TextInitOptions};
    let texts: Vec<String> = serde_json::from_str(&std::fs::read_to_string(input)?)?;
    let info = TextEmbedding::get_model_info(&EmbeddingModel::BGESmallENV15).map_err(|e| anyhow!("{e}"))?;
    println!(
        "[builtin] model_code={} file={} default_pooling={:?}",
        info.model_code,
        info.model_file,
        TextEmbedding::get_default_pooling_method(&EmbeddingModel::BGESmallENV15)
    );
    let mut m = TextEmbedding::try_new(
        TextInitOptions::new(EmbeddingModel::BGESmallENV15).with_cache_dir(PathBuf::from("target/hf-cache")),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let v = m.embed(&texts, Some(16)).map_err(|e| anyhow!("{e}"))?;
    std::fs::write(output, serde_json::to_string(&v)?)?;
    println!("[builtin] wrote {} vectors", v.len());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Synthetic corpus
// ---------------------------------------------------------------------------------------------

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
}

const TOPICS: &[(&str, &[&str])] = &[
    ("database", &["sqlite", "transaction", "index", "migration", "query planner", "WAL journal", "vacuum", "foreign key"]),
    ("networking", &["TCP socket", "TLS handshake", "HTTP request", "proxy", "DNS lookup", "retry backoff", "timeout", "certificate"]),
    ("filesystem", &["directory walker", "file watcher", "symlink", "path canonicalization", "mtime", "rename event", "NTFS stream", "lock file"]),
    ("machine learning", &["embedding model", "tokenizer", "attention layer", "quantization", "batch inference", "cosine similarity", "ONNX graph", "pooling"]),
    ("user interface", &["settings page", "progress bar", "toast notification", "wizard step", "dark theme", "keyboard shortcut", "tray icon", "dialog"]),
    ("security", &["access token", "sandbox", "path traversal", "credential manager", "DACL", "signature", "encryption key", "audit log"]),
    ("cooking", &["sourdough bread", "tomato sauce", "roasted garlic", "olive oil", "baking temperature", "fresh basil", "simmering broth", "kneading dough"]),
    ("astronomy", &["red giant star", "orbital period", "telescope mirror", "dark matter", "exoplanet transit", "solar flare", "lunar eclipse", "galaxy cluster"]),
];
const VERBS: &[&str] = &["handles", "configures", "validates", "optimizes", "breaks", "monitors", "caches", "rebuilds", "explains", "measures"];
const CONNECT: &[&str] = &["when", "after", "before", "because", "while", "unless"];

fn synth_chunks(n: usize) -> Vec<(String, String, String)> {
    // (chunk_id, file_id, text)
    let mut r = Lcg(42);
    (0..n)
        .map(|i| {
            let (topic, terms) = TOPICS[i % TOPICS.len()];
            let mut s = format!("Notes on {topic}. ");
            for _ in 0..(4 + r.next() as usize % 4) {
                s.push_str(&format!(
                    "The {} {} the {} {} the {} is {}. ",
                    r.pick(terms),
                    r.pick(VERBS),
                    r.pick(terms),
                    r.pick(CONNECT),
                    r.pick(terms),
                    r.pick(&["slow", "fast", "broken", "stable", "large", "new", "missing", "stale"])
                ));
            }
            s.push_str(&format!("Reference code TKN{:05}.", i));
            (format!("c{i:06}"), format!("f{:05}", i / 4), s)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Test 1: end-to-end
// ---------------------------------------------------------------------------------------------

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("chunk_id", DataType::Utf8, false),
        Field::new("file_id", DataType::Utf8, false),
        Field::new("text_hash", DataType::Utf8, false),
        Field::new("seq", DataType::Int64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), DIM),
            true,
        ),
    ]))
}

fn batch(rows: &[(String, String, String)], vecs: &[Vec<f32>], seq: i64) -> Result<RecordBatch> {
    let s = schema();
    Ok(RecordBatch::try_new(
        s,
        vec![
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.0.clone()))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.1.clone()))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:016x}", fxhash(&r.2))),
            )),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|_| seq))),
            Arc::new(FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                vecs.iter().map(|v| Some(v.iter().map(|x| Some(*x)).collect::<Vec<_>>())),
                DIM,
            )),
        ],
    )?)
}

fn reader(b: RecordBatch) -> Box<dyn RecordBatchReader + Send> {
    let s = b.schema();
    Box::new(RecordBatchIterator::new(vec![Ok(b)], s))
}

fn fxhash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

async fn lance_ids(
    tbl: &lancedb::Table,
    q: &[f32],
    k: usize,
    flat: bool,
) -> Result<Vec<String>> {
    let mut vq = tbl
        .vector_search(q)?
        .distance_type(DistanceType::Cosine)
        .select(Select::columns(&["chunk_id"]))
        .limit(k);
    if flat {
        vq = vq.bypass_vector_index();
    } else {
        vq = vq.nprobes(8);
    }
    let batches: Vec<RecordBatch> = vq.execute().await?.try_collect().await?;
    let mut out = Vec::new();
    for b in batches {
        let col = b
            .column_by_name("chunk_id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .clone();
        for i in 0..col.len() {
            out.push(col.value(i).to_string());
        }
    }
    Ok(out)
}

fn rrf(lists: &[&[String]], k: f64, top: usize) -> Vec<(String, f64)> {
    let mut m: HashMap<String, f64> = HashMap::new();
    for l in lists {
        for (rank, id) in l.iter().enumerate() {
            *m.entry(id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    v.truncate(top);
    v
}

async fn cmd_e2e(model_dir: &str, onnx: &str, n: usize) -> Result<()> {
    harden_dll_search();
    module_report("start");
    let t_all = Instant::now();
    let work = PathBuf::from("target/e2e-data");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    // --- rusqlite (bundled) catalog presence check
    let db = rusqlite::Connection::open(work.join("catalog.db"))?;
    let sqlite_ver: String = db.query_row("select sqlite_version()", [], |r| r.get(0))?;
    db.execute_batch("create table chunk_text(text_hash text primary key, body text);")?;

    // --- embed
    let chunks = synth_chunks(n);
    let t = Instant::now();
    let mut model = load_user_defined(Path::new(model_dir), onnx)?;
    let load_ms = ms(t.elapsed());
    module_report("after-ort-init");
    let texts: Vec<&str> = chunks.iter().map(|c| c.2.as_str()).collect();
    let t = Instant::now();
    let vecs = model.embed(&texts, Some(64)).map_err(|e| anyhow!("{e}"))?;
    let embed_ms = ms(t.elapsed());
    println!(
        "[embed] model_load={load_ms:.0}ms embed {n} chunks={embed_ms:.0}ms ({:.1} chunks/s)",
        n as f64 / (embed_ms / 1000.0)
    );
    {
        let tx = db.unchecked_transaction()?;
        for c in &chunks {
            tx.execute(
                "insert or ignore into chunk_text values (?1, ?2)",
                (format!("{:016x}", fxhash(&c.2)), &c.2),
            )?;
        }
        tx.commit()?;
    }

    // --- lance: one table per embedding profile
    let uri = work.join("lance");
    let conn = lancedb::connect(uri.to_str().unwrap()).execute().await?;
    let t = Instant::now();
    let tbl = conn
        .create_table("profile_bge_small_en_v15_384", reader(batch(&chunks, &vecs, 1)?))
        .execute()
        .await?;
    let write_ms = ms(t.elapsed());
    let t = Instant::now();
    tbl.create_index(
        &["vector"],
        Index::IvfHnswSq(
            IvfHnswSqIndexBuilder::default()
                .distance_type(DistanceType::Cosine)
                .num_partitions(4),
        ),
    )
    .execute()
    .await?;
    let hnsw_ms = ms(t.elapsed());
    tbl.create_index(&["text_hash"], Index::BTree(Default::default()))
        .execute()
        .await?;
    println!(
        "[lance] write {n} rows={write_ms:.0}ms  IVF_HNSW_SQ build={hnsw_ms:.0}ms  indices={:?}",
        tbl.list_indices().await?.iter().map(|i| format!("{}:{:?}", i.name, i.index_type)).collect::<Vec<_>>()
    );

    // IVF_PQ in a second table to time it too
    let tbl_pq = conn
        .create_table("profile_pq_check", reader(batch(&chunks, &vecs, 1)?))
        .execute()
        .await?;
    let t = Instant::now();
    tbl_pq
        .create_index(
            &["vector"],
            Index::IvfPq(
                IvfPqIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(8)
                    .num_sub_vectors(48),
            ),
        )
        .execute()
        .await?;
    println!("[lance] IVF_PQ build (8 parts, 48 subvec)={:.0}ms", ms(t.elapsed()));

    // --- tantivy
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{STORED, STRING, Schema as TSchema, TEXT, Value};
    use tantivy::{Index as TIndex, TantivyDocument, doc};
    let mut sb = TSchema::builder();
    let f_chunk = sb.add_text_field("chunk_id", STRING | STORED);
    let f_file = sb.add_text_field("file_id", STRING);
    let f_body = sb.add_text_field("body", TEXT);
    let tschema = sb.build();
    let tdir = work.join("tantivy");
    std::fs::create_dir_all(&tdir)?;
    let tindex = TIndex::create_in_dir(&tdir, tschema)?;
    let t = Instant::now();
    let mut w = tindex.writer(128_000_000)?;
    for c in &chunks {
        w.add_document(doc!(f_chunk => c.0.clone(), f_file => c.1.clone(), f_body => c.2.clone()))?;
    }
    let mut pc = w.prepare_commit()?;
    pc.set_payload("seq=1");
    pc.commit()?;
    println!("[tantivy] index {n} docs + commit={:.0}ms", ms(t.elapsed()));
    let treader = tindex.reader()?;
    let qp = QueryParser::for_index(&tindex, vec![f_body]);

    // --- queries
    let queries = [
        "how does the sqlite query planner use an index",
        "TLS certificate validation fails behind a proxy",
        "file watcher misses rename events on NTFS",
        "quantization of the embedding model hurts cosine similarity",
        "wizard step shows a progress bar",
        "credential manager stores the access token",
        "sourdough bread baking temperature",
        "exoplanet transit observed by telescope",
        "Reference code TKN01234",
        "retry backoff after DNS lookup timeout",
    ];
    let qprefix = "Represent this sentence for searching relevant passages: ";
    let (mut lat_embed, mut lat_ann, mut lat_flat, mut lat_bm25, mut lat_total) =
        (vec![], vec![], vec![], vec![], vec![]);
    let mut recall_hits = 0usize;
    let mut recall_total = 0usize;
    let mut lat_pq = vec![];
    let mut recall_pq_hits = 0usize;
    for round in 0..5 {
        for q in &queries {
            let t0 = Instant::now();
            let qv = model
                .embed(&[format!("{qprefix}{q}")], None)
                .map_err(|e| anyhow!("{e}"))?
                .remove(0);
            let e = t0.elapsed();
            let t1 = Instant::now();
            let ann = lance_ids(&tbl, &qv, 50, false).await?;
            let a = t1.elapsed();
            let t2 = Instant::now();
            let searcher = treader.searcher();
            let query = qp.parse_query_lenient(q).0;
            let top = searcher.search(&query, &TopDocs::with_limit(50).order_by_score())?;
            let mut bm25 = Vec::new();
            for (_s, addr) in top {
                let d: TantivyDocument = searcher.doc(addr)?;
                bm25.push(d.get_first(f_chunk).unwrap().as_str().unwrap().to_string());
            }
            let b = t2.elapsed();
            let fused = rrf(&[&ann, &bm25], 60.0, 10);
            // hydrate snippets from catalog (part of a real query)
            let total = t0.elapsed();
            let t3 = Instant::now();
            let flat = lance_ids(&tbl, &qv, 10, true).await?;
            let f = t3.elapsed();
            recall_total += flat.len();
            recall_hits += flat.iter().filter(|id| ann[..10.min(ann.len())].contains(id)).count();
            let t4 = Instant::now();
            let pq = lance_ids(&tbl_pq, &qv, 10, false).await?;
            let pqd = t4.elapsed();
            recall_pq_hits += flat.iter().filter(|id| pq.contains(id)).count();
            if round == 0 {
                println!(
                    "  q={q:?} top3={:?}",
                    fused.iter().take(3).map(|x| &x.0).collect::<Vec<_>>()
                );
            }
            if round > 0 {
                lat_embed.push(ms(e));
                lat_ann.push(ms(a));
                lat_bm25.push(ms(b));
                lat_flat.push(ms(f));
                lat_total.push(ms(total));
                lat_pq.push(ms(pqd));
            }
        }
    }
    println!(
        "[query] p50/p95 ms: embed {:.1}/{:.1}  lance-ANN {:.1}/{:.1}  lance-flat {:.1}/{:.1}  bm25 {:.2}/{:.2}  hybrid-total {:.1}/{:.1}",
        pct(&mut lat_embed.clone(), 0.5), pct(&mut lat_embed, 0.95),
        pct(&mut lat_ann.clone(), 0.5), pct(&mut lat_ann, 0.95),
        pct(&mut lat_flat.clone(), 0.5), pct(&mut lat_flat, 0.95),
        pct(&mut lat_bm25.clone(), 0.5), pct(&mut lat_bm25, 0.95),
        pct(&mut lat_total.clone(), 0.5), pct(&mut lat_total, 0.95),
    );
    println!(
        "[query] IVF_HNSW_SQ recall@10 vs flat = {:.3}   IVF_PQ recall@10 vs flat = {:.3}  IVF_PQ ANN p50/p95 {:.1}/{:.1} ms",
        recall_hits as f64 / recall_total as f64,
        recall_pq_hits as f64 / recall_total as f64,
        pct(&mut lat_pq.clone(), 0.5), pct(&mut lat_pq, 0.95),
    );

    // --- merge_insert after optimize with scalar index (cf. lancedb#3177; design relies on it)
    tbl.optimize(lancedb::table::OptimizeAction::All).await?;
    let upd: Vec<_> = chunks[..100]
        .iter()
        .map(|c| (c.0.clone(), c.1.clone(), format!("{} UPDATED", c.2)))
        .chain((0..50).map(|i| (format!("new{i:04}"), "fnew".into(), format!("new text {i}"))))
        .collect();
    let upd_v: Vec<Vec<f32>> = (0..upd.len()).map(|i| vecs[i % vecs.len()].clone()).collect();
    let mut mi = tbl.merge_insert(&["chunk_id"]);
    mi.when_matched_update_all(None).when_not_matched_insert_all();
    let res = mi.execute(reader(batch(&upd, &upd_v, 2)?)).await?;
    let cnt = tbl.count_rows(None).await?;
    let cnt_seq2 = tbl.count_rows(Some("seq = 2".into())).await?;
    println!(
        "[lance] merge_insert after optimize+BTREE: result={res:?} rows={cnt} (expect {}) seq2_rows={cnt_seq2} (expect 150)",
        n + 50
    );
    // delete by file_id + text_hash lookup through BTREE
    tbl.delete("file_id = 'f00000'").await?;
    let h = format!("{:016x}", fxhash(&chunks[10].2));
    let hits = tbl.count_rows(Some(format!("text_hash = '{h}'"))).await?;
    println!("[lance] after delete rows={} text_hash lookup hits={hits}", tbl.count_rows(None).await?);
    let _ = Float32Array::from(vec![0.0f32]); // keep import used

    module_report("end");
    println!(
        "[summary] sqlite={sqlite_ver} total_wall={:.1}s peak_rss={:.0}MB",
        t_all.elapsed().as_secs_f64(),
        peak_rss_mb()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Test 12: FTS5
// ---------------------------------------------------------------------------------------------

fn cmd_fts5() -> Result<()> {
    let db = rusqlite::Connection::open_in_memory()?;
    let v: String = db.query_row("select sqlite_version()", [], |r| r.get(0))?;
    let opts: Vec<String> = db
        .prepare("pragma compile_options")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    println!("sqlite {v}; compile options with FTS/THREAD: {:?}",
        opts.iter().filter(|o| o.contains("FTS") || o.contains("THREADSAFE") || o.contains("SECURE_DELETE")).collect::<Vec<_>>());
    db.execute_batch(
        "create virtual table paths using fts5(path, tokenize='trigram');
         create virtual table docs using fts5(title, body);
         insert into paths values ('src/indexer/lance_store.rs'),('docs/02-architecture.md'),('C:/Users/me/Notes/recipes/sourdough.md');
         insert into docs values ('Sourdough','kneading dough and baking temperature'),('Lance','vector index IVF HNSW');",
    )?;
    let p: Vec<String> = db
        .prepare("select path from paths where paths match ?1")?
        .query_map(["ance_st"], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let d: Vec<(String, f64)> = db
        .prepare("select title, bm25(docs) from docs where docs match ?1 order by rank")?
        .query_map(["baking OR vector"], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    println!("trigram match 'ance_st' -> {p:?}");
    println!("fts5 bm25 'baking OR vector' -> {d:?}");
    anyhow::ensure!(p.len() == 1 && d.len() == 2, "unexpected FTS5 results");
    println!("FTS5 PASS");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Test 7: tantivy commit/merge/GC loop
// ---------------------------------------------------------------------------------------------

fn cmd_tantivy_gc(dir: &str, commits: usize, hold_reader: bool) -> Result<()> {
    use tantivy::schema::{STORED, STRING, Schema as TSchema, TEXT};
    use tantivy::{Index as TIndex, ReloadPolicy, Term, doc};
    let dir = PathBuf::from(dir);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let mut sb = TSchema::builder();
    let f_file = sb.add_text_field("file_id", STRING | STORED);
    let f_body = sb.add_text_field("body", TEXT);
    let index = TIndex::create_in_dir(&dir, sb.build())?;
    let mut w = index.writer_with_num_threads(2, 64_000_000)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let chunks = synth_chunks(400);
    let mut commit_err = 0usize;
    let mut gc_failed_total = 0usize;
    let mut gc_deleted_total = 0usize;
    let mut errors: Vec<String> = vec![];
    let mut held = None;
    let t = Instant::now();
    for i in 0..commits {
        // delete the docs of 5 earlier files, re-add them (an "update"), add 20 new docs
        for j in 0..5 {
            let fid = format!("f{:05}", (i * 7 + j) % 100);
            w.delete_term(Term::from_field_text(f_file, &fid));
        }
        for j in 0..40 {
            let c = &chunks[(i * 40 + j) % chunks.len()];
            w.add_document(doc!(f_file => c.1.clone(), f_body => c.2.clone()))?;
        }
        let mut pc = match w.prepare_commit() {
            Ok(p) => p,
            Err(e) => {
                commit_err += 1;
                errors.push(format!("prepare_commit#{i}: {e}"));
                continue;
            }
        };
        pc.set_payload(&format!("seq={i}"));
        if let Err(e) = pc.commit() {
            commit_err += 1;
            errors.push(format!("commit#{i}: {e}"));
        }
        if let Err(e) = reader.reload() {
            errors.push(format!("reload#{i}: {e}"));
        }
        // hold a searcher (mmaps) across the next commit to emulate concurrent queries
        held = if hold_reader { Some(reader.searcher()) } else { None };
        if i % 10 == 9 {
            match w.garbage_collect_files().wait() {
                Ok(r) => {
                    gc_deleted_total += r.deleted_files.len();
                    gc_failed_total += r.failed_to_delete_files.len();
                }
                Err(e) => errors.push(format!("gc#{i}: {e}")),
            }
        }
    }
    let loop_s = t.elapsed().as_secs_f64();
    drop(held);
    // explicit final merge into one segment, then GC with no searcher held
    let ids = index.searchable_segment_ids()?;
    let segs_before = ids.len();
    if ids.len() > 1 {
        if let Err(e) = w.merge(&ids).wait() {
            errors.push(format!("final merge: {e}"));
        }
    }
    w.commit()?;
    reader.reload()?;
    drop(reader);
    let mut final_failed = 0;
    let mut retries = 0;
    for attempt in 0..5 {
        let r = w.garbage_collect_files().wait()?;
        gc_deleted_total += r.deleted_files.len();
        final_failed = r.failed_to_delete_files.len();
        if final_failed == 0 {
            break;
        }
        retries = attempt + 1;
        println!("  final GC attempt {attempt}: failed={:?}", r.failed_to_delete_files);
        std::thread::sleep(Duration::from_millis(200 * (attempt as u64 + 1)));
    }
    w.wait_merging_threads()?;
    let files = std::fs::read_dir(&dir)?.count();
    println!(
        "[tantivy-gc] commits={commits} hold_reader={hold_reader} loop={loop_s:.1}s commit_errors={commit_err} \
         gc_deleted={gc_deleted_total} gc_failed_during_loop={gc_failed_total} segs_before_final_merge={segs_before} \
         final_gc_failed={final_failed} final_gc_retries={retries} files_left={files}"
    );
    for e in &errors {
        println!("  ERROR {e}");
    }
    // verify doc count is sane
    let idx2 = TIndex::open_in_dir(&dir)?;
    let n = idx2.reader()?.searcher().num_docs();
    println!("[tantivy-gc] reopened num_docs={n}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Test 8: lancedb#1051 (memory:// on Windows) + Windows path forms
// ---------------------------------------------------------------------------------------------

async fn cmd_lance1051(dir: &str) -> Result<()> {
    // std canonicalize returns a \\?\ verbatim path; strip it so the plain forms are what users type.
    let canon = std::fs::canonicalize(".")?.display().to_string();
    let base = PathBuf::from(canon.strip_prefix(r"\\?\").unwrap_or(&canon)).join(dir);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base)?;
    let vecs: Vec<Vec<f32>> = (0..64).map(|i| vec![i as f32 / 64.0; DIM as usize]).collect();
    let rows: Vec<_> = (0..64).map(|i| (format!("c{i}"), "f0".to_string(), format!("t{i}"))).collect();
    let b = base.display().to_string();
    let fwd = b.replace('\\', "/");
    let unc = format!(r"\\?\{}", b);
    let long = base.join("a".repeat(120)).join("b".repeat(120));
    let uris = vec![
        ("memory://foo (issue #1051)".to_string(), "memory://foo".to_string()),
        ("memory:// (no name)".into(), "memory://".into()),
        ("abs backslash".into(), format!(r"{b}\abs")),
        ("abs forward slash".into(), format!("{fwd}/fwd")),
        ("file:/// URI".into(), format!("file:///{fwd}/fileuri")),
        ("relative".into(), format!("{dir}/rel")),
        (r"\\?\ verbatim".into(), format!(r"{unc}\verbatim")),
        ("non-ASCII path".into(), format!(r"{b}\ünïcødé 数据")),
        ("long path >260".into(), long.display().to_string()),
        ("//?/ fwd-slash verbatim".into(), format!("//?/{fwd}/fwdverb")),
        ("UNC localhost admin share".into(), format!(r"\\localhost\{}$\{}\unc", &b[..1], &b[3..])),
    ];
    for (label, uri) in uris {
        let fut = async {
            let conn = lancedb::connect(&uri).execute().await?;
            let t = conn.create_table("my_table", reader(batch(&rows, &vecs, 1)?)).execute().await?;
            t.add(reader(batch(&rows, &vecs, 2)?)).execute().await?;
            let conn2 = lancedb::connect(&uri).execute().await?;
            let names = conn2.table_names().execute().await?;
            let n = if uri.starts_with("memory") { t.count_rows(None).await? } else {
                conn2.open_table("my_table").execute().await?.count_rows(None).await?
            };
            let _ = names;
            Ok::<usize, anyhow::Error>(n)
        };
        // A panic inside lance must not hide the remaining cases.
        let r = match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await {
            Ok(r) => r,
            Err(p) => Err(anyhow!(
                "PANIC: {}",
                p.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default()
            )),
        };
        match r {
            Ok(n) => println!("[lance1051] {label:<28} OK rows={n}   uri={uri}"),
            Err(e) => println!("[lance1051] {label:<28} FAIL {e}   uri={uri}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match a.get(1).map(|s| s.as_str()) {
        Some("e2e") => rt.block_on(cmd_e2e(
            &a[2],
            &a[3],
            a.get(4).map(|s| s.parse().unwrap()).unwrap_or(2000),
        )),
        Some("embed") => cmd_embed(&a[2], &a[3], &a[4], &a[5]),
        #[cfg(feature = "builtin")]
        Some("builtin") => cmd_builtin(&a[2], &a[3]),
        Some("fts5") => cmd_fts5(),
        Some("tantivy-gc") => cmd_tantivy_gc(
            &a[2],
            a.get(3).map(|s| s.parse().unwrap()).unwrap_or(200),
            a.get(4).map(|s| s == "1").unwrap_or(true),
        ),
        Some("lance1051") => rt.block_on(cmd_lance1051(&a[2])),
        Some("modules") => {
            harden_dll_search();
            module_report("modules");
            Ok(())
        }
        _ => {
            eprintln!("usage: see src/main.rs header");
            Ok(())
        }
    }
}
