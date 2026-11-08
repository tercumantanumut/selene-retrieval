//! Chunk writes (architect D2/D3) and read-side hydration.
//!
//! * **T1** [`write_file_chunks`]: one transaction bumps `files.version`, replaces the file's
//!   chunks, stores new text in `chunk_text` (zstd, refcounted by triggers), appends the outbox op
//!   and sets `pending_index`.
//! * Stores apply the op and persist their watermarks (engine).
//! * **T2** [`finalize_applied`] (or per file [`finalize_file`] + [`crate::repo::outbox::prune`]):
//!   `indexed` (only if the version is still current / fully applied) and outbox pruning up to
//!   `min(watermarks)`. Run [`finalize_applied`] at startup after replay as well.
//!
//! A crash between T1 and T2 leaves the op in the outbox: startup replay re-applies it.

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::{ChunkCfgFp, ChunkId, ContentHash, FileId, FolderId, TextHash};

use crate::db::{DbResultExt, atomic};
use crate::model::{IndexOpKind, RowExt, Seq};
use crate::repo::outbox;

/// zstd level for chunk text (fast; text compresses ~3–4×).
const ZSTD_LEVEL: i32 = 3;
/// Upper bound for one chunk's text (a 400-token chunk is a few KiB; guards decompression).
pub const MAX_CHUNK_TEXT_BYTES: usize = 1 << 20;

/// One chunk produced by the chunker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChunk {
    pub ord: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u32,
    pub line_end: u32,
    pub tokens: u32,
    pub text: String,
}

/// T1 input for one file version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunks {
    pub file_id: FileId,
    pub content_hash: ContentHash,
    pub chunk_cfg_fp: ChunkCfgFp,
    pub extractor_version: String,
    pub chunks: Vec<NewChunk>,
}

/// T1 result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct T1Outcome {
    pub file_version: i64,
    pub seq: Seq,
    pub chunk_ids: Vec<ChunkId>,
    /// Texts not seen before in the catalog (the rest can reuse vectors by text_hash).
    pub new_texts: usize,
}

/// T1 (see module docs).
pub fn write_file_chunks(conn: &Connection, fc: &FileChunks, now: i64) -> Result<T1Outcome> {
    if fc
        .chunks
        .iter()
        .any(|c| c.text.len() > MAX_CHUNK_TEXT_BYTES)
    {
        return Err(Error::new(ErrorCode::InvalidInput).with_detail("chunk text too large"));
    }
    atomic(conn, |c| {
        let (collection_id, profile_id, version): (
            sr_common::ids::CollectionId,
            i64,
            i64,
        ) = c
            .prepare_cached(
                "SELECT fo.collection_id, co.embedding_profile_id, f.version + 1
                 FROM files f JOIN folders fo ON fo.id = f.folder_id JOIN collections co ON co.id = fo.collection_id
                 WHERE f.id = ?1",
            )
            .db()?
            .query_row([fc.file_id.to_bytes()], |r| {
                Ok((r.collection_id(0)?, r.get(1)?, r.get(2)?))
            })
            .db()?;
        c.prepare_cached(
            "UPDATE files SET version = ?2, content_hash = ?3, chunk_cfg_fp = ?4, extractor_version = ?5,
                 state = 'pending_index', skip_reason = NULL, error_code = NULL, attempts = 0, updated_at = ?6
             WHERE id = ?1",
        )
        .db()?
        .execute(params![
            fc.file_id.to_bytes(),
            version,
            fc.content_hash.as_bytes(),
            fc.chunk_cfg_fp.as_bytes(),
            fc.extractor_version,
            now
        ])
        .db()?;
        // Old version's chunks go first (refcounts drop; unreferenced text is deleted).
        c.prepare_cached("DELETE FROM chunks WHERE file_id = ?1")
            .db()?
            .execute([fc.file_id.to_bytes()])
            .db()?;
        let mut ins_text = c
            .prepare_cached(
                "INSERT INTO chunk_text(text_hash, zstd, bytes, refcount) VALUES (?1, ?2, ?3, 0)
                 ON CONFLICT(text_hash) DO NOTHING",
            )
            .db()?;
        let mut has_text = c
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM chunk_text WHERE text_hash = ?1)")
            .db()?;
        let mut ins_chunk = c
            .prepare_cached(
                "INSERT INTO chunks(chunk_id, file_id, file_version, ord, byte_start, byte_end, line_start,
                     line_end, tokens, text_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .db()?;
        let mut ids = Vec::with_capacity(fc.chunks.len());
        let mut new_texts = 0;
        for ch in &fc.chunks {
            let th = TextHash::of_text(&ch.text);
            let exists: bool = has_text.query_row([th.as_bytes()], |r| r.get(0)).db()?;
            if !exists {
                let z = zstd::bulk::compress(ch.text.as_bytes(), ZSTD_LEVEL)
                    .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
                ins_text
                    .execute(params![th.as_bytes(), z, ch.text.len()])
                    .db()?;
                new_texts += 1;
            }
            let id = ChunkId::derive(fc.file_id, &fc.content_hash, &fc.chunk_cfg_fp, ch.ord);
            ins_chunk
                .execute(params![
                    id.as_bytes(),
                    fc.file_id.to_bytes(),
                    version,
                    ch.ord,
                    ch.byte_start,
                    ch.byte_end,
                    ch.line_start,
                    ch.line_end,
                    ch.tokens,
                    th.as_bytes()
                ])
                .db()?;
            ids.push(id);
        }
        let seq = outbox::append(
            c,
            collection_id,
            Some(profile_id),
            None,
            Some(fc.file_id),
            IndexOpKind::Upsert,
            version,
            now,
        )?;
        Ok(T1Outcome {
            file_version: version,
            seq,
            chunk_ids: ids,
            new_texts,
        })
    })
}

/// T2: `indexed`, but only if `version` is still the file's current version and it is pending
/// (a newer T1 in between keeps it `pending_index`). Returns whether the row changed.
pub fn finalize_file(conn: &Connection, file_id: FileId, version: i64, now: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "UPDATE files SET state = 'indexed', updated_at = ?3
             WHERE id = ?1 AND version = ?2 AND state = 'pending_index'",
        )
        .db()?
        .execute(params![file_id.to_bytes(), version, now])
        .db()?
        == 1)
}

/// T2 for a whole collection, and startup recovery: marks `indexed` every `pending_index` file
/// of the collection that has had a T1 (`version > 0`) and has **no outbox op above the applied
/// floor** (`min` of every store watermark), then prunes the applied ops. Covers a crash after a
/// store persisted its watermark but before T2 ran, which replay alone would never revisit.
pub fn finalize_applied(
    conn: &Connection,
    collection_id: sr_common::ids::CollectionId,
    now: i64,
) -> Result<usize> {
    atomic(conn, |c| {
        let floor = outbox::applied_floor(c, collection_id)?;
        let n = c
            .prepare_cached(
                "UPDATE files SET state = 'indexed', updated_at = ?3
                 WHERE state = 'pending_index' AND version > 0
                   AND folder_id IN (SELECT id FROM folders WHERE collection_id = ?1)
                   AND NOT EXISTS (SELECT 1 FROM index_ops o WHERE o.file_id = files.id AND o.seq > ?2)",
            )
            .db()?
            .execute(params![collection_id.to_bytes(), floor, now])
            .db()?;
        outbox::prune(c)?;
        Ok(n)
    })
}

/// One stored chunk of the current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRecord {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub file_version: i64,
    pub ord: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u32,
    pub line_end: u32,
    pub tokens: u32,
    pub text_hash: TextHash,
}

fn chunk_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRecord> {
    Ok(ChunkRecord {
        chunk_id: r.chunk_id(0)?,
        file_id: r.file_id(1)?,
        file_version: r.get(2)?,
        ord: r.get(3)?,
        byte_start: r.get(4)?,
        byte_end: r.get(5)?,
        line_start: r.get(6)?,
        line_end: r.get(7)?,
        tokens: r.get(8)?,
        text_hash: r.text_hash(9)?,
    })
}

/// Current chunks of a file, in order (what an `Upsert` op must write to the stores).
pub fn file_chunks(conn: &Connection, file_id: FileId) -> Result<Vec<ChunkRecord>> {
    let mut st = conn
        .prepare_cached(
            "SELECT chunk_id, file_id, file_version, ord, byte_start, byte_end, line_start, line_end, tokens,
                 text_hash FROM chunks WHERE file_id = ?1 ORDER BY ord",
        )
        .db()?;
    st.query_map([file_id.to_bytes()], chunk_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Decompressed text of one `text_hash`.
pub fn text(conn: &Connection, h: &TextHash) -> Result<Option<String>> {
    let row: Option<(Vec<u8>, usize)> = conn
        .prepare_cached("SELECT zstd, bytes FROM chunk_text WHERE text_hash = ?1")
        .db()?
        .query_row([h.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()?;
    let Some((z, bytes)) = row else {
        return Ok(None);
    };
    decompress_text(&z, bytes).map(Some)
}

fn decompress_text(z: &[u8], bytes: usize) -> Result<String> {
    let raw = zstd::bulk::decompress(z, bytes.min(MAX_CHUNK_TEXT_BYTES))
        .map_err(|e| Error::new(ErrorCode::Corrupt).with_source(e))?;
    String::from_utf8(raw).map_err(|e| Error::new(ErrorCode::Corrupt).with_source(e))
}

/// A search hit hydrated from the catalog (the stores hold no paths and no text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hydrated {
    pub chunk_id: ChunkId,
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub rel_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub text: String,
    /// Already selected to fetch the text; kept for query-time duplicate collapsing (docs/06).
    pub text_hash: TextHash,
}

/// Chunk ids per `IN (…)` statement (far below SQLite's bound-variable limit).
pub const HYDRATE_BATCH: usize = 500;

/// Hydrates hits in the given order; stale ids (not part of their file's current version) are
/// skipped. One statement per [`HYDRATE_BATCH`] ids, chunk row and text in the same query
/// (docs/06 issue 2: no per-id round-trip).
pub fn hydrate(conn: &Connection, ids: &[ChunkId]) -> Result<Vec<Hydrated>> {
    let mut found: std::collections::HashMap<ChunkId, Hydrated> =
        std::collections::HashMap::with_capacity(ids.len());
    for batch in ids.chunks(HYDRATE_BATCH) {
        let marks = vec!["?"; batch.len()].join(",");
        let mut st = conn
            .prepare_cached(&format!(
                "SELECT c.chunk_id, c.file_id, f.folder_id, f.rel_path, c.line_start, c.line_end,
                        c.text_hash, t.zstd, t.bytes
                 FROM chunks c JOIN files f ON f.id = c.file_id
                 LEFT JOIN chunk_text t ON t.text_hash = c.text_hash
                 WHERE c.chunk_id IN ({marks})"
            ))
            .db()?;
        let keys: Vec<[u8; 32]> = batch.iter().map(|id| *id.as_bytes()).collect();
        let rows = st
            .query_map(rusqlite::params_from_iter(keys.iter()), |r| {
                Ok((
                    r.chunk_id(0)?,
                    r.file_id(1)?,
                    r.folder_id(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, u32>(5)?,
                    r.text_hash(6)?,
                    r.get::<_, Option<Vec<u8>>>(7)?,
                    r.get::<_, Option<usize>>(8)?,
                ))
            })
            .db()?;
        for row in rows {
            let (chunk_id, file_id, folder_id, rel_path, line_start, line_end, th, z, bytes) =
                row.db()?;
            let text = match (z, bytes) {
                (Some(z), Some(b)) => decompress_text(&z, b)?,
                _ => String::new(),
            };
            found.insert(
                chunk_id,
                Hydrated {
                    chunk_id,
                    file_id,
                    folder_id,
                    rel_path,
                    line_start,
                    line_end,
                    text,
                    text_hash: th,
                },
            );
        }
    }
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}
