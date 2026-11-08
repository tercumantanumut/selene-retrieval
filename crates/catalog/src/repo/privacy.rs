//! Approvals, clients, egress consent and audit. **Stubs** with the §2.4 shape: the flows
//! (headless `add_folder` approval M2, consent prompts M3, per-client scopes M4) build on these.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sr_common::error::{ClientKind, Error, ErrorCode, Purpose, Result};
use sr_common::ids::{ApprovalId, CollectionId};

use crate::db::DbResultExt;

fn enum_str<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Approval requests expire after 10 minutes and are never auto-approved (§0.A Q5).
pub const APPROVAL_TTL_MS: i64 = 10 * 60 * 1000;

pub fn create_approval(
    conn: &Connection,
    client_id: &str,
    payload: &Value,
    now: i64,
) -> Result<ApprovalId> {
    let id = ApprovalId::new();
    conn.execute(
        "INSERT INTO approvals(id, kind, client_id, payload, created_at, expires_at)
         VALUES (?1, 'add_folder', ?2, ?3, ?4, ?5)",
        params![
            id.to_bytes(),
            client_id,
            payload.to_string(),
            now,
            now + APPROVAL_TTL_MS
        ],
    )
    .db()?;
    Ok(id)
}

/// `pending` | `approved` | `denied` | `expired` (expiry is applied first).
pub fn approval_state(conn: &Connection, id: ApprovalId, now: i64) -> Result<String> {
    conn.execute(
        "UPDATE approvals SET state = 'expired' WHERE state = 'pending' AND expires_at <= ?1",
        [now],
    )
    .db()?;
    conn.query_row(
        "SELECT state FROM approvals WHERE id = ?1",
        [id.to_bytes()],
        |r| r.get(0),
    )
    .db()
}

/// Approve or deny a still-pending, unexpired request (`ApprovalDenied` otherwise).
pub fn decide_approval(conn: &Connection, id: ApprovalId, approve: bool, now: i64) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE approvals SET state = ?2 WHERE id = ?1 AND state = 'pending' AND expires_at > ?3",
            params![
                id.to_bytes(),
                if approve { "approved" } else { "denied" },
                now
            ],
        )
        .db()?;
    if n == 0 {
        Err(Error::new(ErrorCode::ApprovalDenied))
    } else {
        Ok(())
    }
}

pub fn upsert_client(
    conn: &Connection,
    id: &str,
    kind: ClientKind,
    display_name: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO clients(id, kind, display_name, connected_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(id) DO UPDATE SET last_seen_at = excluded.last_seen_at,
             display_name = excluded.display_name",
        params![id, enum_str(kind), display_name, now],
    )
    .db()?;
    Ok(())
}

pub fn grant_consent(conn: &Connection, provider: &str, purpose: Purpose, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO egress_consent(provider, purpose, granted_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(provider, purpose) DO NOTHING",
        params![provider, enum_str(purpose), now],
    )
    .db()?;
    Ok(())
}

pub fn revoke_consent(conn: &Connection, provider: &str, purpose: Purpose) -> Result<()> {
    conn.execute(
        "DELETE FROM egress_consent WHERE provider = ?1 AND purpose = ?2",
        params![provider, enum_str(purpose)],
    )
    .db()?;
    Ok(())
}

/// No row means `local_only` for that (provider, purpose): the PrivacyGate refuses.
pub fn has_consent(conn: &Connection, provider: &str, purpose: Purpose) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM egress_consent WHERE provider = ?1 AND purpose = ?2",
            params![provider, enum_str(purpose)],
            |_| Ok(()),
        )
        .optional()
        .db()?
        .is_some())
}

/// Every remote call is audited, including query embeddings and rerank.
pub fn audit_egress(
    conn: &Connection,
    provider: &str,
    purpose: Purpose,
    bytes: u64,
    collection_id: Option<CollectionId>,
    client_id: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO egress_audit(ts, provider, purpose, bytes, collection_id, client_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            now,
            provider,
            enum_str(purpose),
            bytes,
            collection_id.map(|c| c.to_bytes()),
            client_id
        ],
    )
    .db()?;
    Ok(())
}
