//! `ErrorCode` → `UserMessage` table (docs/02-architecture.md §2.5). English only for now;
//! localisation replaces the string literals with catalog lookups but keeps this one renderer.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ClientKind, Error, ErrorCode, Purpose, Section};
use crate::ids::{ApprovalId, FolderId, TraceId};

/// The one next action shown with every user-visible error (P1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "arg", rename_all = "snake_case")]
pub enum SuggestedAction {
    Retry,
    WaitAndRetry(u32),
    OpenSettings(Section),
    ReenterKey(String),
    AddProvider,
    AllowRemote(String, Purpose),
    ChooseFolder,
    ChooseSubfolder(PathBuf),
    ReviewApproval(ApprovalId),
    ViewSkippedFiles(Option<FolderId>),
    FreeDiskSpace,
    RestartApp,
    RestartClient(ClientKind),
    ReconnectClient(ClientKind),
    CopyDiagnostics,
    None,
}

impl SuggestedAction {
    /// Button label in the app ("" for `None`).
    pub fn label(&self) -> &'static str {
        match self {
            SuggestedAction::Retry => "Retry",
            SuggestedAction::WaitAndRetry(_) => "Try again",
            SuggestedAction::OpenSettings(_) => "Open settings",
            SuggestedAction::ReenterKey(_) => "Re-enter key",
            SuggestedAction::AddProvider => "Add a provider",
            SuggestedAction::AllowRemote(..) => "Allow",
            SuggestedAction::ChooseFolder => "Choose a folder",
            SuggestedAction::ChooseSubfolder(_) => "Choose a subfolder",
            SuggestedAction::ReviewApproval(_) => "Review",
            SuggestedAction::ViewSkippedFiles(_) => "View skipped files",
            SuggestedAction::FreeDiskSpace => "Free up space",
            SuggestedAction::RestartApp => "Restart Selene Retrieval",
            SuggestedAction::RestartClient(_) => "How to restart",
            SuggestedAction::ReconnectClient(_) => "Reconnect",
            SuggestedAction::CopyDiagnostics => "Copy diagnostics",
            SuggestedAction::None => "",
        }
    }

    /// Phrasing for an MCP agent, which cannot press buttons and must relay to the user.
    pub fn agent_phrase(&self) -> String {
        match self {
            SuggestedAction::Retry => "You can retry the same call.".into(),
            SuggestedAction::WaitAndRetry(s) => format!("Retry in about {s} seconds."),
            SuggestedAction::OpenSettings(_) => {
                "Ask the user to review the settings in Selene Retrieval.".into()
            }
            SuggestedAction::ReenterKey(p) => {
                format!("Ask the user to re-enter their {p} key in Selene Retrieval.")
            }
            SuggestedAction::AddProvider => {
                "Ask the user to add an AI provider in Selene Retrieval.".into()
            }
            SuggestedAction::AllowRemote(p, _) => {
                format!("Ask the user to allow sending this to {p} in Selene Retrieval.")
            }
            SuggestedAction::ChooseFolder => {
                "Ask the user to add the folder in Selene Retrieval.".into()
            }
            SuggestedAction::ChooseSubfolder(_) => {
                "Ask the user to pick a subfolder in Selene Retrieval.".into()
            }
            SuggestedAction::ReviewApproval(_) => {
                "Waiting for the user to approve this in Selene Retrieval.".into()
            }
            SuggestedAction::ViewSkippedFiles(_) => {
                "The file was skipped; the user can see skipped files in Selene Retrieval.".into()
            }
            SuggestedAction::FreeDiskSpace => "Ask the user to free up disk space.".into(),
            SuggestedAction::RestartApp => "Ask the user to restart Selene Retrieval.".into(),
            SuggestedAction::RestartClient(c) => {
                format!("Ask the user to restart {}.", c.display_name())
            }
            SuggestedAction::ReconnectClient(c) => format!(
                "Ask the user to reconnect {} in Selene Retrieval.",
                c.display_name()
            ),
            SuggestedAction::CopyDiagnostics => {
                "Ask the user to use \"Copy diagnostics\" in Selene Retrieval and contact support."
                    .into()
            }
            SuggestedAction::None => String::new(),
        }
    }
}

/// Shown only when the user opens "Details".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Details {
    pub code: ErrorCode,
    pub chain: Vec<String>,
    pub trace_id: TraceId,
}

/// User-facing rendering of an [`Error`]: plain title + body + one action (+ Details).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserMessage {
    pub title: String,
    pub body: String,
    pub action: SuggestedAction,
    pub details: Details,
}

impl UserMessage {
    /// Text for an MCP `isError: true` tool result: "Title. Body Agent phrase". Titles carry no
    /// final punctuation, so the separator ends the title as a sentence (M1 tester: a bare space
    /// read as one run-on sentence).
    pub fn agent_text(&self) -> String {
        let mut t = sentence(&self.title);
        if !self.body.is_empty() {
            t.push(' ');
            t.push_str(&sentence(&self.body));
        }
        let phrase = match self.details.code {
            // R34: the generic "review the settings" phrase would not tell the agent what to say.
            ErrorCode::IndexMissing => "Ask the user to reconnect the drive that holds the search index, or to open Selene Retrieval and move the index back.".into(),
            // M5-E: the user can fix this in File Explorer (no app setting needed).
            ErrorCode::FileOnlineOnly => "Ask the user to right-click the file in File Explorer and choose \"Always keep on this device\"; it can be read and searched once it is downloaded.".into(),
            ErrorCode::ManagedByPolicy => "This is blocked by the user's organisation and can't be changed in Selene Retrieval; don't retry.".into(),
            ErrorCode::ConnectionNotTrusted => "Ask the user to restart Windows; don't retry.".into(),
            ErrorCode::PortInUse => "Ask the user to close the program that uses this port, then turn HTTP for local tools on again in Selene Retrieval.".into(),
            _ => self.action.agent_phrase(),
        };
        if !phrase.is_empty() {
            t.push(' ');
            t.push_str(&phrase);
        }
        t
    }
}

/// `s` as a sentence: a final `.` unless it already ends with sentence punctuation.
fn sentence(s: &str) -> String {
    let s = s.trim_end();
    if s.is_empty() || s.ends_with(['.', '!', '?', '…', ':']) {
        s.to_string()
    } else {
        format!("{s}.")
    }
}

fn cap_first(s: String) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

impl From<&Error> for UserMessage {
    fn from(e: &Error) -> Self {
        let c = &e.ctx;
        let provider = c
            .provider
            .clone()
            .unwrap_or_else(|| "your AI provider".into());
        let your_provider = match &c.provider {
            Some(p) => format!("Your {p}"),
            None => "Your AI provider".into(),
        };
        let file = c.file_name().unwrap_or_else(|| "This file".into());
        let quoted_file = if c.path.is_some() {
            format!("\"{file}\"")
        } else {
            file.clone()
        };
        let name = c
            .name
            .clone()
            .map(|n| format!("\"{n}\""))
            .unwrap_or_else(|| "This folder".into());
        let client = c
            .client_name
            .clone()
            .or_else(|| c.client.map(|k| k.display_name().to_string()))
            .unwrap_or_else(|| "A connected app".into());
        let client_kind = c.client.unwrap_or(ClientKind::Other);
        let host = c.host.clone().unwrap_or_else(|| "the internet".into());
        let model = c.model.clone().unwrap_or_else(|| "model".into());
        let secs = c
            .secs
            .or_else(|| e.retry_after.and_then(|d| u32::try_from(d.as_secs()).ok()));
        let purpose = c.purpose.unwrap_or(Purpose::EmbedDoc);

        use ErrorCode as C;
        use SuggestedAction as A;
        let (title, body, action): (String, String, SuggestedAction) = match e.code {
            C::InvalidInput => (
                "That request wasn't valid".into(),
                "Something in the request was missing or out of range.".into(),
                A::None,
            ),
            C::NotFound => (
                "Not found".into(),
                "That folder or file no longer exists in Selene Retrieval.".into(),
                A::Retry,
            ),
            C::PathOutsideSandbox => (
                "That file is outside your folders".into(),
                "Selene Retrieval can only open files inside folders you've added.".into(),
                A::ChooseFolder,
            ),
            C::PathRefused => (
                "This folder can't be added".into(),
                "It contains private system or account settings. Pick a folder inside it instead."
                    .into(),
                A::ChooseSubfolder(c.path.clone().unwrap_or_default()),
            ),
            C::PathDenied => (
                "This file is private".into(),
                "Files that usually hold passwords or keys (like .env or .pem) are never read."
                    .into(),
                A::None,
            ),
            C::FolderOverlap => (
                "Already included".into(),
                match &c.other {
                    Some(o) => format!("This folder is already part of \"{o}\"."),
                    None => "This folder is already part of another folder you added.".into(),
                },
                A::None,
            ),
            C::FolderUnavailable => (
                "Folder not available".into(),
                format!("{name} can't be reached. The drive may be unplugged or the folder moved."),
                A::ChooseFolder,
            ),
            C::PermissionDenied => (
                "No permission to read".into(),
                match &c.name {
                    Some(n) => format!("Windows didn't allow access to \"{n}\"."),
                    None if c.path.is_some() => {
                        format!("Windows didn't allow access to {quoted_file}.")
                    }
                    None => "Windows didn't allow access to this file.".into(),
                },
                A::Retry,
            ),
            C::FileTooLarge => (
                "File too large".into(),
                match &c.limit {
                    Some(l) => format!("{quoted_file} is bigger than the {l} limit and was skipped."),
                    None => format!("{quoted_file} is bigger than the size limit and was skipped."),
                },
                A::ViewSkippedFiles(c.folder_id),
            ),
            C::FileLocked => (
                "File is in use".into(),
                format!(
                    "Another program is using {}. It will be retried automatically.",
                    if c.path.is_some() { quoted_file.clone() } else { "this file".into() }
                ),
                A::Retry,
            ),
            C::FileOnlineOnly => (
                "File is online-only".into(),
                format!("{quoted_file} is stored only in OneDrive and was skipped."),
                A::OpenSettings(Section::Folder),
            ),
            C::FileUnreadable => (
                "Couldn't read this file".into(),
                format!(
                    "{quoted_file} may be damaged, password-protected or in an unsupported format."
                ),
                A::ViewSkippedFiles(c.folder_id),
            ),
            C::ExtractTimeout => (
                "File took too long".into(),
                format!("{quoted_file} took too long to read and was skipped."),
                A::ViewSkippedFiles(c.folder_id),
            ),
            C::ApprovalRequired => (
                "Waiting for your approval".into(),
                cap_first(match &c.path {
                    Some(p) => format!("{client} wants to add \"{}\".", p.display()),
                    None => format!("{client} wants to add a folder."),
                }),
                match c.approval_id {
                    Some(id) => A::ReviewApproval(id),
                    None => A::OpenSettings(Section::ConnectedApps),
                },
            ),
            C::ApprovalDenied => (
                "Request declined".into(),
                match (&c.client_name, c.client) {
                    (None, None) => "You declined a connected app's request, or it expired.".into(),
                    _ => format!("You declined {client}'s request, or it expired."),
                },
                A::None,
            ),
            C::Conflict => (
                "Still busy with that folder".into(),
                "This folder is being changed right now. Try again in a moment.".into(),
                A::WaitAndRetry(secs.unwrap_or(5)),
            ),
            C::Busy => (
                "Busy indexing".into(),
                "Selene Retrieval is catching up. Search still works; this action will run shortly."
                    .into(),
                A::WaitAndRetry(secs.unwrap_or(30)),
            ),
            C::Cancelled => ("Stopped".into(), "The operation was stopped.".into(), A::None),
            C::Timeout => (
                "That took too long".into(),
                "The operation didn't finish in time.".into(),
                A::Retry,
            ),
            C::ProviderNotConfigured => (
                "No AI provider set up".into(),
                "Answers and prompt improvement need an AI provider. Search works without one."
                    .into(),
                A::AddProvider,
            ),
            C::ProviderAuth => (
                format!("{your_provider} key was rejected"),
                "The key may be wrong, expired or revoked.".into(),
                A::ReenterKey(provider.clone()),
            ),
            C::ProviderQuota => (
                format!("{your_provider} account is out of credit"),
                format!("{your_provider} account has no remaining quota."),
                A::OpenSettings(Section::Providers),
            ),
            C::RateLimited => (
                cap_first(format!("{provider} is limiting requests")),
                match secs {
                    Some(s) => format!("Too many requests right now. Retrying in {s} seconds."),
                    None => "Too many requests right now. Retrying shortly.".into(),
                },
                A::WaitAndRetry(secs.unwrap_or(30)),
            ),
            C::ProviderUnavailable => (
                cap_first(format!("{provider} isn't responding")),
                "The service is having problems.".into(),
                A::Retry,
            ),
            C::ProviderRequest => (
                cap_first(format!("{provider} couldn't handle this request")),
                "The model may not exist anymore or the input was too long.".into(),
                A::OpenSettings(Section::Providers),
            ),
            C::PrivacyBlocked => (
                cap_first(format!("Not sent to {provider}")),
                format!("You haven't allowed sending {} to {provider}.", purpose.plain()),
                A::AllowRemote(provider.clone(), purpose),
            ),
            C::Offline => (
                "You're offline".into(),
                format!("Selene Retrieval couldn't reach {host}. Local search still works."),
                A::Retry,
            ),
            C::NetworkBlocked => (
                "Connection blocked".into(),
                format!("A proxy or firewall blocked the connection to {host}."),
                A::CopyDiagnostics,
            ),
            C::ModelDownloadFailed => (
                "Download didn't finish".into(),
                format!("The {model} download stopped. It will resume where it left off."),
                A::Retry,
            ),
            C::ModelIntegrity => (
                "Download was damaged".into(),
                format!("The downloaded {model} didn't pass its safety check and was deleted."),
                A::Retry,
            ),
            C::DiskFull => (
                "Not enough disk space".into(),
                match (&c.need, &c.drive) {
                    (Some(n), Some(d)) => {
                        format!("Indexing paused. About {n} more is needed on {d}.")
                    }
                    (Some(n), None) => format!("Indexing paused. About {n} more is needed."),
                    (None, Some(d)) => format!("Indexing paused. More space is needed on {d}."),
                    (None, None) => "Indexing paused. More disk space is needed.".into(),
                },
                A::FreeDiskSpace,
            ),
            C::Corrupt => (
                "Repairing search index".into(),
                "Part of the index was damaged and is being rebuilt. Results may be incomplete for a while."
                    .into(),
                A::None,
            ),
            C::ServiceUnavailable => (
                "Background service didn't start".into(),
                "Selene Retrieval's background service isn't running.".into(),
                A::RestartApp,
            ),
            C::UpgradeRequired => (
                "Finish updating".into(),
                format!(
                    "Selene Retrieval was updated. Restart {} to finish.",
                    match c.client {
                        Some(k) => k.display_name().to_string(),
                        None => c.client_name.clone().unwrap_or_else(|| "the connected app".into()),
                    }
                ),
                A::RestartClient(client_kind),
            ),
            C::SettingsInvalid => (
                "Some settings were reset".into(),
                "A few settings had invalid values and were set back to defaults.".into(),
                A::OpenSettings(c.section.unwrap_or(Section::General)),
            ),
            C::Internal => (
                "Something went wrong".into(),
                "An unexpected error happened.".into(),
                A::CopyDiagnostics,
            ),
            C::ModelIncompatible => (
                "Couldn't switch search model".into(),
                "The new search model didn't work as expected, so your current search was kept."
                    .into(),
                A::CopyDiagnostics,
            ),
            // R34 (M5-E): never the added-folder sentence; the index location lives in
            // Settings → Advanced → System (page 32), where "Move it back" / "Start fresh" are.
            C::IndexMissing => (
                "Search index not found".into(),
                match &c.name {
                    Some(n) => format!(
                        "Your search index is in \"{n}\", which can't be reached. Reconnect the drive, or move the index back."
                    ),
                    None => "Your search index is on a drive that can't be reached. Reconnect the drive, or move the index back.".into(),
                },
                A::OpenSettings(Section::System),
            ),
            // M5-E: policy.toml / registry policy. Nothing the user can change in the app.
            C::ManagedByPolicy => (
                "Managed by your organisation".into(),
                match (&c.name, &c.provider) {
                    (Some(n), _) => format!(
                        "Your organisation's settings don't allow \"{n}\" on this computer."
                    ),
                    (None, Some(p)) => format!(
                        "Your organisation's settings don't allow using {p} on this computer."
                    ),
                    (None, None) => {
                        "Your organisation's settings don't allow this on this computer.".into()
                    }
                },
                A::None,
            ),
            // S-01 (M5-E): a pipe squatter. Nothing was sent; only a restart frees the name.
            C::ConnectionNotTrusted => (
                "Connection not safe".into(),
                "Another program is using Selene Retrieval's connection, so nothing was sent. Restart Windows to fix this."
                    .into(),
                A::None,
            ),
            // S-03 (M5-E): the remembered port is taken. HTTP stays off; never another port.
            C::PortInUse => (
                "Port in use".into(),
                match &c.name {
                    Some(p) => format!(
                        "Port {p} is in use by another program, so HTTP for local tools is off and nothing was sent to it. Close that program, then try again."
                    ),
                    None => "The port for local tools is in use by another program, so HTTP for local tools is off and nothing was sent to it. Close that program, then try again.".into(),
                },
                A::Retry,
            ),
        };
        // M5-C (Amendments 10.2 / 10.3): providers that sign in through the user's own CLI have
        // no key and no "provider" to add: name the tool and point at its card on Providers & Keys
        // (the card offers the one action: the install link or [Sign in]).
        let (title, body, action) = match (
            e.code,
            c.provider
                .as_deref()
                .and_then(crate::api::cli_signin_by_label),
        ) {
            (C::ProviderNotConfigured, Some(p)) => (
                format!("{} isn't installed on this computer", p.tool),
                format!(
                    "{} uses your {} sign-in. Install {}, sign in to it, then try again.",
                    p.label, p.tool, p.tool
                ),
                A::OpenSettings(Section::Providers),
            ),
            (C::ProviderAuth, Some(p)) => (
                format!("{} isn't signed in", p.tool),
                format!(
                    "{} uses your {} sign-in on this computer. Sign in to {}, then try again.",
                    p.label, p.tool, p.tool
                ),
                A::OpenSettings(Section::Providers),
            ),
            _ => (title, body, action),
        };

        UserMessage {
            title,
            body,
            action,
            details: Details {
                code: e.code,
                chain: e.chain(),
                trace_id: e.trace_id,
            },
        }
    }
}
