//! Panic and error boundary for the native stores (Amendment 3.12).
//!
//! LanceDB panicked in M0 on some path forms (`//?/C:/…`, lance-io `object_store.rs:544`), and the
//! tantivy / tokenizers stacks have `expect`s on malformed input. Every call into them goes through
//! [`lance`] (async) or [`blocking`] (sync), which turn both errors and panics into an
//! [`sr_common::Error`] so one bad call never takes the daemon down.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};

use futures::FutureExt;
use sr_common::error::{Error, ErrorCode};

/// Render a panic payload for `Details` (never shown in the basic UI).
pub fn panic_message(p: &(dyn Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Map a store error to the one error model. Disk-full is the only condition with its own
/// user-facing code ("Not enough disk space" + FreeDiskSpace); everything else is `Internal`.
pub fn store_error(what: &str, e: impl std::fmt::Display) -> Error {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    // Windows ERROR_DISK_FULL (112) / ERROR_HANDLE_DISK_FULL (39), POSIX ENOSPC.
    let code = if lower.contains("os error 112")
        || lower.contains("os error 39")
        || lower.contains("no space left")
        || lower.contains("not enough space on the disk")
    {
        ErrorCode::DiskFull
    } else {
        ErrorCode::Internal
    };
    Error::new(code).with_detail(format!("{what}: {text}"))
}

/// Run one LanceDB future under `catch_unwind`, mapping errors and panics.
pub async fn lance<T, F>(what: &str, fut: F) -> sr_common::Result<T>
where
    F: Future<Output = lancedb::Result<T>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(store_error(what, e)),
        Err(p) => Err(panicked(what, p.as_ref())),
    }
}

/// Run an async block that already returns `sr_common::Result` under `catch_unwind`.
pub async fn unwind<T, F>(what: &str, fut: F) -> sr_common::Result<T>
where
    F: Future<Output = sr_common::Result<T>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(p) => Err(panicked(what, p.as_ref())),
    }
}

/// Run a synchronous closure under `catch_unwind`.
pub fn blocking<T>(what: &str, f: impl FnOnce() -> sr_common::Result<T>) -> sr_common::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(panicked(what, p.as_ref())),
    }
}

fn panicked(what: &str, p: &(dyn Any + Send)) -> Error {
    let msg = panic_message(p);
    tracing::error!(target: "sr_core::guard", what, panic = %msg, "native store panicked");
    Error::new(ErrorCode::Internal).with_detail(format!("{what}: panic: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_turns_panic_into_internal_error() {
        let r: sr_common::Result<()> = blocking("unit", || panic!("boom"));
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(e.ctx.detail.as_deref().unwrap_or("").contains("boom"));
    }

    #[tokio::test]
    async fn async_panic_is_caught() {
        let r: sr_common::Result<u8> = unwind("unit", async { panic!("kaboom") }).await;
        assert_eq!(r.unwrap_err().code, ErrorCode::Internal);
    }

    #[test]
    fn disk_full_maps_to_disk_full() {
        let e = store_error(
            "write",
            "There is not enough space on the disk. (os error 112)",
        );
        assert_eq!(e.code, ErrorCode::DiskFull);
        assert_eq!(store_error("w", "other").code, ErrorCode::Internal);
    }
}
