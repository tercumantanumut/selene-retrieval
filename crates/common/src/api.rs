//! App-RPC data shapes added in M4 (`docs/research/m4-rpc-contract.md`): folder groups, connected
//! apps and scopes, the opt-in HTTP transport, folder detail, activity, the index move and request
//! progress. The desktop app depends on `sr-common` (not on the engine), so every type it reads or
//! sends lives here. Plain serde data only: no I/O.
//!
//! Times are Unix milliseconds (`i64`), sizes bytes (`u64`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::FolderSettings;
use crate::error::{ClientKind, ErrorCode, Purpose};
use crate::events::FolderStatus;
use crate::ids::{FileId, FolderId, GroupId};

/// Every app-RPC method name M4 adds (the M1–M3 names stay in `sr_engine::rpc`).
pub mod methods {
    /// Served by the pipe server itself: drain and exit; the app re-attaches (page 03).
    pub const HOST_RESTART: &str = "host.restart";

    pub const GROUPS_LIST: &str = "groups.list";
    pub const GROUPS_CREATE: &str = "groups.create";
    pub const GROUPS_RENAME: &str = "groups.rename";
    pub const GROUPS_DELETE: &str = "groups.delete";
    pub const GROUPS_MOVE_FOLDER: &str = "groups.move_folder";

    pub const CLIENTS_LIST: &str = "clients.list";
    pub const CLIENTS_SET_SCOPE: &str = "clients.set_scope";
    pub const CLIENTS_FORGET: &str = "clients.forget";
    pub const CLIENTS_SNIPPET: &str = "clients.snippet";

    pub const HTTP_STATUS: &str = "http.status";
    pub const HTTP_SET_ENABLED: &str = "http.set_enabled";
    pub const HTTP_CREATE_TOKEN: &str = "http.create_token";
    pub const HTTP_ROTATE_TOKEN: &str = "http.rotate_token";
    pub const HTTP_REVOKE: &str = "http.revoke";

    pub const FOLDERS_DETAIL: &str = "folders.detail";
    pub const FOLDERS_SKIPPED: &str = "folders.skipped";
    pub const FOLDERS_RETRY_FILES: &str = "folders.retry_files";
    pub const FOLDERS_UPDATE_SETTINGS: &str = "folders.update_settings";

    pub const REQUESTS_CANCEL: &str = "requests.cancel";

    pub const ACTIVITY_LIST: &str = "activity.list";
    pub const ACTIVITY_EGRESS: &str = "activity.egress";
    pub const ACTIVITY_NOTE: &str = "activity.note";

    pub const INDEX_LOCATION_STATUS: &str = "index_location.status";
    pub const INDEX_LOCATION_CHECK: &str = "index_location.check";
    pub const INDEX_LOCATION_MOVE: &str = "index_location.move";
    pub const INDEX_LOCATION_CANCEL: &str = "index_location.cancel";
    /// M5-E (R34): the moved index is missing at start; `{choice: "move_back" | "start_fresh"}`.
    /// Answered by the service's recovery mode (the engine itself is not running then).
    pub const INDEX_LOCATION_RECOVER: &str = "index_location.recover";
    /// M5-E: what an administrator's `policy.toml` / registry policy locks (read-only).
    pub const POLICY_STATUS: &str = "policy.status";

    /// Existing methods that became privileged in M4 (R31).
    pub const CONSENT_SET: &str = "consent.set";
    pub const APPROVALS_APPROVE: &str = "approvals.approve";
    pub const PROVIDERS_SAVE: &str = "providers.save";

    /// Existing methods that became privileged in M5 (S-05, R31 update). The engine-side
    /// constants live in `sr_engine::rpc` / `sr_common::protocol::rpc`; the strings are equal.
    pub const FOLDERS_ADD: &str = "folders.add";
    pub const SETTINGS_UPDATE_ADVANCED: &str = "settings.update_advanced";
    pub const SEARCH_MODEL_CHOOSE: &str = "search_model.choose";
    pub const SEARCH_MODEL_USE_RECOMMENDED: &str = "search_model.use_recommended";
    pub const RERANKER_DOWNLOAD: &str = "reranker.download";
    /// Served by the pipe server: drain and exit (updater, installer).
    pub const HOST_SHUTDOWN: &str = "host.shutdown";

    /// M5-C (Amendments 10.2 / 10.3): account sign-in through the local Claude Code / Codex CLI.
    /// `cli_signin.*` take `{ provider }` (`claudecode` | `codex`); `claude_signin.*` are the
    /// same calls for `claudecode` (the names M5-A's UI first bound to). All privileged.
    pub const CLI_SIGNIN_START: &str = "cli_signin.start";
    pub const CLI_SIGNIN_STATUS: &str = "cli_signin.status";
    pub const CLAUDE_SIGNIN_START: &str = "claude_signin.start";
    pub const CLAUDE_SIGNIN_STATUS: &str = "claude_signin.status";

    /// Every M4 engine method (not `host.restart`, which the pipe server serves).
    pub const ALL: &[&str] = &[
        GROUPS_LIST,
        GROUPS_CREATE,
        GROUPS_RENAME,
        GROUPS_DELETE,
        GROUPS_MOVE_FOLDER,
        CLIENTS_LIST,
        CLIENTS_SET_SCOPE,
        CLIENTS_FORGET,
        CLIENTS_SNIPPET,
        HTTP_STATUS,
        HTTP_SET_ENABLED,
        HTTP_CREATE_TOKEN,
        HTTP_ROTATE_TOKEN,
        HTTP_REVOKE,
        FOLDERS_DETAIL,
        FOLDERS_SKIPPED,
        FOLDERS_RETRY_FILES,
        FOLDERS_UPDATE_SETTINGS,
        REQUESTS_CANCEL,
        ACTIVITY_LIST,
        ACTIVITY_EGRESS,
        ACTIVITY_NOTE,
        INDEX_LOCATION_STATUS,
        INDEX_LOCATION_CHECK,
        INDEX_LOCATION_MOVE,
        INDEX_LOCATION_CANCEL,
        INDEX_LOCATION_RECOVER,
        POLICY_STATUS,
        CLI_SIGNIN_START,
        CLI_SIGNIN_STATUS,
        CLAUDE_SIGNIN_START,
        CLAUDE_SIGNIN_STATUS,
    ];
}

/// R31: methods only our own app (or the admin CLI) may call over the pipe. Each one widens access
/// or sends data out: granting consent, approving a folder, storing a provider key, widening an
/// app's scope, opening the HTTP port, minting a token, moving the index.
///
/// S-05 (M5): also every method that reaches the same effect another way: adding a folder
/// (skips the approval flow), moving a folder into a group a tool is scoped to (a scope change),
/// the advanced switches (MCP `add_folder`, reranker = consent to a download), model downloads,
/// and stopping or restarting the service (`host.*`, checked by the pipe server itself).
/// Methods that only narrow access (`http.revoke`, `clients.forget`, `approvals.deny`) stay open.
pub const PRIVILEGED: &[&str] = &[
    methods::CONSENT_SET,
    methods::APPROVALS_APPROVE,
    methods::PROVIDERS_SAVE,
    methods::CLIENTS_SET_SCOPE,
    methods::HTTP_SET_ENABLED,
    methods::HTTP_CREATE_TOKEN,
    methods::HTTP_ROTATE_TOKEN,
    methods::INDEX_LOCATION_MOVE,
    // M5-E (R34): changes where the index lives or starts an empty one.
    methods::INDEX_LOCATION_RECOVER,
    // M5-E S-05.
    methods::FOLDERS_ADD,
    methods::GROUPS_MOVE_FOLDER,
    methods::SETTINGS_UPDATE_ADVANCED,
    methods::SEARCH_MODEL_CHOOSE,
    methods::SEARCH_MODEL_USE_RECOMMENDED,
    methods::RERANKER_DOWNLOAD,
    methods::HOST_SHUTDOWN,
    methods::HOST_RESTART,
    // Amendments 10.2 / 10.3: account sign-in (account email; starts the CLI's own sign-in).
    methods::CLI_SIGNIN_START,
    methods::CLI_SIGNIN_STATUS,
    methods::CLAUDE_SIGNIN_START,
    methods::CLAUDE_SIGNIN_STATUS,
];

/// Whether `method` needs a trusted app session (R31).
pub fn is_privileged(method: &str) -> bool {
    PRIVILEGED.contains(&method)
}

// ------------------------------------------------------------------------------------------------
// Folders

/// M5-E (U19): the folder note when files were skipped because they are online-only in OneDrive.
pub const ONLINE_ONLY_NOTE: &str = "Some files are online-only in OneDrive; make them \"Always keep on this device\" to search them.";

/// serde helper: skip a zero count.
pub fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The online-only skip count shown next to the note (0 when online-only files are included).
pub fn online_only_skipped(skipped: u64, include_online_only: bool) -> u64 {
    if include_online_only { 0 } else { skipped }
}

/// The online-only note for a folder with `skipped` cloud-only files (none when the folder
/// includes online-only files on purpose, or nothing was skipped).
pub fn online_only_note(skipped: u64, include_online_only: bool) -> Option<String> {
    (skipped > 0 && !include_online_only).then(|| ONLINE_ONLY_NOTE.to_string())
}

/// One folder as shown to clients (`folders.list`, MCP `list_folders`, `folders.add` reply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FolderInfo {
    pub folder_id: FolderId,
    pub name: String,
    pub path: String,
    pub status: FolderStatus,
    pub available: bool,
    pub files_indexed: u64,
    pub files_total: u64,
    /// Files that couldn't be read ("View skipped files").
    pub files_skipped: u64,
    pub last_success_at: Option<i64>,
    /// Plain note when the live watcher can't serve the folder (OQ-38).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_note: Option<String>,
    /// M5-E: plain note when online-only OneDrive files were skipped ([`ONLINE_ONLY_NOTE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_note: Option<String>,
    /// M5-E: how many files were skipped because they are online-only in OneDrive (0 when the
    /// folder includes online-only files on purpose).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub online_only_skipped: u64,
    /// Removed; listed until its data is purged ("Removing…", OQ-18).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removing: bool,
    /// Folder group (M4; the default group "My folders" unless moved).
    #[serde(default)]
    pub group_id: GroupId,
    /// The user stopped the last run and no full check finished since (OQ-37 "stopped").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// The folder was set to only list its files, a mode that was removed (Amendment 17). It
    /// stays paused, so nothing in it is read, until the user chooses: search its contents
    /// (resume) or remove it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub was_files_only: bool,
}

/// `folders.detail` reply (page 25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderDetail {
    pub folder: FolderInfo,
    pub settings: FolderSettings,
    pub group_id: GroupId,
    pub group_name: String,
    pub paused: bool,
    pub last_run: LastRun,
    pub counts: FileCounts,
    /// The first [`SKIPPED_PAGE_DEFAULT`] skipped files, newest first.
    pub skipped: Vec<SkippedFile>,
    pub skipped_total: u64,
}

/// Default and maximum page sizes of `folders.skipped`.
pub const SKIPPED_PAGE_DEFAULT: u32 = 50;
pub const SKIPPED_PAGE_MAX: u32 = 500;

/// The folder's last run (page 25 header).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LastRun {
    /// `startup | watcher | schedule | manual | mcp | settings_change | reconnect`.
    pub trigger: Option<String>,
    pub last_full_scan_at: Option<i64>,
    /// "Last checked {time}".
    pub last_success_at: Option<i64>,
    /// The last full scan couldn't open some subfolders.
    pub partial: bool,
    /// The user pressed Stop and no full scan finished since.
    pub stopped: bool,
}

/// File counts of one folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileCounts {
    pub indexed: u64,
    pub pending: u64,
    /// Couldn't be read (the "Skipped files" list).
    pub skipped: u64,
    /// Listed, not read and never counted as skipped: files of an access grant, and of a folder
    /// paused by Amendment 17 until the user chooses.
    pub listed: u64,
}

/// Why a file is in the skipped list (page 25 reasons table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// "Bigger than the {limit} limit" (no retry; change the limit).
    TooLarge,
    /// "Stored only in OneDrive" (no retry; M5 switch).
    OnlineOnly,
    /// "May be damaged, password-protected or in an unsupported format".
    Unreadable,
    /// "Took too long to read".
    Timeout,
    /// "Windows didn't allow access".
    Permission,
    /// "In use by another program; retried automatically".
    Locked,
    /// "Looks like a data file (very long lines), not a document" (docs/06 change 5; no retry).
    DataFile,
    Other,
}

impl SkipReason {
    /// Reason from a file's `skip_reason` / `error_code` (catalog values).
    pub fn classify(skip_reason: Option<&str>, code: Option<ErrorCode>) -> Self {
        match (skip_reason, code) {
            (Some("too_large"), _) | (_, Some(ErrorCode::FileTooLarge)) => Self::TooLarge,
            (Some("online_only"), _) | (_, Some(ErrorCode::FileOnlineOnly)) => Self::OnlineOnly,
            (Some("data_file"), _) => Self::DataFile,
            (_, Some(ErrorCode::ExtractTimeout | ErrorCode::Timeout)) => Self::Timeout,
            (_, Some(ErrorCode::PermissionDenied)) => Self::Permission,
            (_, Some(ErrorCode::FileLocked)) => Self::Locked,
            (_, Some(ErrorCode::FileUnreadable | ErrorCode::Corrupt)) => Self::Unreadable,
            (Some("unreadable" | "binary" | "unsupported"), _) => Self::Unreadable,
            _ => Self::Other,
        }
    }

    /// Whether [Retry] makes sense (a size limit or an online-only file won't change by retrying).
    pub const fn retryable(self) -> bool {
        !matches!(self, Self::TooLarge | Self::OnlineOnly | Self::DataFile)
    }
}

/// One skipped file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedFile {
    pub file_id: FileId,
    pub rel_path: String,
    pub reason: SkipReason,
    pub code: Option<ErrorCode>,
    pub retryable: bool,
    pub size: u64,
    pub updated_at: i64,
}

/// `folders.skipped` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedPage {
    pub total: u64,
    pub files: Vec<SkippedFile>,
}

// ------------------------------------------------------------------------------------------------
// Folder groups

/// Name of the default group (0.A Q3 / U15).
pub const DEFAULT_GROUP_NAME: &str = "My folders";
/// Longest group name.
pub const MAX_GROUP_NAME_CHARS: usize = 80;

/// One folder group (page 31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GroupView {
    pub group_id: GroupId,
    pub name: String,
    pub is_default: bool,
    pub folder_ids: Vec<FolderId>,
    pub created_at: i64,
}

/// A trimmed group name, or `None` when empty or too long.
pub fn clean_group_name(name: &str) -> Option<String> {
    let n = name.trim();
    (!n.is_empty() && n.chars().count() <= MAX_GROUP_NAME_CHARS && !n.chars().any(char::is_control))
        .then(|| n.to_string())
}

// ------------------------------------------------------------------------------------------------
// Connected apps

/// What a connected app may see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ClientScope {
    /// "All my folders" (decision 6, the default).
    #[default]
    All,
    /// Only these groups. Unknown or deleted groups grant nothing (fail closed).
    Groups { group_ids: Vec<GroupId> },
}

/// One connected app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientView {
    pub client_id: String,
    pub kind: ClientKind,
    pub display_name: String,
    pub scope: ClientScope,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub sessions_open: u32,
    /// HTTP tools only: the token's last 4 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_last4: Option<String>,
}

/// `clients.list` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientsList {
    pub clients: Vec<ClientView>,
    pub http: HttpStatus,
}

/// Setup text for one app kind (OQ-44).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Snippet {
    pub kind: ClientKind,
    /// Where the text goes, in plain words or a path with `%USERPROFILE%`.
    pub file_hint: String,
    pub text: String,
}

/// Longest client id kept (the bridge's `--client` value).
pub const MAX_CLIENT_ID_CHARS: usize = 64;
/// Prefix of HTTP tool client ids (`http-<ulid>`).
pub const HTTP_CLIENT_PREFIX: &str = "http-";

/// The canonical id of a pipe client from its handshake `client_id`: lowercase ASCII letters,
/// digits, `-`, `_` and `.` only, at most [`MAX_CLIENT_ID_CHARS`]; empty → `other`. A pipe client
/// can never claim an HTTP tool's id.
pub fn normalize_client_id(raw: &str) -> String {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(MAX_CLIENT_ID_CHARS)
        .collect::<String>()
        .to_ascii_lowercase();
    if s.is_empty() || s.starts_with(HTTP_CLIENT_PREFIX) {
        "other".into()
    } else {
        s
    }
}

/// The client kind of a canonical pipe client id.
pub fn client_kind_of(id: &str) -> ClientKind {
    if id.starts_with(HTTP_CLIENT_PREFIX) {
        ClientKind::Http
    } else if id.starts_with("claude") {
        ClientKind::ClaudeDesktop
    } else if id.starts_with("cursor") {
        ClientKind::Cursor
    } else if id.starts_with("vscode") || id.starts_with("code") {
        ClientKind::VsCode
    } else {
        ClientKind::Other
    }
}

/// Default display name of a pipe client.
pub fn default_client_name(id: &str) -> String {
    match client_kind_of(id) {
        ClientKind::ClaudeDesktop => "Claude Desktop".into(),
        ClientKind::Cursor => "Cursor".into(),
        ClientKind::VsCode => "VS Code".into(),
        ClientKind::Other | ClientKind::Http => id.to_string(),
    }
}

// ------------------------------------------------------------------------------------------------
// HTTP

/// `http.status` reply and the `http.state` event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HttpStatus {
    pub enabled: bool,
    pub running: bool,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub error: Option<ErrorCode>,
    pub tools: u32,
}

/// `http.create_token` / `http.rotate_token` reply. The token is shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenCreated {
    pub client: ClientView,
    pub token: String,
    pub snippet: Snippet,
}

/// Longest HTTP tool name.
pub const MAX_TOOL_NAME_CHARS: usize = 60;

// ------------------------------------------------------------------------------------------------
// Request progress (answer, enhance)

/// Longest client-chosen `request_id`.
pub const MAX_REQUEST_ID_CHARS: usize = 64;

/// A valid `request_id`: 1–64 of `[A-Za-z0-9_-]`.
pub fn is_valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Answer,
    Enhance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    /// Searching the folders.
    Searching,
    /// "Reading your files…"
    ReadingFiles,
    /// "Writing the answer…"
    Writing,
    /// "Searching again (round {n} of {max})…"
    SearchingAgain,
    /// Enhance: the provider is improving the prompt.
    Improving,
}

/// `request.progress` event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestProgress {
    pub request_id: String,
    pub kind: RequestKind,
    pub phase: RequestPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

// ------------------------------------------------------------------------------------------------
// Activity

/// What happened (page 30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    FolderAdded,
    FolderRemoved,
    FolderSynced,
    FilesSkipped,
    SearchUpgraded,
    ClientConnected,
    ClientDisconnected,
    ClientFirstSeen,
    ApprovalAllowed,
    ApprovalDeclined,
    ApprovalExpired,
    Updated,
    IndexMoved,
}

impl ActivityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FolderAdded => "folder_added",
            Self::FolderRemoved => "folder_removed",
            Self::FolderSynced => "folder_synced",
            Self::FilesSkipped => "files_skipped",
            Self::SearchUpgraded => "search_upgraded",
            Self::ClientConnected => "client_connected",
            Self::ClientDisconnected => "client_disconnected",
            Self::ClientFirstSeen => "client_first_seen",
            Self::ApprovalAllowed => "approval_allowed",
            Self::ApprovalDeclined => "approval_declined",
            Self::ApprovalExpired => "approval_expired",
            Self::Updated => "updated",
            Self::IndexMoved => "index_moved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::FolderAdded,
            Self::FolderRemoved,
            Self::FolderSynced,
            Self::FilesSkipped,
            Self::SearchUpgraded,
            Self::ClientConnected,
            Self::ClientDisconnected,
            Self::ClientFirstSeen,
            Self::ApprovalAllowed,
            Self::ApprovalDeclined,
            Self::ApprovalExpired,
            Self::Updated,
            Self::IndexMoved,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }
}

/// One activity line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityEntry {
    pub id: i64,
    pub ts: i64,
    pub kind: ActivityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<FolderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<ClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `activity.list` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityPage {
    pub entries: Vec<ActivityEntry>,
    pub next_before_id: Option<i64>,
}

/// Default and maximum page sizes of `activity.list` / `activity.egress`.
pub const ACTIVITY_PAGE_DEFAULT: u32 = 50;
pub const ACTIVITY_PAGE_MAX: u32 = 200;
/// Activity is kept for 7 days (the `job_history` retention, OQ-47).
pub const ACTIVITY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One request sent to an AI provider (`egress_audit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EgressRow {
    pub id: i64,
    pub ts: i64,
    pub provider: String,
    pub provider_name: String,
    pub purpose: Purpose,
    pub bytes: u64,
    /// `None` = this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// "This app", "Claude Desktop", …
    pub client_name: String,
}

/// A provider id and its display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderRef {
    pub id: String,
    pub name: String,
}

/// `activity.egress` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EgressPage {
    pub rows: Vec<EgressRow>,
    pub next_before_id: Option<i64>,
    /// Every provider ever audited (empty → hide the card).
    pub providers: Vec<ProviderRef>,
}

// ------------------------------------------------------------------------------------------------
// Index location (page 32)

/// Folder created inside the folder the user picks.
pub const INDEX_DIR_NAME: &str = "SeleneRetrieval Index";

/// `index_location.status` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexLocationStatus {
    pub path: String,
    pub is_default: bool,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub moving: Option<IndexMoveProgress>,
    /// M5-E (R34): set while the moved index is missing (the service's recovery mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<IndexMissingInfo>,
}

/// Why a target can't take the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MoveProblem {
    NotLocal,
    TooLong,
    InsideFolder,
    InsideApp,
    NotEmpty,
    NotWritable,
    SameAsCurrent,
}

/// `index_location.check` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexMoveCheck {
    pub ok: bool,
    pub problem: Option<MoveProblem>,
    pub index_path: String,
    pub drive: String,
    pub need_bytes: u64,
    pub free_bytes: u64,
    pub enough_space: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    /// Saved; runs when the background service starts again.
    Waiting,
    Copying,
    Verifying,
    /// The pointer switch (can't be cancelled).
    Switching,
    /// Deleting the old copy after a successful start.
    Cleaning,
}

/// Move progress (`index.move.progress`, `EngineStatus::index_move`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexMoveProgress {
    pub phase: MovePhase,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub from: String,
    pub to: String,
}

/// M5-E (R34): the moved search index can't be found at start (`EngineStatus::index_missing`).
/// The service waits in a recovery mode: nothing is deleted, nothing is forked, and it starts by
/// itself as soon as `path` is back (drive reconnected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IndexMissingInfo {
    /// Where the index should be (the folder the user moved it to).
    pub path: String,
    /// Drive of `path` ("D:"), when it has one.
    pub drive: Option<String>,
    /// A complete earlier copy that "Move it back" can use (usually the default place, kept when
    /// the old copy's cleanup didn't run); `None` → only "Start fresh" is offered.
    pub old_copy: Option<String>,
    /// Folders "Start fresh" adds back and indexes again (from the service's folder list backup).
    pub folders_to_restore: u32,
}

/// `policy.status` reply (M5-E): what an administrator's policy locks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyStatus {
    /// Any setting is managed.
    pub managed: bool,
    /// Each locked setting with the line "Managed by your organisation" (also in the status
    /// snapshot's `managed`).
    pub fields: Vec<crate::config::policy::ManagedField>,
    /// Plain problems found in the policy (for an administrator; shown under Details).
    pub issues: Vec<String>,
    /// Where the policy came from: "policy.toml", "HKLM", "HKCU".
    pub sources: Vec<String>,
}

/// `index_location.recover` choice (R34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoverChoice {
    /// Use [`IndexMissingInfo::old_copy`] again (changes since the move are picked up by the
    /// normal re-scan).
    MoveBack,
    /// Start an empty index in the default place and index the remembered folders again. The
    /// missing copy is not touched (it may come back on another day; it is simply not used).
    StartFresh,
}

/// `index_location.recover` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoverParams {
    pub choice: RecoverChoice,
}

impl IndexMoveProgress {
    /// Whole percent, 0..=100.
    pub fn percent(&self) -> u64 {
        self.done_bytes
            .min(self.total_bytes)
            .saturating_mul(100)
            .checked_div(self.total_bytes)
            .unwrap_or(100)
    }
}

// ------------------------------------------------------------------------------------------------
// Account sign-in through a local CLI — M5-C, Amendments 10.2 (Claude) / 10.3 (Codex).
// Contract: docs/research/m5-c-cli-providers.md. The CLI owns the login; Selene Retrieval never
// holds, reads or stores a token.

/// The provider id of "Claude (sign in)" (the provider table row).
pub const CLAUDE_SIGNIN_PROVIDER: &str = "claudecode";
/// The only label the app shows for it in the provider list.
pub const CLAUDE_SIGNIN_LABEL: &str = "Claude (sign in)";
/// The provider id of "Codex (sign in)".
pub const CODEX_SIGNIN_PROVIDER: &str = "codex";
/// The label of the Codex row.
pub const CODEX_SIGNIN_LABEL: &str = "Codex (sign in)";

/// One provider that signs in through the user's own CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliSigninProvider {
    /// Provider table id.
    pub id: &'static str,
    /// Provider list label.
    pub label: &'static str,
    /// The user's own tool, named in plain text ("Claude Code isn't signed in").
    pub tool: &'static str,
    /// Where to get the tool (opened through the app's `https://` allow list).
    pub install_url: &'static str,
}

/// Every CLI sign-in provider.
pub const CLI_SIGNIN_PROVIDERS: &[CliSigninProvider] = &[
    CliSigninProvider {
        id: CLAUDE_SIGNIN_PROVIDER,
        label: CLAUDE_SIGNIN_LABEL,
        tool: "Claude Code",
        install_url: "https://claude.com/claude-code",
    },
    CliSigninProvider {
        id: CODEX_SIGNIN_PROVIDER,
        label: CODEX_SIGNIN_LABEL,
        tool: "Codex",
        install_url: "https://developers.openai.com/codex/cli",
    },
];

/// The CLI sign-in provider with this id.
pub fn cli_signin_provider(id: &str) -> Option<&'static CliSigninProvider> {
    CLI_SIGNIN_PROVIDERS.iter().find(|p| p.id == id)
}

/// The CLI sign-in provider shown with this label (error texts carry the label).
pub fn cli_signin_by_label(label: &str) -> Option<&'static CliSigninProvider> {
    CLI_SIGNIN_PROVIDERS.iter().find(|p| p.label == label)
}

/// Where the CLI sign-in stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CliSigninState {
    /// The CLI isn't on this computer: "{tool} isn't installed on this computer" + link.
    NotInstalled,
    /// Installed, not signed in: "{tool} isn't signed in" + [Sign in].
    SignedOut,
    /// `cli_signin.start` opened the CLI's own sign-in; waiting for it to finish.
    SigningIn,
    /// Signed in; requests use the CLI's login.
    SignedIn,
}

/// `cli_signin.status` / `claude_signin.status` reply and the `cli_signin.changed` event. Never
/// carries a token (Selene Retrieval never has one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CliSigninStatus {
    /// `claudecode` | `codex`.
    pub provider: String,
    /// Provider list label ("Claude (sign in)").
    pub label: String,
    /// Signed in and usable (`state == signed_in`).
    pub authenticated: bool,
    /// The account's email when the CLI reports one (Claude); never logged.
    pub email: Option<String>,
    /// Plan or login kind for display ("Max", "Pro", "ChatGPT").
    #[serde(default)]
    pub plan: Option<String>,
    /// The CLI was found (by absolute path).
    pub cli_found: bool,
    /// `claude --version` / `codex --version`, when found.
    #[serde(default)]
    pub cli_version: Option<String>,
    pub state: CliSigninState,
    /// The user's tool name ("Claude Code", "Codex").
    pub tool: String,
    /// Where to get the tool (`https://`).
    pub install_url: String,
    /// Why the last check failed (e.g. `Timeout`); `None` when the check itself worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
}

impl CliSigninStatus {
    /// A status for `p` with `authenticated` and `cli_found` derived from `state`.
    pub fn new(p: &CliSigninProvider, state: CliSigninState) -> Self {
        Self {
            provider: p.id.to_string(),
            label: p.label.to_string(),
            authenticated: state == CliSigninState::SignedIn,
            email: None,
            plan: None,
            cli_found: state != CliSigninState::NotInstalled,
            cli_version: None,
            state,
            tool: p.tool.to_string(),
            install_url: p.install_url.to_string(),
            error: None,
        }
    }
}

/// `cli_signin.status` / `cli_signin.start` params (`claude_signin.*` always use `claudecode`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CliSigninParams {
    /// `claudecode` (default) | `codex`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Ask the CLI again now ([Check again], window focus); otherwise a recent answer (≤ 30 s)
    /// may be reused.
    #[serde(default)]
    pub refresh: bool,
}

/// `cli_signin.start` reply: the status plus whether the CLI's own sign-in window opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CliSigninStart {
    #[serde(flatten)]
    pub status: CliSigninStatus,
    /// The CLI's sign-in started in its own window (it opens the browser itself). `false` when
    /// already signed in or not installed.
    pub started: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_set_is_the_contract() {
        for m in [
            "consent.set",
            "approvals.approve",
            "providers.save",
            "clients.set_scope",
            "http.set_enabled",
            "http.create_token",
            "http.rotate_token",
            "index_location.move",
            "index_location.recover",
            // S-05
            "folders.add",
            "groups.move_folder",
            "settings.update_advanced",
            "search_model.choose",
            "search_model.use_recommended",
            "reranker.download",
            "host.shutdown",
            "host.restart",
            "cli_signin.start",
            "cli_signin.status",
            "claude_signin.start",
            "claude_signin.status",
        ] {
            assert!(is_privileged(m), "{m}");
        }
        for m in [
            "approvals.deny",
            "http.revoke",
            "clients.forget",
            "search",
            "engine.status",
        ] {
            assert!(!is_privileged(m), "{m}");
        }
    }

    #[test]
    fn scope_wire_shape() {
        assert_eq!(
            serde_json::to_value(ClientScope::All).unwrap(),
            serde_json::json!({"mode": "all"})
        );
        let g = GroupId::new();
        let v = serde_json::to_value(ClientScope::Groups { group_ids: vec![g] }).unwrap();
        assert_eq!(v["mode"], "groups");
        assert_eq!(v["group_ids"][0], g.to_string());
        let back: ClientScope = serde_json::from_value(v).unwrap();
        assert_eq!(back, ClientScope::Groups { group_ids: vec![g] });
    }

    #[test]
    fn client_ids_are_normalised_and_never_http() {
        assert_eq!(normalize_client_id(" Claude "), "claude");
        assert_eq!(normalize_client_id("VSCode"), "vscode");
        assert_eq!(normalize_client_id(""), "other");
        assert_eq!(normalize_client_id("http-01ABC"), "other");
        assert_eq!(normalize_client_id("a b/c\\d"), "abcd");
        assert_eq!(
            normalize_client_id(&"x".repeat(200)).len(),
            MAX_CLIENT_ID_CHARS
        );
        assert_eq!(client_kind_of("claude"), ClientKind::ClaudeDesktop);
        assert_eq!(client_kind_of("cursor"), ClientKind::Cursor);
        assert_eq!(client_kind_of("vscode"), ClientKind::VsCode);
        assert_eq!(client_kind_of("http-01"), ClientKind::Http);
        assert_eq!(client_kind_of("zed"), ClientKind::Other);
        assert_eq!(default_client_name("claude"), "Claude Desktop");
        assert_eq!(default_client_name("zed"), "zed");
    }

    #[test]
    fn request_ids_and_group_names() {
        assert!(is_valid_request_id("01HZX-abc_9"));
        assert!(!is_valid_request_id(""));
        assert!(!is_valid_request_id("a b"));
        assert!(!is_valid_request_id(&"a".repeat(65)));
        assert_eq!(clean_group_name("  Work "), Some("Work".into()));
        assert_eq!(clean_group_name("   "), None);
        assert_eq!(clean_group_name(&"g".repeat(81)), None);
        assert_eq!(clean_group_name("a\nb"), None);
    }

    #[test]
    fn skip_reasons_follow_page_25() {
        assert_eq!(
            SkipReason::classify(Some("too_large"), None),
            SkipReason::TooLarge
        );
        assert_eq!(
            SkipReason::classify(None, Some(ErrorCode::FileOnlineOnly)),
            SkipReason::OnlineOnly
        );
        assert_eq!(
            SkipReason::classify(None, Some(ErrorCode::ExtractTimeout)),
            SkipReason::Timeout
        );
        assert_eq!(
            SkipReason::classify(None, Some(ErrorCode::FileUnreadable)),
            SkipReason::Unreadable
        );
        assert!(!SkipReason::TooLarge.retryable());
        assert!(SkipReason::Locked.retryable());
        assert_eq!(
            SkipReason::classify(Some("data_file"), None),
            SkipReason::DataFile
        );
        assert!(!SkipReason::DataFile.retryable());
    }

    #[test]
    fn activity_kinds_roundtrip() {
        for k in [
            ActivityKind::FolderAdded,
            ActivityKind::IndexMoved,
            ActivityKind::ClientFirstSeen,
        ] {
            assert_eq!(ActivityKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        let p = IndexMoveProgress {
            phase: MovePhase::Copying,
            done_bytes: 50,
            total_bytes: 200,
            from: "C:\\a".into(),
            to: "D:\\b".into(),
        };
        assert_eq!(p.percent(), 25);
    }
}
