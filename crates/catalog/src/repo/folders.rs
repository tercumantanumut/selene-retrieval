//! Folders: insert (with the overlap rule), settings, run metadata, removal.
//!
//! Per-folder settings live here (one home per setting) but their schema, defaults and
//! validation are `sr_common::config::FolderSettings`; rows are read back through
//! `FolderSettings::from_json_lenient`, so a bad stored value falls back to its default.
//!
//! The `indexing_mode` column is no longer a setting (Amendment 17): new rows keep its default
//! `vector`, and `files_only` marks a folder that was in the removed Files-only mode
//! ([`FolderRecord::was_files_only`], set by migration V5, cleared by [`clear_files_only_mark`]).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use sr_common::config::FolderSettings;
use sr_common::error::{ErrCtx, Error, ErrorCode, Result};
use sr_common::events::FolderStatus;
use sr_common::ids::{CollectionId, FolderId};

use crate::db::{DbResultExt, atomic};
use crate::model::{
    FolderKind, IndexOpKind, RowExt, RunTrigger, WatchSupport, vs_from_sql, vs_to_sql,
};
use crate::repo::outbox;

/// Catalog key of an absolute root: `/`-separated, case-folded, no trailing separator,
/// no verbatim prefix. Used for `UNIQUE(collection_id, root_key)` and the overlap rule.
#[cfg(windows)]
pub fn root_key(root: &Path) -> String {
    let s = root.to_string_lossy();
    let s = s
        .strip_prefix(r"\\?\")
        .map_or_else(|| s.to_string(), str::to_string);
    let mut k = s.replace('\\', "/").to_lowercase();
    while k.len() > 1 && k.ends_with('/') && !k.ends_with(":/") {
        k.pop();
    }
    k
}

/// Catalog key of an absolute root on Unix: the path as given (exact case, `\` is part of a
/// name), no trailing `/`. Roots are canonical (`Sandbox::add_root`), and on case-insensitive
/// APFS `canonicalize` returns the on-disk case, so one folder has one key; on a case-sensitive
/// volume `Docs` and `docs` are two folders and do not overlap.
#[cfg(not(windows))]
pub fn root_key(root: &Path) -> String {
    let mut k = root.to_string_lossy().into_owned();
    while k.len() > 1 && k.ends_with('/') {
        k.pop();
    }
    k
}

fn overlaps(a: &str, b: &str) -> bool {
    let inside = |child: &str, parent: &str| {
        child == parent
            || child
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/') || parent.ends_with('/'))
    };
    inside(a, b) || inside(b, a)
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewFolder {
    pub collection_id: CollectionId,
    pub kind: FolderKind,
    pub root_path: PathBuf,
    /// Full 64-bit volume serial of the root (ReFS uses all 64 bits).
    pub volume_serial: Option<u64>,
    pub is_primary: bool,
    pub settings: FolderSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FolderRecord {
    pub id: FolderId,
    pub collection_id: CollectionId,
    pub kind: FolderKind,
    pub root_path: PathBuf,
    pub root_key: String,
    /// 64-bit volume serial. Catalogs written before V3 hold only the low 32 bits until sr-sync
    /// upgrades them ([`upgrade_volume_serial`]).
    pub volume_serial: Option<u64>,
    pub is_primary: bool,
    pub settings: FolderSettings,
    pub paused: bool,
    /// When the folder was paused (V3; `None` when not paused).
    pub paused_at: Option<i64>,
    /// The user stopped the last run (V3). Clears when the next full scan finishes.
    pub cancelled_at: Option<i64>,
    pub available: bool,
    /// Whether the live watcher serves this folder (V3).
    pub watch_support: WatchSupport,
    /// The folder was in the removed Files-only mode (Amendment 17): migration V5 paused it, so
    /// nothing in it is read, and the app asks the user to search its contents or remove it.
    /// Cleared when the user resumes it ([`clear_files_only_mark`]).
    pub was_files_only: bool,
    /// Cached derived status (see [`crate::repo::status`]).
    pub status: FolderStatus,
    pub status_reason_code: Option<String>,
    pub generation: i64,
    pub last_full_scan_at: Option<i64>,
    pub last_scan_partial: bool,
    pub last_success_at: Option<i64>,
    pub last_run_trigger: Option<RunTrigger>,
    pub stats: Value,
}

impl FolderRecord {
    /// Name shown to the user: the display name, else the folder's own name.
    pub fn display_name(&self) -> String {
        if self.settings.display_name.trim().is_empty() {
            self.root_path.file_name().map_or_else(
                || self.root_path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        } else {
            self.settings.display_name.clone()
        }
    }
}

pub(crate) fn status_str(s: FolderStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "error".into())
}

pub(crate) fn parse_status(s: &str) -> FolderStatus {
    serde_json::from_value(Value::from(s)).unwrap_or(FolderStatus::Error)
}

/// Settings → column values (`(column, value)`), from the schema's serde names.
fn settings_columns(s: &FolderSettings) -> Result<Map<String, Value>> {
    match serde_json::to_value(s) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(Error::new(ErrorCode::Internal).with_detail("folder settings did not serialise")),
    }
}

const SETTINGS_COLS: [&str; 15] = [
    "display_name",
    "keep_up_to_date",
    "file_types_preset",
    "include_online_only",
    "sync_mode",
    "cadence_min",
    "reindex_policy",
    "recursive",
    "include_extensions",
    "exclude_globs",
    "max_file_bytes",
    "chunk_preset",
    "chunk_size_override",
    "chunk_overlap_override",
    "follow_symlinks",
];

/// JSON value → SQL value for one settings column (arrays stored as JSON text, bools as 0/1).
fn to_sql_value(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as V;
    match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map_or(V::Null, V::Integer),
        Value::String(s) => V::Text(s.clone()),
        other => V::Text(other.to_string()),
    }
}

fn check_overlap(conn: &Connection, f: &NewFolder, key: &str) -> Result<()> {
    if f.kind != FolderKind::Indexed {
        return Ok(());
    }
    let mut st = conn
        .prepare(
            "SELECT root_key, display_name, root_path FROM folders
             WHERE collection_id = ?1 AND kind = 'indexed'",
        )
        .db()?;
    let rows = st
        .query_map([f.collection_id.to_bytes()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .db()?;
    for row in rows {
        let (other_key, name, path) = row.db()?;
        if overlaps(key, &other_key) {
            let other = if name.trim().is_empty() {
                Path::new(&path)
                    .file_name()
                    .map_or(path.clone(), |n| n.to_string_lossy().into_owned())
            } else {
                name
            };
            return Err(Error::new(ErrorCode::FolderOverlap).with_ctx(ErrCtx {
                other: Some(other),
                ..ErrCtx::default()
            }));
        }
    }
    Ok(())
}

/// The overlap rule of [`insert_folder`] for a root key, without inserting: `FolderOverlap`
/// ("Already included in …") if an indexed folder of `collection_id` contains or is inside it.
/// Lets an MCP `add_folder` request be refused before the user is asked to approve it.
pub fn check_overlap_key(
    conn: &Connection,
    collection_id: CollectionId,
    root_key: &str,
) -> Result<()> {
    let probe = NewFolder {
        collection_id,
        kind: FolderKind::Indexed,
        root_path: PathBuf::new(),
        volume_serial: None,
        is_primary: false,
        settings: FolderSettings::default(),
    };
    check_overlap(conn, &probe, root_key)
}

/// Adds a folder. Within one collection an overlapping indexed folder is refused with
/// `FolderOverlap` ("Already included in …"); across collections overlap is allowed.
pub fn insert_folder(conn: &Connection, f: &NewFolder, now: i64) -> Result<FolderId> {
    let key = root_key(&f.root_path);
    atomic(conn, |c| {
        check_overlap(c, f, &key)?;
        let id = FolderId::new();
        c.execute(
            "INSERT INTO folders(id, collection_id, kind, is_primary, root_path, root_key, volume_serial,
                 status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?8)",
            params![
                id.to_bytes(),
                f.collection_id.to_bytes(),
                f.kind,
                f.is_primary,
                f.root_path.to_string_lossy(),
                key,
                f.volume_serial.map(vs_to_sql),
                now
            ],
        )
        .db()?;
        write_settings(c, id, &f.settings, now, false)?;
        Ok(id)
    })
}

fn write_settings(
    conn: &Connection,
    id: FolderId,
    s: &FolderSettings,
    now: i64,
    bump: bool,
) -> Result<()> {
    let m = settings_columns(s)?;
    let sets = SETTINGS_COLS
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ?{}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE folders SET {sets}, updated_at = ?{n}, generation = generation + ?{g} WHERE id = ?1",
        n = SETTINGS_COLS.len() + 2,
        g = SETTINGS_COLS.len() + 3
    );
    let mut vals: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Blob(id.to_bytes().to_vec())];
    vals.extend(
        SETTINGS_COLS
            .iter()
            .map(|c| to_sql_value(m.get(*c).unwrap_or(&Value::Null))),
    );
    vals.push(rusqlite::types::Value::Integer(now));
    vals.push(rusqlite::types::Value::Integer(i64::from(bump)));
    let n = conn.execute(&sql, rusqlite::params_from_iter(vals)).db()?;
    if n == 0 {
        return Err(Error::new(ErrorCode::NotFound));
    }
    Ok(())
}

/// Replaces a folder's settings. Bumps `generation` when anything that changes indexing output
/// changed, so the status derivation and the reconciler know older work is stale.
pub fn update_settings(
    conn: &Connection,
    id: FolderId,
    s: &FolderSettings,
    now: i64,
) -> Result<bool> {
    atomic(conn, |c| {
        let old = folder(c, id)?.settings;
        let affects = old.file_types_preset != s.file_types_preset
            || old.include_extensions != s.include_extensions
            || old.exclude_globs != s.exclude_globs
            || old.max_file_bytes != s.max_file_bytes
            || old.recursive != s.recursive
            || old.include_online_only != s.include_online_only
            || old.follow_symlinks != s.follow_symlinks;
        write_settings(c, id, s, now, affects)?;
        Ok(affects)
    })
}

/// Columns 7..22 are [`SETTINGS_COLS`]; `indexing_mode` (22) is the Amendment 17 mark, not a
/// setting.
const FOLDER_COLS: &str = "id, collection_id, kind, root_path, root_key, volume_serial, is_primary,
     display_name, keep_up_to_date, file_types_preset, include_online_only, sync_mode,
     cadence_min, reindex_policy, recursive, include_extensions, exclude_globs, max_file_bytes,
     chunk_preset, chunk_size_override, chunk_overlap_override, follow_symlinks, indexing_mode,
     paused, available, status, status_reason_code, generation, last_full_scan_at, last_scan_partial,
     last_success_at, last_run_trigger, stats_json, paused_at, cancelled_at, watch_support";

fn folder_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FolderRecord> {
    use rusqlite::types::ValueRef;
    let mut m = Map::new();
    for (i, col) in SETTINGS_COLS.iter().enumerate() {
        let v = match r.get_ref(7 + i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => match *col {
                "keep_up_to_date" | "include_online_only" | "recursive" | "follow_symlinks" => {
                    Value::Bool(n != 0)
                }
                _ => json!(n),
            },
            ValueRef::Text(t) => {
                let s = String::from_utf8_lossy(t).into_owned();
                if matches!(*col, "include_extensions" | "exclude_globs") {
                    serde_json::from_str(&s).unwrap_or(Value::Null)
                } else {
                    Value::String(s)
                }
            }
            _ => Value::Null,
        };
        if !v.is_null() {
            m.insert((*col).to_string(), v);
        }
    }
    let (settings, issues) = FolderSettings::from_json_lenient(&Value::Object(m));
    if !issues.is_empty() {
        tracing::warn!(
            n = issues.len(),
            "stored folder settings were reset to defaults"
        );
    }
    let stats: String = r.get(32)?;
    let kind: FolderKind = r.get(2)?;
    let was_files_only = kind == FolderKind::Indexed && r.get::<_, String>(22)? == FILES_ONLY_MARK;
    Ok(FolderRecord {
        id: r.folder_id(0)?,
        collection_id: r.collection_id(1)?,
        kind,
        root_path: PathBuf::from(r.get::<_, String>(3)?),
        root_key: r.get(4)?,
        volume_serial: r.get::<_, Option<i64>>(5)?.map(vs_from_sql),
        is_primary: r.get(6)?,
        settings,
        paused: r.get(23)?,
        paused_at: r.get(33)?,
        cancelled_at: r.get(34)?,
        available: r.get(24)?,
        watch_support: r.get(35)?,
        was_files_only,
        status: parse_status(&r.get::<_, String>(25)?),
        status_reason_code: r.get(26)?,
        generation: r.get(27)?,
        last_full_scan_at: r.get(28)?,
        last_scan_partial: r.get(29)?,
        last_success_at: r.get(30)?,
        last_run_trigger: r.get(31)?,
        stats: serde_json::from_str(&stats).unwrap_or_else(|_| json!({})),
    })
}

pub fn folder(conn: &Connection, id: FolderId) -> Result<FolderRecord> {
    conn.query_row(
        &format!("SELECT {FOLDER_COLS} FROM folders WHERE id = ?1"),
        [id.to_bytes()],
        folder_from_row,
    )
    .db()
}

pub fn folders(conn: &Connection) -> Result<Vec<FolderRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {FOLDER_COLS} FROM folders ORDER BY created_at, id"
        ))
        .db()?;
    st.query_map([], folder_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

pub fn folder_ids(conn: &Connection) -> Result<Vec<FolderId>> {
    let mut st = conn.prepare_cached("SELECT id FROM folders").db()?;
    st.query_map([], |r| r.folder_id(0))
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// `folders.indexing_mode` of a folder that was in the removed Files-only mode (Amendment 17).
const FILES_ONLY_MARK: &str = "files_only";

/// Clears [`FolderRecord::was_files_only`] (the user chose to search the folder's contents).
/// Returns whether the mark was set.
pub fn clear_files_only_mark(conn: &Connection, id: FolderId, now: i64) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE folders SET indexing_mode = 'vector', updated_at = ?3
             WHERE id = ?1 AND indexing_mode = ?2",
            params![id.to_bytes(), FILES_ONLY_MARK, now],
        )
        .db()?;
    Ok(n > 0)
}

/// Pauses or resumes a folder. While paused, the claim holds every job of the folder (nothing is
/// dropped and no attempt is consumed) and the status shows `paused`.
pub fn set_paused(conn: &Connection, id: FolderId, paused: bool, now: i64) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE folders SET paused = ?2, paused_at = CASE WHEN ?2 THEN coalesce(paused_at, ?3) END,
                 updated_at = ?3 WHERE id = ?1",
            params![id.to_bytes(), paused, now],
        )
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::NotFound))
    } else {
        Ok(())
    }
}

/// Marks (or clears) a user-cancelled run. See [`FolderRecord::cancelled_at`].
pub fn set_cancelled(conn: &Connection, id: FolderId, at: Option<i64>, now: i64) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE folders SET cancelled_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_bytes(), at, now],
        )
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::NotFound))
    } else {
        Ok(())
    }
}

/// Records whether the live watcher can serve the folder. Returns whether the value changed.
pub fn set_watch_support(
    conn: &Connection,
    id: FolderId,
    ws: WatchSupport,
    now: i64,
) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE folders SET watch_support = ?2, updated_at = ?3 WHERE id = ?1 AND watch_support <> ?2",
            params![id.to_bytes(), ws, now],
        )
        .db()?;
    Ok(n == 1)
}

/// One-time upgrade of a pre-V3 32-bit volume serial to the full 64-bit value, for the folder and
/// every file row that still carries the legacy value (rename detection matches on it). Only
/// applies when the stored value equals the low 32 bits of `full`; returns the files updated, or
/// `None` if the folder did not hold the legacy value.
pub fn upgrade_volume_serial(
    conn: &Connection,
    id: FolderId,
    full: u64,
    now: i64,
) -> Result<Option<usize>> {
    let low = full & 0xFFFF_FFFF;
    if low == full {
        return Ok(None);
    }
    atomic(conn, |c| {
        let n = c
            .execute(
                "UPDATE folders SET volume_serial = ?2, updated_at = ?4 WHERE id = ?1 AND volume_serial = ?3",
                params![id.to_bytes(), vs_to_sql(full), vs_to_sql(low), now],
            )
            .db()?;
        if n == 0 {
            return Ok(None);
        }
        let files = c
            .execute(
                "UPDATE files SET volume_serial = ?2 WHERE folder_id = ?1 AND volume_serial = ?3",
                params![id.to_bytes(), vs_to_sql(full), vs_to_sql(low)],
            )
            .db()?;
        Ok(Some(files))
    })
}

/// Root re-validation result: an unreachable root is `unavailable` (never deleted).
pub fn set_available(
    conn: &Connection,
    id: FolderId,
    available: bool,
    reason: Option<ErrorCode>,
    now: i64,
) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE folders SET available = ?2, status_reason_code = ?3, updated_at = ?4 WHERE id = ?1",
            params![id.to_bytes(), available, reason.map(ErrorCode::as_str), now],
        )
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::NotFound))
    } else {
        Ok(())
    }
}

/// Records how and when a run started (`last_run_trigger`) and its stats when it finished.
pub fn record_run(
    conn: &Connection,
    id: FolderId,
    trigger: RunTrigger,
    stats: Option<&Value>,
    success_at: Option<i64>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET last_run_trigger = ?2, stats_json = coalesce(?3, stats_json),
             last_success_at = coalesce(?4, last_success_at), updated_at = ?5 WHERE id = ?1",
        params![
            id.to_bytes(),
            trigger,
            stats.map(Value::to_string),
            success_at,
            now
        ],
    )
    .db()?;
    Ok(())
}

/// Removes a folder (purge SLA step 1+2, §0.A M9): one transaction writes a `purge_folder` outbox
/// op, then deletes the folder; files, chunks (chunk_text refcounts via triggers), scans and jobs
/// cascade. The derived stores drop the folder's rows when they apply the op.
pub fn remove_folder(conn: &Connection, id: FolderId, now: i64) -> Result<()> {
    atomic(conn, |c| {
        let f = folder(c, id)?;
        outbox::append(
            c,
            f.collection_id,
            None,
            Some(id),
            None,
            IndexOpKind::PurgeFolder,
            0,
            now,
        )?;
        c.execute("DELETE FROM folders WHERE id = ?1", [id.to_bytes()])
            .db()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn root_keys_fold_case_and_separators() {
        assert_eq!(
            root_key(Path::new(r"C:\Users\Me\Docs\")),
            "c:/users/me/docs"
        );
        assert_eq!(root_key(Path::new(r"\\?\C:\Data")), "c:/data");
        assert_eq!(root_key(Path::new(r"D:\")), "d:/");
    }

    #[test]
    fn overlap_rule() {
        assert!(overlaps("c:/a", "c:/a"));
        assert!(overlaps("c:/a/b", "c:/a"));
        assert!(overlaps("c:/a", "c:/a/b"));
        assert!(!overlaps("c:/ab", "c:/a"));
        assert!(overlaps("d:/x", "d:/"));
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_root_keys_keep_case_and_backslashes() {
        assert_eq!(root_key(Path::new("/Users/Me/Docs/")), "/Users/Me/Docs");
        assert_eq!(root_key(Path::new("/")), "/");
        assert_eq!(root_key(Path::new(r"/Vol/a\b")), r"/Vol/a\b");
        assert_ne!(
            root_key(Path::new("/Vol/Docs")),
            root_key(Path::new("/Vol/docs"))
        );
        assert!(overlaps("/Users/me", "/Users/me/Docs"));
        assert!(!overlaps("/Vol/Docs", "/Vol/docs/sub"));
        assert!(!overlaps("/Vol/a", "/Vol/ab"));
        assert!(overlaps("/Vol/x", "/"));
    }
}
