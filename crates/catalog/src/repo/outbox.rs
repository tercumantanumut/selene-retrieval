//! The outbox (`index_ops`) and store watermarks (architect D3, docs/00 §0.A C2).
//!
//! Rule: **at-least-once + idempotent.** Each store persists its watermark after its own data
//! commit; on restart ops with `seq > watermark` are replayed, coalesced per file. Replay is safe
//! because stores delete by `file_id` and upsert by `chunk_id`.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::error::Result;
use sr_common::ids::{CollectionId, FileId, FolderId};

use crate::db::DbResultExt;
use crate::model::{IndexOpKind, ProfileId, RowExt, Seq, Store};

/// Appends one op; returns its `seq` (strictly increasing, never reused).
#[allow(clippy::too_many_arguments)]
pub fn append(
    conn: &Connection,
    collection_id: CollectionId,
    profile_id: Option<ProfileId>,
    folder_id: Option<FolderId>,
    file_id: Option<FileId>,
    op: IndexOpKind,
    file_version: i64,
    now: i64,
) -> Result<Seq> {
    conn.prepare_cached(
        "INSERT INTO index_ops(collection_id, profile_id, folder_id, file_id, op, file_version, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .db()?
    .execute(params![
        collection_id.to_bytes(),
        profile_id,
        folder_id.map(|f| f.to_bytes()),
        file_id.map(|f| f.to_bytes()),
        op,
        file_version,
        now
    ])
    .db()?;
    Ok(conn.last_insert_rowid())
}

/// Highest seq ever assigned (0 if none); survives pruning (`sqlite_sequence`).
pub fn max_seq(conn: &Connection) -> Result<Seq> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'index_ops'",
            [],
            |r| r.get::<_, Seq>(0),
        )
        .optional()
        .db()?
        .unwrap_or(0))
}

/// One raw outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawOp {
    pub seq: Seq,
    pub collection_id: CollectionId,
    pub folder_id: Option<FolderId>,
    pub file_id: Option<FileId>,
    pub op: IndexOpKind,
    pub file_version: i64,
}

/// What a store must do, after per-file coalescing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoalescedOp {
    /// Delete every row of the file, then insert its **current** chunks (catalog state at read
    /// time, `file_version` = the version they belong to).
    Upsert { file_id: FileId, file_version: i64 },
    /// Delete every row of the file (deleted, or no longer has chunks).
    Delete { file_id: FileId },
    /// Delete every row of the folder.
    PurgeFolder { folder_id: FolderId },
}

/// A batch of coalesced ops. After applying it, the store's watermark becomes `through_seq`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpBatch {
    /// In order of each entry's latest seq.
    pub ops: Vec<(Seq, CoalescedOp)>,
    /// Last raw seq covered (including ops coalesced away). Equals the input `after` when empty.
    pub through_seq: Seq,
    /// Raw rows read.
    pub raw: usize,
}

/// Reads up to `limit` raw ops of `collection_id` after `after` and coalesces them per file:
/// the last op for a file wins; an upsert whose file no longer exists (or has no chunks at its
/// current version) becomes a delete, so replay never resurrects data.
pub fn ops_after(
    conn: &Connection,
    collection_id: CollectionId,
    after: Seq,
    limit: usize,
) -> Result<OpBatch> {
    let mut st = conn
        .prepare_cached(
            "SELECT seq, collection_id, folder_id, file_id, op, file_version FROM index_ops
             WHERE collection_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )
        .db()?;
    let raw: Vec<RawOp> = st
        .query_map(
            params![
                collection_id.to_bytes(),
                after,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(RawOp {
                    seq: r.get(0)?,
                    collection_id: r.collection_id(1)?,
                    folder_id: r.opt_folder_id(2)?,
                    file_id: r.opt_file_id(3)?,
                    op: r.get(4)?,
                    file_version: r.get(5)?,
                })
            },
        )
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    let through_seq = raw.last().map_or(after, |o| o.seq);
    let n = raw.len();

    #[derive(PartialEq, Eq, Hash)]
    enum Key {
        File(FileId),
        Folder(FolderId),
    }
    let mut last: HashMap<Key, RawOp> = HashMap::new();
    for o in raw {
        let key = match (o.op, o.file_id, o.folder_id) {
            (IndexOpKind::PurgeFolder, _, Some(f)) => Key::Folder(f),
            (_, Some(f), _) => Key::File(f),
            _ => continue, // rejected by CHECK constraints; defensive
        };
        last.insert(key, o);
    }
    let mut cur = conn
        .prepare_cached(
            "SELECT f.version FROM files f WHERE f.id = ?1
             AND EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id AND c.file_version = f.version)",
        )
        .db()?;
    let mut ops = Vec::with_capacity(last.len());
    for (_, o) in last {
        let op = match (o.op, o.file_id, o.folder_id) {
            (IndexOpKind::PurgeFolder, _, Some(folder_id)) => {
                CoalescedOp::PurgeFolder { folder_id }
            }
            (IndexOpKind::Delete, Some(file_id), _) => CoalescedOp::Delete { file_id },
            (IndexOpKind::Upsert, Some(file_id), _) => {
                match cur
                    .query_row([file_id.to_bytes()], |r| r.get::<_, i64>(0))
                    .optional()
                    .db()?
                {
                    Some(file_version) => CoalescedOp::Upsert {
                        file_id,
                        file_version,
                    },
                    None => CoalescedOp::Delete { file_id },
                }
            }
            _ => continue,
        };
        ops.push((o.seq, op));
    }
    ops.sort_by_key(|(s, _)| *s);
    Ok(OpBatch {
        ops,
        through_seq,
        raw: n,
    })
}

/// Raises a store's cached watermark (never lowers it).
pub fn set_watermark(conn: &Connection, store: Store, scope_id: &[u8; 16], seq: Seq) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO store_watermarks(store, scope_id, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT(store, scope_id) DO UPDATE SET seq = max(seq, excluded.seq)",
    )
    .db()?
    .execute(params![store, scope_id, seq])
    .db()?;
    Ok(())
}

/// Cached watermark of a store scope (0 = replay everything still in the outbox).
pub fn watermark(conn: &Connection, store: Store, scope_id: &[u8; 16]) -> Result<Seq> {
    Ok(conn
        .prepare_cached("SELECT seq FROM store_watermarks WHERE store = ?1 AND scope_id = ?2")
        .db()?
        .query_row(params![store, scope_id], |r| r.get(0))
        .optional()
        .db()?
        .unwrap_or(0))
}

/// The seq up to which every consumer of `collection_id` has applied the outbox:
/// `min(tantivy[collection], lance[every active or building index of the collection])`.
pub fn applied_floor(conn: &Connection, collection_id: CollectionId) -> Result<Seq> {
    let mut floor = watermark(conn, Store::Tantivy, &collection_id.to_bytes())?;
    let mut st = conn
        .prepare_cached(
            "SELECT id FROM vector_indexes WHERE collection_id = ?1 AND state IN ('active','building')",
        )
        .db()?;
    let ids = st
        .query_map([collection_id.to_bytes()], |r| r.get::<_, [u8; 16]>(0))
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    for id in ids {
        floor = floor.min(watermark(conn, Store::Lance, &id)?);
    }
    Ok(floor)
}

/// Prunes ops every store has applied (`seq <= applied_floor`), per collection. Ops of deleted
/// collections are dropped (their stores are gone with the collection directory).
pub fn prune(conn: &Connection) -> Result<usize> {
    let mut st = conn
        .prepare("SELECT DISTINCT collection_id FROM index_ops")
        .db()?;
    let cols = st
        .query_map([], |r| r.collection_id(0))
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    let mut n = 0;
    for c in cols {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM collections WHERE id = ?1)",
                [c.to_bytes()],
                |r| r.get(0),
            )
            .db()?;
        n += if exists {
            let floor = applied_floor(conn, c)?;
            conn.execute(
                "DELETE FROM index_ops WHERE collection_id = ?1 AND seq <= ?2",
                params![c.to_bytes(), floor],
            )
            .db()?
        } else {
            conn.execute(
                "DELETE FROM index_ops WHERE collection_id = ?1",
                [c.to_bytes()],
            )
            .db()?
        };
    }
    Ok(n)
}

/// Pending (unpruned) ops of a collection.
pub fn pending(conn: &Connection, collection_id: CollectionId) -> Result<u64> {
    conn.query_row(
        "SELECT count(*) FROM index_ops WHERE collection_id = ?1",
        [collection_id.to_bytes()],
        |r| r.get(0),
    )
    .db()
}
