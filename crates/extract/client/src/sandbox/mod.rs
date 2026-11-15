//! Sandbox primitives for the extract worker, one implementation per platform with the same API.
//!
//! * Windows ([`windows`]): Job Object, AppContainer / low-IL token, locked-down process creation.
//! * macOS ([`macos`]): Seatbelt deny-default profile (applied by the worker, verified here from
//!   outside), resource limits, footprint and CPU watchdog, process-group kill.
//! * Other platforms ([`unix`]): no sandbox, so no worker ever starts (fail closed). The pool
//!   refuses to start ([`platform_supported`]) and every primitive returns an error as well.

use sr_common::error::Error;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(all(not(windows), not(target_os = "macos")))]
mod unix;
#[cfg(all(not(windows), not(target_os = "macos")))]
pub use unix::*;

/// Whether this platform can run sandboxed workers. `Err` holds the plain-language reason.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn platform_supported() -> Result<(), Error> {
    Ok(())
}

/// Whether this platform can run sandboxed workers. `Err` holds the plain-language reason.
#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn platform_supported() -> Result<(), Error> {
    Err(unix::unavailable_error())
}

/// Windows: the token and job are set by the parent at creation, so there is nothing to check
/// after the ready frame.
#[cfg(windows)]
pub(crate) fn verify_worker(
    _child: &ChildProcess,
    _ready: &sr_common::protocol::extract::WorkerReady,
    _exe: &std::path::Path,
) -> std::io::Result<()> {
    Ok(())
}

/// Windows: the pool recycles by jobs and peak memory only.
#[cfg(windows)]
pub(crate) fn needs_recycle(_job: &Job) -> bool {
    false
}

/// macOS: recycle a worker that has used half of its lifetime CPU ceiling.
#[cfg(target_os = "macos")]
pub(crate) fn needs_recycle(job: &Job) -> bool {
    job.needs_recycle()
}

/// No sandbox here: never verified.
#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn verify_worker(
    _child: &ChildProcess,
    _ready: &sr_common::protocol::extract::WorkerReady,
    _exe: &std::path::Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        unix::UNAVAILABLE,
    ))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn needs_recycle(_job: &Job) -> bool {
    false
}
