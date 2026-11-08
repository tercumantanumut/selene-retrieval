//! Folder status derivation (§2.4 integrity rules, review M5).
//!
//! `synced` only when no queued, leased or failed jobs remain and nothing is pending;
//! `synced_with_errors` when the only problems are dead (dead-letter) files.
//!
//! M2 (V3): listed files (access grants; folders paused by Amendment 17) are `listed_files`, never
//! counted as skipped; a run the user cancelled shows `partial` while files it never reached are
//! still pending.

use rusqlite::{Connection, params};
use sr_common::error::Result;
use sr_common::events::FolderStatus;
use sr_common::ids::FolderId;

use crate::db::DbResultExt;
use crate::model::SKIP_REASON_FILES_ONLY;
use crate::repo::folders::{self, status_str};

/// Inputs of the derivation (all cheap indexed counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderCounts {
    pub available: bool,
    pub paused: bool,
    /// The user cancelled the last run and no full scan finished since (V3).
    pub cancelled: bool,
    pub ever_scanned: bool,
    pub last_scan_partial: bool,
    /// A `scan_folder` job is leased (running).
    pub scanning: bool,
    /// Queued or leased jobs of any kind.
    pub live_jobs: u64,
    /// Terminal `failed` jobs (folder-level failures).
    pub failed_jobs: u64,
    pub pending_files: u64,
    pub indexed_files: u64,
    /// Intentionally not indexed (too large, online-only, ...). Excludes listed files.
    pub skipped_files: u64,
    /// Listed only, not read ([`SKIP_REASON_FILES_ONLY`]). Not "skipped".
    pub listed_files: u64,
    pub dead_files: u64,
    /// M5-E: skipped because they are online-only (cloud placeholders); part of `skipped_files`.
    pub online_only_files: u64,
}

impl FolderCounts {
    pub fn total_files(&self) -> u64 {
        self.pending_files
            + self.indexed_files
            + self.skipped_files
            + self.listed_files
            + self.dead_files
    }
}

/// Pure derivation, in precedence order.
pub fn derive(c: &FolderCounts) -> FolderStatus {
    if !c.available {
        FolderStatus::Unavailable
    } else if c.paused {
        FolderStatus::Paused
    } else if c.failed_jobs > 0 {
        FolderStatus::Error
    } else if c.scanning {
        FolderStatus::Scanning
    } else if c.live_jobs > 0 || (c.pending_files > 0 && !c.cancelled) {
        if c.ever_scanned {
            FolderStatus::Indexing
        } else {
            FolderStatus::Pending
        }
    } else if !c.ever_scanned {
        FolderStatus::Pending
    } else if c.last_scan_partial || (c.cancelled && c.pending_files > 0) {
        FolderStatus::Partial
    } else if c.dead_files > 0 {
        FolderStatus::SyncedWithErrors
    } else {
        FolderStatus::Synced
    }
}

pub fn counts(conn: &Connection, folder_id: FolderId) -> Result<FolderCounts> {
    let f = folders::folder(conn, folder_id)?;
    let key = folder_id.to_bytes();
    let (scanning, live, failed): (bool, u64, u64) = conn
        .prepare_cached(
            "SELECT coalesce(max(kind = 'scan_folder' AND state = 'leased'), 0),
                    coalesce(sum(state IN ('queued','leased')), 0),
                    coalesce(sum(state = 'failed'), 0)
             FROM jobs WHERE folder_id = ?1",
        )
        .db()?
        .query_row([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .db()?;
    let (pending, indexed, skipped, listed, dead, online_only): (u64, u64, u64, u64, u64, u64) =
        conn.prepare_cached(
            "SELECT coalesce(sum(state = 'pending_index'), 0), coalesce(sum(state = 'indexed'), 0),
                    coalesce(sum(state = 'skipped' AND skip_reason IS NOT ?2), 0),
                    coalesce(sum(state = 'skipped' AND skip_reason IS ?2), 0),
                    coalesce(sum(state = 'dead'), 0),
                    coalesce(sum(state = 'skipped' AND skip_reason IS ?3), 0)
             FROM files WHERE folder_id = ?1",
        )
        .db()?
        .query_row(
            params![key, SKIP_REASON_FILES_ONLY, SKIP_REASON_ONLINE_ONLY],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .db()?;
    Ok(FolderCounts {
        available: f.available,
        paused: f.paused,
        cancelled: f.cancelled_at.is_some(),
        ever_scanned: f.last_full_scan_at.is_some(),
        last_scan_partial: f.last_scan_partial,
        scanning,
        live_jobs: live,
        failed_jobs: failed,
        pending_files: pending,
        indexed_files: indexed,
        skipped_files: skipped,
        listed_files: listed,
        dead_files: dead,
        online_only_files: online_only,
    })
}

/// `files.skip_reason` of an online-only (cloud placeholder) file; sr-sync writes it.
pub const SKIP_REASON_ONLINE_ONLY: &str = "online_only";

/// Derives, caches in `folders.status` and returns `(status, changed)`; emit
/// `FolderStatusChanged` when `changed`.
pub fn refresh(conn: &Connection, folder_id: FolderId) -> Result<(FolderStatus, bool)> {
    let s = derive(&counts(conn, folder_id)?);
    let n = conn
        .prepare_cached("UPDATE folders SET status = ?2 WHERE id = ?1 AND status <> ?2")
        .db()?
        .execute(params![folder_id.to_bytes(), status_str(s)])
        .db()?;
    Ok((s, n == 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced() -> FolderCounts {
        FolderCounts {
            available: true,
            ever_scanned: true,
            indexed_files: 10,
            ..FolderCounts::default()
        }
    }

    #[test]
    fn derivation_precedence() {
        let s = synced();
        assert_eq!(derive(&s), FolderStatus::Synced);
        let c = |f: fn(&mut FolderCounts)| {
            let mut x = s;
            f(&mut x);
            derive(&x)
        };
        assert_eq!(c(|x| x.dead_files = 2), FolderStatus::SyncedWithErrors);
        assert_eq!(c(|x| x.last_scan_partial = true), FolderStatus::Partial);
        assert_eq!(
            c(|x| {
                x.live_jobs = 1;
                x.dead_files = 1;
            }),
            FolderStatus::Indexing
        );
        assert_eq!(c(|x| x.pending_files = 1), FolderStatus::Indexing);
        assert_eq!(
            c(|x| {
                x.scanning = true;
                x.live_jobs = 1;
            }),
            FolderStatus::Scanning
        );
        assert_eq!(c(|x| x.failed_jobs = 1), FolderStatus::Error);
        assert_eq!(
            c(|x| {
                x.paused = true;
                x.failed_jobs = 1;
            }),
            FolderStatus::Paused
        );
        assert_eq!(
            c(|x| {
                x.available = false;
                x.paused = true;
            }),
            FolderStatus::Unavailable
        );
        let fresh = FolderCounts {
            available: true,
            live_jobs: 1,
            ..FolderCounts::default()
        };
        assert_eq!(derive(&fresh), FolderStatus::Pending);
        let empty = FolderCounts {
            available: true,
            ..FolderCounts::default()
        };
        assert_eq!(derive(&empty), FolderStatus::Pending);
        assert_eq!(s.total_files(), 10);
        // A cancelled run with files it never reached is partial, not "indexing" forever; once
        // nothing is pending it is synced again, and live work still shows as indexing.
        assert_eq!(
            c(|x| {
                x.cancelled = true;
                x.pending_files = 3;
            }),
            FolderStatus::Partial
        );
        assert_eq!(c(|x| x.cancelled = true), FolderStatus::Synced);
        assert_eq!(
            c(|x| {
                x.cancelled = true;
                x.pending_files = 3;
                x.live_jobs = 1;
            }),
            FolderStatus::Indexing
        );
        // Listed files are neither problems nor pending work.
        let listed = FolderCounts {
            available: true,
            ever_scanned: true,
            listed_files: 40,
            ..FolderCounts::default()
        };
        assert_eq!(derive(&listed), FolderStatus::Synced);
        assert_eq!(listed.total_files(), 40);
    }
}
