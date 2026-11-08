//! Files: scan observation (rename detection by volume serial + 128-bit file id), mark-and-sweep
//! gated by `scan_dirs.complete` (W4), skip/dead-letter state, path search (FTS5).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::{ChunkCfgFp, ContentHash, FileId, FolderId};

use crate::db::{DbResultExt, atomic};
use crate::model::{FileState, IndexOpKind, RowExt, SKIP_REASON_FILES_ONLY, ScanId, vs_to_sql};
use crate::repo::{folders, outbox};

/// Parent of a `/`-separated path key (`""` for files directly in the root).
pub fn dir_key_of(path_key: &str) -> &str {
    path_key.rsplit_once('/').map_or("", |(d, _)| d)
}

/// What the walker saw for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObservation {
    /// Root-relative path as shown to users.
    pub rel_path: String,
    /// `sr_common::sandbox::path_key(rel_path)`.
    pub path_key: String,
    /// Lower-case extension without the dot.
    pub ext: String,
    /// 64-bit volume serial (`FILE_ID_INFO.VolumeSerialNumber`).
    pub volume_serial: Option<u64>,
    /// `FILE_ID_INFO.FileId` (128-bit).
    pub ntfs_file_id: Option<[u8; 16]>,
    pub size: u64,
    pub mtime_ns: i64,
}

/// How an observation relates to the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub file_id: FileId,
    /// Not in the catalog before.
    pub new: bool,
    /// Same NTFS identity at another path in this folder: path updated, `file_id` kept, so chunk
    /// IDs stay stable across the rename.
    pub renamed_from: Option<String>,
    /// Size or mtime differ from the catalog (the change detector decides whether to hash).
    pub metadata_changed: bool,
    pub state: FileState,
    pub content_hash: Option<ContentHash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub id: FileId,
    pub folder_id: FolderId,
    pub rel_path: String,
    pub path_key: String,
    pub ext: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub content_hash: Option<ContentHash>,
    pub version: i64,
    pub extractor_version: Option<String>,
    pub chunk_cfg_fp: Option<ChunkCfgFp>,
    pub scan_id: ScanId,
    pub state: FileState,
    pub skip_reason: Option<String>,
    pub error_code: Option<String>,
    pub attempts: u32,
}

const FILE_COLS: &str =
    "id, folder_id, rel_path, path_key, ext, size, mtime_ns, content_hash, version,
     extractor_version, chunk_cfg_fp, scan_id, state, skip_reason, error_code, attempts";

fn file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: r.file_id(0)?,
        folder_id: r.folder_id(1)?,
        rel_path: r.get(2)?,
        path_key: r.get(3)?,
        ext: r.get(4)?,
        size: r.get(5)?,
        mtime_ns: r.get(6)?,
        content_hash: r.opt_content_hash(7)?,
        version: r.get(8)?,
        extractor_version: r.get(9)?,
        chunk_cfg_fp: r
            .get::<_, Option<[u8; 32]>>(10)?
            .map(ChunkCfgFp::from_bytes),
        scan_id: r.get(11)?,
        state: r.get(12)?,
        skip_reason: r.get(13)?,
        error_code: r.get(14)?,
        attempts: r.get(15)?,
    })
}

pub fn file(conn: &Connection, id: FileId) -> Result<FileRecord> {
    conn.prepare_cached(&format!("SELECT {FILE_COLS} FROM files WHERE id = ?1"))
        .db()?
        .query_row([id.to_bytes()], file_from_row)
        .db()
}

pub fn file_by_path(
    conn: &Connection,
    folder_id: FolderId,
    path_key: &str,
) -> Result<Option<FileRecord>> {
    conn.prepare_cached(&format!(
        "SELECT {FILE_COLS} FROM files WHERE folder_id = ?1 AND path_key = ?2"
    ))
    .db()?
    .query_row(params![folder_id.to_bytes(), path_key], file_from_row)
    .optional()
    .db()
}

/// The folder's file with this NTFS identity, if any (a rename candidate; [`observe`] makes the
/// final call, which also excludes rows already seen by the running scan).
pub fn file_by_identity(
    conn: &Connection,
    folder_id: FolderId,
    volume_serial: u64,
    ntfs_file_id: [u8; 16],
) -> Result<Option<FileRecord>> {
    conn.prepare_cached(&format!(
        "SELECT {FILE_COLS} FROM files WHERE volume_serial = ?1 AND ntfs_file_id = ?2
         AND folder_id = ?3 LIMIT 1"
    ))
    .db()?
    .query_row(
        params![vs_to_sql(volume_serial), ntfs_file_id, folder_id.to_bytes()],
        file_from_row,
    )
    .optional()
    .db()
}

/// Newest scan id of a folder (running or finished), if it was ever scanned. Watcher observations
/// join this mark pass, so a file seen by the watcher counts as seen by the scan in progress.
pub fn latest_scan_id(conn: &Connection, folder_id: FolderId) -> Result<Option<ScanId>> {
    conn.prepare_cached("SELECT max(id) FROM scans WHERE folder_id = ?1")
        .db()?
        .query_row([folder_id.to_bytes()], |r| r.get(0))
        .db()
}

/// Whether the folder has cataloged files below directory `dir_key` (a removed or renamed
/// directory then needs a sweep; an unknown path with nothing below it does not).
pub fn any_under(conn: &Connection, folder_id: FolderId, dir_key: &str) -> Result<bool> {
    if dir_key.is_empty() {
        return conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM files WHERE folder_id = ?1)")
            .db()?
            .query_row([folder_id.to_bytes()], |r| r.get(0))
            .db();
    }
    // Range scan on UNIQUE(folder_id, path_key): '0' is the character after '/'.
    conn.prepare_cached(
        "SELECT EXISTS (SELECT 1 FROM files WHERE folder_id = ?1 AND path_key >= ?2 AND path_key < ?3)",
    )
    .db()?
    .query_row(
        params![folder_id.to_bytes(), format!("{dir_key}/"), format!("{dir_key}0")],
        |r| r.get(0),
    )
    .db()
}

/// Forgets the stored mtime of a file so the next scan hashes it and compares the content. Used
/// when a queued change is dropped (cancel): `observe` already recorded the new size/mtime, so
/// without this the next scan would trust the metadata and never index the edit.
pub fn invalidate_metadata(conn: &Connection, id: FileId, now: i64) -> Result<()> {
    conn.prepare_cached("UPDATE files SET mtime_ns = 0, updated_at = ?2 WHERE id = ?1")
        .db()?
        .execute(params![id.to_bytes(), now])
        .db()?;
    Ok(())
}

/// Starts a mark-and-sweep pass over a folder.
pub fn begin_scan(conn: &Connection, folder_id: FolderId, now: i64) -> Result<ScanId> {
    conn.execute(
        "INSERT INTO scans(folder_id, started_at) VALUES (?1, ?2)",
        params![folder_id.to_bytes(), now],
    )
    .db()?;
    Ok(conn.last_insert_rowid())
}

/// Records one file seen by scan `scan_id` (or by the watcher with the folder's latest scan id).
pub fn observe(
    conn: &Connection,
    folder_id: FolderId,
    scan_id: ScanId,
    obs: &FileObservation,
    now: i64,
) -> Result<Observed> {
    atomic(conn, |c| {
        let dir_key = dir_key_of(&obs.path_key);
        if let Some(f) = file_by_path(c, folder_id, &obs.path_key)? {
            let changed = f.size != obs.size || f.mtime_ns != obs.mtime_ns;
            c.prepare_cached(
                "UPDATE files SET scan_id = ?2, rel_path = ?3, size = ?4, mtime_ns = ?5, ext = ?6,
                     volume_serial = coalesce(?7, volume_serial), ntfs_file_id = coalesce(?8, ntfs_file_id),
                     updated_at = ?9 WHERE id = ?1",
            )
            .db()?
            .execute(params![
                f.id.to_bytes(),
                scan_id,
                obs.rel_path,
                obs.size,
                obs.mtime_ns,
                obs.ext,
                obs.volume_serial.map(vs_to_sql),
                obs.ntfs_file_id,
                now
            ])
            .db()?;
            return Ok(Observed {
                file_id: f.id,
                new: false,
                renamed_from: None,
                metadata_changed: changed,
                state: f.state,
                content_hash: f.content_hash,
            });
        }
        // Rename: same NTFS identity in this folder, not yet seen by this scan (a row already seen
        // by this scan at its own path is a hard link, not a rename).
        if let (Some(vs), Some(fid)) = (obs.volume_serial.map(vs_to_sql), obs.ntfs_file_id) {
            let hit: Option<FileRecord> = c
                .prepare_cached(&format!(
                    "SELECT {FILE_COLS} FROM files WHERE volume_serial = ?1 AND ntfs_file_id = ?2
                     AND folder_id = ?3 AND scan_id <> ?4 LIMIT 1"
                ))
                .db()?
                .query_row(
                    params![vs, fid, folder_id.to_bytes(), scan_id],
                    file_from_row,
                )
                .optional()
                .db()?;
            if let Some(f) = hit {
                let changed = f.size != obs.size || f.mtime_ns != obs.mtime_ns;
                c.prepare_cached(
                    "UPDATE files SET rel_path = ?2, path_key = ?3, dir_key = ?4, ext = ?5, scan_id = ?6,
                         size = ?7, mtime_ns = ?8, updated_at = ?9 WHERE id = ?1",
                )
                .db()?
                .execute(params![
                    f.id.to_bytes(),
                    obs.rel_path,
                    obs.path_key,
                    dir_key,
                    obs.ext,
                    scan_id,
                    obs.size,
                    obs.mtime_ns,
                    now
                ])
                .db()?;
                return Ok(Observed {
                    file_id: f.id,
                    new: false,
                    renamed_from: Some(f.rel_path),
                    metadata_changed: changed,
                    state: f.state,
                    content_hash: f.content_hash,
                });
            }
        }
        let id = FileId::new();
        c.prepare_cached(
            "INSERT INTO files(id, folder_id, rel_path, path_key, dir_key, ext, volume_serial, ntfs_file_id,
                 size, mtime_ns, scan_id, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending_index', ?12, ?12)",
        )
        .db()?
        .execute(params![
            id.to_bytes(),
            folder_id.to_bytes(),
            obs.rel_path,
            obs.path_key,
            dir_key,
            obs.ext,
            obs.volume_serial.map(vs_to_sql),
            obs.ntfs_file_id,
            obs.size,
            obs.mtime_ns,
            scan_id,
            now
        ])
        .db()?;
        Ok(Observed {
            file_id: id,
            new: true,
            renamed_from: None,
            metadata_changed: true,
            state: FileState::PendingIndex,
            content_hash: None,
        })
    })
}

/// Records whether a directory was listed completely by `scan_id`. Only complete directories
/// may delete files (W4); a directory that failed (permission, AV lock) keeps its files.
pub fn mark_dir(
    conn: &Connection,
    scan_id: ScanId,
    folder_id: FolderId,
    rel_dir_key: &str,
    complete: bool,
    error_code: Option<ErrorCode>,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO scan_dirs(scan_id, folder_id, rel_dir_key, complete, error_code) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(scan_id, folder_id, rel_dir_key) DO UPDATE SET complete = excluded.complete,
             error_code = excluded.error_code",
    )
    .db()?
    .execute(params![
        scan_id,
        folder_id.to_bytes(),
        rel_dir_key,
        complete,
        error_code.map(ErrorCode::as_str)
    ])
    .db()?;
    Ok(())
}

/// Result of [`finish_scan`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepResult {
    pub deleted: Vec<FileId>,
    /// Unseen files kept because their directory was not listed completely.
    pub kept_unseen: usize,
    /// Some directory was incomplete: the folder is `partial`.
    pub partial: bool,
}

/// Ends a scan: deletes files not seen by `scan_id` **only** where the nearest scanned ancestor
/// directory (the file's own directory, or the parent that listed a now-missing directory) was
/// listed completely. Each delete writes an outbox op in the same transaction.
pub fn finish_scan(
    conn: &Connection,
    folder_id: FolderId,
    scan_id: ScanId,
    now: i64,
) -> Result<SweepResult> {
    atomic(conn, |c| {
        let collection_id = folders::folder(c, folder_id)?.collection_id;
        let mut dirs: HashMap<String, bool> = HashMap::new();
        {
            let mut st = c
                .prepare("SELECT rel_dir_key, complete FROM scan_dirs WHERE scan_id = ?1 AND folder_id = ?2")
                .db()?;
            let rows = st
                .query_map(params![scan_id, folder_id.to_bytes()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
                })
                .db()?;
            for row in rows {
                let (k, done) = row.db()?;
                dirs.insert(k, done);
            }
        }
        let partial = dirs.values().any(|d| !d);
        let unseen: Vec<(FileId, String)> = {
            let mut st = c
                .prepare("SELECT id, dir_key FROM files WHERE folder_id = ?1 AND scan_id <> ?2")
                .db()?;
            st.query_map(params![folder_id.to_bytes(), scan_id], |r| {
                Ok((r.file_id(0)?, r.get::<_, String>(1)?))
            })
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?
        };
        let mut res = SweepResult {
            partial,
            ..SweepResult::default()
        };
        for (id, dir) in unseen {
            let mut cur = dir.as_str();
            let verdict = loop {
                if let Some(done) = dirs.get(cur) {
                    break Some(*done);
                }
                if cur.is_empty() {
                    break None;
                }
                cur = dir_key_of(cur);
            };
            if verdict == Some(true) {
                delete_file_in(c, collection_id, id, now)?;
                res.deleted.push(id);
            } else {
                res.kept_unseen += 1;
            }
        }
        c.execute(
            "UPDATE scans SET finished_at = ?2, partial = ?3 WHERE id = ?1",
            params![scan_id, now, partial],
        )
        .db()?;
        c.execute(
            "UPDATE folders SET last_full_scan_at = ?2, last_scan_partial = ?3, cancelled_at = NULL,
                 updated_at = ?2 WHERE id = ?1",
            params![folder_id.to_bytes(), now, partial],
        )
        .db()?;
        // Older scans' directory marks are no longer needed.
        c.execute(
            "DELETE FROM scans WHERE folder_id = ?1 AND id < ?2",
            params![folder_id.to_bytes(), scan_id],
        )
        .db()?;
        Ok(res)
    })
}

fn delete_file_in(
    conn: &Connection,
    collection_id: sr_common::ids::CollectionId,
    id: FileId,
    now: i64,
) -> Result<()> {
    outbox::append(
        conn,
        collection_id,
        None,
        None,
        Some(id),
        IndexOpKind::Delete,
        0,
        now,
    )?;
    conn.prepare_cached("DELETE FROM files WHERE id = ?1")
        .db()?
        .execute([id.to_bytes()])
        .db()?;
    Ok(())
}

/// Deletes one file (watcher event) with its outbox op. Returns false if it was already gone.
pub fn delete_file(conn: &Connection, id: FileId, now: i64) -> Result<bool> {
    atomic(conn, |c| {
        let row: Option<[u8; 16]> = c
            .query_row(
                "SELECT fo.collection_id FROM files f JOIN folders fo ON fo.id = f.folder_id WHERE f.id = ?1",
                [id.to_bytes()],
                |r| r.get(0),
            )
            .optional()
            .db()?;
        match row {
            Some(col) => {
                delete_file_in(c, sr_common::ids::CollectionId::from_bytes(col), id, now)?;
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// Watcher rename inside one folder: path updated, identity (and chunk IDs) kept.
pub fn rename_file(
    conn: &Connection,
    id: FileId,
    rel_path: &str,
    path_key: &str,
    now: i64,
) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE files SET rel_path = ?2, path_key = ?3, dir_key = ?4, updated_at = ?5 WHERE id = ?1",
            params![id.to_bytes(), rel_path, path_key, dir_key_of(path_key), now],
        )
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::NotFound))
    } else {
        Ok(())
    }
}

/// The file is intentionally not indexed (too large, online-only, …). Its chunks are removed.
pub fn mark_skipped(
    conn: &Connection,
    id: FileId,
    reason: &str,
    code: Option<ErrorCode>,
    now: i64,
) -> Result<()> {
    set_terminal(conn, id, FileState::Skipped, Some(reason), code, now)
}

/// Dead-letter: the file couldn't be read after its retries. Listed as "couldn't be read" and
/// excluded from the folder status (which becomes `synced_with_errors`).
pub fn mark_dead(conn: &Connection, id: FileId, code: ErrorCode, now: i64) -> Result<()> {
    set_terminal(conn, id, FileState::Dead, None, Some(code), now)
}

fn set_terminal(
    conn: &Connection,
    id: FileId,
    state: FileState,
    reason: Option<&str>,
    code: Option<ErrorCode>,
    now: i64,
) -> Result<()> {
    atomic(conn, |c| {
        let had_chunks: bool = c
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM chunks WHERE file_id = ?1)",
                [id.to_bytes()],
                |r| r.get(0),
            )
            .db()?;
        let n = c
            .execute(
                "UPDATE files SET state = ?2, skip_reason = ?3, error_code = ?4, updated_at = ?5 WHERE id = ?1",
                params![id.to_bytes(), state, reason, code.map(ErrorCode::as_str), now],
            )
            .db()?;
        if n == 0 {
            return Err(Error::new(ErrorCode::NotFound));
        }
        if had_chunks {
            let f = file(c, id)?;
            let col = folders::folder(c, f.folder_id)?.collection_id;
            c.execute("DELETE FROM chunks WHERE file_id = ?1", [id.to_bytes()])
                .db()?;
            outbox::append(
                c,
                col,
                None,
                None,
                Some(id),
                IndexOpKind::Delete,
                f.version,
                now,
            )?;
        }
        Ok(())
    })
}

/// Dead-letter list of a folder ("couldn't be read").
pub fn dead_letter(conn: &Connection, folder_id: FolderId) -> Result<Vec<FileRecord>> {
    files_in_state(conn, folder_id, FileState::Dead)
}

pub fn files_in_state(
    conn: &Connection,
    folder_id: FolderId,
    state: FileState,
) -> Result<Vec<FileRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {FILE_COLS} FROM files WHERE folder_id = ?1 AND state = ?2 ORDER BY path_key"
        ))
        .db()?;
    st.query_map(params![folder_id.to_bytes(), state], file_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Files the user should see as **skipped** ("too large", "online-only", ...). Listed files
/// ([`listed_files`]) are not skipped, so they never appear here.
pub fn skipped_files(conn: &Connection, folder_id: FolderId) -> Result<Vec<FileRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {FILE_COLS} FROM files WHERE folder_id = ?1 AND state = 'skipped'
             AND skip_reason IS NOT ?2 ORDER BY path_key"
        ))
        .db()?;
    st.query_map(
        params![folder_id.to_bytes(), SKIP_REASON_FILES_ONLY],
        file_from_row,
    )
    .db()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .db()
}

/// One row of the folder detail's "Skipped files" list (M4, page 25).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    pub id: FileId,
    pub rel_path: String,
    pub state: FileState,
    pub skip_reason: Option<String>,
    pub error_code: Option<String>,
    pub size: u64,
    pub updated_at: i64,
}

/// "Skipped files" (page 25): dead files ("couldn't be read") and skipped ones (too large,
/// online-only), **never** listed files. Newest first, paged. Returns `(total, page)`.
pub fn skipped_page(
    conn: &Connection,
    folder_id: FolderId,
    offset: u32,
    limit: u32,
) -> Result<(u64, Vec<SkippedRow>)> {
    let key = folder_id.to_bytes();
    let total: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM files WHERE folder_id = ?1
               AND (state = 'dead' OR (state = 'skipped' AND skip_reason IS NOT ?2))",
        )
        .db()?
        .query_row(params![key, SKIP_REASON_FILES_ONLY], |r| r.get(0))
        .db()?;
    let mut st = conn
        .prepare_cached(
            "SELECT id, rel_path, state, skip_reason, error_code, size, updated_at FROM files
             WHERE folder_id = ?1 AND (state = 'dead' OR (state = 'skipped' AND skip_reason IS NOT ?2))
             ORDER BY updated_at DESC, row DESC LIMIT ?3 OFFSET ?4",
        )
        .db()?;
    let rows = st
        .query_map(
            params![
                key,
                SKIP_REASON_FILES_ONLY,
                i64::from(limit.max(1)),
                i64::from(offset)
            ],
            |r| {
                Ok(SkippedRow {
                    id: r.file_id(0)?,
                    rel_path: r.get(1)?,
                    state: r.get(2)?,
                    skip_reason: r.get(3)?,
                    error_code: r.get(4)?,
                    size: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    Ok((total.max(0).cast_unsigned(), rows))
}

/// "Retry" (page 25, OQ-39): dead files of the folder (all, or those in `only`) go back to
/// `pending_index` with a fresh attempt budget, and a forced `index_file` job is queued for each.
/// Skipped files (too large, online-only) are left alone: retrying can't change them. Returns the
/// files re-queued.
pub fn retry_dead(
    conn: &Connection,
    folder_id: FolderId,
    only: Option<&[FileId]>,
    now: i64,
) -> Result<Vec<FileId>> {
    atomic(conn, |c| {
        let dead = files_in_state(c, folder_id, FileState::Dead)?;
        let mut out = Vec::new();
        for f in dead {
            if only.is_some_and(|ids| !ids.contains(&f.id)) {
                continue;
            }
            c.execute(
                "UPDATE files SET state = 'pending_index', error_code = NULL, attempts = 0,
                     updated_at = ?2 WHERE id = ?1",
                params![f.id.to_bytes(), now],
            )
            .db()?;
            let mut j = crate::repo::jobs::NewJob::new(
                crate::model::JobKind::IndexFile,
                Some(folder_id),
                Some(f.id),
                crate::model::PRIORITY_INTERACTIVE,
            );
            j.payload = Some(serde_json::json!({ crate::repo::jobs::PAYLOAD_FORCE: true }));
            crate::repo::jobs::enqueue(c, &j, now)?;
            out.push(f.id);
        }
        Ok(out)
    })
}

/// Files listed, not read ([`SKIP_REASON_FILES_ONLY`]): of an access grant, or of a folder paused
/// by Amendment 17 until the user chooses.
pub fn listed_files(conn: &Connection, folder_id: FolderId) -> Result<Vec<FileRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {FILE_COLS} FROM files WHERE folder_id = ?1 AND state = 'skipped'
             AND skip_reason = ?2 ORDER BY path_key"
        ))
        .db()?;
    st.query_map(
        params![folder_id.to_bytes(), SKIP_REASON_FILES_ONLY],
        file_from_row,
    )
    .db()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .db()
}

/// One path-search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHit {
    pub file_id: FileId,
    pub folder_id: FolderId,
    pub rel_path: String,
}

/// `@`-picker / `files.find`: every whitespace-separated term must prefix-match a path token.
/// The user's text is quoted per term, so FTS5 syntax in it is inert.
pub fn find_files(conn: &Connection, query: &str, limit: usize) -> Result<Vec<PathHit>> {
    let terms: Vec<String> = query
        .split(|c: char| c.is_whitespace() || matches!(c, '/' | '\\'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare_cached(
            "SELECT f.id, f.folder_id, f.rel_path FROM files_fts
             JOIN files f ON f.row = files_fts.rowid
             WHERE files_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )
        .db()?;
    st.query_map(
        params![terms.join(" "), i64::try_from(limit).unwrap_or(i64::MAX)],
        |r| {
            Ok(PathHit {
                file_id: r.file_id(0)?,
                folder_id: r.folder_id(1)?,
                rel_path: r.get(2)?,
            })
        },
    )
    .db()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .db()
}
