//! Activity and "Sent to AI providers" (page 30, OQ-47) over `activity.list` / `activity.egress`
//! / `activity.note` (contract §8). Read-only: one plain sentence per entry; egress rows show only
//! the purpose and the size, never queries or file text.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::api::{ActivityEntry, ActivityKind, ActivityPage, EgressPage, EgressRow};
use sr_common::error::{ClientKind, Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::UiError;

/// Kind of icon the row shows (DS-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ActivityIcon {
    FolderAdded,
    FolderRemoved,
    UpToDate,
    Skipped,
    Upgraded,
    App,
    Request,
    Updated,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ActivityRow {
    pub id: f64,
    /// ms since the Unix epoch (day headings and the time column are formatted by the UI).
    pub ts_ms: f64,
    pub icon: ActivityIcon,
    /// One plain sentence, e.g. "“Documents” is up to date · 2,100 files".
    pub text: String,
    /// The folder the row is about (its name links to the folder detail when it still exists).
    pub folder_id: Option<String>,
    /// "{k} files skipped in …": the link goes to that folder's skipped list.
    pub to_skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ActivityPageView {
    pub rows: Vec<ActivityRow>,
    /// Paging key for "Show more" (`None` = no more).
    pub next_before_id: Option<f64>,
}

#[allow(clippy::cast_precision_loss)] // ids and ms timestamps stay far below 2^53
fn f(n: i64) -> f64 {
    n as f64
}

#[allow(clippy::cast_possible_truncation)] // paging keys come from our own f64 ids
fn back(n: f64) -> i64 {
    n as i64
}

fn client_word(kind: Option<ClientKind>, name: Option<&str>) -> String {
    match (kind, name) {
        (_, Some(n)) if !n.trim().is_empty() => n.to_owned(),
        (Some(ClientKind::ClaudeDesktop), _) => "Claude Desktop".into(),
        (Some(ClientKind::Cursor), _) => "Cursor".into(),
        (Some(ClientKind::VsCode), _) => "VS Code".into(),
        _ => "An app".into(),
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
}

/// The page 30 sentence for one entry (exact copy where the brief has it).
pub fn sentence(e: &ActivityEntry) -> String {
    let folder = e.folder_name.as_deref().unwrap_or("a folder");
    let client = client_word(e.client_kind, e.client_name.as_deref());
    let n = crate::state::thousands(e.count.unwrap_or(0));
    match e.kind {
        ActivityKind::FolderAdded => format!("Added “{folder}”"),
        ActivityKind::FolderRemoved => format!("Removed “{folder}”"),
        ActivityKind::FolderSynced => format!("“{folder}” is up to date · {n} files"),
        ActivityKind::FilesSkipped => format!("{n} files skipped in “{folder}”"),
        ActivityKind::SearchUpgraded => format!(
            "Search upgraded to {} search",
            e.text.as_deref().unwrap_or("better")
        ),
        ActivityKind::ClientConnected => format!("{client} connected"),
        ActivityKind::ClientDisconnected => format!("{client} disconnected"),
        ActivityKind::ClientFirstSeen => {
            format!("{client} used Selene Retrieval for the first time")
        }
        ActivityKind::ApprovalAllowed => {
            let name = e
                .text
                .as_deref()
                .map(base_name)
                .or(e.folder_name.as_deref())
                .unwrap_or("a folder");
            format!(
                "Allowed {} to add “{name}”",
                client_short(e.client_kind, &client)
            )
        }
        ActivityKind::ApprovalDeclined => format!(
            "Declined a folder request from {}",
            client_short(e.client_kind, &client)
        ),
        ActivityKind::ApprovalExpired => format!(
            "A folder request from {} expired",
            client_short(e.client_kind, &client)
        ),
        ActivityKind::Updated => format!(
            "Updated to version {}",
            e.text.as_deref().unwrap_or_default()
        ),
        ActivityKind::IndexMoved => format!(
            "Search index moved to {}",
            e.text.as_deref().unwrap_or_default()
        ),
    }
}

/// "Claude" rather than "Claude Desktop" inside approval sentences (page 30 copy).
fn client_short(kind: Option<ClientKind>, word: &str) -> String {
    match kind {
        Some(ClientKind::ClaudeDesktop) => "Claude".into(),
        _ => word.to_owned(),
    }
}

fn icon(k: ActivityKind) -> ActivityIcon {
    match k {
        ActivityKind::FolderAdded => ActivityIcon::FolderAdded,
        ActivityKind::FolderRemoved => ActivityIcon::FolderRemoved,
        ActivityKind::FolderSynced => ActivityIcon::UpToDate,
        ActivityKind::FilesSkipped => ActivityIcon::Skipped,
        ActivityKind::SearchUpgraded => ActivityIcon::Upgraded,
        ActivityKind::ClientConnected
        | ActivityKind::ClientDisconnected
        | ActivityKind::ClientFirstSeen => ActivityIcon::App,
        ActivityKind::ApprovalAllowed
        | ActivityKind::ApprovalDeclined
        | ActivityKind::ApprovalExpired => ActivityIcon::Request,
        ActivityKind::Updated => ActivityIcon::Updated,
        ActivityKind::IndexMoved => ActivityIcon::Moved,
    }
}

pub fn row(e: &ActivityEntry) -> ActivityRow {
    ActivityRow {
        id: f(e.id),
        ts_ms: f(e.ts),
        icon: icon(e.kind),
        text: sentence(e),
        folder_id: e.folder_id.as_ref().map(ToString::to_string),
        to_skipped: e.kind == ActivityKind::FilesSkipped,
    }
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, UiError> {
    serde_json::from_value(v).map_err(|e| {
        UiError::from(Error::new(ErrorCode::Internal).with_detail(format!("unexpected reply: {e}")))
    })
}

/// One page, newest first ("Show more" passes `next_before_id`). A read, not a poll: on page
/// open, on "Show more", and after a pushed `activity.added`.
pub async fn list<R: Rpc>(rpc: &R, before_id: Option<f64>) -> Result<ActivityPageView, UiError> {
    let v = rpc
        .call(
            method::ACTIVITY_LIST,
            json!({ "before_id": before_id.map(back), "limit": 50 }),
        )
        .await
        .map_err(UiError::from)?;
    let p: ActivityPage = parse(v)?;
    Ok(ActivityPageView {
        rows: p.entries.iter().map(row).collect(),
        next_before_id: p.next_before_id.map(f),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct EgressRowView {
    pub id: f64,
    pub ts_ms: f64,
    pub provider_name: String,
    /// The purpose in plain words (`Purpose::plain()`), e.g. "questions and matching file text".
    pub what: String,
    /// "12 KB".
    pub size: String,
    /// "This app" / "Claude Desktop" / …
    pub requested_by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ProviderChoice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct EgressPageView {
    pub rows: Vec<EgressRowView>,
    pub next_before_id: Option<f64>,
    /// Every provider ever used; empty = hide the "Sent to AI providers" card.
    pub providers: Vec<ProviderChoice>,
}

/// "12 KB", "3.4 MB", "820 bytes".
pub fn size_text(bytes: u64) -> String {
    match bytes {
        0..=999 => format!("{bytes} bytes"),
        1_000..=999_999 => format!("{} KB", bytes.div_ceil(1_000)),
        _ => {
            #[allow(clippy::cast_precision_loss)] // display only
            let mb = bytes as f64 / 1_000_000.0;
            format!("{mb:.1} MB")
        }
    }
}

fn egress_row(r: &EgressRow) -> EgressRowView {
    EgressRowView {
        id: f(r.id),
        ts_ms: f(r.ts),
        provider_name: r.provider_name.clone(),
        what: capitalise(r.purpose.plain()),
        size: size_text(r.bytes),
        requested_by: if r.client_name.trim().is_empty() {
            "This app".into()
        } else {
            r.client_name.clone()
        },
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

pub async fn egress<R: Rpc>(
    rpc: &R,
    before_id: Option<f64>,
    provider: Option<String>,
) -> Result<EgressPageView, UiError> {
    let v = rpc
        .call(
            method::ACTIVITY_EGRESS,
            json!({ "before_id": before_id.map(back), "limit": 50, "provider": provider }),
        )
        .await
        .map_err(UiError::from)?;
    let p: EgressPage = parse(v)?;
    Ok(EgressPageView {
        rows: p.rows.iter().map(egress_row).collect(),
        next_before_id: p.next_before_id.map(f),
        providers: p
            .providers
            .iter()
            .map(|x| ProviderChoice {
                id: x.id.clone(),
                name: x.name.clone(),
            })
            .collect(),
    })
}

/// Records an app-side Connect / Disconnect (the config writers run here, not in the service).
/// Best effort: a failure never blocks the Connect itself.
pub async fn note<R: Rpc>(rpc: &R, connected: bool, kind: ClientKind) {
    let k = if connected {
        "client_connected"
    } else {
        "client_disconnected"
    };
    let r = rpc
        .call(
            method::ACTIVITY_NOTE,
            json!({ "kind": k, "client_kind": kind }),
        )
        .await;
    if let Err(e) = r {
        tracing::debug!(code = ?e.code, "activity note not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;
    use sr_common::error::Purpose;

    fn entry(kind: ActivityKind) -> ActivityEntry {
        ActivityEntry {
            id: 7,
            ts: 1_700_000_000_000,
            kind,
            folder_id: None,
            folder_name: Some("Documents".into()),
            client_kind: Some(ClientKind::ClaudeDesktop),
            client_name: Some("Claude Desktop".into()),
            count: Some(2_100),
            text: None,
        }
    }

    #[test]
    fn sentences_follow_the_brief() {
        assert_eq!(
            sentence(&entry(ActivityKind::FolderAdded)),
            "Added “Documents”"
        );
        assert_eq!(
            sentence(&entry(ActivityKind::FolderRemoved)),
            "Removed “Documents”"
        );
        assert_eq!(
            sentence(&entry(ActivityKind::FolderSynced)),
            "“Documents” is up to date · 2,100 files"
        );
        assert_eq!(
            sentence(&entry(ActivityKind::FilesSkipped)),
            "2,100 files skipped in “Documents”"
        );
        assert_eq!(
            sentence(&entry(ActivityKind::ClientConnected)),
            "Claude Desktop connected"
        );
        let mut a = entry(ActivityKind::ApprovalAllowed);
        a.text = Some(r"C:\Users\Ana\Contracts".into());
        assert_eq!(sentence(&a), "Allowed Claude to add “Contracts”");
        assert_eq!(
            sentence(&entry(ActivityKind::ApprovalDeclined)),
            "Declined a folder request from Claude"
        );
        let mut u = entry(ActivityKind::SearchUpgraded);
        u.text = Some("Better".into());
        assert_eq!(sentence(&u), "Search upgraded to Better search");
        let mut v = entry(ActivityKind::Updated);
        v.text = Some("0.2.0".into());
        assert_eq!(sentence(&v), "Updated to version 0.2.0");
        assert!(row(&entry(ActivityKind::FilesSkipped)).to_skipped);
    }

    #[test]
    fn sizes_and_purposes_are_plain() {
        assert_eq!(size_text(820), "820 bytes");
        assert_eq!(size_text(12_000), "12 KB");
        assert_eq!(size_text(3_400_000), "3.4 MB");
        let r = egress_row(&EgressRow {
            id: 1,
            ts: 2,
            provider: "p".into(),
            provider_name: "Provider".into(),
            purpose: Purpose::Answer,
            bytes: 12_000,
            client_id: None,
            client_name: String::new(),
        });
        assert_eq!(r.requested_by, "This app");
        assert!(
            r.what.chars().next().is_some_and(char::is_uppercase),
            "{}",
            r.what
        );
    }

    #[tokio::test]
    async fn paging_and_notes_forward() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"entries": [], "next_before_id": null})));
        let p = list(&rpc, Some(42.0)).await.unwrap();
        assert!(p.rows.is_empty() && p.next_before_id.is_none());
        assert_eq!(rpc.calls()[0].1, json!({"before_id": 42, "limit": 50}));
        note(&rpc, true, ClientKind::Cursor).await;
        assert_eq!(
            rpc.calls()[1],
            (
                "activity.note".to_owned(),
                json!({"kind": "client_connected", "client_kind": "cursor"})
            )
        );
    }
}
