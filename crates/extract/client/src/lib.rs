//! `sr-extract-client`: the daemon-side supervisor of the sandboxed `sr-extract` workers
//! (docs/02-architecture.md §2.1/§2.6, Amendment 3.11, docs/research/m1-b-extract.md).
//!
//! ```text
//! ExtractPool::extract(source, kind).await
//!   └─ bounded permits (workers + queue)  ── backpressure: callers wait here
//!       └─ slot thread i (one per worker) ── opens/streams the file, never the worker
//!           └─ sr-extract.exe in a Job Object (1 GiB, kill-on-close, 1 process, UI limits)
//!              token: AppContainer, zero capabilities (fallback: low-IL restricted token)
//!              watchdog: wall-clock timeout → TerminateJobObject → restart on next job
//! ```
//!
//! * The worker gets **bytes, never a path**: the supervisor (medium IL, inside the daemon's
//!   `SandboxFs` checks) reads the file and streams it over the pipe. Under AppContainer the
//!   worker has no file-system or network access at all.
//! * Every failure is an [`ExtractError`] holding one `sr_common::Error` (plain `UserMessage`) and
//!   a [`Disposition`]: `DeadLetter` (a property of the file: record it per file and skip it until
//!   it changes) or `Retry` (transient: file locked, worker could not start, shutting down).
//! * Workers are recycled after a job whose peak commit exceeds `recycle_peak_bytes` and every
//!   `recycle_after_jobs` jobs; a killed or crashed worker is replaced on the next job.

mod pool;
pub mod sandbox;
mod worker;

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sr_common::ErrorCode;
use sr_common::error::Error;
use sr_common::protocol::extract::{DocKind, MAX_PAYLOAD_BYTES, MAX_TEXT_BYTES, WorkerFailure};

pub use pool::ExtractPool;
pub use sandbox::{WorkerId, delete_app_container_profile, ensure_app_packages_read};

/// Default AppContainer profile name of the extractor (deleted on uninstall, Amendment 3.17).
pub const APP_CONTAINER_NAME: &str = "SeleneRetrieval.Extract";

/// Which token the workers run with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// AppContainer; fall back to low integrity (logged, shown in diagnostics and status) only
    /// when the AppContainer is classified unsupported on this PC (S-04): access denied / not
    /// supported, the loader refusing the worker, or pdfium not loading in it, twice in a row.
    /// Timeouts and other transient failures are retried with backoff and never fall back.
    Auto,
    /// AppContainer only (no fallback).
    AppContainer,
    /// Low-integrity restricted token only.
    LowIntegrity,
}

/// The token workers are actually using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSandbox {
    AppContainer,
    LowIntegrity,
}

/// Supervisor configuration. [`ExtractConfig::new`] gives the defaults.
#[derive(Debug, Clone)]
pub struct ExtractConfig {
    /// Absolute path of `sr-extract.exe`.
    pub worker_exe: PathBuf,
    /// Absolute path of `pdfium.dll` (normally next to the exe). `None`: PDFs fail with `Internal`.
    pub pdfium_dll: Option<PathBuf>,
    /// Concurrent workers (default 2: one warm worker parses > 1000 small PDFs/s; M0-C).
    pub workers: usize,
    /// Jobs that may wait for a worker beyond the ones running (default 2 × workers).
    pub queue: usize,
    pub sandbox: SandboxMode,
    pub app_container_name: String,
    /// At start, make sure "ALL APPLICATION PACKAGES" has inheritable read+execute on the
    /// directories holding the exe and pdfium (what Program Files grants by default; the per-user
    /// installer sets it too). Needed by AppContainer, and by low IL whenever any package SID
    /// appears in those DACLs. Idempotent; applied in every sandbox mode.
    pub ensure_install_acl: bool,
    /// Job Object commit cap (process and job), default 1 GiB.
    pub memory_limit: u64,
    /// Recycle the worker after a job when its peak commit exceeded this (default 256 MiB).
    pub recycle_peak_bytes: u64,
    /// Recycle the worker after this many jobs (default 5000).
    pub recycle_after_jobs: u32,
    /// Wall-clock timeout = base + per_mib × size in MiB, capped at max.
    pub timeout_base: Duration,
    pub timeout_per_mib: Duration,
    pub timeout_max: Duration,
    /// Deadline for a new worker to send its ready frame.
    pub spawn_timeout: Duration,
    /// Files above this are refused without contacting a worker (default 200 MiB).
    pub max_file_bytes: u64,
    /// Text cap per file (default 16 MiB).
    pub max_text_bytes: u64,
}

impl ExtractConfig {
    pub fn new(worker_exe: impl Into<PathBuf>, pdfium_dll: Option<PathBuf>) -> Self {
        Self {
            worker_exe: worker_exe.into(),
            pdfium_dll,
            workers: 2,
            queue: 4,
            sandbox: SandboxMode::Auto,
            app_container_name: APP_CONTAINER_NAME.into(),
            ensure_install_acl: true,
            memory_limit: 1 << 30,
            recycle_peak_bytes: 256 << 20,
            recycle_after_jobs: 5000,
            timeout_base: Duration::from_secs(15),
            timeout_per_mib: Duration::from_millis(500),
            timeout_max: Duration::from_secs(120),
            spawn_timeout: Duration::from_secs(15),
            max_file_bytes: MAX_PAYLOAD_BYTES,
            max_text_bytes: MAX_TEXT_BYTES,
        }
    }

    /// The wall-clock timeout for a file of `len` bytes.
    pub fn timeout_for(&self, len: u64) -> Duration {
        let mib = u32::try_from(len.div_ceil(1 << 20)).unwrap_or(u32::MAX);
        self.timeout_base
            .saturating_add(self.timeout_per_mib.saturating_mul(mib))
            .min(self.timeout_max)
    }
}

/// What to extract. The worker never sees a path.
#[derive(Debug)]
pub enum ExtractSource {
    /// Opened by the supervisor (tests, admin tools). Production callers open through `SandboxFs`
    /// and pass [`ExtractSource::File`].
    Path(PathBuf),
    /// An already-opened file (the handle `SandboxFs` returned). Read from its current position
    /// to its end.
    File(File),
    /// In-memory bytes.
    Bytes(Vec<u8>),
}

/// Kind for a path from its extension ([`DocKind::Auto`] when unknown).
pub fn kind_for_path(path: &Path) -> DocKind {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(DocKind::from_extension)
        .unwrap_or(DocKind::Auto)
}

/// A successful extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    /// UTF-8 text. Text kinds are verbatim (line numbers match the file); PDF pages are separated
    /// by form feeds (`\u{c}`); XLSX sheets start with `Sheet: <name>`, PPTX slides with
    /// `Slide <n>`.
    pub text: String,
    pub detected: DocKind,
    pub encoding: Option<String>,
    pub pages: Option<u32>,
    pub truncated: bool,
    /// Bytes streamed to the worker.
    pub bytes_in: u64,
    pub elapsed: Duration,
    pub sandbox: ActiveSandbox,
}

/// What the caller should do with a failed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The file itself is the problem: record it in the per-file dead letter (`FileFailed{code}`
    /// event, "View skipped files") and don't retry until it changes.
    DeadLetter,
    /// Transient: retry later (file locked, worker could not start, shutting down).
    Retry,
}

/// Internal cause, for diagnostics and stats (the user sees only `error`'s `UserMessage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureCause {
    /// The parser reported a typed failure.
    Worker(WorkerFailure),
    /// Wall-clock timeout; `memory_capped` if the worker also sat at its memory cap.
    Timeout { memory_capped: bool },
    /// Job CPU backstop.
    CpuLimit,
    /// Job memory cap killed the worker.
    MemoryLimit,
    /// The worker died (access violation, stack overflow ...).
    Crashed { exit_code: Option<u32> },
    /// The worker broke the protocol (treated as compromised and killed).
    ProtocolViolation,
    /// Larger than `max_file_bytes`; no worker was contacted.
    TooLarge,
    /// Opening or reading the source failed.
    Source,
    /// No worker could be started.
    SpawnFailed,
    /// Every permit is taken (`try_extract` only).
    Busy,
    /// The pool is shutting down.
    ShutDown,
}

/// A failed extraction: one error (rendered by `UserMessage`), what to do, and why.
pub struct ExtractError {
    pub error: Error,
    pub disposition: Disposition,
    pub cause: FailureCause,
}

impl ExtractError {
    pub(crate) fn new(error: Error, disposition: Disposition, cause: FailureCause) -> Self {
        Self {
            error,
            disposition,
            cause,
        }
    }

    pub fn code(&self) -> ErrorCode {
        self.error.code
    }

    /// Attaches the file path (used for the `{file}` name in the message; never logged).
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.error = self.error.with_path(path);
        self
    }
}

impl fmt::Debug for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtractError")
            .field("code", &self.error.code)
            .field("disposition", &self.disposition)
            .field("cause", &self.cause)
            .field("detail", &self.error.ctx.detail)
            .finish()
    }
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for ExtractError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Counters since the pool started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub jobs_ok: u64,
    pub jobs_failed: u64,
    pub timeouts: u64,
    pub cpu_limit_kills: u64,
    pub memory_kills: u64,
    pub crashes: u64,
    pub protocol_violations: u64,
    pub spawns: u64,
    pub spawn_failures: u64,
    pub recycles: u64,
    pub bytes_in: u64,
    pub text_bytes_out: u64,
    pub sandbox: Option<ActiveSandbox>,
    /// Why Auto fell back to low integrity (diagnostics).
    pub fallback_reason: Option<String>,
    /// Why pdfium failed to load in the most recent worker, if it did (diagnostics).
    pub pdf_engine_error: Option<String>,
}

/// "10 MB"-style size for user text.
pub(crate) fn format_bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        format!("{} MB", n / MB)
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_scales_with_size_and_is_capped() {
        let c = ExtractConfig::new("x", None);
        assert_eq!(c.timeout_for(0), Duration::from_secs(15));
        assert_eq!(c.timeout_for(1), Duration::from_millis(15_500));
        assert_eq!(c.timeout_for(10 << 20), Duration::from_secs(20));
        assert_eq!(c.timeout_for(u64::MAX), Duration::from_secs(120));
    }

    #[test]
    fn kind_from_path() {
        assert_eq!(kind_for_path(Path::new("a/b.PDF")), DocKind::Pdf);
        assert_eq!(kind_for_path(Path::new("noext")), DocKind::Auto);
        assert_eq!(kind_for_path(Path::new("x.weird")), DocKind::Auto);
    }

    #[test]
    fn sizes_render_plainly() {
        assert_eq!(format_bytes(200 << 20), "200 MB");
        assert_eq!(format_bytes(1500), "2 KB");
    }
}
