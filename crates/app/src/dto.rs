//! Types crossing the Tauri IPC boundary (exported to `ui/src/bindings.ts` by tauri-specta).
//!
//! These are view models, not the daemon protocol: the app maps `sr_common` types into them so
//! the UI never sees error codes outside "Details" and never needs jargon (R-UX4).

use serde::{Deserialize, Serialize};
use specta::Type;
use sr_common::error::{Error, Purpose, Section, SuggestedAction, UserMessage};

/// The operating system as the user knows it, at the start of a sentence ("Windows didn't allow
/// the change", "Your Mac didn't allow the change").
pub const OS_NAME: &str = if cfg!(target_os = "macos") {
    "Your Mac"
} else {
    "Windows"
};

/// The one user-facing error shape: title, body, one action, details hidden by default (P1.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct UiError {
    pub title: String,
    pub body: String,
    pub action: UiAction,
    /// Button label for `action` ("" when there is none).
    pub action_label: String,
    /// Shown only under "Details": error code and trace id. Never file contents or secrets.
    pub details: String,
}

/// The next step offered with an error. Only actions the app can perform are listed; the rest
/// map to the closest one (documented in `UiError::from`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAction {
    None,
    Retry,
    WaitAndRetry {
        secs: u32,
    },
    ChooseFolder,
    ChooseSubfolder {
        path: String,
    },
    OpenSettings,
    RestartApp,
    RestartClaude,
    CopyDiagnostics,
    CopySnippet,
    FreeDiskSpace,
    /// M3: the provider rejected the key: focus the key field (wizard) or open "Replace key".
    /// `provider` is the display name.
    ReenterKey {
        provider: String,
    },
    /// M3: no provider yet: open Providers & Keys.
    AddProvider,
    /// M3: `OpenSettings(Providers)` (quota, unknown model): open Providers & Keys.
    OpenProviders,
    /// M3: code 26 `PrivacyBlocked`: open the consent dialog for this purpose. `provider` is the
    /// display name from the error text; the dialog records the consent for the configured
    /// provider (one provider in v1, OQ-33).
    AllowRemote {
        provider: String,
        purpose: ConsentPurpose,
    },
    /// A pending folder request: bring its card into view.
    ReviewApproval {
        id: String,
    },
    /// M4 (page 25): open the folder's detail at its "Skipped files" list (codes 9, 12, 13).
    /// `None` = the folder isn't known: the Folders list.
    ViewSkippedFiles {
        folder_id: Option<String>,
    },
    /// M4: `OpenSettings(Folder)` (code 11): the folder's detail, or the Folders list.
    OpenFolder {
        folder_id: Option<String>,
    },
    /// M4: `OpenSettings(ConnectedApps)`: the Connected apps page (page 29).
    OpenConnectedApps,
}

impl UiError {
    pub fn new(title: &str, body: &str, action: UiAction, label: &str, details: &str) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            action,
            action_label: label.into(),
            details: details.into(),
        }
    }
}

impl From<&Error> for UiError {
    fn from(e: &Error) -> Self {
        let mut u = UiError::from(&e.user_message());
        // `OpenSettings(Folder)` carries no id in the message; the error's context has it.
        if let UiAction::OpenFolder { folder_id } = &mut u.action
            && folder_id.is_none()
        {
            *folder_id = e.ctx.folder_id.as_ref().map(ToString::to_string);
        }
        u
    }
}

impl From<&UserMessage> for UiError {
    fn from(m: &UserMessage) -> Self {
        let m = m.clone();
        let action = match &m.action {
            SuggestedAction::None => UiAction::None,
            SuggestedAction::Retry => UiAction::Retry,
            SuggestedAction::WaitAndRetry(s) => UiAction::WaitAndRetry { secs: *s },
            SuggestedAction::ChooseFolder => UiAction::ChooseFolder,
            SuggestedAction::ChooseSubfolder(p) => UiAction::ChooseSubfolder {
                path: p.to_string_lossy().into_owned(),
            },
            SuggestedAction::RestartApp => UiAction::RestartApp,
            SuggestedAction::RestartClient(_) | SuggestedAction::ReconnectClient(_) => {
                UiAction::RestartClaude
            }
            SuggestedAction::CopyDiagnostics => UiAction::CopyDiagnostics,
            SuggestedAction::FreeDiskSpace => UiAction::FreeDiskSpace,
            SuggestedAction::ReenterKey(p) => UiAction::ReenterKey {
                provider: p.clone(),
            },
            SuggestedAction::AddProvider => UiAction::AddProvider,
            SuggestedAction::OpenSettings(Section::Providers) => UiAction::OpenProviders,
            SuggestedAction::AllowRemote(p, purpose) => UiAction::AllowRemote {
                provider: p.clone(),
                purpose: ConsentPurpose::from(*purpose),
            },
            SuggestedAction::ReviewApproval(id) => UiAction::ReviewApproval { id: id.to_string() },
            // M4: the folder detail (page 25) holds the skipped files and the folder settings.
            SuggestedAction::ViewSkippedFiles(id) => UiAction::ViewSkippedFiles {
                folder_id: id.as_ref().map(ToString::to_string),
            },
            SuggestedAction::OpenSettings(Section::Folder) => {
                UiAction::OpenFolder { folder_id: None }
            }
            SuggestedAction::OpenSettings(Section::ConnectedApps) => UiAction::OpenConnectedApps,
            SuggestedAction::OpenSettings(_) => UiAction::OpenSettings,
        };
        let label = match &action {
            UiAction::RestartClaude => "How to restart Claude",
            UiAction::OpenSettings
            | UiAction::OpenProviders
            | UiAction::OpenFolder { .. }
            | UiAction::OpenConnectedApps => "Open settings",
            _ => m.action.label(),
        };
        Self {
            title: m.title,
            body: m.body,
            action,
            action_label: label.into(),
            details: format!("{:?} · trace {}", m.details.code, m.details.trace_id),
        }
    }
}

impl From<Error> for UiError {
    fn from(e: Error) -> Self {
        UiError::from(&e)
    }
}

/// Overall state, used for the tray icon and the header line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Overall {
    /// Attaching to (or starting) the background service.
    Starting,
    NoFolders,
    Indexing,
    Synced,
    /// Something needs the user (folder unavailable, service down).
    Problem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FolderState {
    Waiting,
    Scanning,
    Indexing,
    UpToDate,
    UpToDateWithSkips,
    Paused,
    Unavailable,
    Problem,
    /// Removed by the user, still being deleted from the index until `PurgeCompleted` (OQ-18).
    Removing,
    /// A run stopped before the end (`partial`, OQ-37): search covers what's done.
    Stopped,
}

/// Kind of the extra note under a folder's status line (DS-5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FolderNoteKind {
    /// Network drive, or a drive without live updates: changes are picked up on a schedule.
    Network,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FolderNote {
    pub kind: FolderNoteKind,
    /// Exact text from the service (`sr_sync::watch_note`), e.g. "This folder is on a network
    /// drive, so changes are picked up every hour."
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FolderView {
    pub id: String,
    pub name: String,
    /// Full path, when known (from `folders.list`); the snapshot carries only names.
    pub path: Option<String>,
    pub state: FolderState,
    /// Plain one-liner, e.g. "Indexing 340 of 2,100 files · about 4 minutes left".
    pub line: String,
    pub files_done: u32,
    pub files_total: u32,
    pub files_skipped: u32,
    pub eta_s: Option<u32>,
    /// Network-drive / no-live-updates note (OQ-38), when the service sends one.
    pub note: Option<FolderNote>,
    /// Folder group (M4, page 31; Advanced only). `None` from an older service.
    pub group_id: Option<String>,
    /// Paused because it was set to only list its files, a mode that was removed (Amendment 17):
    /// the card asks the user to search its contents (resume) or remove it.
    pub was_files_only: bool,
    /// M5 (DS-5.3): files skipped because they are stored only in OneDrive ("Some files are
    /// stored only in OneDrive and were skipped." + [Include them]). 0 = none or not known.
    pub online_only_skipped: u32,
    /// M5: the service's OneDrive note for this folder, when it sends one (`cloud_note`).
    pub cloud_note: Option<String>,
}

/// Search quality tiers in plain words (Basic / Better / Best).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TierName {
    Basic,
    Better,
    Best,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TierView {
    pub active: TierName,
    pub recommended: TierName,
    /// Uses the graphics card for indexing.
    pub gpu: bool,
    /// The active index uses the multilingual model (snapshot `tier.multilingual`, M4 contract
    /// Changes 1); `active` then reads `basic`, its resource class. Shown as "Multilingual search".
    pub multilingual: bool,
}

/// Where a search model change ("Use it") is. Pushed by the service's `model.download.*`,
/// `reembed.*` and `index.swapped` events (never polled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ModelStep {
    Downloading,
    /// Metered connection: paused until the user picks "Download anyway" (or another network).
    WaitingForUnmetered,
    /// Re-indexing with the new model; search keeps working on the current one.
    Improving,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ModelChangeView {
    pub step: ModelStep,
    /// Progress: MB for a download, chunks for the re-index.
    pub done: u32,
    pub total: u32,
    /// Plain line, e.g. "Downloading better search… 52 of 208 MB".
    pub text: String,
    /// Set when the change stopped and needs the user (one action).
    pub error: Option<UiError>,
}

/// Settings → Advanced (02 §2.5): the switches the M2 app can change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct AdvancedView {
    /// "Use graphics card": Auto (true, default) or Off.
    pub use_graphics_card: bool,
    /// Connected apps may ask to add folders (each request needs approval here). Off by default.
    pub allow_app_folder_requests: bool,
    /// Connected apps may ask to check a folder for changes now (MCP `sync_folder`). Off by
    /// default.
    pub allow_app_folder_checks: bool,
    /// A change applies after Selene Retrieval restarts.
    pub restart_needed: bool,
}

/// A connected app asks to add a folder (MCP `add_folder`, off by default): the user decides here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ApprovalView {
    pub id: String,
    /// "Claude", "Cursor", "VS Code" or "An app".
    pub client: String,
    pub path: String,
    /// When the request expires, in ms since the Unix epoch (0 = unknown). The card shows
    /// "Expires in about {m} minutes." once under 5 minutes remain, recomputed on each push.
    pub expires_at_ms: f64,
}

/// What a provider may receive (02 §2.5 purposes). `answer` and `iterate` are one consent
/// (OQ-34); `validate_key` and `list_models` are consented by the [Check and save] click itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConsentPurpose {
    /// Questions and matching file text (answers and deeper answers).
    Answer,
    /// Your prompt (better prompts).
    Enhance,
    /// Search results (remote re-ranking).
    Rerank,
    /// The text of your files (remote embeddings): the stronger wording.
    EmbedDoc,
    /// Your search text (remote embeddings).
    EmbedQuery,
}

impl From<Purpose> for ConsentPurpose {
    fn from(p: Purpose) -> Self {
        match p {
            Purpose::EmbedDoc => ConsentPurpose::EmbedDoc,
            Purpose::EmbedQuery => ConsentPurpose::EmbedQuery,
            Purpose::Rerank => ConsentPurpose::Rerank,
            Purpose::Enhance => ConsentPurpose::Enhance,
            // validate_key / list_models are never asked separately (the click is the consent);
            // a block on them is shown with the answer wording, the closest plain description.
            Purpose::Answer | Purpose::Iterate | Purpose::ValidateKey | Purpose::ListModels => {
                ConsentPurpose::Answer
            }
        }
    }
}

impl ConsentPurpose {
    /// Wire name (the `answer` consent also covers `iterate`).
    pub const fn wire(self) -> &'static str {
        match self {
            ConsentPurpose::Answer => "answer",
            ConsentPurpose::Enhance => "enhance",
            ConsentPurpose::Rerank => "rerank",
            ConsentPurpose::EmbedDoc => "embed_doc",
            ConsentPurpose::EmbedQuery => "embed_query",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "answer" | "iterate" => ConsentPurpose::Answer,
            "enhance" => ConsentPurpose::Enhance,
            "rerank" => ConsentPurpose::Rerank,
            "embed_doc" => ConsentPurpose::EmbedDoc,
            "embed_query" => ConsentPurpose::EmbedQuery,
            _ => return None,
        })
    }
}

/// The service needs a consent the user hasn't given yet (e.g. Claude asked for an answer): the
/// app shows the consent dialog (page 22) while it is open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConsentRequest {
    /// Provider id (from the provider list).
    pub provider: String,
    /// Display name for the dialog text.
    pub provider_name: String,
    pub purpose: ConsentPurpose,
}

/// Health of the configured AI provider (`ProviderHealth` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthView {
    Ok,
    Degraded,
    Down,
}

/// Everything the UI shows about the engine. Pushed whole on every change (`StateChanged`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct AppView {
    pub overall: Overall,
    /// Plain one-liner for the header and the tray tooltip.
    pub summary: String,
    pub folders: Vec<FolderView>,
    /// Set while the background service can't be reached.
    pub problem: Option<UiError>,
    pub tier: Option<TierView>,
    /// Increments whenever the background service restarted and the app re-attached.
    pub reconnects: u32,
    /// Notices from the service, e.g. "Some settings were reset" (R-UX19).
    pub notices: Vec<UiError>,
    /// A search model change in progress or just finished ([Use it]).
    pub model_change: Option<ModelChangeView>,
    /// Pending `add_folder` requests from connected apps.
    pub approvals: Vec<ApprovalView>,
    /// The restart catch-up is running (Amendment 4.24): the Overview shows the page 03 info row.
    pub catching_up: bool,
    /// GPU indexing waits while a full-screen app is open (R-UX32); the summary says so.
    pub waiting_for_fullscreen: bool,
    /// Health of the configured AI provider (pushed `ProviderHealth`).
    pub provider_health: Option<ProviderHealthView>,
    /// A consent the service asked for that the user hasn't answered yet (page 22).
    pub consent_request: Option<ConsentRequest>,
    /// Set when the app was opened for a folder request (`sr-app --approve <id>`, R-UX30): the UI
    /// brings that card into view and focuses its heading.
    pub focus_approval: Option<String>,
    /// M4 (page 03): [Restart now] / [Restart Selene Retrieval] is restarting the background
    /// service; the shell shows "Restarting the background service…" until it is back.
    pub restarting: bool,
    /// M4 (page 32): the search index is being moved (pushed `index.move.progress`).
    pub index_move: Option<IndexMoveView>,
    /// M5 (page 33): what the organisation's policy sets; empty without a policy.
    pub policy: crate::policy::PolicyView,
    /// M5 (R34): the moved search index can't be found; the service waits in its recovery mode.
    pub index_missing: Option<crate::policy::IndexMissingView>,
    /// M5 S-04 (R40): the file reader runs with the low-integrity fallback; Settings shows the
    /// quiet line "File reader running with reduced protection".
    pub reduced_protection: bool,
}

/// Progress of "Move the search index" (page 32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexMoveView {
    /// Waiting / copying / verifying (can be cancelled) or switching / cleaning (can't).
    pub can_cancel: bool,
    pub percent: u32,
    /// "Moving… {done} of {total} GB" (or "Starting Selene Retrieval…" while waiting).
    pub line: String,
    pub from: String,
    pub to: String,
}

/// A pushed service change the UI reacts to by re-reading one thing (never on a timer), or a
/// one-off result. Sent as the `ServiceSignal` event, not through `AppView`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceSignal {
    /// `groups.changed`: re-read the folder groups.
    GroupsChanged,
    /// `clients.changed` / `http.state`: re-read Connected apps.
    ClientsChanged,
    /// `activity.added`: re-read the first Activity page.
    ActivityAdded,
    /// `egress.recorded`: re-read the first "Sent to AI providers" page.
    EgressRecorded,
    /// `request.progress`: the plain phase line of an Ask / Prompt Enhancer request.
    RequestProgress { request_id: String, line: String },
    /// `index.move.done`: "Search index moved."
    IndexMoveDone { path: String },
    /// `index.move.failed`: "Nothing was moved. Your index is still in {old path}."
    IndexMoveFailed { error: UiError, kept_path: String },
    /// M5-C `cli_signin.changed`: "Claude (sign in)" / "Codex (sign in)" changed (its sign-in
    /// window closed, a check found a new state). No token, ever.
    CliSigninChanged {
        status: crate::cli_signin::CliSignInStatus,
    },
}

impl Default for AppView {
    fn default() -> Self {
        Self {
            overall: Overall::Starting,
            summary: "Starting…".into(),
            folders: Vec::new(),
            problem: None,
            tier: None,
            reconnects: 0,
            notices: Vec::new(),
            model_change: None,
            approvals: Vec::new(),
            catching_up: false,
            waiting_for_fullscreen: false,
            provider_health: None,
            consent_request: None,
            focus_approval: None,
            restarting: false,
            index_move: None,
            policy: crate::policy::PolicyView::default(),
            index_missing: None,
            reduced_protection: false,
        }
    }
}

/// Pre-scan estimate shown in the wizard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Estimate {
    pub files: u32,
    /// True when the count stopped early (very large folder); `files` is then a lower bound.
    pub capped: bool,
    pub eta_s: u32,
    /// Plain sentence, e.g. "About 2,300 files. This first pass should take about 10 minutes."
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeState {
    NotInstalled,
    NotConnected,
    Connected,
    /// Our entry exists but points to an old location: "Reconnect".
    NeedsReconnect,
    /// Claude's settings file isn't valid; we won't edit it. Offer the snippet.
    CantRead,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConnectStatus {
    pub claude: ClaudeState,
    /// Copy-paste text for any MCP client (stdio; no token needed).
    pub snippet: String,
    /// Claude itself rewrote its settings and our entry is gone (R-UX23): the card says
    /// "Claude's settings were changed and no longer include Selene Retrieval." + Reconnect.
    pub changed_externally: bool,
}

/// The AI apps Selene Retrieval can connect with one click (page 29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ClientApp {
    ClaudeDesktop,
    Cursor,
    VsCode,
}

impl ClientApp {
    pub const ALL: [ClientApp; 3] = [
        ClientApp::ClaudeDesktop,
        ClientApp::Cursor,
        ClientApp::VsCode,
    ];

    /// Display name ("Claude Desktop", "Cursor", "VS Code").
    pub const fn name(self) -> &'static str {
        match self {
            ClientApp::ClaudeDesktop => "Claude Desktop",
            ClientApp::Cursor => "Cursor",
            ClientApp::VsCode => "VS Code",
        }
    }

    /// Short name used in sentences ("Quit Claude and try again").
    pub const fn short(self) -> &'static str {
        match self {
            ClientApp::ClaudeDesktop => "Claude",
            ClientApp::Cursor => "Cursor",
            ClientApp::VsCode => "VS Code",
        }
    }
}

/// One app card on Connected apps (page 29). The connection states are the DS-5.5 states, used
/// for every app (`ClaudeState` is the shared name).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ClientStatus {
    pub app: ClientApp,
    pub name: String,
    pub state: ClaudeState,
    /// Setup text in this app's own format (OQ-44), with no token (the pipe is user-only).
    pub snippet: String,
    /// The app removed our entry itself (R-UX23; tracked for Claude Desktop).
    pub changed_externally: bool,
    /// When the app last used Selene Retrieval (ms since the Unix epoch), if the service knows.
    pub last_used_ms: Option<f64>,
}

/// Connected apps (page 29): the one-click apps and the "Other app" setup text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ClientsView {
    pub apps: Vec<ClientStatus>,
    /// "Other app": Claude-format setup text with the generic client id.
    pub other_snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConnectResult {
    pub status: ConnectStatus,
    /// Plain next step, e.g. "Restart Claude to finish."
    pub message: String,
}

/// One bundled licence text (`<install dir>\licenses\*.txt`), for "View licences" (page 18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct LicenseText {
    /// File name without `.txt`, e.g. "LICENSE-selene-retrieval".
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct AboutInfo {
    pub version: String,
    /// Where the index is kept (Advanced only).
    pub data_dir: String,
    pub start_with_windows: bool,
    /// "Start with Windows" was already chosen once (wizard or Settings). The wizard then shows
    /// the current state and never re-enables autostart behind the user's back (OQ-16).
    pub autostart_decided: bool,
}

// ---------------------------------------------------------------------------------------------
// M3: providers, consent, answers (pages 20–23)

/// How a provider signs in (provider list `auth_kind`). Account sign-in providers are available
/// only when they reuse the user's own CLI login ("Claude (sign in)", "Codex (sign in)", M5-C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Paste an API key (validated live, kept in Windows Credential Manager).
    ApiKey,
    /// Signs in with an account: "Claude (sign in)" / "Codex (sign in)" use the local CLI's
    /// login (no key); other account rows are shown disabled.
    Oauth,
    /// Runs on this computer: an address instead of a key.
    Local,
}

/// One provider from the generated provider list (`selene-providers.json`, synced from Selene by
/// `xtask sync-providers`). No provider or model name is hard-coded in the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ProviderOption {
    pub id: String,
    pub name: String,
    pub auth: AuthKind,
    /// False for providers the app can't set up yet: shown, not selectable. "Claude (sign in)" and
    /// "Codex (sign in)" are available (Amendments 10.2 / 10.3).
    pub available: bool,
    /// Why an unavailable provider can't be chosen, from the provider list (e.g.
    /// `pending_vendor_approval`); `None` when it is available.
    pub disabled_reason: Option<String>,
    /// Pre-filled address for providers that run on this computer.
    pub default_address: Option<String>,
    /// Example model, shown only as an "e.g. …" hint; the real list comes from the provider.
    pub model_hint: Option<String>,
    /// "Where do I get a key?" (page 21, Amendment 8.3): the provider's own `https://` page, from
    /// the provider list. `None` = no link is shown.
    pub key_url: Option<String>,
}

/// One model the provider offers (`list_models`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
}

/// Where the configured provider's key stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum KeyCheck {
    /// Checked with the provider and working.
    Working,
    /// Saved while offline; it is checked when online ("Saved, not checked yet").
    NotChecked,
    /// The last check failed (`last_error` says why, with one action).
    Failed,
}

/// Providers & Keys (page 21). The key itself never reaches the UI: only whether one is saved
/// and its last 4 characters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ProviderStatus {
    /// Configured provider id; `None` = no provider (search works without one).
    pub provider: Option<String>,
    pub provider_name: Option<String>,
    pub key_saved: bool,
    /// Last 4 characters of the saved key ("" when none, or for a local provider).
    pub key_last4: String,
    /// Address of a provider that runs on this computer.
    pub address: Option<String>,
    pub check: KeyCheck,
    /// The last check's error when it failed.
    pub last_error: Option<UiError>,
    /// Chosen answer model; `None` = automatic (`default_model`).
    pub model: Option<String>,
    /// The automatically chosen model ("Automatic ({model})").
    pub default_model: Option<String>,
    pub models: Vec<ModelOption>,
    /// Remote embeddings are in use (Advanced): the file-text and search-text rows appear.
    pub remote_embeddings: bool,
    /// Remote re-ranking is in use: the search-results row appears.
    pub remote_rerank: bool,
}

impl ProviderStatus {
    /// No provider configured.
    pub fn none() -> Self {
        Self {
            provider: None,
            provider_name: None,
            key_saved: false,
            key_last4: String::new(),
            address: None,
            check: KeyCheck::NotChecked,
            last_error: None,
            model: None,
            default_model: None,
            models: Vec::new(),
            remote_embeddings: false,
            remote_rerank: false,
        }
    }
}

/// Result of [Check and save].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SaveResult {
    pub status: ProviderStatus,
    /// False when the key was saved without checking (offline, "Save without checking").
    pub validated: bool,
}

/// One "Allowed to send" row (page 21).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConsentRow {
    pub purpose: ConsentPurpose,
    pub allowed: bool,
}

/// Settings → Answers (page 23), shown only once a provider exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct AnswersView {
    pub answers_enabled: bool,
    /// Off until the user turns it on (P1.2, U26).
    pub deeper_answers: bool,
    /// Extra search rounds for deeper answers (Advanced, 1–5).
    pub max_rounds: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::error::ErrorCode;

    #[test]
    fn every_error_code_maps_to_plain_text_with_a_label() {
        for code in ErrorCode::ALL {
            let u = UiError::from(Error::new(*code));
            assert!(!u.title.is_empty(), "{code:?}");
            assert!(!u.body.is_empty(), "{code:?}");
            assert!(
                !u.title.contains("daemon") && !u.body.contains("daemon"),
                "jargon in {code:?}"
            );
            if u.action != UiAction::None {
                assert!(!u.action_label.is_empty(), "{code:?} needs a button label");
            }
            assert!(u.details.contains(&format!("{code:?}")));
        }
    }
}
