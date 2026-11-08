//! Lance vector store (D6, C5, Amendments 3.8 and 3.12).
//!
//! One table per vector index (one embedding profile): `chunk_id, text_hash, file_id, folder_id,
//! collection_id, seq, vector`. No text and no paths. Scalar indexes: BTREE on `chunk_id`,
//! `text_hash`, `file_id`; BITMAP on `collection_id`, `folder_id`. IVF_HNSW_SQ is built once the
//! table has [`AnnConfig::min_rows`] rows and retrained when the row count doubles. Queries whose
//! scope keeps fewer than [`AnnConfig::flat_selectivity`] of the rows use an exact filtered scan.
//!
//! **Same-commit watermark.** lancedb 0.40 exposes no commit metadata on `merge_insert`, so the
//! watermark lives in a sentinel row (`chunk_id = "~watermark"`, `seq = watermark`) that is
//! upserted *in the same `merge_insert`* as the data. Data and watermark therefore become visible
//! atomically in one Lance version. The sentinel has placeholder filter columns (`"~"`) that no
//! scoped filter can match, so it never surfaces in search, dedupe lookups or counts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::index::Index;
use lancedb::index::scalar::{BTreeIndexBuilder, BitmapIndexBuilder};
use lancedb::index::vector::IvfHnswSqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::{CompactionOptions, OptimizeAction};
use serde::{Deserialize, Serialize};
use sr_common::Result;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, TextHash};

use super::datadir::validate_store_root;
use super::filter::{Filter, Scope};
use crate::guard;

pub const COL_CHUNK_ID: &str = "chunk_id";
pub const COL_TEXT_HASH: &str = "text_hash";
pub const COL_FILE_ID: &str = "file_id";
pub const COL_FOLDER_ID: &str = "folder_id";
pub const COL_COLLECTION_ID: &str = "collection_id";
pub const COL_SEQ: &str = "seq";
pub const COL_VECTOR: &str = "vector";

/// `chunk_id` of the watermark sentinel row.
pub const SENTINEL_ID: &str = "~watermark";
const SENTINEL_FILL: &str = "~";
/// Table config key: rows in the table when the ANN index was last trained.
const CFG_ANN_TRAINED_ROWS: &str = "sr.ann_trained_rows";
const VECTOR_INDEX_NAME: &str = "vector_idx";
/// Text-hash / chunk-id lookups are batched (≤ 500 literals per filter, §0.A M2).
const LOOKUP_BATCH: usize = 500;

/// ANN and query knobs (TierProfile `ann`, docs/02 §2.8), in the store's own units.
///
/// **No numbers live here** (CO-1): every default comes from `sr_common::tier::ANN_DEFAULT`, the
/// single source of truth (the M1-E benchmark's best setting for 1M × 384 on CPU,
/// docs/research/m1-e-core.md §5/§8). Build one for a tier with `AnnConfig::from(&profile.ann)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnConfig {
    /// Build IVF_HNSW_SQ only at or above this many rows (C5: 50k).
    pub min_rows: usize,
    /// IVF partitions; `None` = `max(1, rows / rows_per_partition)` at build time.
    pub num_partitions: Option<u32>,
    pub rows_per_partition: usize,
    /// HNSW `m` (edges per node) and `ef_construction`.
    pub hnsw_m: u32,
    pub hnsw_ef_construction: u32,
    /// Query: IVF partitions probed.
    pub nprobes: usize,
    /// Query: HNSW search breadth (`ef`). `None` = lance default.
    pub ef: Option<usize>,
    /// Query: re-rank `k × refine_factor` candidates with full-precision vectors.
    pub refine_factor: Option<u32>,
    /// Use an exact filtered scan when the scope keeps fewer than this fraction of rows.
    pub flat_selectivity: f64,
    /// Partial scopes widen `nprobes` and `ef` by `clamp(1/selectivity, min, max)`.
    pub partial_widen_min: f64,
    pub partial_widen_max: f64,
    /// Cap for the widened `ef`.
    pub max_ef: usize,
    /// Retrain when rows ≥ trained_rows × this factor.
    pub retrain_growth: f64,
    /// `optimize(Index)` once unindexed rows exceed this fraction of indexed rows.
    pub reindex_unindexed_fraction: f64,
}

impl Default for AnnConfig {
    fn default() -> Self {
        Self::from(&sr_common::tier::ANN_DEFAULT)
    }
}

impl From<&sr_common::tier::AnnParams> for AnnConfig {
    fn from(p: &sr_common::tier::AnnParams) -> Self {
        use sr_common::tier::AnnIndexType;
        let to_usize = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
        Self {
            // `Flat` = never build an ANN index. IVF_PQ is not implemented in M1; it gets the
            // default IVF_HNSW_SQ (the only kind any tier selects today).
            min_rows: match p.index_type {
                AnnIndexType::Flat => usize::MAX,
                AnnIndexType::IvfHnswSq | AnnIndexType::IvfPq => to_usize(p.min_rows),
            },
            num_partitions: None,
            rows_per_partition: to_usize(u64::from(p.rows_per_partition.max(1))),
            hnsw_m: p.hnsw_m,
            hnsw_ef_construction: p.hnsw_ef_construction,
            nprobes: to_usize(u64::from(p.nprobes.max(1))),
            ef: Some(to_usize(u64::from(p.ef))),
            refine_factor: Some(p.refine_factor),
            flat_selectivity: p.flat_scan_selectivity,
            partial_widen_min: p.partial_widen_min,
            partial_widen_max: p.partial_widen_max,
            max_ef: to_usize(u64::from(p.max_ef)),
            retrain_growth: p.retrain_growth,
            reindex_unindexed_fraction: p.reindex_unindexed_fraction,
        }
    }
}

/// One row to write.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorRow {
    pub chunk_id: ChunkId,
    pub text_hash: TextHash,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    pub vector: Vec<f32>,
}

/// One atomic write: after it commits, the rows of `replace_files` are exactly the `rows` that
/// belong to them, `purge_folders` have no rows, and the watermark is `seq`.
#[derive(Debug, Clone, Default)]
pub struct VectorBatch {
    pub rows: Vec<VectorRow>,
    pub replace_files: Vec<FileId>,
    pub purge_folders: Vec<FolderId>,
    pub seq: i64,
}

/// One dense hit. `distance` is the cosine distance (`1 − cos`).
#[derive(Debug, Clone, PartialEq)]
pub struct DenseHit {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub collection_id: CollectionId,
    pub distance: f32,
}

impl DenseHit {
    /// Cosine similarity.
    pub fn similarity(&self) -> f32 {
        1.0 - self.distance
    }
}

/// Which path a search took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DensePath {
    /// No ANN index yet (small table): exact scan.
    Flat,
    /// Scope too selective for ANN prefiltering: exact filtered scan.
    FlatSelective,
    /// ANN over the whole table (the scope covers every row): no filter.
    Ann,
    /// ANN over a partial scope: pre-filter, with `nprobes` and `ef` widened by 1/selectivity.
    /// (Post-filtering with over-fetch was measured and dropped: when the query's topic lies
    /// outside the scope, its in-scope neighbours rank far beyond any over-fetch.)
    AnnPrefilter,
}

/// What [`VectorTable::maintain`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintainReport {
    pub scalar_indexes_created: bool,
    pub ann_trained: bool,
    pub index_optimized: bool,
}

/// A LanceDB database rooted at a validated plain local path.
#[derive(Clone)]
pub struct VectorStore {
    conn: lancedb::Connection,
    root: PathBuf,
}

impl std::fmt::Debug for VectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorStore")
            .field("root", &self.root)
            .finish()
    }
}

fn internal(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal).with_detail(detail)
}

/// Table names are generated by the engine (`vec_<index_id>`); refuse anything else.
fn check_table_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::InvalidInput).with_detail("bad vector table name"))
    }
}

impl VectorStore {
    /// Open (creating the directory if needed) a store under `root`, which must satisfy the
    /// plain-local-path rule ([`validate_store_root`]).
    pub async fn open(root: &Path) -> Result<Self> {
        let root = validate_store_root(root)?;
        std::fs::create_dir_all(&root).map_err(|e| guard::store_error("create lance dir", e))?;
        let uri = root
            .to_str()
            .ok_or_else(|| internal("lance root not unicode"))?
            .to_string();
        let conn = guard::lance("lance connect", lancedb::connect(&uri).execute()).await?;
        Ok(Self { conn, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn table_names(&self) -> Result<Vec<String>> {
        guard::lance("lance table_names", self.conn.table_names().execute()).await
    }

    /// Open `name`, creating it (with its watermark sentinel at seq 0) if it does not exist.
    pub async fn open_table(&self, name: &str, dims: usize, cfg: AnnConfig) -> Result<VectorTable> {
        check_table_name(name)?;
        if dims == 0 || dims > 8192 {
            return Err(internal(format!("bad vector dims {dims}")));
        }
        let names = self.table_names().await?;
        let table = if names.iter().any(|n| n == name) {
            let t = guard::lance("lance open_table", self.conn.open_table(name).execute()).await?;
            let schema = guard::lance("lance schema", t.schema()).await?;
            let have = vector_dims(&schema)?;
            if have != dims {
                return Err(
                    Error::new(ErrorCode::ModelIncompatible).with_detail(format!(
                        "vector table {name} has {have} dims, embedder has {dims}"
                    )),
                );
            }
            t
        } else {
            let schema = table_schema(dims)?;
            let t = guard::lance(
                "lance create_table",
                self.conn.create_empty_table(name, schema.clone()).execute(),
            )
            .await?;
            let sentinel = sentinel_batch(&schema, dims, 0)?;
            guard::lance("lance add sentinel", t.add(reader(sentinel)).execute()).await?;
            t
        };
        Ok(VectorTable {
            table,
            name: name.to_string(),
            dims,
            cfg,
            schema: table_schema(dims)?,
            counts: Mutex::new(CountCache::default()),
        })
    }

    /// Drop a table and its files (collection removed / retired index GC).
    pub async fn drop_table(&self, name: &str) -> Result<()> {
        check_table_name(name)?;
        guard::lance("lance drop_table", self.conn.drop_table(name, &[])).await
    }
}

#[derive(Debug, Default)]
struct CountCache {
    version: u64,
    by_filter: HashMap<String, usize>,
    /// Whether the vector column has an ANN index at `version`.
    has_ann: Option<bool>,
}

/// One vector index's table.
pub struct VectorTable {
    table: lancedb::Table,
    name: String,
    dims: usize,
    cfg: AnnConfig,
    schema: SchemaRef,
    counts: Mutex<CountCache>,
}

impl std::fmt::Debug for VectorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorTable")
            .field("name", &self.name)
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

fn table_schema(dims: usize) -> Result<SchemaRef> {
    let d = i32::try_from(dims).map_err(|_| internal("dims overflow"))?;
    Ok(Arc::new(Schema::new(vec![
        Field::new(COL_CHUNK_ID, DataType::Utf8, false),
        Field::new(COL_TEXT_HASH, DataType::Utf8, false),
        Field::new(COL_FILE_ID, DataType::Utf8, false),
        Field::new(COL_FOLDER_ID, DataType::Utf8, false),
        Field::new(COL_COLLECTION_ID, DataType::Utf8, false),
        Field::new(COL_SEQ, DataType::Int64, false),
        Field::new(
            COL_VECTOR,
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), d),
            false,
        ),
    ])))
}

fn vector_dims(schema: &Schema) -> Result<usize> {
    match schema.field_with_name(COL_VECTOR).map(|f| f.data_type()) {
        Ok(DataType::FixedSizeList(_, n)) => usize::try_from(*n).map_err(|_| internal("dims")),
        _ => Err(internal("vector column missing or wrong type")),
    }
}

fn reader(b: RecordBatch) -> Box<dyn RecordBatchReader + Send> {
    let s = b.schema();
    Box::new(RecordBatchIterator::new(vec![Ok(b)], s))
}

fn sentinel_strings() -> RowStrings {
    (
        SENTINEL_ID.to_string(),
        SENTINEL_FILL.to_string(),
        SENTINEL_FILL.to_string(),
        SENTINEL_FILL.to_string(),
        SENTINEL_FILL.to_string(),
    )
}

/// Unit vector: never NaN under cosine, harmless for IVF training.
fn sentinel_vector(dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    v[0] = 1.0;
    v
}

fn sentinel_batch(schema: &SchemaRef, dims: usize, seq: i64) -> Result<RecordBatch> {
    rows_batch(
        schema,
        dims,
        &[sentinel_strings()],
        &[sentinel_vector(dims)],
        seq,
    )
}

type RowStrings = (String, String, String, String, String);

fn rows_batch(
    schema: &SchemaRef,
    dims: usize,
    rows: &[RowStrings],
    vecs: &[Vec<f32>],
    seq: i64,
) -> Result<RecordBatch> {
    let d = i32::try_from(dims).map_err(|_| internal("dims overflow"))?;
    let col = |i: usize| -> Arc<dyn Array> {
        Arc::new(StringArray::from_iter_values(rows.iter().map(
            |r| match i {
                0 => r.0.as_str(),
                1 => r.1.as_str(),
                2 => r.2.as_str(),
                3 => r.3.as_str(),
                _ => r.4.as_str(),
            },
        )))
    };
    let flat: Vec<f32> = vecs.iter().flat_map(|v| v.iter().copied()).collect();
    let values = Arc::new(Float32Array::from(flat));
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let list = FixedSizeListArray::try_new(item, d, values, None)
        .map_err(|e| internal(format!("vector array: {e}")))?;
    RecordBatch::try_new(
        schema.clone(),
        vec![
            col(0),
            col(1),
            col(2),
            col(3),
            col(4),
            Arc::new(Int64Array::from(vec![seq; rows.len()])),
            Arc::new(list),
        ],
    )
    .map_err(|e| internal(format!("record batch: {e}")))
}

fn str_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| internal(format!("lance result missing {name}")))
}

fn vec_col(b: &RecordBatch) -> Result<&FixedSizeListArray> {
    b.column_by_name(COL_VECTOR)
        .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>())
        .ok_or_else(|| internal("lance result missing vector"))
}

fn row_vector(list: &FixedSizeListArray, i: usize) -> Option<Vec<f32>> {
    let v = list.value(i);
    v.as_any()
        .downcast_ref::<Float32Array>()
        .map(|a| a.values().to_vec())
}

/// Exclude the sentinel from an unscoped filter.
fn not_sentinel() -> String {
    format!("{COL_CHUNK_ID} != '{SENTINEL_ID}'")
}

impl VectorTable {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn config(&self) -> &AnnConfig {
        &self.cfg
    }

    /// Current Lance version (one per commit).
    pub async fn version(&self) -> Result<u64> {
        guard::lance("lance version", self.table.version()).await
    }

    /// The watermark committed with the data (sentinel row).
    pub async fn watermark(&self) -> Result<i64> {
        let q = self
            .table
            .query()
            .only_if(format!("{COL_CHUNK_ID} = '{SENTINEL_ID}'"))
            .select(Select::columns(&[COL_SEQ]))
            .limit(1);
        let batches: Vec<RecordBatch> = guard::unwind("lance watermark", async {
            let s = q
                .execute()
                .await
                .map_err(|e| guard::store_error("lance watermark", e))?;
            s.try_collect()
                .await
                .map_err(|e| guard::store_error("lance watermark", e))
        })
        .await?;
        for b in batches {
            if let Some(a) = b
                .column_by_name(COL_SEQ)
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                && !a.is_empty()
            {
                return Ok(a.value(0));
            }
        }
        Err(Error::new(ErrorCode::Corrupt)
            .with_detail(format!("vector table {} lost its watermark row", self.name)))
    }

    /// Apply one batch atomically (one `merge_insert`, one Lance commit).
    ///
    /// * New chunk ids are inserted; chunk ids that already exist are left untouched (a chunk id
    ///   identifies immutable content, so its vector cannot have changed).
    /// * Rows of `replace_files` / `purge_folders` that are not in `rows` are deleted.
    /// * The sentinel's `seq` becomes `batch.seq` in the same commit.
    ///
    /// Re-applying the same batch is a no-op apart from the watermark, which is what makes replay
    /// idempotent.
    pub async fn apply(&self, batch: &VectorBatch) -> Result<()> {
        let mut strings: Vec<RowStrings> = Vec::with_capacity(batch.rows.len() + 1);
        let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(batch.rows.len() + 1);
        let mut seen = HashSet::new();
        for r in &batch.rows {
            if r.vector.len() != self.dims {
                return Err(
                    Error::new(ErrorCode::ModelIncompatible).with_detail(format!(
                        "vector has {} dims, table {} has {}",
                        r.vector.len(),
                        self.name,
                        self.dims
                    )),
                );
            }
            if !r.vector.iter().all(|x| x.is_finite()) {
                return Err(internal("non-finite embedding"));
            }
            if !seen.insert(r.chunk_id) {
                continue; // merge_insert rejects duplicate keys in the source
            }
            strings.push((
                r.chunk_id.to_hex(),
                r.text_hash.to_hex(),
                r.file_id.to_string(),
                r.folder_id.to_string(),
                r.collection_id.to_string(),
            ));
            vecs.push(r.vector.clone());
        }
        strings.push(sentinel_strings());
        vecs.push(sentinel_vector(self.dims));
        let data = rows_batch(&self.schema, self.dims, &strings, &vecs, batch.seq)?;

        let mut delete_scope = Vec::new();
        if !batch.replace_files.is_empty() {
            delete_scope.push(Filter::FileIn(batch.replace_files.clone()));
        }
        if !batch.purge_folders.is_empty() {
            delete_scope.push(Filter::FolderIn(batch.purge_folders.clone()));
        }
        // Skip the by-source delete (a scan of the filtered target) when nothing could match.
        let need_delete = if delete_scope.is_empty() {
            false
        } else {
            let f = Filter::Or(delete_scope.clone()).to_sql();
            guard::lance("lance count", self.table.count_rows(Some(f))).await? > 0
        };

        let mut mi = self.table.merge_insert(&[COL_CHUNK_ID]);
        mi.when_matched_update_all(Some(format!("target.{COL_CHUNK_ID} = '{SENTINEL_ID}'")))
            .when_not_matched_insert_all();
        if need_delete {
            mi.when_not_matched_by_source_delete(Some(Filter::Or(delete_scope).to_sql()));
        }
        guard::lance("lance merge_insert", mi.execute(reader(data))).await?;
        Ok(())
    }

    /// Bulk append of new rows plus the watermark (fresh tables, rebuilds, benchmarks). The caller
    /// guarantees none of the chunk ids exist yet. Two commits: data first, then the watermark
    /// (`apply` with no rows), so a crash in between only lowers the watermark (replay re-applies).
    pub async fn bulk_append(&self, rows: &[VectorRow], seq: i64) -> Result<()> {
        if !rows.is_empty() {
            let strings: Vec<RowStrings> = rows
                .iter()
                .map(|r| {
                    (
                        r.chunk_id.to_hex(),
                        r.text_hash.to_hex(),
                        r.file_id.to_string(),
                        r.folder_id.to_string(),
                        r.collection_id.to_string(),
                    )
                })
                .collect();
            let vecs: Vec<Vec<f32>> = rows.iter().map(|r| r.vector.clone()).collect();
            if vecs.iter().any(|v| v.len() != self.dims) {
                return Err(Error::new(ErrorCode::ModelIncompatible).with_detail("dims mismatch"));
            }
            let b = rows_batch(&self.schema, self.dims, &strings, &vecs, seq)?;
            guard::lance("lance add", self.table.add(reader(b)).execute()).await?;
        }
        self.apply(&VectorBatch {
            seq,
            ..Default::default()
        })
        .await
    }

    /// Rows matching `filter` (sentinel excluded). Cached per Lance version.
    pub async fn count(&self, filter: Option<&Filter>) -> Result<usize> {
        let sql = match filter {
            Some(f) => format!("({}) AND {}", f.to_sql(), not_sentinel()),
            None => not_sentinel(),
        };
        let version = self.version().await?;
        {
            let g = self.counts.lock().unwrap_or_else(|p| p.into_inner());
            if g.version == version
                && let Some(n) = g.by_filter.get(&sql)
            {
                return Ok(*n);
            }
        }
        let n = guard::lance("lance count", self.table.count_rows(Some(sql.clone()))).await?;
        let mut g = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        if g.version != version {
            g.version = version;
            g.by_filter.clear();
            g.has_ann = None;
        }
        g.by_filter.insert(sql, n);
        Ok(n)
    }

    async fn collect(&self, q: lancedb::query::Query, what: &str) -> Result<Vec<RecordBatch>> {
        guard::unwind(what, async {
            let s = q.execute().await.map_err(|e| guard::store_error(what, e))?;
            s.try_collect()
                .await
                .map_err(|e| guard::store_error(what, e))
        })
        .await
    }

    /// Vectors already stored for these text hashes (embedding dedupe, §0.A M2).
    pub async fn vectors_by_text_hash(
        &self,
        hashes: &[TextHash],
    ) -> Result<HashMap<TextHash, Vec<f32>>> {
        let mut out = HashMap::new();
        let uniq: Vec<TextHash> = {
            let mut s: Vec<TextHash> = hashes.to_vec();
            s.sort();
            s.dedup();
            s
        };
        for part in uniq.chunks(LOOKUP_BATCH) {
            let q = self
                .table
                .query()
                .only_if(Filter::TextHashIn(part.to_vec()).to_sql())
                .select(Select::columns(&[COL_TEXT_HASH, COL_VECTOR]));
            for b in self.collect(q, "lance text_hash lookup").await? {
                let hs = str_col(&b, COL_TEXT_HASH)?;
                let vs = vec_col(&b)?;
                for i in 0..b.num_rows() {
                    if let (Ok(h), Some(v)) = (hs.value(i).parse::<TextHash>(), row_vector(vs, i)) {
                        out.entry(h).or_insert(v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Stored vectors by chunk id (MMR, verification).
    pub async fn vectors_by_chunk_id(&self, ids: &[ChunkId]) -> Result<HashMap<ChunkId, Vec<f32>>> {
        let mut out = HashMap::new();
        for part in ids.chunks(LOOKUP_BATCH) {
            let q = self
                .table
                .query()
                .only_if(Filter::ChunkIn(part.to_vec()).to_sql())
                .select(Select::columns(&[COL_CHUNK_ID, COL_VECTOR]));
            for b in self.collect(q, "lance chunk lookup").await? {
                let cs = str_col(&b, COL_CHUNK_ID)?;
                let vs = vec_col(&b)?;
                for i in 0..b.num_rows() {
                    if let (Ok(c), Some(v)) = (cs.value(i).parse::<ChunkId>(), row_vector(vs, i)) {
                        out.insert(c, v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// `(chunk_id, file_id)` of every row matching `filter` (verification, tests).
    pub async fn rows(&self, filter: Option<&Filter>) -> Result<Vec<(ChunkId, FileId)>> {
        let sql = match filter {
            Some(f) => format!("({}) AND {}", f.to_sql(), not_sentinel()),
            None => not_sentinel(),
        };
        let q = self
            .table
            .query()
            .only_if(sql)
            .select(Select::columns(&[COL_CHUNK_ID, COL_FILE_ID]));
        let mut out = Vec::new();
        for b in self.collect(q, "lance scan").await? {
            let cs = str_col(&b, COL_CHUNK_ID)?;
            let fs = str_col(&b, COL_FILE_ID)?;
            for i in 0..b.num_rows() {
                if let (Ok(c), Ok(f)) = (cs.value(i).parse(), fs.value(i).parse()) {
                    out.push((c, f));
                }
            }
        }
        Ok(out)
    }

    /// Cached per Lance version (the query path must not re-read index metadata each time).
    async fn has_vector_index(&self) -> Result<bool> {
        let version = self.version().await?;
        {
            let g = self.counts.lock().unwrap_or_else(|p| p.into_inner());
            if g.version == version
                && let Some(h) = g.has_ann
            {
                return Ok(h);
            }
        }
        let idx = guard::lance("lance list_indices", self.table.list_indices()).await?;
        let has = idx
            .iter()
            .any(|i| i.columns.iter().any(|c| c == COL_VECTOR));
        let mut g = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        if g.version != version {
            g.version = version;
            g.by_filter.clear();
        }
        g.has_ann = Some(has);
        Ok(has)
    }

    /// Dense search within `scope`. Returns at most `k` hits ordered by distance and the path used.
    pub async fn search(
        &self,
        query: &[f32],
        scope: &Scope,
        k: usize,
    ) -> Result<(Vec<DenseHit>, DensePath)> {
        if query.len() != self.dims {
            // §2.4: a query vector whose length differs from the index dims is refused.
            return Err(internal(format!(
                "query vector has {} dims, index {} has {}",
                query.len(),
                self.name,
                self.dims
            )));
        }
        if k == 0 || scope.collections.is_empty() {
            return Ok((Vec::new(), DensePath::Flat));
        }
        let filter = scope.filter();
        let total = self.count(None).await?.max(1);
        let in_scope = self.count(Some(&filter)).await?;
        let selectivity = in_scope as f64 / total as f64;
        let path = if !self.has_vector_index().await? {
            DensePath::Flat
        } else if selectivity < self.cfg.flat_selectivity {
            DensePath::FlatSelective
        } else if in_scope >= total {
            DensePath::Ann
        } else {
            DensePath::AnnPrefilter
        };
        let mut vq = guard::blocking("lance vector_search", || {
            self.table
                .vector_search(query)
                .map_err(|e| guard::store_error("lance vector_search", e))
        })?
        .distance_type(DistanceType::Cosine)
        .select(Select::columns(&[
            COL_CHUNK_ID,
            COL_FILE_ID,
            COL_FOLDER_ID,
            COL_COLLECTION_ID,
        ]));
        // The scope covers every row: no filter at all (a prefilter would build a mask over the
        // whole table on every query). The sentinel can then come back; it is skipped below, so
        // fetch one extra.
        let fetch = if path == DensePath::Ann { k + 1 } else { k };
        vq = if path == DensePath::Ann {
            vq
        } else {
            vq.only_if(filter.to_sql())
        }
        .limit(fetch);
        match path {
            DensePath::Ann | DensePath::AnnPrefilter => {
                let k = fetch;
                // A partial scope widens the search by 1/selectivity: in-scope neighbours of a
                // query whose topic is out of scope live in other partitions / graph regions.
                let widen = if path == DensePath::AnnPrefilter {
                    (1.0 / selectivity.max(self.cfg.flat_selectivity)).clamp(
                        self.cfg.partial_widen_min,
                        self.cfg.partial_widen_max.max(self.cfg.partial_widen_min),
                    )
                } else {
                    1.0
                };
                vq = vq.nprobes(((self.cfg.nprobes as f64) * widen).ceil() as usize);
                // lance requires ef ≥ the number of candidates it pulls from HNSW
                // (k × refine_factor), so ef scales with k.
                let pulled = k * self.cfg.refine_factor.map_or(1, |r| r.max(1) as usize);
                if let Some(ef) = self.cfg.ef {
                    let ef = (((ef as f64) * widen).ceil() as usize).min(self.cfg.max_ef.max(ef));
                    vq = vq.ef(ef.max(pulled));
                }
                if let Some(r) = self.cfg.refine_factor {
                    vq = vq.refine_factor(r);
                }
            }
            DensePath::Flat | DensePath::FlatSelective => vq = vq.bypass_vector_index(),
        }
        let batches: Vec<RecordBatch> = guard::unwind("lance search", async {
            let s = vq
                .execute()
                .await
                .map_err(|e| guard::store_error("lance search", e))?;
            s.try_collect()
                .await
                .map_err(|e| guard::store_error("lance search", e))
        })
        .await?;
        let mut hits = Vec::with_capacity(k);
        for b in batches {
            let cs = str_col(&b, COL_CHUNK_ID)?;
            let fs = str_col(&b, COL_FILE_ID)?;
            let ds = str_col(&b, COL_FOLDER_ID)?;
            let ls = str_col(&b, COL_COLLECTION_ID)?;
            let dist = b
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
                .ok_or_else(|| internal("lance result missing _distance"))?;
            for i in 0..b.num_rows() {
                let (Ok(chunk_id), Ok(file_id), Ok(folder_id), Ok(collection_id)) = (
                    cs.value(i).parse(),
                    fs.value(i).parse(),
                    ds.value(i).parse(),
                    ls.value(i).parse(),
                ) else {
                    continue; // the sentinel (unfiltered path) or a malformed row
                };
                // Defence in depth for the unfiltered / post-filtered paths.
                if !scope.collections.contains(&collection_id)
                    || scope
                        .folders
                        .as_ref()
                        .is_some_and(|f| !f.contains(&folder_id))
                {
                    continue;
                }
                hits.push(DenseHit {
                    chunk_id,
                    file_id,
                    folder_id,
                    collection_id,
                    distance: dist.value(i),
                });
            }
        }
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        hits.truncate(k);
        Ok((hits, path))
    }

    async fn ann_trained_rows(&self) -> Result<Option<usize>> {
        let m = guard::lance("lance manifest", async {
            match self.table.as_native() {
                Some(n) => n.manifest().await.map(Some),
                None => Ok(None),
            }
        })
        .await?;
        Ok(m.and_then(|m| {
            m.config
                .get(CFG_ANN_TRAINED_ROWS)
                .and_then(|v| v.parse().ok())
        }))
    }

    async fn set_ann_trained_rows(&self, rows: usize) -> Result<()> {
        guard::lance("lance update_config", async {
            match self.table.as_native() {
                Some(n) => {
                    n.update_config([(CFG_ANN_TRAINED_ROWS.to_string(), rows.to_string())])
                        .await
                }
                None => Ok(()),
            }
        })
        .await
    }

    /// Index upkeep, called by the writer after a drain (never on the query path):
    /// scalar indexes once data exists; IVF_HNSW_SQ at `min_rows` and retrained on doubling;
    /// `optimize(Index)` when unindexed rows pile up.
    pub async fn maintain(&self) -> Result<MaintainReport> {
        let mut rep = MaintainReport::default();
        let rows = self.count(None).await?;
        if rows == 0 {
            return Ok(rep);
        }
        let indices = guard::lance("lance list_indices", self.table.list_indices()).await?;
        let has = |col: &str| indices.iter().any(|i| i.columns.iter().any(|c| c == col));
        for (col, bitmap) in [
            (COL_CHUNK_ID, false),
            (COL_TEXT_HASH, false),
            (COL_FILE_ID, false),
            (COL_COLLECTION_ID, true),
            (COL_FOLDER_ID, true),
        ] {
            if !has(col) {
                let idx = if bitmap {
                    Index::Bitmap(BitmapIndexBuilder::default())
                } else {
                    Index::BTree(BTreeIndexBuilder::default())
                };
                guard::lance(
                    "lance scalar index",
                    self.table.create_index(&[col], idx).execute(),
                )
                .await?;
                rep.scalar_indexes_created = true;
            }
        }
        let trained = self.ann_trained_rows().await?;
        let want_train = if rows < self.cfg.min_rows {
            false
        } else if !has(COL_VECTOR) {
            true
        } else {
            trained.is_none_or(|t| (rows as f64) >= (t as f64) * self.cfg.retrain_growth)
        };
        if want_train {
            self.train_ann(rows).await?;
            rep.ann_trained = true;
        } else if !rep.scalar_indexes_created {
            // Fold unindexed rows into existing indexes when they exceed the threshold.
            let mut unindexed = 0usize;
            let mut indexed = 0usize;
            for i in &indices {
                if let Some(s) =
                    guard::lance("lance index_stats", self.table.index_stats(&i.name)).await?
                {
                    unindexed = unindexed.max(s.num_unindexed_rows);
                    indexed = indexed.max(s.num_indexed_rows);
                }
            }
            if unindexed > 0
                && (unindexed as f64)
                    > (indexed.max(1) as f64) * self.cfg.reindex_unindexed_fraction
            {
                guard::lance(
                    "lance optimize index",
                    self.table
                        .optimize(OptimizeAction::Index(Default::default())),
                )
                .await?;
                rep.index_optimized = true;
            }
        }
        Ok(rep)
    }

    /// (Re)train IVF_HNSW_SQ over the current rows.
    pub async fn train_ann(&self, rows: usize) -> Result<()> {
        let parts = self.cfg.num_partitions.unwrap_or_else(|| {
            u32::try_from((rows / self.cfg.rows_per_partition.max(1)).max(1)).unwrap_or(u32::MAX)
        });
        let b = IvfHnswSqIndexBuilder::default()
            .distance_type(DistanceType::Cosine)
            .num_partitions(parts)
            .num_edges(self.cfg.hnsw_m)
            .ef_construction(self.cfg.hnsw_ef_construction);
        guard::lance(
            "lance ann index",
            self.table
                .create_index(&[COL_VECTOR], Index::IvfHnswSq(b))
                .name(VECTOR_INDEX_NAME.to_string())
                .replace(true)
                .execute(),
        )
        .await?;
        self.set_ann_trained_rows(rows).await
    }

    /// Physical purge (M9): compact (rewrites fragments without deleted rows), prune every old
    /// version so deleted vectors are gone from disk, then fold the compacted data into the
    /// indexes. Safe because the daemon is the only process using the data dir (host.lock).
    pub async fn compact_and_purge_old_versions(&self) -> Result<()> {
        guard::lance(
            "lance compact",
            self.table.optimize(OptimizeAction::Compact {
                options: CompactionOptions {
                    materialize_deletions_threshold: 0.0,
                    ..CompactionOptions::default()
                },
                remap_options: None,
            }),
        )
        .await?;
        self.prune_versions(lancedb::table::optimize::Duration::zero())
            .await?;
        guard::lance(
            "lance optimize index",
            self.table
                .optimize(OptimizeAction::Index(Default::default())),
        )
        .await?;
        // the index optimisation made a new version; drop the pre-optimisation one too
        self.prune_versions(lancedb::table::optimize::Duration::zero())
            .await
    }

    /// Merge the small fragments that bounded group commits leave behind (one per commit,
    /// Amendment 4.25). Default compaction options: only small fragments are rewritten, indexes
    /// are remapped; old versions stay until retention prunes them.
    pub async fn compact_small_fragments(&self) -> Result<()> {
        guard::lance(
            "lance compact",
            self.table.optimize(OptimizeAction::Compact {
                options: CompactionOptions::default(),
                remap_options: None,
            }),
        )
        .await?;
        Ok(())
    }

    /// Retention: delete versions older than `older_than` (nightly job keeps ≤ 1 h).
    pub async fn prune_versions(
        &self,
        older_than: lancedb::table::optimize::Duration,
    ) -> Result<()> {
        guard::lance(
            "lance prune",
            self.table.optimize(OptimizeAction::Prune {
                older_than: Some(older_than),
                delete_unverified: Some(true),
                error_if_tagged_old_versions: Some(false),
            }),
        )
        .await?;
        Ok(())
    }

    /// Number of versions currently on disk.
    pub async fn version_count(&self) -> Result<usize> {
        Ok(
            guard::lance("lance list_versions", self.table.list_versions())
                .await?
                .len(),
        )
    }
}
