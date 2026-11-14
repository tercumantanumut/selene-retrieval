//! Extract client for ingest (owner: M1-B). The supervisor itself lives in `sr-extract-client`
//! (its own small crate: Win32/`unsafe` stays out of sr-core, and sr-core never links pdfium);
//! this module is the ingest-facing entry point.
//!
//! Usage (sr-engine wiring):
//! ```ignore
//! let pool = ExtractPool::start(config_for_install_dir(&install_dir))?;   // once, at startup
//! // per file, after SandboxFs opened it (the worker never gets a path):
//! match pool.extract(ExtractSource::File(handle), kind_for_path(&rel_path)).await {
//!     Ok(x) => chunk(x.text, x.detected),                 // text kinds are verbatim
//!     Err(e) if e.disposition == Disposition::DeadLetter => record_failure(file, e.error), // FileFailed{code}
//!     Err(e) => retry_later(file, e.error),               // Disposition::Retry
//! }
//! ```
//! `extract` awaits a permit first, so the claim loop gets backpressure for free.

use std::path::Path;

pub use sr_common::protocol::extract::DocKind;
pub use sr_extract_client::{
    APP_CONTAINER_NAME, ActiveSandbox, Disposition, ExtractConfig, ExtractError, ExtractPool,
    ExtractSource, Extraction, FailureCause, PoolStats, SandboxMode, delete_app_container_profile,
    kind_for_path,
};

/// File names in the install directory (next to `sr-daemon.exe`).
#[cfg(windows)]
pub const WORKER_EXE: &str = "sr-extract.exe";
#[cfg(windows)]
pub const PDFIUM_DLL: &str = "pdfium.dll";

/// File names in the install directory (next to the daemon binary).
#[cfg(not(windows))]
pub const WORKER_EXE: &str = "sr-extract";
#[cfg(target_os = "macos")]
pub const PDFIUM_DLL: &str = "libpdfium.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
pub const PDFIUM_DLL: &str = "libpdfium.so";

/// Default supervisor configuration for an install directory holding `sr-extract.exe` and
/// `pdfium.dll` side by side (the NSIS layout). In a macOS app bundle the library sits in
/// `Contents/Frameworks` ([`sr_common::install::libraries_dir`]).
pub fn config_for_install_dir(install_dir: &Path) -> ExtractConfig {
    ExtractConfig::new(
        install_dir.join(WORKER_EXE),
        Some(sr_common::install::libraries_dir(install_dir).join(PDFIUM_DLL)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn install_layout_paths() {
        let c = config_for_install_dir(Path::new(r"C:\Apps\SR"));
        assert_eq!(c.worker_exe, Path::new(r"C:\Apps\SR\sr-extract.exe"));
        assert_eq!(
            c.pdfium_dll.as_deref(),
            Some(Path::new(r"C:\Apps\SR\pdfium.dll"))
        );
        assert_eq!(c.sandbox, SandboxMode::Auto);
        assert_eq!(kind_for_path(Path::new("a.docx")), DocKind::Docx);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_layout_paths() {
        let c = config_for_install_dir(Path::new("/Applications/SR.app/Contents/MacOS"));
        assert_eq!(
            c.worker_exe,
            Path::new("/Applications/SR.app/Contents/MacOS/sr-extract")
        );
        assert_eq!(
            c.pdfium_dll.as_deref(),
            Some(Path::new(
                "/Applications/SR.app/Contents/Frameworks/libpdfium.dylib"
            ))
        );
        assert_eq!(c.sandbox, SandboxMode::Auto);
        // Dev layout (target/debug): side by side.
        let d = config_for_install_dir(Path::new("/src/target/debug"));
        assert_eq!(
            d.pdfium_dll.as_deref(),
            Some(Path::new("/src/target/debug/libpdfium.dylib"))
        );
    }
}
