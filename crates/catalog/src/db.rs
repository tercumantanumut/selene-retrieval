//! Connections: **one writer thread** plus a **read-only pool** (review M3).
//!
//! * The writer owns the only read-write connection. Work is sent as closures and runs one at a
//!   time, so writes are serialised without `SQLITE_BUSY` between our own connections. A panic
//!   inside a closure is caught: the open transaction rolls back (rusqlite `Drop`), the caller
//!   gets `Internal`, and the writer keeps serving.
//! * Readers are `SQLITE_OPEN_READ_ONLY` + `query_only`, so a read closure can never write. WAL
//!   gives each read a consistent snapshot while the writer holds a long transaction.
//! * Pragmas (M3): WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`,
//!   `wal_autocheckpoint=1000`, `secure_delete=FAST`, `auto_vacuum=INCREMENTAL`; `PRAGMA optimize`
//!   at open (0x10002), on [`crate::Catalog::optimize`] (engine timer, every 6 h) and at close.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use sr_common::error::{Error, ErrorCode, Result};
use tokio::sync::oneshot;

/// Default number of read-only connections.
pub const DEFAULT_READERS: usize = 3;

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Maps a rusqlite error to the one error model. The SQL text never reaches the user (Details
/// only, through the source chain).
pub fn map_db_err(e: rusqlite::Error) -> Error {
    use rusqlite::ErrorCode as C;
    let code = match e.sqlite_error_code() {
        Some(C::DatabaseBusy | C::DatabaseLocked) => ErrorCode::Busy,
        Some(C::DiskFull) => ErrorCode::DiskFull,
        Some(C::DatabaseCorrupt | C::NotADatabase) => ErrorCode::Corrupt,
        Some(C::PermissionDenied | C::ReadOnly) => ErrorCode::PermissionDenied,
        Some(C::ConstraintViolation) => ErrorCode::Conflict,
        _ if matches!(e, rusqlite::Error::QueryReturnedNoRows) => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    Error::new(code).with_source(e)
}

/// `.db()?` converts rusqlite results (the orphan rule forbids a `From` impl here).
pub trait DbResultExt<T> {
    fn db(self) -> Result<T>;
}

impl<T> DbResultExt<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(map_db_err)
    }
}

/// Runs `f` atomically inside a SAVEPOINT. Works both outside a transaction (it then acts as
/// `BEGIN DEFERRED`) and nested inside a caller's transaction, so repository functions can be
/// composed into one larger transaction.
pub fn atomic<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT sr_atomic").db()?;
    match catch_unwind(AssertUnwindSafe(|| f(conn))) {
        Ok(Ok(v)) => {
            conn.execute_batch("RELEASE sr_atomic").db()?;
            Ok(v)
        }
        Ok(Err(e)) => {
            let _ = conn.execute_batch("ROLLBACK TO sr_atomic; RELEASE sr_atomic");
            Err(e)
        }
        Err(panic) => {
            let _ = conn.execute_batch("ROLLBACK TO sr_atomic; RELEASE sr_atomic");
            std::panic::resume_unwind(panic)
        }
    }
}

fn set_common_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(())
}

/// Opens (creating if needed) the read-write connection and applies writer pragmas.
pub fn open_writer(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .db()?;
    // auto_vacuum only takes effect before the first table exists (fresh catalogs).
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .db()?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .db()?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::new(ErrorCode::Internal)
            .with_detail(format!("catalog journal_mode is {mode}, WAL required")));
    }
    conn.pragma_update(None, "synchronous", "NORMAL").db()?;
    conn.pragma_update(None, "wal_autocheckpoint", 1000).db()?;
    conn.pragma_update(None, "secure_delete", "FAST").db()?;
    conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)
        .db()?;
    set_common_pragmas(&conn).db()?;
    Ok(conn)
}

/// Opens one read-only pool connection.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .db()?;
    set_common_pragmas(&conn).db()?;
    conn.pragma_update(None, "query_only", "ON").db()?;
    Ok(conn)
}

/// The single writer thread.
pub struct Writer {
    tx: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer").finish_non_exhaustive()
    }
}

fn writer_gone() -> Error {
    Error::new(ErrorCode::Internal).with_detail("catalog writer stopped")
}

impl Writer {
    pub fn spawn(mut conn: Connection) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("sr-catalog-writer".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // A panicking job must not take the writer down; its reply channel is dropped
                    // and the caller sees `Internal`.
                    let _ = catch_unwind(AssertUnwindSafe(|| job(&mut conn)));
                    if !conn.is_autocommit() {
                        tracing::error!("catalog writer job left a transaction open; rolling back");
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                }
                let _ = conn.execute_batch("PRAGMA optimize");
            })
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    fn submit(&self, job: Job) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(writer_gone)?
            .send(job)
            .map_err(|_| writer_gone())
    }

    /// Runs `f` on the writer and waits. Uses a std channel, so it is safe from any thread; from
    /// async code prefer [`Writer::call`], which does not tie up a runtime worker.
    pub fn call_blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply, rx) = mpsc::sync_channel(1);
        self.submit(Box::new(move |conn| {
            let _ = reply.send(f(conn));
        }))?;
        rx.recv().map_err(|_| writer_gone())?
    }

    /// Runs `f` on the writer without blocking the async runtime.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        self.submit(Box::new(move |conn| {
            let _ = reply.send(f(conn));
        }))?;
        rx.await.map_err(|_| writer_gone())?
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Fixed-size pool of read-only connections.
pub struct ReadPool {
    conns: Mutex<Vec<Connection>>,
    ready: Condvar,
    size: usize,
}

impl std::fmt::Debug for ReadPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadPool")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

struct Lease<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let mut g = self
                .pool
                .conns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.push(c);
            self.pool.ready.notify_one();
        }
    }
}

impl ReadPool {
    pub fn open(path: &Path, size: usize) -> Result<Self> {
        let size = size.clamp(1, 8);
        let conns = (0..size)
            .map(|_| open_reader(path))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            conns: Mutex::new(conns),
            ready: Condvar::new(),
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Runs `f` on a free read connection, waiting for one if all are busy.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = {
            let mut g = self
                .conns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(c) = g.pop() {
                    break c;
                }
                g = self
                    .ready
                    .wait(g)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let lease = Lease {
            pool: self,
            conn: Some(conn),
        };
        let c = lease.conn.as_ref().ok_or_else(writer_gone)?;
        // Each read is its own snapshot.
        let tx = c.unchecked_transaction().db()?;
        let out = f(&tx);
        drop(tx);
        out
    }
}
