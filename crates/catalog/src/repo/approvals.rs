//! Approvals (M2): requests from MCP clients that the user must approve in our own UI
//! (docs/02-architecture.md §2.6 "Folder overreach"). Only `add_folder` exists.
//!
//! * The MCP `add_folder` tool is **off by default** (`settings.toml` `[integrations]
//!   mcp_add_folder_enabled`); the gate is enforced by sr-sync, the catalog only records requests.
//! * A request is `pending` until the user approves or denies it, or it **expires** (default
//!   10 minutes, R-UX12). Expiry is applied lazily by every read and by [`expire`].
//! * A client asking again for the same folder while a request is pending gets the same request
//!   back (`UNIQUE(kind, client_id, target_key) WHERE state = 'pending'`), so retries never pile up
//!   prompts.
//! * Decisions are final: deciding a request that is no longer pending is a `Conflict`; deciding an
//!   expired one marks it `expired` and returns `ApprovalDenied` ("…or it expired").

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sr_common::error::{ClientKind, Error, ErrorCode, Result};
use sr_common::ids::{ApprovalId, CollectionId, FolderId};

use crate::db::{DbResultExt, atomic};
use crate::model::{ApprovalKind, ApprovalState};

/// How long a request waits for the user (R-UX12).
pub const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;

/// What an `add_folder` request asks for (stored as the approval payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddFolderRequest {
    /// Connection id of the asking client (handshake `client_id`).
    pub client_id: String,
    pub client_kind: ClientKind,
    /// Display name when the client is not a known kind.
    pub client_name: Option<String>,
    pub collection_id: CollectionId,
    /// Canonical root (validated by sr-sync before the request is recorded).
    pub root_path: PathBuf,
    /// `folders::root_key(root_path)`: the dedupe key.
    pub root_key: String,
}

/// One approval row.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRecord {
    pub id: ApprovalId,
    pub kind: ApprovalKind,
    pub client_id: String,
    pub client_kind: ClientKind,
    pub client_name: Option<String>,
    pub collection_id: Option<CollectionId>,
    /// The folder the client asked for (canonical).
    pub root_path: PathBuf,
    pub state: ApprovalState,
    pub created_at: i64,
    pub expires_at: i64,
    pub decided_at: Option<i64>,
    /// Folder created on approval.
    pub folder_id: Option<FolderId>,
    /// Why an approved request could not be carried out (e.g. `FolderOverlap`).
    pub error_code: Option<String>,
    pub payload: Value,
}

fn client_kind_str(k: ClientKind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

fn parse_client_kind(s: &str) -> ClientKind {
    serde_json::from_value(Value::from(s)).unwrap_or(ClientKind::Other)
}

const COLS: &str = "id, kind, client_id, client_kind, payload, state, created_at, expires_at,
     decided_at, folder_id, error_code";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    let payload: Value = serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(Value::Null);
    let text = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(ApprovalRecord {
        id: ApprovalId::from_bytes(r.get(0)?),
        kind: r.get(1)?,
        client_id: r.get(2)?,
        client_kind: parse_client_kind(&r.get::<_, String>(3)?),
        client_name: text("client_name"),
        collection_id: text("collection_id").and_then(|s| s.parse().ok()),
        root_path: PathBuf::from(text("root_path").unwrap_or_default()),
        state: r.get(5)?,
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        decided_at: r.get(8)?,
        folder_id: r.get::<_, Option<[u8; 16]>>(9)?.map(FolderId::from_bytes),
        error_code: r.get(10)?,
        payload,
    })
}

/// Marks every pending request whose time ran out as `expired`; returns their ids.
pub fn expire(conn: &Connection, now: i64) -> Result<Vec<ApprovalId>> {
    let mut st = conn
        .prepare_cached(
            "UPDATE approvals SET state = 'expired', decided_at = ?1
             WHERE state = 'pending' AND expires_at <= ?1 RETURNING id",
        )
        .db()?;
    st.query_map([now], |r| Ok(ApprovalId::from_bytes(r.get(0)?)))
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Records an `add_folder` request (or returns the pending one for the same client and folder).
/// Returns the record and whether it was newly created (emit `ApprovalRequested` only then).
pub fn request_add_folder(
    conn: &Connection,
    req: &AddFolderRequest,
    now: i64,
    ttl_ms: i64,
) -> Result<(ApprovalRecord, bool)> {
    atomic(conn, |c| {
        expire(c, now)?;
        let existing: Option<ApprovalRecord> = c
            .prepare_cached(&format!(
                "SELECT {COLS} FROM approvals WHERE kind = 'add_folder' AND client_id = ?1
                 AND target_key = ?2 AND state = 'pending'"
            ))
            .db()?
            .query_row(params![req.client_id, req.root_key], from_row)
            .optional()
            .db()?;
        if let Some(a) = existing {
            return Ok((a, false));
        }
        let id = ApprovalId::new();
        let payload = json!({
            "root_path": req.root_path.to_string_lossy(),
            "collection_id": req.collection_id.to_string(),
            "client_name": req.client_name,
        });
        c.prepare_cached(
            "INSERT INTO approvals(id, kind, client_id, client_kind, payload, state, created_at,
                 expires_at, target_key)
             VALUES (?1, 'add_folder', ?2, ?3, ?4, 'pending', ?5, ?6, ?7)",
        )
        .db()?
        .execute(params![
            id.to_bytes(),
            req.client_id,
            client_kind_str(req.client_kind),
            payload.to_string(),
            now,
            now.saturating_add(ttl_ms.max(1)),
            req.root_key
        ])
        .db()?;
        Ok((approval(c, id)?, true))
    })
}

pub fn approval(conn: &Connection, id: ApprovalId) -> Result<ApprovalRecord> {
    conn.prepare_cached(&format!("SELECT {COLS} FROM approvals WHERE id = ?1"))
        .db()?
        .query_row([id.to_bytes()], from_row)
        .db()
}

/// Requests still waiting for the user, oldest first. Expired ones are excluded (a read
/// connection can't mark them; the writer does in [`expire`]).
pub fn pending(conn: &Connection, now: i64) -> Result<Vec<ApprovalRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {COLS} FROM approvals WHERE state = 'pending' AND expires_at > ?1
             ORDER BY created_at, id"
        ))
        .db()?;
    st.query_map([now], from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Recent requests (any state) for the app's history view, newest first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<ApprovalRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {COLS} FROM approvals ORDER BY created_at DESC, id DESC LIMIT ?1"
        ))
        .db()?;
    st.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// The user's decision. Only a pending, unexpired request can be decided:
/// * expired → it is marked `expired` and the call fails with `ApprovalDenied`;
/// * already decided → `Conflict`.
pub fn decide(
    conn: &Connection,
    id: ApprovalId,
    approve: bool,
    now: i64,
) -> Result<ApprovalRecord> {
    // Expiry is committed on its own (an error inside the decision's savepoint would roll it back).
    expire(conn, now)?;
    atomic(conn, |c| {
        let a = approval(c, id)?;
        if a.state == ApprovalState::Expired {
            return Err(Error::new(ErrorCode::ApprovalDenied).with_detail("approval expired"));
        }
        if a.state != ApprovalState::Pending {
            return Err(Error::new(ErrorCode::Conflict)
                .with_detail(format!("approval already {}", a.state.as_str())));
        }
        let state = if approve {
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };
        c.prepare_cached(
            "UPDATE approvals SET state = ?2, decided_at = ?3 WHERE id = ?1 AND state = 'pending'",
        )
        .db()?
        .execute(params![id.to_bytes(), state, now])
        .db()?;
        approval(c, id)
    })
}

/// Stores what an approved request led to: the new folder, or the error that prevented it.
pub fn record_result(
    conn: &Connection,
    id: ApprovalId,
    folder_id: Option<FolderId>,
    error: Option<ErrorCode>,
) -> Result<()> {
    let n = conn
        .prepare_cached("UPDATE approvals SET folder_id = ?2, error_code = ?3 WHERE id = ?1")
        .db()?
        .execute(params![
            id.to_bytes(),
            folder_id.map(|f| f.to_bytes()),
            error.map(ErrorCode::as_str)
        ])
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::NotFound))
    } else {
        Ok(())
    }
}

/// Drops decided requests older than `keep_ms` (pending ones are kept until they expire).
pub fn prune(conn: &Connection, now: i64, keep_ms: i64) -> Result<usize> {
    conn.execute(
        "DELETE FROM approvals WHERE state <> 'pending' AND created_at < ?1",
        [now - keep_ms],
    )
    .db()
}
