//! Sandbox for Unix systems other than macOS: none exists, so it fails closed. Every call that
//! would start or manage a worker returns an error; nothing here ever spawns `sr-extract`, and
//! the worker never runs unconfined (the worker binary itself also refuses to run there). The
//! pool checks [`super::platform_supported`] first and refuses to start, so callers see one plain
//! error instead of a stream of spawn failures.
//!
//! The types mirror the Windows module so the pool and worker code compile unchanged.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Duration;

use sr_common::ErrorCode;
use sr_common::error::Error;

/// Why no worker starts on this platform (shown under Details).
pub const UNAVAILABLE: &str = "document parsing is not available on this system";

/// Exit code used when the supervisor kills a worker's job (same value as on Windows).
pub const EXIT_KILLED: u32 = 0xE7E7;

// Job message codes (same values as on Windows; never posted here).
pub const MSG_END_OF_JOB_TIME: u32 = 1;
pub const MSG_END_OF_PROCESS_TIME: u32 = 2;
pub const MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
pub const MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
pub const MSG_JOB_MEMORY_LIMIT: u32 = 10;

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, UNAVAILABLE)
}

/// The pool's start error on this platform.
pub(crate) fn unavailable_error() -> Error {
    Error::new(ErrorCode::Internal).with_detail(UNAVAILABLE)
}

/// Placeholder for a process handle. Never created on this platform.
#[derive(Debug)]
pub struct OwnedHandle {
    _private: (),
}

// ------------------------------------------------------------------ job

/// Worker limits. [`Job::new`] always fails here, so no job (and no worker) ever exists.
#[derive(Debug)]
pub struct Job {
    _private: (),
}

/// Kills the worker's job. Never created on this platform.
#[derive(Debug, Clone)]
pub struct JobKiller {
    _private: (),
}

impl JobKiller {
    pub fn kill(&self) {}
}

impl Job {
    pub fn new(_memory_limit: u64, _cpu_budget: Duration) -> io::Result<Job> {
        Err(unsupported())
    }

    pub fn killer(&self) -> JobKiller {
        JobKiller { _private: () }
    }

    pub fn rearm_cpu(&mut self, _budget: Duration) {}

    pub fn terminate(&self) {}

    pub fn peak_process_memory(&self) -> u64 {
        0
    }

    pub fn user_time(&self) -> Duration {
        Duration::ZERO
    }

    pub fn drain_messages(&self) -> Vec<u32> {
        Vec::new()
    }
}

// ------------------------------------------------------------------ AppContainer

/// Windows-only concept; [`AppContainer::ensure`] always fails here.
#[derive(Debug)]
pub struct AppContainer {
    pub name: String,
}

impl AppContainer {
    pub fn ensure(_name: &str, _display: &str) -> io::Result<AppContainer> {
        Err(unsupported())
    }

    pub fn sid_string(&self) -> io::Result<String> {
        Err(unsupported())
    }
}

/// Nothing to delete on this platform.
pub fn delete_app_container_profile(_name: &str) -> io::Result<()> {
    Ok(())
}

/// No install-directory ACL to set on this platform. Returns "unchanged".
pub fn ensure_app_packages_read(_dir: &Path) -> io::Result<bool> {
    Ok(false)
}

// ------------------------------------------------------------------ process

/// Token used for the worker (Windows names, kept for the shared pool code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    AppContainer,
    LowIntegrity,
    Seatbelt,
}

/// A running worker. Never created on this platform.
#[derive(Debug)]
pub struct ChildProcess {
    pub process: OwnedHandle,
    pub pid: u32,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

impl ChildProcess {
    pub fn wait_exit(&self, _timeout: Duration) -> Option<u32> {
        None
    }
}

/// Always fails: there is no sandbox here, and the worker never runs without one.
pub fn spawn(
    _exe: &Path,
    _args: &[&OsStr],
    _kind: TokenKind,
    _app_container: Option<&AppContainer>,
    _job: &Job,
) -> io::Result<ChildProcess> {
    Err(unsupported())
}

/// A worker process identity (pid plus creation time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId {
    pub pid: u32,
    pub created: u64,
}

impl WorkerId {
    pub fn of(_h: &OwnedHandle, pid: u32) -> WorkerId {
        WorkerId { pid, created: 0 }
    }

    /// No worker ever runs on this platform.
    pub fn is_alive(&self) -> bool {
        false
    }
}

/// Last OS error code (diagnostics).
pub fn last_error_code() -> u32 {
    io::Error::last_os_error()
        .raw_os_error()
        .map_or(0, |c| c as u32)
}
