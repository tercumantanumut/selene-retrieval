//! Spike-level simulation of the M2 re-embed flow (tier or model change).
//!
//! Models the contracts, not LanceDB: an "index" is a directory with `meta.json` (model id, revision,
//! variant sha256, dims), `vectors.jsonl` (chunk_id → vector, upsert semantics like Lance
//! `merge_insert(chunk_id)`) and `checkpoint.json`. `active.json` is the single pointer search reads.
//!
//! Invariants proved by the tests:
//! 1. Search always reads the index named by `active.json` and embeds the query with *that index's*
//!    model, never the tier's new model, until the swap.
//! 2. Resumable: the checkpoint is written (tmp + rename) after each committed batch; a restart
//!    continues from it, and rows written past the checkpoint before a crash are idempotent upserts.
//! 3. Atomic swap: `active.json` is replaced by rename (MoveFileExW REPLACE_EXISTING on Windows) only
//!    after the shadow holds exactly the catalog's chunk_id set at the expected dims.
//! 4. Dimension mismatch is rejected (before any write if the model declares the wrong dims; mid-run
//!    if it returns wrong-length vectors) and the shadow is discarded. Active is untouched.
//! 5. A failed or corrupt download never changes the active index. `.part` is kept for resume
//!    (network) or deleted (sha256 mismatch).
//! 6. Chunk IDs never change: the shadow is keyed by the catalog's chunk_ids.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------------------------ events

/// Wire names match docs/02-architecture.md §2.5 (EngineEvent, serde tag = "type").
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "model.download.progress")]
    ModelDownloadProgress { model_id: String, bytes: u64, total: u64 },
    #[serde(rename = "model.download.completed")]
    ModelDownloadCompleted { model_id: String },
    #[serde(rename = "model.download.failed")]
    ModelDownloadFailed { model_id: String, code: &'static str, resumable: bool },
    #[serde(rename = "reembed.started")]
    ReembedStarted { index_id: String, model_id: String, dims: u32, total: u64, resumed_from: u64 },
    #[serde(rename = "reembed.progress")]
    ReembedProgress { index_id: String, done: u64, total: u64 },
    #[serde(rename = "reembed.failed")]
    ReembedFailed { index_id: String, code: &'static str, resumable: bool },
    #[serde(rename = "index.swapped")]
    IndexSwapped { old_index_id: Option<String>, new_index_id: String, model_id: String, dims: u32 },
}

// ------------------------------------------------------------------------------------------ catalog

#[derive(Debug, Clone)]
pub struct Chunk {
    pub chunk_id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum Op {
    Upsert(Chunk),
    Delete(String),
}

/// Stand-in for the SQLite catalog: chunks (source of truth) + the outbox (`index_ops`, seq = position).
#[derive(Default)]
pub struct Catalog {
    pub chunks: BTreeMap<String, String>,
    pub outbox: Vec<Op>,
}

impl Catalog {
    pub fn with_chunks(n: usize) -> Self {
        let mut c = Catalog::default();
        for i in 0..n {
            c.chunks.insert(format!("c{i:05}"), format!("chunk text number {i} about topic {}", i % 7));
        }
        c
    }
    pub fn apply(&mut self, op: Op) {
        match &op {
            Op::Upsert(ch) => {
                self.chunks.insert(ch.chunk_id.clone(), ch.text.clone());
            }
            Op::Delete(id) => {
                self.chunks.remove(id);
            }
        }
        self.outbox.push(op);
    }
    pub fn seq(&self) -> u64 {
        self.outbox.len() as u64
    }
}

// ------------------------------------------------------------------------------------------ models

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub model_id: String,
    pub revision: String,
    pub file_sha256: String,
    pub dims: u32,
}

pub trait Embedder {
    fn spec(&self) -> &ModelSpec;
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
}

/// Deterministic fake: hashes words into `out_dims` buckets and L2-normalises.
pub struct FakeEmbedder {
    pub spec: ModelSpec,
    /// Length of the vectors actually returned (≠ spec.dims simulates a wrong model file).
    pub out_dims: u32,
    /// Return Err on this call number (0-based): simulates a crash / device loss mid-run.
    pub fail_on_call: Option<usize>,
    pub calls: usize,
    pub embedded: usize,
}

impl FakeEmbedder {
    pub fn new(spec: ModelSpec) -> Self {
        let d = spec.dims;
        Self { spec, out_dims: d, fail_on_call: None, calls: 0, embedded: 0 }
    }
}

impl Embedder for FakeEmbedder {
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let call = self.calls;
        self.calls += 1;
        if self.fail_on_call == Some(call) {
            return Err("simulated interruption".into());
        }
        self.embedded += texts.len();
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.out_dims as usize];
                for w in t.split_whitespace() {
                    let h = Sha256::digest(format!("{}:{w}", self.spec.model_id));
                    let i = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize % v.len();
                    v[i] += 1.0;
                }
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                v.iter_mut().for_each(|x| *x /= n);
                v
            })
            .collect())
    }
}

// ------------------------------------------------------------------------------------------ download

#[derive(Debug, PartialEq)]
pub enum DlError {
    /// Network dropped; `.part` kept, next attempt resumes with a Range request.
    Network { have: u64 },
    /// sha256 mismatch; `.part` deleted.
    Integrity,
}

/// Fake HTTP source with Range support. `drop_at` = connection drops once after this many bytes.
pub struct FakeRemote {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub drop_at: Option<u64>,
    pub corrupt: bool,
    pub served: u64,
}

impl FakeRemote {
    pub fn new(len: usize) -> Self {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let sha256 = hex(&Sha256::digest(&bytes));
        Self { bytes, sha256, drop_at: None, corrupt: false, served: 0 }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Resumable, sha256-verified download into `dest` (via `dest.part`). Emits progress every 25 %.
pub fn download(remote: &mut FakeRemote, model_id: &str, dest: &Path, ev: &mut Vec<Event>) -> Result<(), DlError> {
    let part = dest.with_extension("part");
    let have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let total = remote.bytes.len() as u64;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&part).unwrap();
    let mut pos = have; // Range: bytes=have-
    let step = (total / 4).max(1);
    while pos < total {
        if let Some(d) = remote.drop_at {
            if pos >= d {
                remote.drop_at = None;
                f.sync_all().unwrap();
                ev.push(Event::ModelDownloadFailed { model_id: model_id.into(), code: "ModelDownloadFailed", resumable: true });
                return Err(DlError::Network { have: pos });
            }
        }
        let end = (pos + step).min(total).min(remote.drop_at.unwrap_or(u64::MAX).max(pos + 1));
        let mut chunk = remote.bytes[pos as usize..end as usize].to_vec();
        if remote.corrupt && pos == 0 {
            chunk[0] ^= 0xff;
        }
        f.write_all(&chunk).unwrap();
        remote.served += end - pos;
        pos = end;
        ev.push(Event::ModelDownloadProgress { model_id: model_id.into(), bytes: pos, total });
    }
    f.sync_all().unwrap();
    drop(f);
    let got = hex(&Sha256::digest(fs::read(&part).unwrap()));
    if got != remote.sha256 {
        fs::remove_file(&part).unwrap();
        ev.push(Event::ModelDownloadFailed { model_id: model_id.into(), code: "ModelIntegrity", resumable: false });
        return Err(DlError::Integrity);
    }
    fs::rename(&part, dest).unwrap();
    ev.push(Event::ModelDownloadCompleted { model_id: model_id.into() });
    Ok(())
}

// ------------------------------------------------------------------------------------------ store

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum IndexState {
    Building,
    Active,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub index_id: String,
    pub model: ModelSpec,
    pub chunk_cfg_fp: String,
    pub state: IndexState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Checkpoint {
    /// Chunks in chunk_id order below this one are committed.
    next_after: Option<String>,
    done: u64,
    /// Outbox seq at start; ops after it are caught up before the swap.
    start_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pointer {
    index_id: String,
}

pub struct Store {
    pub root: PathBuf,
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
    }
    fs::rename(&tmp, path).unwrap(); // MoveFileExW(MOVEFILE_REPLACE_EXISTING) on Windows
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("idx-{id}"))
    }
    pub fn active_id(&self) -> Option<String> {
        let s = fs::read_to_string(self.root.join("active.json")).ok()?;
        Some(serde_json::from_str::<Pointer>(&s).ok()?.index_id)
    }
    pub fn meta(&self, id: &str) -> Option<IndexMeta> {
        serde_json::from_str(&fs::read_to_string(self.dir(id).join("meta.json")).ok()?).ok()
    }
    fn write_meta(&self, m: &IndexMeta) {
        fs::create_dir_all(self.dir(&m.index_id)).unwrap();
        write_atomic(&self.dir(&m.index_id).join("meta.json"), &serde_json::to_vec(m).unwrap());
    }
    /// chunk_id → vector (last write wins = idempotent upsert; tombstone = empty vector).
    pub fn vectors(&self, id: &str) -> BTreeMap<String, Vec<f32>> {
        let mut out = BTreeMap::new();
        if let Ok(s) = fs::read_to_string(self.dir(id).join("vectors.jsonl")) {
            for l in s.lines() {
                let (k, v): (String, Vec<f32>) = serde_json::from_str(l).unwrap();
                if v.is_empty() {
                    out.remove(&k);
                } else {
                    out.insert(k, v);
                }
            }
        }
        out
    }
    fn append(&self, id: &str, rows: &[(String, Vec<f32>)]) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir(id).join("vectors.jsonl")).unwrap();
        for r in rows {
            writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
        }
        f.sync_all().unwrap();
    }
    fn checkpoint(&self, id: &str) -> Option<Checkpoint> {
        serde_json::from_str(&fs::read_to_string(self.dir(id).join("checkpoint.json")).ok()?).ok()
    }
    fn write_checkpoint(&self, id: &str, c: &Checkpoint) {
        write_atomic(&self.dir(id).join("checkpoint.json"), &serde_json::to_vec(c).unwrap());
    }
    fn building_for(&self, model: &ModelSpec) -> Option<IndexMeta> {
        let mut found: Vec<IndexMeta> = fs::read_dir(&self.root)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("idx-").map(String::from))
            .filter_map(|id| self.meta(&id))
            .filter(|m| m.state == IndexState::Building && &m.model == model)
            .collect();
        found.sort_by(|a, b| a.index_id.cmp(&b.index_id));
        found.pop()
    }
    /// Create the very first active index (installer path: bundled Minimum model).
    pub fn bootstrap(&self, id: &str, cat: &Catalog, emb: &mut dyn Embedder) {
        let m = IndexMeta { index_id: id.into(), model: emb.spec().clone(), chunk_cfg_fp: "cfg1".into(), state: IndexState::Active };
        self.write_meta(&m);
        let texts: Vec<&str> = cat.chunks.values().map(String::as_str).collect();
        let vecs = emb.embed(&texts).unwrap();
        let rows: Vec<_> = cat.chunks.keys().cloned().zip(vecs).collect();
        self.append(id, &rows);
        write_atomic(&self.root.join("active.json"), &serde_json::to_vec(&Pointer { index_id: id.into() }).unwrap());
    }
}

// ------------------------------------------------------------------------------------------ search

#[derive(Debug, PartialEq)]
pub enum SearchError {
    NoIndex,
    /// The query embedder doesn't match the active index (model or dims). Never compare across spaces.
    ModelMismatch { index_model: String, index_dims: u32, query_model: String, query_dims: u32 },
}

/// Embeds the query with an embedder chosen *by the active index's model*.
pub fn search(
    store: &Store,
    embedders: &mut BTreeMap<String, Box<dyn Embedder>>,
    q: &str,
    k: usize,
) -> Result<(String, Vec<(String, f32)>), SearchError> {
    let id = store.active_id().ok_or(SearchError::NoIndex)?;
    let meta = store.meta(&id).ok_or(SearchError::NoIndex)?;
    let emb = embedders.get_mut(&meta.model.model_id).ok_or(SearchError::ModelMismatch {
        index_model: meta.model.model_id.clone(),
        index_dims: meta.model.dims,
        query_model: "<none loaded>".into(),
        query_dims: 0,
    })?;
    let qv = emb.embed(&[q]).map_err(|_| SearchError::NoIndex)?.remove(0);
    if emb.spec() != &meta.model || qv.len() != meta.model.dims as usize {
        return Err(SearchError::ModelMismatch {
            index_model: meta.model.model_id.clone(),
            index_dims: meta.model.dims,
            query_model: emb.spec().model_id.clone(),
            query_dims: qv.len() as u32,
        });
    }
    let mut hits: Vec<(String, f32)> = store
        .vectors(&id)
        .into_iter()
        .map(|(c, v)| (c, v.iter().zip(&qv).map(|(a, b)| a * b).sum()))
        .collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    hits.truncate(k);
    Ok((id, hits))
}

// ------------------------------------------------------------------------------------------ re-embed job

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Swapped { new_index_id: String },
    /// Checkpoint kept; the job is re-queued (next start or retry). Active index unchanged.
    Interrupted { index_id: String, done: u64 },
    DownloadFailed(DlError),
    /// Shadow discarded. Active index unchanged.
    DimensionMismatch { expected: u32, got: u32 },
}

pub struct Job<'a> {
    pub store: &'a Store,
    pub target: ModelSpec,
    pub batch: usize,
    pub new_index_id: String,
    /// Test hook: stop after writing the new pointer's tmp file but before the rename.
    pub crash_before_swap_rename: bool,
}

impl Job<'_> {
    /// `model_path` present → no download. Otherwise download from `remote` first.
    pub fn run(
        &self,
        cat: &Catalog,
        model_path: &Path,
        remote: &mut FakeRemote,
        emb: &mut dyn Embedder,
        ev: &mut Vec<Event>,
    ) -> Outcome {
        // 1. Model file (on demand, resumable, sha256-checked). Failure never touches the active index.
        if !model_path.exists() {
            if let Err(e) = download(remote, &self.target.model_id, model_path, ev) {
                return Outcome::DownloadFailed(e);
            }
        }
        // 2. Declared-dims check before any write.
        if emb.spec().dims != self.target.dims {
            ev.push(Event::ReembedFailed { index_id: self.new_index_id.clone(), code: "DimensionMismatch", resumable: false });
            return Outcome::DimensionMismatch { expected: self.target.dims, got: emb.spec().dims };
        }
        // 3. Resume a building shadow for the same model, or create one.
        let (meta, mut cp) = match self.store.building_for(&self.target) {
            Some(m) => {
                let cp = self.store.checkpoint(&m.index_id).unwrap_or_default();
                (m, cp)
            }
            None => {
                let m = IndexMeta {
                    index_id: self.new_index_id.clone(),
                    model: self.target.clone(),
                    chunk_cfg_fp: "cfg1".into(),
                    state: IndexState::Building,
                };
                self.store.write_meta(&m);
                let cp = Checkpoint { next_after: None, done: 0, start_seq: cat.seq() };
                self.store.write_checkpoint(&m.index_id, &cp);
                (m, cp)
            }
        };
        let id = meta.index_id.clone();
        let total = cat.chunks.len() as u64;
        ev.push(Event::ReembedStarted { index_id: id.clone(), model_id: self.target.model_id.clone(), dims: self.target.dims, total, resumed_from: cp.done });

        // 4. Bulk pass in chunk_id order from the checkpoint.
        let todo: Vec<(&String, &String)> = match &cp.next_after {
            Some(after) => cat.chunks.range::<String, _>((std::ops::Bound::Excluded(after.clone()), std::ops::Bound::Unbounded)).collect(),
            None => cat.chunks.iter().collect(),
        };
        for b in todo.chunks(self.batch) {
            let texts: Vec<&str> = b.iter().map(|(_, t)| t.as_str()).collect();
            let vecs = match emb.embed(&texts) {
                Ok(v) => v,
                Err(_) => {
                    ev.push(Event::ReembedFailed { index_id: id.clone(), code: "Interrupted", resumable: true });
                    return Outcome::Interrupted { index_id: id, done: cp.done };
                }
            };
            if let Some(bad) = vecs.iter().find(|v| v.len() != self.target.dims as usize) {
                let got = bad.len() as u32;
                fs::remove_dir_all(self.store.dir(&id)).unwrap();
                ev.push(Event::ReembedFailed { index_id: id, code: "DimensionMismatch", resumable: false });
                return Outcome::DimensionMismatch { expected: self.target.dims, got };
            }
            let rows: Vec<_> = b.iter().map(|(c, _)| (*c).clone()).zip(vecs).collect();
            self.store.append(&id, &rows); // commit data …
            cp.done += rows.len() as u64;
            cp.next_after = Some(b.last().unwrap().0.clone());
            self.store.write_checkpoint(&id, &cp); // … then the checkpoint (crash between = harmless upserts)
            ev.push(Event::ReembedProgress { index_id: id.clone(), done: cp.done, total });
        }

        // 5. Catch up ops that arrived during the run (same coalescing as startup replay).
        let mut latest: BTreeMap<String, Option<String>> = BTreeMap::new();
        for op in &cat.outbox[cp.start_seq as usize..] {
            match op {
                Op::Upsert(c) => latest.insert(c.chunk_id.clone(), Some(c.text.clone())),
                Op::Delete(cid) => latest.insert(cid.clone(), None),
            };
        }
        let ups: Vec<(&String, &String)> = latest.iter().filter_map(|(k, v)| v.as_ref().map(|t| (k, t))).collect();
        if !ups.is_empty() {
            let texts: Vec<&str> = ups.iter().map(|(_, t)| t.as_str()).collect();
            let vecs = emb.embed(&texts).expect("catch-up embed");
            self.store.append(&id, &ups.iter().map(|(k, _)| (*k).clone()).zip(vecs).collect::<Vec<_>>());
        }
        let dels: Vec<(String, Vec<f32>)> = latest.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| (k.clone(), vec![])).collect();
        self.store.append(&id, &dels);

        // 6. Validate before the swap: exact chunk_id set, right dims.
        let shadow = self.store.vectors(&id);
        let want: BTreeSet<&String> = cat.chunks.keys().collect();
        let have: BTreeSet<&String> = shadow.keys().collect();
        assert_eq!(want, have, "shadow must hold exactly the catalog's chunk_ids");
        assert!(shadow.values().all(|v| v.len() == self.target.dims as usize));

        // 7. Atomic swap.
        let old = self.store.active_id();
        let ptr = self.store.root.join("active.json");
        if self.crash_before_swap_rename {
            fs::write(ptr.with_extension("tmp"), b"{\"index_id\":\"half-written").unwrap();
            return Outcome::Interrupted { index_id: id, done: cp.done };
        }
        let mut m = meta;
        m.state = IndexState::Active;
        self.store.write_meta(&m);
        write_atomic(&ptr, &serde_json::to_vec(&Pointer { index_id: id.clone() }).unwrap());
        if let Some(o) = &old {
            if let Some(mut om) = self.store.meta(o) {
                om.state = IndexState::Retired; // GC'd after in-flight readers drain (retention job)
                self.store.write_meta(&om);
            }
        }
        ev.push(Event::IndexSwapped { old_index_id: old, new_index_id: id.clone(), model_id: self.target.model_id.clone(), dims: self.target.dims });
        Outcome::Swapped { new_index_id: id }
    }
}

// ------------------------------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);
    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("sr-reembed-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        p
    }
    fn spec(id: &str, dims: u32) -> ModelSpec {
        ModelSpec { model_id: id.into(), revision: "r1".into(), file_sha256: "x".repeat(64), dims }
    }
    fn small() -> ModelSpec {
        spec("bge-small", 384)
    }
    fn std768() -> ModelSpec {
        spec("arctic-m-v1.5", 768)
    }
    struct World {
        store: Store,
        cat: Catalog,
        embs: BTreeMap<String, Box<dyn Embedder>>,
        model_path: PathBuf,
        remote: FakeRemote,
    }
    fn world(n: usize) -> World {
        let root = tmp();
        let store = Store::new(root.join("index"));
        let cat = Catalog::with_chunks(n);
        let mut s = FakeEmbedder::new(small());
        store.bootstrap("A", &cat, &mut s);
        let mut embs: BTreeMap<String, Box<dyn Embedder>> = BTreeMap::new();
        embs.insert("bge-small".into(), Box::new(FakeEmbedder::new(small())));
        embs.insert("arctic-m-v1.5".into(), Box::new(FakeEmbedder::new(std768())));
        World { model_path: root.join("arctic.onnx"), store, cat, embs, remote: FakeRemote::new(10_000) }
    }
    fn job(store: &Store) -> Job<'_> {
        Job { store, target: std768(), batch: 16, new_index_id: "B".into(), crash_before_swap_rename: false }
    }

    #[test]
    fn happy_path_swaps_and_keeps_chunk_ids() {
        let mut w = world(100);
        let before: BTreeSet<String> = w.store.vectors("A").into_keys().collect();
        let mut e = FakeEmbedder::new(std768());
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        assert_eq!(w.store.active_id().as_deref(), Some("B"));
        let after: BTreeSet<String> = w.store.vectors("B").into_keys().collect();
        assert_eq!(before, after, "chunk IDs never change");
        let (id, hits) = search(&w.store, &mut w.embs, "topic 3", 5).unwrap();
        assert_eq!(id, "B");
        assert_eq!(hits.len(), 5);
        assert_eq!(w.store.meta("A").unwrap().state, IndexState::Retired);
        assert!(matches!(ev.last(), Some(Event::IndexSwapped { new_index_id, dims: 768, .. }) if new_index_id == "B"));
        assert!(ev.iter().any(|e| matches!(e, Event::ModelDownloadCompleted { .. })));
        assert!(!w.store.root.join("active.tmp").exists());
    }

    #[test]
    fn resume_after_interruption_continues_from_checkpoint() {
        let mut w = world(100);
        let mut e = FakeEmbedder::new(std768());
        e.fail_on_call = Some(3); // 3 batches of 16 committed, then "crash"
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::Interrupted { index_id: "B".into(), done: 48 });
        // Search during the gap: old index, old model.
        assert_eq!(w.store.active_id().as_deref(), Some("A"));
        assert_eq!(search(&w.store, &mut w.embs, "topic 3", 3).unwrap().0, "A");
        // Restart with a fresh process/embedder: only the remaining 52 chunks are embedded.
        let mut e2 = FakeEmbedder::new(std768());
        let mut ev2 = vec![];
        let j = Job { new_index_id: "C-unused".into(), ..job(&w.store) };
        let out = j.run(&w.cat, &w.model_path, &mut w.remote, &mut e2, &mut ev2);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        assert_eq!(e2.embedded, 52);
        assert!(matches!(&ev2[0], Event::ReembedStarted { resumed_from: 48, .. }));
        assert_eq!(w.store.vectors("B").len(), 100);
    }

    #[test]
    fn crash_between_data_commit_and_checkpoint_is_idempotent() {
        let mut w = world(40);
        let mut e = FakeEmbedder::new(std768());
        e.fail_on_call = Some(1);
        let mut ev = vec![];
        job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        // Rows for the next batch hit disk but the checkpoint didn't (crash in between).
        let extra: Vec<_> = w.cat.chunks.keys().skip(16).take(16).map(|k| (k.clone(), vec![0.5f32; 768])).collect();
        w.store.append("B", &extra);
        let mut e2 = FakeEmbedder::new(std768());
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e2, &mut ev);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        assert_eq!(w.store.vectors("B").len(), 40, "no duplicates");
        assert!(w.store.vectors("B").values().all(|v| v[0] != 0.5 || v[1] != 0.5), "re-embedded rows replaced the orphans");
    }

    #[test]
    fn declared_dimension_mismatch_rejected_before_any_write() {
        let mut w = world(20);
        let mut wrong = FakeEmbedder::new(spec("arctic-m-v1.5", 1024));
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut wrong, &mut ev);
        assert_eq!(out, Outcome::DimensionMismatch { expected: 768, got: 1024 });
        assert!(!w.store.root.join("idx-B").exists());
        assert_eq!(w.store.active_id().as_deref(), Some("A"));
        assert!(search(&w.store, &mut w.embs, "topic", 3).is_ok());
    }

    #[test]
    fn returned_dimension_mismatch_discards_shadow() {
        let mut w = world(40);
        let mut e = FakeEmbedder::new(std768());
        e.out_dims = 1024; // file claims 768 but produces 1024
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::DimensionMismatch { expected: 768, got: 1024 });
        assert!(!w.store.root.join("idx-B").exists(), "shadow discarded");
        assert_eq!(w.store.active_id().as_deref(), Some("A"));
        assert!(matches!(ev.last(), Some(Event::ReembedFailed { code: "DimensionMismatch", resumable: false, .. })));
    }

    #[test]
    fn query_with_wrong_model_is_refused_not_compared() {
        let w = world(10);
        let mut embs: BTreeMap<String, Box<dyn Embedder>> = BTreeMap::new();
        let mut liar = FakeEmbedder::new(small());
        liar.out_dims = 768;
        embs.insert("bge-small".into(), Box::new(liar));
        assert!(matches!(search(&w.store, &mut embs, "q", 3), Err(SearchError::ModelMismatch { index_dims: 384, query_dims: 768, .. })));
    }

    #[test]
    fn download_failure_falls_back_to_current_index_then_resumes() {
        let mut w = world(30);
        w.remote.drop_at = Some(4_000);
        let mut e = FakeEmbedder::new(std768());
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::DownloadFailed(DlError::Network { have: 4_000 }));
        assert_eq!(e.calls, 0, "nothing embedded");
        assert_eq!(w.store.active_id().as_deref(), Some("A"));
        assert_eq!(search(&w.store, &mut w.embs, "topic 1", 3).unwrap().0, "A", "search still works");
        assert!(ev.iter().any(|e| matches!(e, Event::ModelDownloadFailed { resumable: true, .. })));
        // Retry: resumes at 4 000 bytes (Range), not from 0.
        let served_before = w.remote.served;
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        assert_eq!(w.remote.served - served_before, 6_000);
    }

    #[test]
    fn corrupt_download_is_deleted_and_active_kept() {
        let mut w = world(10);
        w.remote.corrupt = true;
        let mut e = FakeEmbedder::new(std768());
        let mut ev = vec![];
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::DownloadFailed(DlError::Integrity));
        assert!(!w.model_path.exists() && !w.model_path.with_extension("part").exists());
        assert_eq!(w.store.active_id().as_deref(), Some("A"));
        assert!(ev.iter().any(|e| matches!(e, Event::ModelDownloadFailed { code: "ModelIntegrity", resumable: false, .. })));
    }

    #[test]
    fn edits_during_reembed_are_caught_up_before_swap() {
        let mut w = world(50);
        let mut e = FakeEmbedder::new(std768());
        e.fail_on_call = Some(1);
        let mut ev = vec![];
        job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        // User edits while the shadow builds: one new chunk, one deleted, one changed (already embedded).
        w.cat.apply(Op::Upsert(Chunk { chunk_id: "c99999".into(), text: "brand new note".into() }));
        w.cat.apply(Op::Delete("c00003".into()));
        w.cat.apply(Op::Upsert(Chunk { chunk_id: "c00001".into(), text: "edited text".into() }));
        let mut e2 = FakeEmbedder::new(std768());
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e2, &mut ev);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        let v = w.store.vectors("B");
        assert!(v.contains_key("c99999") && !v.contains_key("c00003"));
        assert_eq!(v.len(), 50);
    }

    #[test]
    fn crash_before_pointer_rename_keeps_old_index_then_completes() {
        let mut w = world(20);
        let mut e = FakeEmbedder::new(std768());
        let mut ev = vec![];
        let j = Job { crash_before_swap_rename: true, ..job(&w.store) };
        let out = j.run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert!(matches!(out, Outcome::Interrupted { .. }));
        assert_eq!(w.store.active_id().as_deref(), Some("A"), "half-written tmp is never read");
        let out = job(&w.store).run(&w.cat, &w.model_path, &mut w.remote, &mut e, &mut ev);
        assert_eq!(out, Outcome::Swapped { new_index_id: "B".into() });
        assert_eq!(search(&w.store, &mut w.embs, "topic 2", 2).unwrap().0, "B");
    }
}
