//! Activity (M4-E, page 30): `activity_log` lines and paged reads of `egress_audit`.
//!
//! The log holds only names, counts and kinds (never file contents or queries). It is kept for
//! 7 days ([`sr_common::api::ACTIVITY_RETENTION_MS`]); `egress_audit` is not pruned here.

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::api::{ActivityEntry, ActivityKind};
use sr_common::error::{ClientKind, Purpose, Result};
use sr_common::ids::FolderId;

use crate::db::DbResultExt;

/// A line to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewActivity {
    pub kind: ActivityKind,
    pub folder_id: Option<FolderId>,
    pub folder_name: Option<String>,
    pub client_kind: Option<ClientKind>,
    pub client_name: Option<String>,
    pub count: Option<u64>,
    pub text: Option<String>,
}

impl NewActivity {
    pub fn new(kind: ActivityKind) -> Self {
        Self {
            kind,
            folder_id: None,
            folder_name: None,
            client_kind: None,
            client_name: None,
            count: None,
            text: None,
        }
    }

    pub fn folder(mut self, id: FolderId, name: impl Into<String>) -> Self {
        self.folder_id = Some(id);
        self.folder_name = Some(name.into());
        self
    }

    pub fn client(mut self, kind: ClientKind, name: impl Into<String>) -> Self {
        self.client_kind = Some(kind);
        self.client_name = Some(name.into());
        self
    }

    pub fn count(mut self, n: u64) -> Self {
        self.count = Some(n);
        self
    }

    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.text = Some(t.into());
        self
    }
}

fn kind_str<T: serde::Serialize>(v: T) -> Option<String> {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

const COLS: &str = "id, ts, kind, folder_id, folder_name, client_kind, client_name, count, text";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ActivityEntry>> {
    let kind: String = r.get(2)?;
    let Some(kind) = ActivityKind::parse(&kind) else {
        return Ok(None); // a kind from a newer build: skipped, never an error
    };
    Ok(Some(ActivityEntry {
        id: r.get(0)?,
        ts: r.get(1)?,
        kind,
        folder_id: r.get::<_, Option<[u8; 16]>>(3)?.map(FolderId::from_bytes),
        folder_name: r.get(4)?,
        client_kind: r
            .get::<_, Option<String>>(5)?
            .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok()),
        client_name: r.get(6)?,
        count: r
            .get::<_, Option<i64>>(7)?
            .map(|n| n.max(0).cast_unsigned()),
        text: r.get(8)?,
    }))
}

/// Append one line; returns it as stored (for the `activity.added` event).
pub fn append(conn: &Connection, a: &NewActivity, now: i64) -> Result<ActivityEntry> {
    let id: i64 = conn
        .query_row(
            "INSERT INTO activity_log(ts, kind, folder_id, folder_name, client_kind, client_name, count, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING id",
            params![
                now,
                a.kind.as_str(),
                a.folder_id.map(|f| f.to_bytes()),
                a.folder_name,
                a.client_kind.and_then(kind_str),
                a.client_name,
                a.count.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
                a.text
            ],
            |r| r.get(0),
        )
        .db()?;
    Ok(ActivityEntry {
        id,
        ts: now,
        kind: a.kind,
        folder_id: a.folder_id,
        folder_name: a.folder_name.clone(),
        client_kind: a.client_kind,
        client_name: a.client_name.clone(),
        count: a.count,
        text: a.text.clone(),
    })
}

/// Newest first, `id < before_id`, not older than `since_ts`. Returns the entries and the
/// `before_id` of the next page (`None` at the end).
pub fn page(
    conn: &Connection,
    before_id: Option<i64>,
    limit: u32,
    since_ts: i64,
) -> Result<(Vec<ActivityEntry>, Option<i64>)> {
    let limit = limit.max(1);
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {COLS} FROM activity_log WHERE id < ?1 AND ts >= ?2 ORDER BY id DESC LIMIT ?3"
        ))
        .db()?;
    let rows = st
        .query_map(
            params![
                before_id.unwrap_or(i64::MAX),
                since_ts,
                i64::from(limit) + 1
            ],
            from_row,
        )
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    let more = rows.len() > limit as usize;
    let mut last_id = None;
    let mut out = Vec::new();
    for (i, r) in rows.into_iter().enumerate() {
        if i == limit as usize {
            break;
        }
        if let Some(e) = r {
            last_id = Some(e.id);
            out.push(e);
        }
    }
    Ok((out, if more { last_id } else { None }))
}

/// Drop lines older than `keep_ms`.
pub fn prune(conn: &Connection, now: i64, keep_ms: i64) -> Result<usize> {
    conn.execute(
        "DELETE FROM activity_log WHERE ts < ?1",
        [now.saturating_sub(keep_ms)],
    )
    .db()
}

/// One `egress_audit` row, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRecord {
    pub id: i64,
    pub ts: i64,
    pub provider: String,
    pub purpose: Option<Purpose>,
    pub bytes: u64,
    pub client_id: Option<String>,
}

fn egress_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EgressRecord> {
    let purpose: String = r.get(3)?;
    Ok(EgressRecord {
        id: r.get(0)?,
        ts: r.get(1)?,
        provider: r.get(2)?,
        purpose: serde_json::from_value(serde_json::Value::String(purpose)).ok(),
        bytes: r.get::<_, i64>(4)?.max(0).cast_unsigned(),
        client_id: r.get(5)?,
    })
}

/// Egress rows, newest first (`id < before_id`), optionally for one provider.
pub fn egress_page(
    conn: &Connection,
    before_id: Option<i64>,
    limit: u32,
    provider: Option<&str>,
) -> Result<(Vec<EgressRecord>, Option<i64>)> {
    let limit = limit.max(1);
    let mut st = conn
        .prepare_cached(
            "SELECT id, ts, provider, purpose, bytes, client_id FROM egress_audit
             WHERE id < ?1 AND (?2 IS NULL OR provider = ?2) ORDER BY id DESC LIMIT ?3",
        )
        .db()?;
    let mut rows = st
        .query_map(
            params![
                before_id.unwrap_or(i64::MAX),
                provider,
                i64::from(limit) + 1
            ],
            egress_from_row,
        )
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    Ok((rows, next))
}

/// The newest egress row (the audit writer publishes it as `egress.recorded`).
pub fn last_egress(conn: &Connection) -> Result<Option<EgressRecord>> {
    conn.query_row(
        "SELECT id, ts, provider, purpose, bytes, client_id FROM egress_audit ORDER BY id DESC LIMIT 1",
        [],
        egress_from_row,
    )
    .optional()
    .db()
}

/// Egress rows after `after_id`, oldest first (event publishing after an audit batch).
pub fn egress_after(conn: &Connection, after_id: i64, limit: u32) -> Result<Vec<EgressRecord>> {
    let mut st = conn
        .prepare_cached(
            "SELECT id, ts, provider, purpose, bytes, client_id FROM egress_audit
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )
        .db()?;
    st.query_map(params![after_id, i64::from(limit.max(1))], egress_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Every provider ever audited.
pub fn egress_providers(conn: &Connection) -> Result<Vec<String>> {
    let mut st = conn
        .prepare_cached("SELECT DISTINCT provider FROM egress_audit ORDER BY provider")
        .db()?;
    st.query_map([], |r| r.get(0))
        .db()?
        .collect::<rusqlite::Result<Vec<String>>>()
        .db()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::privacy;
    use crate::schema::migrations;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrations().to_latest(&mut c).unwrap();
        c
    }

    #[test]
    fn pages_newest_first_and_prunes() {
        let c = db();
        let f = FolderId::new();
        for i in 0..5 {
            append(
                &c,
                &NewActivity::new(ActivityKind::FolderSynced)
                    .folder(f, "Docs")
                    .count(i),
                100 + i64::try_from(i).unwrap(),
            )
            .unwrap();
        }
        let (p1, next) = page(&c, None, 2, 0).unwrap();
        assert_eq!(
            p1.iter().map(|e| e.count).collect::<Vec<_>>(),
            [Some(4), Some(3)]
        );
        assert_eq!(p1[0].folder_name.as_deref(), Some("Docs"));
        let (p2, next2) = page(&c, next, 2, 0).unwrap();
        assert_eq!(
            p2.iter().map(|e| e.count).collect::<Vec<_>>(),
            [Some(2), Some(1)]
        );
        let (p3, next3) = page(&c, next2, 2, 0).unwrap();
        assert_eq!(p3.len(), 1);
        assert!(next3.is_none());
        // Retention: "older than keep_ms" = ts < now - keep_ms (ts 100, 101 go; 102 is exactly
        // keep_ms old and stays, like the 7-day `since` filter of `page`).
        assert_eq!(prune(&c, 103, 1).unwrap(), 2);
        assert_eq!(page(&c, None, 50, 0).unwrap().0.len(), 3);
        // Client kind round trip.
        let e = append(
            &c,
            &NewActivity::new(ActivityKind::ClientConnected).client(ClientKind::VsCode, "VS Code"),
            200,
        )
        .unwrap();
        let (p, _) = page(&c, None, 1, 0).unwrap();
        assert_eq!(p[0], e);
        assert_eq!(p[0].client_kind, Some(ClientKind::VsCode));
    }

    #[test]
    fn egress_pages_by_provider() {
        let c = db();
        for (i, p) in ["deepseek", "openrouter", "deepseek"].iter().enumerate() {
            privacy::audit_egress(
                &c,
                p,
                Purpose::Answer,
                100,
                None,
                Some("claude"),
                i64::try_from(i).unwrap(),
            )
            .unwrap();
        }
        let (rows, next) = egress_page(&c, None, 1, Some("deepseek")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].purpose, Some(Purpose::Answer));
        let (rows2, next2) = egress_page(&c, next, 5, Some("deepseek")).unwrap();
        assert_eq!(rows2.len(), 1);
        assert!(next2.is_none());
        assert_eq!(egress_providers(&c).unwrap(), ["deepseek", "openrouter"]);
        assert_eq!(last_egress(&c).unwrap().unwrap().provider, "deepseek");
        assert_eq!(egress_after(&c, 1, 10).unwrap().len(), 2);
    }
}
