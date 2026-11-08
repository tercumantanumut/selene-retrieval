//! Domain types stored in the catalog. Text enums map 1:1 to the `CHECK (... IN (...))` lists in
//! the schema; ids are converted at the SQL boundary only.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use sr_common::ids::{
    ChunkId, CollectionId, ContentHash, FileId, FolderId, TextHash, VectorIndexId,
};

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$variant),)+ _ => None }
            }
        }
        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }
        impl FromSql for $name {
            fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
                let s = v.as_str()?;
                $name::parse(s).ok_or_else(|| FromSqlError::Other(
                    format!(concat!("unknown ", stringify!($name), " {:?}"), s).into()))
            }
        }
    };
}

text_enum! {
    /// `files.state`. `dead` = dead-letter ("couldn't be read"), excluded from status derivation.
    FileState { PendingIndex => "pending_index", Indexed => "indexed", Skipped => "skipped", Dead => "dead" }
}
text_enum! {
    /// `jobs.kind`.
    JobKind {
        ScanFolder => "scan_folder", IndexFile => "index_file", DeleteFile => "delete_file",
        RebuildProfile => "rebuild_profile", PurgeFolder => "purge_folder", Retention => "retention",
    }
}
text_enum! {
    /// `jobs.state`. Retryable failures go back to `queued` with a backoff; `failed` is a terminal
    /// non-file failure (folder shows `error`); `dead` is the dead-letter state.
    JobState { Queued => "queued", Leased => "leased", Done => "done", Failed => "failed", Dead => "dead" }
}
text_enum! {
    /// `index_ops.op` (the outbox).
    IndexOpKind { Upsert => "upsert", Delete => "delete", PurgeFolder => "purge_folder" }
}
text_enum! {
    /// `folders.kind`.
    FolderKind { Indexed => "indexed", AccessGrant => "access_grant" }
}
text_enum! {
    /// `store_watermarks.store`.
    Store { Lance => "lance", Tantivy => "tantivy" }
}
text_enum! {
    /// `vector_indexes.state`.
    IndexState { Building => "building", Active => "active", Retired => "retired", Failed => "failed" }
}
text_enum! {
    /// `folders.last_run_trigger`.
    RunTrigger {
        Startup => "startup", Watcher => "watcher", Schedule => "schedule", Manual => "manual",
        Mcp => "mcp", SettingsChange => "settings_change", Reconnect => "reconnect",
    }
}
text_enum! {
    /// `embedding_profiles.precision`.
    ProfilePrecision { Fp32 => "fp32", Fp16 => "fp16", Int8 => "int8", Remote => "remote" }
}
text_enum! {
    /// `embedding_profiles.tier`.
    ProfileTier { Minimum => "minimum", Standard => "standard", High => "high", Custom => "custom" }
}

text_enum! {
    /// `folders.watch_support` (V3): can the live watcher serve this folder?
    /// `network` = network share or mapped network drive, `unsupported` = the watcher could not be
    /// started on its file system. Both are reconcile-only (scheduled checks instead of events).
    WatchSupport { Native => "native", Network => "network", Unsupported => "unsupported" }
}
text_enum! {
    /// `approvals.kind`.
    ApprovalKind { AddFolder => "add_folder" }
}
text_enum! {
    /// `approvals.state`.
    ApprovalState { Pending => "pending", Approved => "approved", Denied => "denied", Expired => "expired" }
}

/// `files.skip_reason` of files that are **listed**, not read and not skipped, so status counts and
/// the "skipped" list leave them out (`status::FolderCounts::listed_files`, `files::skipped_files`).
/// Written for access-grant folders. Rows written by the removed Files-only mode keep it until the
/// user chooses what to do with the folder (Amendment 17; the stored value stays `files_only`).
pub const SKIP_REASON_FILES_ONLY: &str = "files_only";

/// A 64-bit volume serial (ReFS serials use all 64 bits) as stored in an `INTEGER` column: the
/// same bit pattern reinterpreted as `i64`, so every value round-trips (rusqlite refuses `u64`
/// above `i64::MAX`).
pub const fn vs_to_sql(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

/// Inverse of [`vs_to_sql`].
pub const fn vs_from_sql(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

/// Job priority: 0 interactive … 3 rebuild.
pub type Priority = u8;
pub const PRIORITY_INTERACTIVE: Priority = 0;
pub const PRIORITY_LIVE: Priority = 1;
pub const PRIORITY_BACKGROUND: Priority = 2;
pub const PRIORITY_REBUILD: Priority = 3;

/// Daemon epoch (`meta.daemon_epoch`), incremented at every start; the lease owner of jobs.
pub type Epoch = i64;
/// Outbox sequence number.
pub type Seq = i64;
/// Mark-and-sweep scan id.
pub type ScanId = i64;
/// `embedding_profiles.id`.
pub type ProfileId = i64;
/// `jobs.id`.
pub type JobId = i64;

// ------------------------------------------------------------------------------------- id codec

/// Row helpers: read typed ids from a row column.
pub(crate) trait RowExt {
    fn file_id(&self, idx: usize) -> rusqlite::Result<FileId>;
    fn folder_id(&self, idx: usize) -> rusqlite::Result<FolderId>;
    fn collection_id(&self, idx: usize) -> rusqlite::Result<CollectionId>;
    fn index_id(&self, idx: usize) -> rusqlite::Result<VectorIndexId>;
    fn opt_file_id(&self, idx: usize) -> rusqlite::Result<Option<FileId>>;
    fn opt_folder_id(&self, idx: usize) -> rusqlite::Result<Option<FolderId>>;
    fn opt_index_id(&self, idx: usize) -> rusqlite::Result<Option<VectorIndexId>>;
    fn chunk_id(&self, idx: usize) -> rusqlite::Result<ChunkId>;
    fn text_hash(&self, idx: usize) -> rusqlite::Result<TextHash>;
    fn opt_content_hash(&self, idx: usize) -> rusqlite::Result<Option<ContentHash>>;
}

impl RowExt for rusqlite::Row<'_> {
    fn file_id(&self, idx: usize) -> rusqlite::Result<FileId> {
        Ok(FileId::from_bytes(self.get(idx)?))
    }
    fn folder_id(&self, idx: usize) -> rusqlite::Result<FolderId> {
        Ok(FolderId::from_bytes(self.get(idx)?))
    }
    fn collection_id(&self, idx: usize) -> rusqlite::Result<CollectionId> {
        Ok(CollectionId::from_bytes(self.get(idx)?))
    }
    fn index_id(&self, idx: usize) -> rusqlite::Result<VectorIndexId> {
        Ok(VectorIndexId::from_bytes(self.get(idx)?))
    }
    fn opt_file_id(&self, idx: usize) -> rusqlite::Result<Option<FileId>> {
        Ok(self
            .get::<_, Option<[u8; 16]>>(idx)?
            .map(FileId::from_bytes))
    }
    fn opt_folder_id(&self, idx: usize) -> rusqlite::Result<Option<FolderId>> {
        Ok(self
            .get::<_, Option<[u8; 16]>>(idx)?
            .map(FolderId::from_bytes))
    }
    fn opt_index_id(&self, idx: usize) -> rusqlite::Result<Option<VectorIndexId>> {
        Ok(self
            .get::<_, Option<[u8; 16]>>(idx)?
            .map(VectorIndexId::from_bytes))
    }
    fn chunk_id(&self, idx: usize) -> rusqlite::Result<ChunkId> {
        Ok(ChunkId::from_bytes(self.get(idx)?))
    }
    fn text_hash(&self, idx: usize) -> rusqlite::Result<TextHash> {
        Ok(TextHash::from_bytes(self.get(idx)?))
    }
    fn opt_content_hash(&self, idx: usize) -> rusqlite::Result<Option<ContentHash>> {
        Ok(self
            .get::<_, Option<[u8; 32]>>(idx)?
            .map(ContentHash::from_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enums_round_trip() {
        for k in JobKind::ALL {
            assert_eq!(JobKind::parse(k.as_str()), Some(*k));
            assert_eq!(
                serde_json::to_value(k).unwrap_or_default(),
                serde_json::Value::from(k.as_str())
            );
        }
        for s in FileState::ALL {
            assert_eq!(FileState::parse(s.as_str()), Some(*s));
        }
        assert_eq!(JobState::parse("nope"), None);
    }

    #[test]
    fn volume_serials_round_trip_all_64_bits() {
        for v in [
            0,
            1,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::MAX,
            1 << 63,
        ] {
            assert_eq!(vs_from_sql(vs_to_sql(v)), v);
        }
        assert_eq!(
            vs_to_sql(305_419_896),
            305_419_896,
            "32-bit values are unchanged"
        );
    }
}
