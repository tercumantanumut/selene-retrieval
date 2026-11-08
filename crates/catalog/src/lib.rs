//! sr-catalog: the SQLite source of truth (docs/02-architecture.md §2.4; architect D2–D4).
//!
//! # Shape
//! * [`Catalog`] owns **one writer thread** and a **read-only pool** (2–4 connections, default 3).
//!   - [`Catalog::write`] (async) / [`Catalog::write_blocking`] run a closure on the writer.
//!   - [`Catalog::read`] (blocking) / [`Catalog::read_async`] run a closure on a pooled
//!     read-only snapshot (WAL: never blocked by a writer transaction).
//! * Repository functions live in [`repo`] and take a `&rusqlite::Connection`, so callers compose
//!   several of them into one transaction inside a single `write` closure.
//! * Migrations: [`schema`] (`rusqlite_migration`, `PRAGMA user_version`), run at open.
//!
//! # Write path (D3)
//! `repo::chunks::write_file_chunks` (T1: version++, chunks, chunk_text, outbox op, pending) →
//! stores apply `repo::outbox::ops_after` and persist watermarks (`repo::outbox::set_watermark`)
//! → `repo::chunks::finalize_file` + `repo::outbox::prune` (T2).
//!
//! # Jobs (D4)
//! `repo::meta::start_epoch` at daemon start (fences older leases) → `repo::jobs::enqueue`
//! (dedupe/coalesce) → `repo::jobs::claim` (priority, aging, DRR, permits) →
//! `repo::jobs::complete` / `repo::jobs::fail` (fenced by epoch; backoff; dead-letter).
//! Paused folders' jobs are held by the claim; `repo::jobs::release` re-queues without an attempt.
//!
//! # Approvals (M2)
//! `repo::approvals`: pending `add_folder` requests from MCP clients (approve / deny / expire).

pub mod db;
pub mod model;
pub mod repo;
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use sr_common::error::{Error, ErrorCode, Result};

pub use db::{DbResultExt, atomic, map_db_err};
pub use model::{
    ApprovalKind, ApprovalState, Epoch, FileState, FolderKind, IndexOpKind, IndexState, JobId,
    JobKind, JobState, Priority, ProfileId, RunTrigger, SKIP_REASON_FILES_ONLY, ScanId, Seq, Store,
    WatchSupport,
};
pub use repo::jobs::{ClaimRequest, Drr, Job, NewJob};
pub use repo::outbox::{CoalescedOp, OpBatch};

/// Milliseconds since the Unix epoch, as stored in the catalog.
pub fn now_ms() -> i64 {
    i64::try_from(sr_common::events::now_ms()).unwrap_or(i64::MAX)
}

/// Open options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogOptions {
    /// Read-only connections (clamped to 2..=4).
    pub readers: usize,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        Self {
            readers: db::DEFAULT_READERS,
        }
    }
}

/// Handle to the catalog database. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct Catalog {
    path: PathBuf,
    writer: db::Writer,
    readers: Arc<db::ReadPool>,
}

fn migrate(conn: &mut Connection) -> Result<()> {
    schema::migrations().to_latest(conn).map_err(|e| {
        let code = match &e {
            rusqlite_migration::Error::RusqliteError { err, .. } => map_db_err(clone_err(err)).code,
            _ => ErrorCode::Internal,
        };
        Error::new(code).with_source(e)
    })
}

fn clone_err(e: &rusqlite::Error) -> rusqlite::Error {
    match e.sqlite_error() {
        Some(f) => rusqlite::Error::SqliteFailure(*f, None),
        None => rusqlite::Error::InvalidQuery,
    }
}

impl Catalog {
    /// Opens (creating and migrating if needed) the catalog at `path` with default options.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, CatalogOptions::default())
    }

    pub fn open_with(path: &Path, opts: CatalogOptions) -> Result<Self> {
        let mut conn = db::open_writer(path)?;
        migrate(&mut conn)?;
        // Long-lived connection: let SQLite gather stats lazily (recommended mask).
        conn.execute_batch("PRAGMA optimize=0x10002").db()?;
        let writer = db::Writer::spawn(conn)?;
        let readers = Arc::new(db::ReadPool::open(path, opts.readers.clamp(2, 4))?);
        Ok(Self {
            path: path.to_path_buf(),
            writer,
            readers,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Schema version (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64> {
        self.read(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)).db())
    }

    /// Runs `f` on the writer connection (async callers).
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.writer.call(f).await
    }

    /// Runs `f` on the writer connection and waits (any thread; async code should prefer `write`).
    pub fn write_blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.writer.call_blocking(f)
    }

    /// Runs `f` on a read-only snapshot (blocking; waits for a free pool connection).
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.readers.read(f)
    }

    /// Runs `f` on a read-only snapshot from async code (on tokio's blocking pool).
    pub async fn read_async<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let pool = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || pool.read(f))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?
    }

    /// Number of read-only connections.
    pub fn readers(&self) -> usize {
        self.readers.size()
    }

    /// `PRAGMA optimize` (engine timer every 6 h; also runs at close).
    pub fn optimize(&self) -> Result<()> {
        self.write_blocking(|c| c.execute_batch("PRAGMA optimize").db())
    }

    /// Daemon start: bump the epoch and fence older leases (see [`repo::meta::start_epoch`]).
    pub fn start_epoch(&self) -> Result<repo::meta::EpochStart> {
        self.write_blocking(|c| repo::meta::start_epoch(c, now_ms()))
    }
}
