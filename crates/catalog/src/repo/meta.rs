//! `meta`: daemon epoch, install id, hardware detection (detected facts only; user choices such as
//! the tier override live in `settings.toml`, Amendment 4.10).

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::TraceId;
use sr_common::tier::{HwInfo, TierDecision};

use crate::db::{DbResultExt, atomic};
use crate::model::Epoch;
use crate::repo::jobs;

pub const KEY_DAEMON_EPOCH: &str = "daemon_epoch";
pub const KEY_INSTALL_ID: &str = "install_id";
pub const KEY_HW_DETECTION: &str = "hw_detection";

pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")
        .db()?
        .query_row([key], |r| r.get(0))
        .optional()
        .db()
}

pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .db()?
    .execute(params![key, value])
    .db()?;
    Ok(())
}

pub fn delete(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])
        .db()?;
    Ok(())
}

/// Current epoch (0 before the first start).
pub fn epoch(conn: &Connection) -> Result<Epoch> {
    Ok(get(conn, KEY_DAEMON_EPOCH)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// Result of [`start_epoch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochStart {
    pub epoch: Epoch,
    /// Leases from older epochs returned to `queued` (work interrupted by a crash or kill).
    pub requeued: usize,
}

/// Daemon start: bump `meta.daemon_epoch` and fence every lease held by an older epoch
/// (`leased` → `queued` where `lease_owner <> epoch`), in one transaction.
pub fn start_epoch(conn: &Connection, now: i64) -> Result<EpochStart> {
    atomic(conn, |c| {
        let epoch = epoch(c)?
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorCode::Internal).with_detail("daemon epoch overflow"))?;
        set(c, KEY_DAEMON_EPOCH, &epoch.to_string())?;
        let requeued = jobs::requeue_stale_leases(c, epoch, now)?;
        Ok(EpochStart { epoch, requeued })
    })
}

/// Stable per-install id (created on first use).
pub fn install_id(conn: &Connection) -> Result<String> {
    if let Some(v) = get(conn, KEY_INSTALL_ID)? {
        return Ok(v);
    }
    let id = TraceId::new().to_string();
    set(conn, KEY_INSTALL_ID, &id)?;
    Ok(id)
}

/// Last hardware detection (`meta.hw_detection`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HwDetection {
    pub hw: HwInfo,
    pub decision: TierDecision,
    pub detected_at: i64,
}

pub fn set_hw_detection(conn: &Connection, d: &HwDetection) -> Result<()> {
    let v = serde_json::to_string(d).map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    set(conn, KEY_HW_DETECTION, &v)
}

/// `None` if never detected or unreadable (re-detected at the next start anyway).
pub fn hw_detection(conn: &Connection) -> Result<Option<HwDetection>> {
    Ok(get(conn, KEY_HW_DETECTION)?.and_then(|v| serde_json::from_str(&v).ok()))
}

// No `tier_override` here (removed in CO-1): it is a user choice and lives only in
// `settings.toml` `[search]` (`sr_common::config::Search::tier_override`, Amendment 4.10).
