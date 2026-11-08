//! M5: settings managed by the organisation (page 33, `policy.toml` / registry policy), the
//! "search index not found" recovery screen (R34) and the OneDrive folder note (DS-5.3).
//!
//! Contract: M5-E's additions to `sr_common` (`EngineStatus.managed: [ManagedField]`,
//! `EngineStatus.index_missing: IndexMissingInfo`, `index_location.recover {choice}`, error codes
//! `IndexMissing` / `ManagedByPolicy`). They are read from the **raw** snapshot JSON under their
//! wire names, like `state::wire` reads M3 additions, so that (a) an older service simply never
//! sends them, and (b) the recovery-mode snapshot is shown even if the rest of it doesn't parse.
//!
//! OneDrive: `cloud_note` and the per-folder `online_only_skipped` count are published by M5-E
//! (`FolderInfo`, `FolderSummary` / `FolderStatusSummary`; the count is omitted when 0). An older
//! service sends neither; the app then counts the skipped rows it shows.
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::error::{Error, ErrorCode};

use crate::daemon::Rpc;
use crate::dto::{UiAction, UiError};

/// Wire names.
pub mod wire {
    /// `EngineStatus.managed`: `[{ "path", "message", "detail"? }]` (M5-E).
    pub const MANAGED: &str = "managed";
    /// `EngineStatus.index_missing`: `{ "path", "drive"?, "old_copy"?, "folders_to_restore" }`.
    pub const INDEX_MISSING: &str = "index_missing";
    /// The dedicated error code (serialised `ErrorCode`), for its renderer text.
    pub const INDEX_MISSING_CODE: &str = "IndexMissing";
    /// App-RPC `{ "choice": "move_back" | "start_fresh" }` (privileged; recovery mode).
    pub const RECOVER: &str = "index_location.recover";
    /// Folder rows / folder detail: files skipped as online-only OneDrive files (M5-E, u64).
    pub const ONLINE_ONLY_SKIPPED: &str = "online_only_skipped";
    /// Folder rows / folder detail: the service's plain OneDrive note (published by M5-E).
    pub const CLOUD_NOTE: &str = "cloud_note";
}

/// The one plain line next to a locked setting (= `sr_common::config::policy::MANAGED_TEXT`).
pub const MANAGED_TEXT: &str = "Managed by your organisation";

/// One setting the organisation set (page 33).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ManagedFieldView {
    /// Dotted settings path, or one of M5-E's pseudo paths (`provider.remote`,
    /// `search.model_downloads`, `folders.allowed_roots`, …).
    pub path: String,
    /// "Managed by your organisation".
    pub message: String,
    /// Plain extra line, e.g. "Allowed: Ollama, OpenAI" or the allowed folder roots.
    pub detail: Option<String>,
}

/// What the organisation's policy sets. Empty = no policy (the default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PolicyView {
    pub managed: Vec<ManagedFieldView>,
}

impl PolicyView {
    /// `path` itself, or a section/prefix that covers it (`"answers"` covers
    /// `"answers.deeper_answers"`).
    pub fn is_managed(&self, path: &str) -> bool {
        self.managed.iter().any(|m| {
            let m = m.path.trim_end_matches(".*");
            path == m
                || path
                    .strip_prefix(m)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }
}

/// `managed` array → view. Entries without a path are dropped; the message falls back to the
/// fixed line (never empty on screen).
pub fn policy_view(v: Option<&Value>) -> PolicyView {
    let managed = v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let path = m
                .as_str()
                .or_else(|| m.get("path").and_then(Value::as_str))?
                .trim();
            if path.is_empty() {
                return None;
            }
            let message = m
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(MANAGED_TEXT);
            Some(ManagedFieldView {
                path: path.to_owned(),
                message: message.to_owned(),
                detail: m
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect();
    PolicyView { managed }
}

/// R34: the moved search index can't be found; the service waits in its recovery mode and
/// starts by itself once the drive is back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexMissingView {
    /// Where the index should be (the folder the user moved it to).
    pub path: String,
    /// Its drive ("D:"), when it has one.
    pub drive: Option<String>,
    /// A complete earlier copy [Move the index back] can use; `None` → only [Start fresh].
    pub old_copy: Option<String>,
    /// Folders [Start fresh] adds back and reads again.
    pub folders_to_restore: u32,
    /// The renderer's title and body for the dedicated code (no action: the screen has its own).
    pub error: UiError,
}

fn missing_error(path: &str) -> UiError {
    // `IndexMissing` (M5-E) by its wire name, so this compiles against a service crate with or
    // without it; the closest older code otherwise. Never a new error string in the app.
    let code = serde_json::from_value::<ErrorCode>(json!(wire::INDEX_MISSING_CODE))
        .unwrap_or(ErrorCode::FolderUnavailable);
    let mut e = Error::new(code);
    if !path.is_empty() {
        // The renderer names the place ("Your search index is in "{name}", which can't be
        // reached. …").
        e.ctx.name = Some(path.to_owned());
    }
    let mut u = UiError::from(e);
    u.action = UiAction::None;
    u.action_label = String::new();
    u
}

/// From the snapshot's `index_missing` object (`None` when absent or without a path).
pub fn index_missing_view(v: Option<&Value>) -> Option<IndexMissingView> {
    let m = v.filter(|m| m.is_object())?;
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let path = s("path")?;
    Some(IndexMissingView {
        drive: s("drive"),
        old_copy: s("old_copy"),
        folders_to_restore: m
            .get("folders_to_restore")
            .and_then(Value::as_u64)
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
        error: missing_error(&path),
        path,
    })
}

/// [Move the index back] / [Start fresh] (R34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecoverChoice {
    /// Use the earlier complete copy again (changes since the move are picked up by a re-scan).
    MoveBack,
    /// An empty index in the usual place; the remembered folders are read again. The missing
    /// copy isn't touched, and neither are the user's files.
    StartFresh,
}

pub async fn recover<R: Rpc>(rpc: &R, choice: RecoverChoice) -> Result<(), UiError> {
    rpc.call(wire::RECOVER, json!({ "choice": choice }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

/// OneDrive files skipped as online-only, and the service's OneDrive note, from a raw folder row
/// (both published by M5-E, see [`wire`]).
pub fn cloud_fields(row: &Value) -> (Option<u64>, Option<String>) {
    let n = row.get(wire::ONLINE_ONLY_SKIPPED).and_then(Value::as_u64);
    let note = row
        .get(wire::CLOUD_NOTE)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (n, note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;

    #[test]
    fn managed_fields_paths_sections_and_detail() {
        let p = policy_view(Some(&json!([
            {"path": "integrations.http_server_enabled", "message": "Managed by your organisation"},
            {"path": "provider.provider", "message": "", "detail": "Allowed: Ollama"},
            {"path": "answers"},
            {"path": "  "},
            "search.model_downloads"
        ])));
        assert_eq!(p.managed.len(), 4, "the empty path is dropped");
        assert!(p.is_managed("integrations.http_server_enabled"));
        assert!(
            p.is_managed("answers.deeper_answers"),
            "a section covers its fields"
        );
        assert!(
            !p.is_managed("answers_extra.x"),
            "a prefix must end at a dot"
        );
        assert!(!p.is_managed("general.start_with_windows"));
        assert_eq!(p.managed[1].message, MANAGED_TEXT, "never an empty line");
        assert_eq!(p.managed[1].detail.as_deref(), Some("Allowed: Ollama"));
        assert_eq!(policy_view(None), PolicyView::default());
        assert_eq!(policy_view(Some(&json!({"x": 1}))), PolicyView::default());
    }

    #[test]
    fn index_missing_view_uses_the_renderer_text_without_its_action() {
        let v = index_missing_view(Some(&json!({
            "path": r"D:\SeleneRetrieval Index", "drive": "D:",
            "old_copy": r"C:\Users\a\AppData\Local\SeleneRetrieval", "folders_to_restore": 3
        })))
        .unwrap();
        assert_eq!(v.drive.as_deref(), Some("D:"));
        assert_eq!(v.folders_to_restore, 3);
        assert!(v.old_copy.is_some());
        assert_eq!(
            v.error.action,
            UiAction::None,
            "the screen has its own choices"
        );
        assert!(!v.error.title.is_empty() && !v.error.body.is_empty());
        assert!(
            !v.error.body.contains("add the folder"),
            "never the added-folder sentence (R34)"
        );
        assert!(index_missing_view(Some(&json!({"path": ""}))).is_none());
        assert!(index_missing_view(None).is_none());
    }

    #[tokio::test]
    async fn recover_sends_the_choice() {
        let rpc = FakeRpc::default();
        recover(&rpc, RecoverChoice::StartFresh).await.unwrap();
        assert_eq!(
            rpc.calls()[0],
            (wire::RECOVER.to_owned(), json!({"choice": "start_fresh"}))
        );
    }

    #[test]
    fn cloud_fields_from_rows() {
        let (n, note) =
            cloud_fields(&json!({"online_only_skipped": 4, "cloud_note": "  In OneDrive. "}));
        assert_eq!((n, note.as_deref()), (Some(4), Some("In OneDrive.")));
        assert_eq!(cloud_fields(&json!({})), (None, None));
    }
}
