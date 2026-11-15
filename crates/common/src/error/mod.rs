//! The one error model (docs/02-architecture.md §2.5, Product Principle P1.5).
//!
//! * [`Error`] is what every crate returns: a stable [`ErrorCode`], context used to fill the
//!   user-facing text, retry information, an optional source chain and a trace id.
//! * [`UserMessage`] is the **only** renderer of user-facing text. It always carries a plain
//!   title, a plain body, exactly one [`SuggestedAction`] and a [`Details`] block (code, chain,
//!   trace id) that the UI shows only under "Details".
//! * Paths are never printed by `Display`/`Debug` of [`Error`]; only the file name is used.

mod messages;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use messages::{Details, SuggestedAction, UserMessage};

use crate::ids::{ApprovalId, FolderId, TraceId};

/// Boxed source error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Convenience alias used across the workspace.
pub type Result<T, E = Error> = std::result::Result<T, E>;

macro_rules! error_codes {
    ($( $(#[$m:meta])* $name:ident => $retry:ident ),+ $(,)?) => {
        /// Stable machine-readable error codes. The order matches the table in
        /// docs/02-architecture.md §2.5 and must never be reordered (codes are serialised by name).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        pub enum ErrorCode { $( $(#[$m])* $name ),+ }

        impl ErrorCode {
            /// Every code, in table order.
            pub const ALL: &'static [ErrorCode] = &[ $( ErrorCode::$name ),+ ];

            /// Stable name (also the serde representation).
            pub const fn as_str(self) -> &'static str {
                match self { $( ErrorCode::$name => stringify!($name) ),+ }
            }

            /// Retry column of the table.
            pub const fn retry_policy(self) -> RetryPolicy {
                match self { $( ErrorCode::$name => RetryPolicy::$retry ),+ }
            }
        }
    };
}

error_codes! {
    InvalidInput => No,
    NotFound => No,
    PathOutsideSandbox => No,
    PathRefused => No,
    PathDenied => No,
    FolderOverlap => No,
    FolderUnavailable => Auto,
    PermissionDenied => Yes,
    FileTooLarge => No,
    FileLocked => Yes,
    FileOnlineOnly => No,
    FileUnreadable => No,
    ExtractTimeout => No,
    ApprovalRequired => NotApplicable,
    ApprovalDenied => No,
    Conflict => Yes,
    Busy => Yes,
    Cancelled => No,
    Timeout => Yes,
    ProviderNotConfigured => No,
    ProviderAuth => No,
    ProviderQuota => No,
    RateLimited => Yes,
    ProviderUnavailable => Yes,
    ProviderRequest => No,
    PrivacyBlocked => No,
    Offline => Yes,
    NetworkBlocked => No,
    ModelDownloadFailed => Yes,
    ModelIntegrity => Yes,
    DiskFull => Auto,
    Corrupt => Auto,
    ServiceUnavailable => Yes,
    UpgradeRequired => No,
    SettingsInvalid => No,
    Internal => No,
    /// #37 (Amendment 1): a new search model failed its checks; the current index is kept.
    ModelIncompatible => No,
    /// #38 (M5-E, R34): the search index was moved to a drive or folder that is missing now.
    /// The service waits in a recovery mode (nothing is deleted or forked) and starts by itself
    /// when the drive comes back; the app offers "Move it back" / "Start fresh".
    IndexMissing => Auto,
    /// #39 (M5-E): an administrator's policy (`policy.toml` / registry) doesn't allow this.
    ManagedByPolicy => No,
    /// #40 (M5-E, S-01 / R38): the pipe server is not this user's own service (another account
    /// squatted the pipe name). Nothing was sent; the client refuses the connection.
    ConnectionNotTrusted => No,
    /// #41 (M5-E, S-03 / R35): the remembered HTTP port is held by another program. HTTP for
    /// local tools stays off (never moves to another port, so no token reaches the squatter);
    /// the service retries the same port by itself.
    PortInUse => Auto,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The "Retry" column of the error table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RetryPolicy {
    /// Retrying the same request will not help.
    No,
    /// The caller may retry (possibly after `retry_after`).
    Yes,
    /// The engine retries by itself when the condition clears (drive back, space freed, repair).
    Auto,
    /// Not an error in the retry sense (e.g. waiting for approval).
    NotApplicable,
}

impl RetryPolicy {
    /// Whether an API caller (MCP agent, app) should be told the operation is retryable.
    pub const fn is_retryable(self) -> bool {
        matches!(self, RetryPolicy::Yes | RetryPolicy::Auto)
    }
}

/// Settings sections, used by `OpenSettings` and config metadata (`x-section`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    General,
    Search,
    Answers,
    Providers,
    Folder,
    ConnectedApps,
    Indexing,
    Ranking,
    Integrations,
    System,
}

/// Egress purposes that need consent (docs/00 §0.A "audit query embeddings and rerank").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    EmbedDoc,
    EmbedQuery,
    Rerank,
    Answer,
    Enhance,
    Iterate,
    ValidateKey,
    ListModels,
}

impl Purpose {
    /// Every purpose, in declaration order (M3-P: consent UI rows, audit summaries).
    pub const ALL: &'static [Purpose] = &[
        Purpose::EmbedDoc,
        Purpose::EmbedQuery,
        Purpose::Rerank,
        Purpose::Answer,
        Purpose::Enhance,
        Purpose::Iterate,
        Purpose::ValidateKey,
        Purpose::ListModels,
    ];

    /// Stable snake_case name: the serde form and the `egress_consent.purpose` /
    /// `egress_audit.purpose` value (M3-P).
    pub const fn as_str(self) -> &'static str {
        match self {
            Purpose::EmbedDoc => "embed_doc",
            Purpose::EmbedQuery => "embed_query",
            Purpose::Rerank => "rerank",
            Purpose::Answer => "answer",
            Purpose::Enhance => "enhance",
            Purpose::Iterate => "iterate",
            Purpose::ValidateKey => "validate_key",
            Purpose::ListModels => "list_models",
        }
    }

    /// Plain-language description used in consent and `PrivacyBlocked` text.
    pub const fn plain(self) -> &'static str {
        match self {
            Purpose::EmbedDoc => "the text of your files",
            Purpose::EmbedQuery => "your search text",
            Purpose::Rerank => "search results",
            Purpose::Answer | Purpose::Iterate => "questions and matching file text",
            Purpose::Enhance => "your prompt",
            Purpose::ValidateKey => "your key for checking",
            Purpose::ListModels => "a model list request",
        }
    }
}

/// MCP client kinds the app knows how to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    ClaudeDesktop,
    Cursor,
    VsCode,
    Other,
    Http,
}

impl ClientKind {
    /// Display name used in user-facing text.
    pub const fn display_name(self) -> &'static str {
        match self {
            ClientKind::ClaudeDesktop => "Claude",
            ClientKind::Cursor => "Cursor",
            ClientKind::VsCode => "VS Code",
            ClientKind::Other | ClientKind::Http => "the connected app",
        }
    }
}

/// Context that fills in the user-facing text. Every field is optional; the renderer falls back
/// to neutral wording when something is missing, so a message is always complete.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrCtx {
    /// AI provider display name ("OpenAI").
    pub provider: Option<String>,
    /// Full path. Only used where the user picked it (ApprovalRequired, PathRefused); never logged.
    pub path: Option<PathBuf>,
    /// Display name of a folder.
    pub name: Option<String>,
    /// Display name of the other folder/collection (FolderOverlap).
    pub other: Option<String>,
    pub folder_id: Option<FolderId>,
    pub approval_id: Option<ApprovalId>,
    /// Pre-formatted size limit ("10 MB").
    pub limit: Option<String>,
    pub client: Option<ClientKind>,
    /// Client display name when it is not one of the known kinds.
    pub client_name: Option<String>,
    pub host: Option<String>,
    pub model: Option<String>,
    /// Pre-formatted amount of space needed ("1.2 GB").
    pub need: Option<String>,
    pub drive: Option<String>,
    pub purpose: Option<Purpose>,
    pub section: Option<Section>,
    pub secs: Option<u32>,
    /// Internal detail (never part of title/body; shown only under Details).
    pub detail: Option<String>,
}

impl ErrCtx {
    /// File name of `path` (never the full path). On Unix a `\` also ends the folder part, so a
    /// Windows-style path that reaches a macOS build (from a client, a setting) is never shown
    /// whole; a name that really contains `\` is shortened, never widened.
    pub fn file_name(&self) -> Option<String> {
        let name = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())?;
        #[cfg(not(windows))]
        let name = name.rsplit('\\').find(|s| !s.is_empty())?.to_string();
        Some(name)
    }
}

/// The workspace error type.
pub struct Error {
    pub code: ErrorCode,
    /// Boxed so `Result<T, Error>` stays small (clippy::result_large_err).
    pub ctx: Box<ErrCtx>,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    source: Option<BoxError>,
    pub trace_id: TraceId,
}

impl Error {
    /// New error with the code's default retry policy and a fresh trace id.
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            ctx: Box::default(),
            retryable: code.retry_policy().is_retryable(),
            retry_after: None,
            source: None,
            trace_id: TraceId::new(),
        }
    }

    /// Placeholder returned by M0 API outlines until the owning milestone implements them.
    /// Renders as the generic `Internal` message; `what` goes to Details only.
    pub fn not_implemented(what: &str) -> Self {
        Error::new(ErrorCode::Internal).with_detail(format!("{what} is not implemented yet"))
    }

    pub fn with_ctx(mut self, ctx: ErrCtx) -> Self {
        self.ctx = Box::new(ctx);
        self
    }
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.ctx.provider = Some(provider.into());
        self
    }
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.ctx.path = Some(path.into());
        self
    }
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.ctx.name = Some(name.into());
        self
    }
    pub fn with_section(mut self, section: Section) -> Self {
        self.ctx.section = Some(section);
        self
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.ctx.detail = Some(detail.into());
        self
    }
    /// Sets both the `{secs}` text and `retry_after`.
    pub fn with_retry_after(mut self, secs: u32) -> Self {
        self.ctx.secs = Some(secs);
        self.retry_after = Some(Duration::from_secs(u64::from(secs)));
        self
    }
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The user-facing rendering (the only renderer).
    pub fn user_message(&self) -> UserMessage {
        UserMessage::from(self)
    }

    /// Source chain as strings (for Details). Paths inside foreign errors are not scrubbed here;
    /// Details is only shown on explicit user request and redacted again by "Copy diagnostics".
    pub fn chain(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Option<&(dyn std::error::Error + 'static)> =
            self.source.as_deref().map(|e| e as _);
        while let Some(e) = cur {
            out.push(e.to_string());
            cur = e.source();
        }
        out
    }

    /// MCP `structuredContent` for an `isError: true` tool result.
    pub fn mcp_structured(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code,
            "retryable": self.retryable,
            "retry_after": self.retry_after.map(|d| d.as_secs()),
        })
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("code", &self.code)
            .field("file", &self.ctx.file_name())
            .field("detail", &self.ctx.detail)
            .field("retryable", &self.retryable)
            .field("trace_id", &self.trace_id)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = self.user_message();
        write!(f, "[{}] {}", self.code, msg.title)?;
        if let Some(d) = &self.ctx.detail {
            write!(f, " ({d})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as _)
    }
}

impl From<ErrorCode> for Error {
    fn from(code: ErrorCode) -> Self {
        Error::new(code)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let code = match e.kind() {
            K::NotFound => ErrorCode::NotFound,
            K::PermissionDenied => ErrorCode::PermissionDenied,
            K::TimedOut => ErrorCode::Timeout,
            K::StorageFull | K::QuotaExceeded => ErrorCode::DiskFull,
            K::InvalidInput | K::InvalidFilename => ErrorCode::InvalidInput,
            // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33): AV or another program.
            _ if matches!(e.raw_os_error(), Some(32 | 33)) => ErrorCode::FileLocked,
            _ => ErrorCode::Internal,
        };
        Error::new(code).with_source(e)
    }
}

#[cfg(test)]
mod tests;
